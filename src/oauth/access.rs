//! OAuth access tokens on the resource side: an in-memory cache over
//! `oauth_tokens`, the audience check (RFC 8707), and the grant check
//! through the key cache, behind [`BearerVerifier`].
//!
//! As with API keys (`auth`), tokens are looked up by their SHA-256 in a
//! `HashMap`; a timing difference tells an attacker only how a guess's
//! *hash* compares with stored ones, which does not help find a token.
//!
//! With `[redis]` the cache is one of several replicas' (spec Decision 15).
//! Every revocation bumps a counter in Redis, and every lookup reads it
//! first: a value this instance has not seen clears its cache. A Redis
//! error skips the cache for that request, and an entry cached with Redis on
//! is trusted for [`SHARED_MAX_AGE`] at most, a backstop for a crash between
//! a revocation's commit and its bump.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::app::App;
use crate::auth::{BearerVerifier, Principal};
use crate::error::OkfError;
use crate::keys;
use crate::oauth::OAuth;
use crate::shared::Redis;
use crate::store::oauth::tokens::{self, LiveAccessToken};
use crate::telemetry::metrics::RedisOp;

/// The most access tokens cached at once. Past it, expired entries are
/// pruned; if it is still full, a token is checked against the database each
/// time instead of cached.
const MAX_CACHED: usize = 10_000;

/// The key space and name of the Redis counter every revocation bumps
/// (`{<key_prefix>}oauth:access-revocations`, spec Decision 15).
const REVOCATIONS_SPACE: &str = "oauth";
const REVOCATIONS_NAME: &str = "access-revocations";

/// With Redis on, how long a cached token is trusted without a database look.
pub const SHARED_MAX_AGE: Duration = Duration::from_secs(60);

/// Reads the counter, first creating it from `ARGV[1]` (a random seed) if it
/// is missing. A counter Redis lost therefore never restarts at a value an
/// instance has already seen (review of PR #552). Redis runs one script at a
/// time, so two instances seeding at once cannot interleave.
const READ_REVOCATIONS: &str = "if redis.call('EXISTS', KEYS[1]) == 0 then\n\
        redis.call('SET', KEYS[1], ARGV[1])\n\
    end\n\
    return redis.call('GET', KEYS[1])";

/// Bumps the counter, seeding it the same way first. The new value goes back
/// through `GET`, as a string: Lua holds a command's integer reply as a
/// double, so returning `INCR`'s reply would round a value above 2^53.
const BUMP_REVOCATIONS: &str = "if redis.call('EXISTS', KEYS[1]) == 0 then\n\
        redis.call('SET', KEYS[1], ARGV[1])\n\
    end\n\
    redis.call('INCR', KEYS[1])\n\
    return redis.call('GET', KEYS[1])";

// `LazyLock`: built on first use, then shared (Python: a module-level value
// computed lazily). `Script` sends `EVALSHA`, falling back to `EVAL`.
static READ_REVOCATIONS_SCRIPT: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(READ_REVOCATIONS));
static BUMP_REVOCATIONS_SCRIPT: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(BUMP_REVOCATIONS));

/// One cached token and when it entered the cache.
#[derive(Debug, Clone)]
struct Cached {
    token: LiveAccessToken,
    // A monotonic instant, so a wall-clock jump cannot keep an entry young.
    cached_at: Instant,
}

/// The cache's contents: the tokens, and how many revocations it has seen.
#[derive(Debug, Default)]
struct Cache {
    tokens: HashMap<[u8; 32], Cached>,
    // Bumped by every `forget`, and by every change of the Redis counter. A
    // lookup records it before reading the database and may cache its result
    // only if it has not moved, so a revocation that commits while the lookup
    // is in flight cannot be undone by the lookup re-caching the token it read
    // before the revocation (PR #243 review).
    generation: u64,
    // The Redis revocation counter as this instance last read it; `None`
    // before the first read, or without Redis.
    seen_revocations: Option<i64>,
}

/// Live access tokens seen recently, by SHA-256.
#[derive(Debug, Default)]
pub struct AccessTokens {
    // A `std` mutex, held only to read or change the cache, never across an
    // `.await` (each use below is its own statement or block).
    cache: Mutex<Cache>,
    // The shared revocation counter's home, with `[redis]`.
    redis: Option<Redis>,
}

impl AccessTokens {
    /// An empty cache.
    ///
    /// # Arguments
    /// * `redis` - the shared Redis, with `[redis]`: revocations are then
    ///   counted there and every lookup checks the count.
    ///
    /// # Returns
    /// The cache.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial constructor: builds a value from its arguments"
        )
    )]
    pub fn new(redis: Option<Redis>) -> Self {
        Self {
            cache: Mutex::default(),
            redis,
        }
    }

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    fn lock(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().expect("access token cache lock poisoned")
    }

    /// The revocation generation. Read it before a database lookup or before
    /// minting, and pass it to [`Self::remember`].
    ///
    /// # Returns
    /// How many times [`Self::forget`] has run, plus how many changes of the
    /// Redis counter this instance has seen.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
        )
    )]
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Caches a live token, unless a revocation happened since
    /// `seen_generation` was read: then the token may be the one just
    /// revoked, and the next request rechecks the database instead.
    ///
    /// # Arguments
    /// * `sha256` - `keys::hash` of the token.
    /// * `token` - its facts.
    /// * `now` - the current time, to prune expired entries when full.
    /// * `seen_generation` - [`Self::generation`], read before the token was
    ///   looked up or minted.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn remember(
        &self,
        sha256: [u8; 32],
        token: LiveAccessToken,
        now: DateTime<Utc>,
        seen_generation: u64,
    ) {
        // The check and the insert happen under one lock, so no `forget` can
        // fall between them.
        let mut cache = self.lock();
        if cache.generation != seen_generation {
            return;
        }
        if cache.tokens.len() >= MAX_CACHED {
            cache
                .tokens
                .retain(|_, cached| cached.token.expires_at > now);
            if cache.tokens.len() >= MAX_CACHED {
                return;
            }
        }
        cache.tokens.insert(
            sha256,
            Cached {
                token,
                cached_at: Instant::now(),
            },
        );
    }

    /// Drops tokens from the cache and bumps the generation, so a revoked
    /// token stops working at once, and no lookup already in flight can cache
    /// it again. With Redis, then bumps the shared counter, so every other
    /// instance clears its cache at its next lookup. Call it after the
    /// revocation commits.
    ///
    /// A failed bump is counted and logged; the other instances then stop
    /// trusting their cached copies within [`SHARED_MAX_AGE`].
    ///
    /// # Arguments
    /// * `hashes` - the revoked tokens' hashes.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn forget(&self, hashes: &[[u8; 32]]) {
        {
            let mut cache = self.lock();
            // `wrapping_add`: a u64 that never realistically wraps, but if it
            // did, a wrapped value still differs from every recent reading.
            cache.generation = cache.generation.wrapping_add(1);
            for hash in hashes {
                cache.tokens.remove(hash);
            }
        }
        // The block above released the lock before this `.await`. The bump's
        // result is not stored as seen: another instance's bump may fall
        // between ours and our next read, and recording a value this instance
        // did not read would hide it.
        if let Some(redis) = &self.redis {
            // The error is already counted and logged by `run_counter`.
            let _ = run_counter(redis, &BUMP_REVOCATIONS_SCRIPT).await;
        }
    }

    /// The principal of an access token.
    ///
    /// # Arguments
    /// * `app` - the shared state (database, clock, key cache).
    /// * `resource` - this server's resource identifier (`<public_url>/mcp`).
    /// * `token` - the presented `okfat_` token.
    ///
    /// # Returns
    /// The grant's principal; `None` for an unknown, revoked or expired
    /// token, one issued for another resource, or one whose grant is revoked.
    ///
    /// # Errors
    /// [`OkfError::Database`] from the lookup on a cache miss.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn verify(
        &self,
        app: &App,
        resource: &str,
        token: &str,
    ) -> Result<Option<Arc<Principal>>, OkfError> {
        let sha256 = keys::hash(token);
        let now = app.clock().now();
        // With Redis, the shared counter first (spec Decision 15). `None`:
        // no Redis. `Some(Err(_))`: Redis failed, so this request neither
        // trusts nor fills the cache.
        let revocations = match &self.redis {
            Some(redis) => Some(run_counter(redis, &READ_REVOCATIONS_SCRIPT).await),
            None => None,
        };
        let use_cache = !matches!(revocations, Some(Err(_)));
        // Under one lock: take in the counter, then read the cache and the
        // generation, before the database read, so a revocation committed
        // after this point bumps the generation. The block's guard is
        // released at its `}`, before the `.await`.
        let (cached, generation) = {
            let mut cache = self.lock();
            if let Some(Ok(revocations)) = revocations {
                if cache.seen_revocations != Some(revocations) {
                    // A revocation somewhere (or a counter Redis lost and
                    // re-seeded): every cached entry may be revoked.
                    cache.tokens.clear();
                    cache.generation = cache.generation.wrapping_add(1);
                    cache.seen_revocations = Some(revocations);
                }
            }
            let cached = if use_cache {
                cache
                    .tokens
                    .get(&sha256)
                    // With Redis, an old entry is a miss (the backstop).
                    .filter(|cached| {
                        self.redis.is_none() || cached.cached_at.elapsed() < SHARED_MAX_AGE
                    })
                    .map(|cached| cached.token.clone())
            } else {
                None
            };
            (cached, cache.generation)
        };
        let live = match cached {
            Some(live) => live,
            None => match tokens::find_access(app.accounts().executor(), &sha256, now).await? {
                Some(live) => {
                    if use_cache {
                        self.remember(sha256, live.clone(), now, generation);
                    }
                    live
                }
                None => return Ok(None),
            },
        };
        if live.expires_at <= now {
            // Expiry is not a revocation: drop the entry without bumping the
            // generation, so lookups of other tokens in flight still cache.
            self.lock().tokens.remove(&sha256);
            return Ok(None);
        }
        // The audience check (RFC 8707): a token issued for another
        // resource is not a credential here.
        if live.resource != resource {
            return Ok(None);
        }
        // The credential cache reads a grant it does not hold, so one made on
        // another replica is found at once.
        app.find_grant(&live.grant_id).await
    }
}

/// A random seed for the revocation counter: non-negative and below 2^62,
/// which leaves room to `INCR` without overflow.
///
/// # Errors
/// [`OkfError::Io`] when the OS random source fails.
#[tracing::instrument(level = "trace", skip_all)]
fn counter_seed() -> Result<i64, OkfError> {
    let random = getrandom::u64().map_err(|error| OkfError::Io {
        path: "<os random source>".to_string(),
        source: std::io::Error::other(error),
    })?;
    // `>> 2` keeps 62 random bits, so the value fits an `i64` below 2^62.
    Ok(i64::try_from(random >> 2).expect("62 bits fit an i64"))
}

/// Runs a counter script (read or bump) with a fresh random seed.
///
/// # Arguments
/// * `redis` - the shared Redis.
/// * `script` - [`READ_REVOCATIONS_SCRIPT`] or [`BUMP_REVOCATIONS_SCRIPT`].
///
/// # Returns
/// The counter's value after the script.
///
/// # Errors
/// [`OkfError::Busy`] when Redis fails (counted and logged as `counter`), or
/// [`OkfError::Io`] when no seed could be drawn (counted the same way).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "redis", db.operation.name = "counter"))]
async fn run_counter(redis: &Redis, script: &redis::Script) -> Result<i64, OkfError> {
    let seed = counter_seed().inspect_err(|_| redis.count(RedisOp::Counter))?;
    script
        .key(redis.key(REVOCATIONS_SPACE, REVOCATIONS_NAME))
        .arg(seed)
        .invoke_async(&mut redis.connection())
        .await
        .map_err(|error| redis.failed(RedisOp::Counter, &error))
}

/// The OAuth implementation of [`BearerVerifier`]: borrows what the lookup
/// needs for one request.
pub struct OAuthVerifier<'a> {
    /// The shared state.
    pub app: &'a App,
    /// The OAuth state (its settings give the resource).
    pub oauth: &'a OAuth,
}

impl BearerVerifier for OAuthVerifier<'_> {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "one-line delegation to an instrumented function"
        )
    )]
    async fn verify(&self, token: &str) -> Result<Option<Arc<Principal>>, OkfError> {
        self.oauth
            .access
            .verify(self.app, &self.oauth.settings.resource, token)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    use crate::auth::{authenticate, CredentialKind};
    use crate::mcp::testing::{app_with, TestApp};
    use crate::store::keys::{self as key_rows, Access};
    use crate::store::oauth::testing::{insert_client, insert_grant, mint_access, oauth_config};

    const RESOURCE: &str = "https://okf.example.com/mcp";

    /// An OAuth-on app with one grant on `main`.
    async fn with_grant() -> (TestApp, String) {
        let test = app_with(&[("main", &[])], oauth_config("https://okf.example.com")).await;
        let now = test.app.clock().now();
        let client = insert_client(
            test.app.accounts().executor(),
            "https://client.example.com/client.json",
            "Example Client",
            now,
        )
        .await;
        let grant = insert_grant(
            test.app.accounts().executor(),
            &client,
            Access::Write,
            &["main"],
            now,
        )
        .await;
        (test, grant)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_token_for_this_resource_acts_as_its_grant() {
        let (test, grant) = with_grant().await;
        let token = mint_access(&test.app, &grant, RESOURCE, TimeDelta::hours(1)).await;

        let principal = authenticate(&test.app, &token)
            .await
            .unwrap()
            .expect("admitted");
        // The second call is served from the cache: the same answer.
        let again = authenticate(&test.app, &token)
            .await
            .unwrap()
            .expect("admitted");

        assert_eq!(principal.key_id, grant);
        assert_eq!(principal.access, Access::Write);
        assert!(principal.reaches("main"));
        assert_eq!(principal.actor, "example-actor");
        assert_eq!(
            principal.kind,
            CredentialKind::OAuth {
                client_id: Some("https://client.example.com/client.json".into()),
                client_name: Some("Example Client".into())
            }
        );
        assert_eq!(again, principal);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_audience_expired_revoked_and_unknown_tokens_are_refused() {
        let (test, grant) = with_grant().await;
        let other = mint_access(
            &test.app,
            &grant,
            "https://other.example.com/mcp",
            TimeDelta::hours(1),
        )
        .await;
        let expired = mint_access(&test.app, &grant, RESOURCE, -TimeDelta::minutes(1)).await;
        let revoked = mint_access(&test.app, &grant, RESOURCE, TimeDelta::hours(1)).await;
        // Seen once (cached), then revoked: `forget` makes it stop at once.
        assert!(authenticate(&test.app, &revoked).await.unwrap().is_some());
        crate::store::oauth::tokens::revoke(
            test.app.accounts().executor(),
            &crate::keys::hash(&revoked),
            test.app.clock().now(),
        )
        .await
        .unwrap();
        test.app
            .oauth()
            .unwrap()
            .access
            .forget(&[crate::keys::hash(&revoked)])
            .await;
        let unknown = crate::keys::generate_with(crate::keys::ACCESS_TOKEN_PREFIX).unwrap();

        for token in [other, expired, revoked, unknown] {
            assert!(authenticate(&test.app, &token).await.unwrap().is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoking_the_grant_refuses_its_cached_token_after_a_reload() {
        let (test, grant) = with_grant().await;
        let token = mint_access(&test.app, &grant, RESOURCE, TimeDelta::hours(1)).await;
        assert!(authenticate(&test.app, &token).await.unwrap().is_some());

        crate::invalidate::testing::applied(&test.app, async {
            key_rows::revoke_by_id(
                test.app.accounts().executor(),
                &grant,
                &crate::store::audit::AuditActor::cli(),
                test.app.clock().now(),
            )
            .await
            .unwrap();
        })
        .await;

        assert!(authenticate(&test.app, &token).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_access_token_reaches_the_api_through_the_router() {
        let (test, grant) = with_grant().await;
        let token = mint_access(&test.app, &grant, RESOURCE, TimeDelta::hours(1)).await;

        let (status, body) =
            crate::http::api::testing::send(&test, "GET", "/api/v1/whoami", Some(&token), None)
                .await;

        assert_eq!(status.as_u16(), 200);
        assert_eq!(body["key_id"], grant.as_str());
        assert_eq!(body["kind"], "oauth");
        assert_eq!(body["client_name"], "Example Client");
        assert_eq!(body["access"], "write");
    }

    #[tokio::test]
    async fn remember_prunes_expired_entries_when_full() {
        let tokens = AccessTokens::new(None);
        let now = chrono::Utc::now();
        let live = |expires_at| LiveAccessToken {
            grant_id: "g".into(),
            resource: RESOURCE.into(),
            expires_at,
        };
        for index in 0..MAX_CACHED {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&(index as u64).to_le_bytes());
            tokens.remember(hash, live(now - TimeDelta::seconds(1)), now, 0);
        }

        tokens.remember([0xff; 32], live(now + TimeDelta::hours(1)), now, 0);

        let cache = tokens.lock();
        assert_eq!(cache.tokens.len(), 1, "the expired entries were pruned");
        assert!(cache.tokens.contains_key(&[0xff; 32]));
    }

    #[tokio::test]
    async fn a_lookup_that_straddles_a_revocation_does_not_re_cache_the_token() {
        // The interleaving from the PR #243 review, made deterministic by
        // calling the cache's steps in that order: a request misses and reads
        // the generation, the revocation commits and forgets, then the first
        // request tries to cache what it read before the revocation.
        let tokens = AccessTokens::new(None);
        let now = chrono::Utc::now();
        let hash = [0x11; 32];
        let token = LiveAccessToken {
            grant_id: "g".into(),
            resource: RESOURCE.into(),
            expires_at: now + TimeDelta::hours(1),
        };
        let seen = tokens.generation();

        tokens.forget(&[hash]).await;
        tokens.remember(hash, token.clone(), now, seen);

        assert!(!tokens.lock().tokens.contains_key(&hash), "not re-cached");
        // A lookup that starts after the revocation caches normally (the
        // database, not the cache, then says whether the token is live).
        tokens.remember(hash, token, now, tokens.generation());
        assert!(tokens.lock().tokens.contains_key(&hash));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revoked_token_read_before_its_revocation_is_refused_next_time() {
        // The same race end to end: a lookup reads the token while it is
        // live, the revocation commits and forgets, then the stale lookup's
        // `remember` runs with the generation it read. The next verify goes
        // to the database, which says revoked.
        let (test, grant) = with_grant().await;
        let oauth = test.app.oauth().expect("OAuth is on");
        let token = mint_access(&test.app, &grant, RESOURCE, TimeDelta::hours(1)).await;
        let hash = crate::keys::hash(&token);
        let now = test.app.clock().now();
        let seen = oauth.access.generation();
        let stale = tokens::find_access(test.app.accounts().executor(), &hash, now)
            .await
            .unwrap()
            .expect("live before the revocation");

        tokens::revoke(test.app.accounts().executor(), &hash, now)
            .await
            .unwrap();
        oauth.access.forget(&[hash]).await;
        oauth.access.remember(hash, stale, now, seen);

        let after = oauth
            .access
            .verify(&test.app, RESOURCE, &token)
            .await
            .unwrap();
        assert!(
            after.is_none(),
            "the revoked token is not accepted from the cache"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cached_token_past_its_expiry_is_refused_and_dropped() {
        // A token that entered the cache while live and has since expired.
        // It is not in the database, so only the cache-expiry check can
        // refuse it: without that check, `verify` would admit the live grant.
        let (test, grant) = with_grant().await;
        let oauth = test.app.oauth().expect("OAuth is on");
        let token = keys::generate_with(keys::ACCESS_TOKEN_PREFIX).unwrap();
        let hash = keys::hash(&token);
        let now = test.app.clock().now();
        let generation = oauth.access.generation();
        oauth.access.remember(
            hash,
            LiveAccessToken {
                grant_id: grant,
                resource: RESOURCE.into(),
                expires_at: now - TimeDelta::seconds(1),
            },
            now,
            generation,
        );
        assert!(oauth.access.lock().tokens.contains_key(&hash), "cached");

        let after = oauth
            .access
            .verify(&test.app, RESOURCE, &token)
            .await
            .unwrap();

        assert!(after.is_none(), "an expired cached token is refused");
        assert!(
            !oauth.access.lock().tokens.contains_key(&hash),
            "and dropped"
        );
        assert_eq!(
            oauth.access.generation(),
            generation,
            "expiry is not a revocation"
        );
    }

    // --- Several replicas (spec Decisions 14 and 15) ---

    use crate::db::redis_support::TestRedis;
    use crate::oauth::testing::{
        authorization_code, exchange_for_tokens, form, oauth_replica_pair, oauth_replica_pair_on,
        send,
    };

    /// `verify` on `app` for this server's resource.
    async fn verify_on(app: &App, token: &str) -> Option<Arc<Principal>> {
        app.oauth()
            .expect("OAuth is on")
            .access
            .verify(app, RESOURCE, token)
            .await
            .expect("verify answers")
    }

    /// Whether `app`'s cache holds `token`.
    fn cached_on(app: &App, token: &str) -> bool {
        app.oauth()
            .expect("OAuth is on")
            .access
            .lock()
            .tokens
            .contains_key(&keys::hash(token))
    }

    /// How many tokens `app`'s cache holds.
    fn cache_len(app: &App) -> usize {
        app.oauth().expect("OAuth is on").access.lock().tokens.len()
    }

    /// Approves and exchanges a code on `a`; returns the access token.
    async fn access_token_on(a: &TestApp) -> String {
        let code = authorization_code(a).await;
        exchange_for_tokens(a, &code).await.0
    }

    /// Revokes `token` through `a`'s `POST /oauth/revoke`.
    async fn revoke_through(a: &TestApp, token: &str) {
        let (status, _, _) =
            send(a, "POST", "/oauth/revoke", Some(form(&[("token", token)]))).await;
        assert_eq!(status.as_u16(), 200, "the revocation answers 200");
    }

    /// Revokes `token`'s row in Postgres only: no `forget`, no counter bump,
    /// as a crash between the commit and the bump would leave it.
    async fn revoke_row_only(app: &App, token: &str) {
        tokens::revoke(
            app.accounts().executor(),
            &keys::hash(token),
            app.clock().now(),
        )
        .await
        .expect("revoke the row");
    }

    /// The counter's key and a raw connection on `server`.
    async fn counter_on(server: &TestRedis) -> (String, crate::shared::RedisConnection) {
        let handle = crate::shared::testing::connect(server).await;
        (
            handle.key(REVOCATIONS_SPACE, REVOCATIONS_NAME),
            handle.connection(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_revoked_on_one_replica_is_refused_on_the_other() {
        let (a, b, _redis) = oauth_replica_pair().await;
        let token = access_token_on(&a).await;
        assert!(verify_on(&b, &token).await.is_some(), "B admits it");
        assert!(cached_on(&b, &token), "and caches it");

        revoke_through(&a, &token).await;

        assert!(
            verify_on(&b, &token).await.is_none(),
            "B refuses at once: the counter moved, so its cache was cleared"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_counter_still_clears_every_cache() {
        let (a, b, server) = oauth_replica_pair().await;
        let (key, mut raw) = counter_on(&server).await;
        let token = access_token_on(&a).await;
        // The case the review of PR #552 named: the counter at 1, seen by B.
        let () = redis::cmd("SET")
            .arg(&key)
            .arg(1)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert!(verify_on(&b, &token).await.is_some());
        assert!(cached_on(&b, &token), "B cached it, having seen 1");

        let () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut raw)
            .await
            .unwrap();
        revoke_through(&a, &token).await;

        let after: i64 = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_ne!(after, 1, "re-created from a random seed, not from 0");
        assert!(verify_on(&b, &token).await.is_none(), "B refuses at once");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_counter_seen_by_a_reader_clears_its_cache() {
        let (a, b, server) = oauth_replica_pair().await;
        let (key, mut raw) = counter_on(&server).await;
        let token = access_token_on(&a).await;
        assert!(verify_on(&b, &token).await.is_some());
        revoke_row_only(&b, &token).await;
        assert!(
            verify_on(&b, &token).await.is_some(),
            "nothing moved the counter, so B still answers from its cache"
        );
        let generation = b.oauth().unwrap().access.generation();

        let () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut raw)
            .await
            .unwrap();

        assert!(
            verify_on(&b, &token).await.is_none(),
            "B re-seeded the counter, found a new value, and asked the database"
        );
        assert_ne!(b.oauth().unwrap().access.generation(), generation);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_redis_failure_skips_the_cache_and_still_answers_from_postgres() {
        // A shared server cannot be stopped for everyone.
        let (a, b, server) =
            oauth_replica_pair_on(TestRedis::start().await.dedicated().await).await;
        let revoked = access_token_on(&a).await;
        let live = access_token_on(&a).await;
        assert!(verify_on(&b, &revoked).await.is_some());
        assert!(cached_on(&b, &revoked));
        revoke_row_only(&b, &revoked).await;
        let cached = cache_len(&b);
        server.stop().await;

        assert!(
            verify_on(&b, &revoked).await.is_none(),
            "the cached copy is not trusted without the counter"
        );
        assert!(
            verify_on(&b, &live).await.is_some(),
            "a live token is still admitted, from Postgres"
        );
        assert!(!cached_on(&b, &live), "and not cached");
        assert_eq!(cache_len(&b), cached, "nothing was cached");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_entry_cached_with_redis_is_rechecked_after_60_s() {
        let (a, b, _redis) = oauth_replica_pair().await;
        let token = access_token_on(&a).await;
        assert!(verify_on(&b, &token).await.is_some());
        revoke_row_only(&b, &token).await;
        assert!(
            verify_on(&b, &token).await.is_some(),
            "a young entry is trusted while the counter is unchanged"
        );

        let aged = Instant::now()
            .checked_sub(SHARED_MAX_AGE + Duration::from_secs(1))
            .expect("the monotonic clock is past 61 s");
        b.oauth()
            .unwrap()
            .access
            .lock()
            .tokens
            .get_mut(&keys::hash(&token))
            .expect("cached")
            .cached_at = aged;

        assert!(
            verify_on(&b, &token).await.is_none(),
            "an entry past 60 s goes back to the database"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_minted_on_one_replica_is_accepted_on_the_other_before_any_announcement() {
        let (a, b, _redis) = oauth_replica_pair().await;
        let token = access_token_on(&a).await;
        let grant = verify_on(&a.app, &token)
            .await
            .expect("A admits its own token")
            .key_id
            .clone();
        // Lib apps run no subscriber, and B polls hourly: B has never read
        // the grant A's consent made; its credential cache reads it on use.

        let principal = verify_on(&b, &token).await;

        assert_eq!(
            principal.map(|principal| principal.key_id.clone()),
            Some(grant),
            "B read the grant and admitted the token"
        );
    }

    /// The counter scripts on `server`: a missing counter is seeded below
    /// 2^62 by a reader or a writer, and a bump adds one.
    async fn counter_scripts_seed_and_bump(server: TestRedis) {
        let handle = crate::shared::testing::connect(&server).await;
        let (key, mut raw) = counter_on(&server).await;
        let below = 1_i64 << 62;

        let read = run_counter(&handle, &READ_REVOCATIONS_SCRIPT)
            .await
            .unwrap();
        let stored: i64 = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut raw)
            .await
            .unwrap();
        let bumped = run_counter(&handle, &BUMP_REVOCATIONS_SCRIPT)
            .await
            .unwrap();
        let () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut raw)
            .await
            .unwrap();
        let reseeded = run_counter(&handle, &BUMP_REVOCATIONS_SCRIPT)
            .await
            .unwrap();

        assert!((0..below).contains(&read), "a reader seeds below 2^62");
        assert_eq!(stored, read, "and stores the seed");
        // Exact, though the seed is far above 2^53 (a Lua double's range).
        assert_eq!(bumped, read + 1, "a bump adds one");
        assert!((1..=below).contains(&reseeded), "a writer seeds too");
        assert_ne!(reseeded, 1, "a lost counter does not restart at 1");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_counter_scripts_seed_and_bump_on_standalone_redis() {
        counter_scripts_seed_and_bump(TestRedis::start().await).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_counter_scripts_seed_and_bump_on_a_cluster() {
        counter_scripts_seed_and_bump(TestRedis::start_cluster().await).await;
    }

    #[test]
    fn counter_seeds_are_below_2_pow_62_and_differ() {
        let seeds: Vec<i64> = (0..64).map(|_| counter_seed().unwrap()).collect();

        assert!(seeds.iter().all(|seed| (0..1_i64 << 62).contains(seed)));
        let distinct: std::collections::HashSet<_> = seeds.iter().collect();
        assert_eq!(distinct.len(), seeds.len(), "random, not a constant");
    }
}
