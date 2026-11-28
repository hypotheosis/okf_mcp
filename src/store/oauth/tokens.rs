//! `oauth_tokens`: access and refresh tokens, by SHA-256. A family is every
//! token minted from one authorization code (Task 6 rotates within it).

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{PgConnection, PgPool};

use crate::error::OkfError;
use crate::store::decode_column;

/// An access or a refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `okfat_`: presented on `/mcp` and `/api/v1`.
    Access,
    /// `okfrt_`: exchanged at `/oauth/token` for a new pair.
    Refresh,
}

impl TokenKind {
    /// The spelling stored in `oauth_tokens.kind`.
    ///
    /// # Returns
    /// `"access"` or `"refresh"`.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Access => "access",
            TokenKind::Refresh => "refresh",
        }
    }

    /// Parses the stored spelling.
    ///
    /// # Arguments
    /// * `text` - an `oauth_tokens.kind` value.
    ///
    /// # Returns
    /// The kind, or `None` for anything else.
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(
            uninstrumented_fn,
            reason = "trivial accessor or conversion: maps a value, no I/O"
        )
    )]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "access" => Some(TokenKind::Access),
            "refresh" => Some(TokenKind::Refresh),
            _ => None,
        }
    }
}

/// A token to store. It holds the hash only, never the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken<'a> {
    /// `keys::hash` of the token.
    pub sha256: [u8; 32],
    /// The grant (`api_keys.id`) it acts for.
    pub grant_id: &'a str,
    /// Access or refresh.
    pub kind: TokenKind,
    /// Its family (one per authorization code).
    pub family_id: &'a str,
    /// The resource it was issued for (the audience).
    pub resource: &'a str,
    /// When it stops being accepted.
    pub expires_at: DateTime<Utc>,
}

/// A live access token's facts, as the bearer check needs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAccessToken {
    /// The grant it acts for.
    pub grant_id: String,
    /// The resource it was issued for.
    pub resource: String,
    /// When it stops being accepted.
    pub expires_at: DateTime<Utc>,
}

/// A token [`revoke`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokedToken {
    /// Its family.
    pub family_id: String,
    /// Access or refresh (revoking a refresh token revokes its family, Task 6).
    pub kind: TokenKind,
}

/// Stores a token.
///
/// # Arguments
/// * `conn` - a connection or an open transaction (minting stores an access
///   and a refresh token together).
/// * `new` - the token.
/// * `now` - its creation time (server clock).
///
/// # Errors
/// [`OkfError::Database`], including a hash collision (a duplicate primary key).
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.insert"))]
pub async fn insert(
    conn: &mut PgConnection,
    new: &NewToken<'_>,
    now: DateTime<Utc>,
) -> Result<(), OkfError> {
    sqlx::query(
        "INSERT INTO oauth_tokens \
         (sha256, grant_id, kind, family_id, resource, created_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(new.sha256.as_slice())
    .bind(new.grant_id)
    .bind(new.kind.as_str())
    .bind(new.family_id)
    .bind(new.resource)
    .bind(now)
    .bind(new.expires_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What the store holds for a presented access token (issue #1012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessState {
    /// Not revoked and not expired.
    Live(LiveAccessToken),
    /// Revoked, whether or not it has also expired.
    Revoked,
    /// Not revoked, but past its expiry.
    Expired,
}

/// An access token by hash, in whatever state: live, revoked or expired.
/// One read by primary key, whatever the answer: a token that is not live
/// costs the same query as a live one, and an unknown one costs no more
/// than a revoked one. Whether a live token's grant is still live is the
/// key cache's to say (`Keys::find_grant`).
///
/// # Arguments
/// * `pool` - the database.
/// * `sha256` - `keys::hash` of the presented token.
/// * `now` - the request's time (server clock).
///
/// # Returns
/// Its state, or `None` for a hash that is not an access token (unknown, or
/// a refresh token's).
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.find_access"))]
pub async fn find_access(
    pool: &PgPool,
    sha256: &[u8; 32],
    now: DateTime<Utc>,
) -> Result<Option<AccessState>, OkfError> {
    // The grant, the resource, the expiry and the revocation time, if any.
    type Row = (String, String, DateTime<Utc>, Option<DateTime<Utc>>);
    let row: Option<Row> = sqlx::query_as(
        "SELECT grant_id, resource, expires_at, revoked_at FROM oauth_tokens \
         WHERE sha256 = $1 AND kind = 'access'",
    )
    .bind(sha256.as_slice())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(grant_id, resource, expires_at, revoked_at)| {
        if revoked_at.is_some() {
            AccessState::Revoked
        } else if expires_at <= now {
            AccessState::Expired
        } else {
            AccessState::Live(LiveAccessToken {
                grant_id,
                resource,
                expires_at,
            })
        }
    }))
}

/// Revokes one token (RFC 7009). Revoking an already revoked token keeps
/// its first revocation time and still reports it.
///
/// # Arguments
/// * `pool` - the database.
/// * `sha256` - `keys::hash` of the token.
/// * `now` - the revocation time (server clock).
///
/// # Returns
/// Its family and kind, or `None` for an unknown token.
///
/// # Errors
/// [`OkfError::Database`], including a stored kind that is not `access` or
/// `refresh` (a hand-edited row); nothing is revoked in that case.
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.revoke"))]
pub async fn revoke(
    pool: &PgPool,
    sha256: &[u8; 32],
    now: DateTime<Utc>,
) -> Result<Option<RevokedToken>, OkfError> {
    // A transaction, so a row whose kind does not parse is rolled back
    // (dropping `tx` without `commit` rolls it back) instead of being
    // revoked while the caller is told the call failed.
    let mut tx = pool.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE oauth_tokens SET revoked_at = COALESCE(revoked_at, $2) \
         WHERE sha256 = $1 RETURNING family_id, kind",
    )
    .bind(sha256.as_slice())
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let revoked = match row {
        None => None,
        Some((family_id, kind)) => {
            // The column's CHECK allows only the two spellings, so this
            // fails only for a hand-edited row; answer an error, not a panic.
            let kind = decode_column(TokenKind::parse(&kind), "oauth_tokens.kind")?;
            Some(RevokedToken { family_id, kind })
        }
    };
    tx.commit().await?;
    Ok(revoked)
}

/// The first key of the token-family locks (`okff` in ASCII): its own
/// namespace, apart from the catalog locks (`okf`) and the DCR registration
/// lock (`okfo`), so a family's hash can never collide with them.
const FAMILY_LOCK_NAMESPACE: i32 = 0x6f6b_6666;

/// Takes the family's lock for the rest of the transaction.
///
/// Every transaction that adds tokens to a family (minting from a code,
/// rotating a refresh token) or revokes it ([`revoke_family`]) takes this
/// lock first, so they run one at a time. That is what makes "reuse revokes
/// the whole family" hold under read committed: a statement's snapshot is
/// taken when the statement starts, so a revocation's `UPDATE` cannot see
/// successors inserted by a rotation that has not committed yet (PR #262
/// review). With the lock, either the revocation runs after the rotation
/// commits, and its `UPDATE`, started after the lock was granted, sees and
/// revokes the successors; or it runs first, and the rotation, reading its
/// token only after the lock was granted, finds it revoked and mints nothing.
///
/// Two families whose ids hash alike share a lock: harmless, they only wait
/// for each other.
///
/// # Arguments
/// * `conn` - the transaction's connection (pass `&mut tx`; outside a
///   transaction the lock would end with the statement).
/// * `family_id` - the family.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.lock_family"))]
pub async fn lock_family(conn: &mut PgConnection, family_id: &str) -> Result<(), OkfError> {
    // `hashtext` maps the text id onto the lock's `int4` second key.
    // `pg_advisory_xact_lock` returns `void`, so run it with `execute`.
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(FAMILY_LOCK_NAMESPACE)
        .bind(family_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// What presenting a refresh token found ([`use_refresh`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshUse {
    /// Live and unused, now marked used; or used within the reuse grace
    /// window and still live (`again`). Mint its successor in the same
    /// transaction.
    Rotated {
        /// Its grant.
        grant_id: String,
        /// Its family, which the successor joins.
        family_id: String,
        /// The resource it was issued for.
        resource: String,
        /// The grant's OAuth client (`None` once the client row is gone),
        /// read under the grant's share lock in this transaction: the caller
        /// checks the presenting client against it without another
        /// connection or a cache load while its locks are held.
        client_id: Option<String>,
        /// `true` when the token had already been used, within the grace
        /// window: another process of the same client lost a race to
        /// rotate it. Its first use stays the window's start.
        again: bool,
    },
    /// Already used, past the grace window (or in a revoked family): a
    /// stolen copy, or the client replaying it. The caller revokes the
    /// family.
    Reused {
        /// The family to revoke.
        family_id: String,
    },
    /// Unknown, not a refresh token, expired, revoked, or its grant revoked
    /// or expired.
    Unknown,
}

/// The row [`use_refresh`] reads: the token's binding, its state, and its
/// grant's revocation time, expiry and client.
type RefreshRow = (
    String,
    String,
    String,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<String>,
);

/// Takes a share lock on a grant's `api_keys` row for the rest of the
/// transaction, and says whether the grant is live.
///
/// This is the grant half of the token lock protocol. Every transaction
/// that issues tokens for a grant (a code exchange, a refresh rotation)
/// takes it first, before the family's lock ([`lock_family`]) and before
/// any token row, and holds it until it commits. Every revocation of the
/// grant (`store::keys::revoke_by_id`, housekeeping's
/// `store::keys::revoke_idle_grants`) locks the same row `FOR UPDATE`, which
/// conflicts with this. So either the revocation commits first, and this
/// (waiting, then reading the committed row) reports the grant revoked and
/// the caller issues nothing; or the issuing transaction holds the lock
/// first, and the revocation waits for it to commit (the idle pass then
/// re-checks and finds the new tokens live). A revoked grant never gains a
/// token, and an issue that passed this check is never undone by an idle
/// revocation.
///
/// Lock order: (a user row, taken only by user deletion, whose cascade
/// reaches the grant and then its tokens) the grant row, then the family
/// lock, then token rows. Revocations of a grant touch no token row and no
/// family lock; a family revocation touches no grant row.
///
/// # Arguments
/// * `conn` - the transaction's connection.
/// * `grant_id` - the grant (`api_keys.id`).
///
/// # Returns
/// `true` for a live grant; `false` for a revoked or missing one.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.lock_grant"))]
pub async fn lock_grant(conn: &mut PgConnection, grant_id: &str) -> Result<bool, OkfError> {
    // `FOR SHARE`: issuers do not block each other, only a writer of the
    // row (a revocation's `FOR UPDATE`) waits, and is waited for.
    let live: Option<bool> =
        sqlx::query_scalar("SELECT revoked_at IS NULL FROM api_keys WHERE id = $1 FOR SHARE")
            .bind(grant_id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(live == Some(true))
}

/// Presents a refresh token: takes its grant's share lock ([`lock_grant`]),
/// then its family's lock ([`lock_family`]), locks its row and, if it is
/// live and unused, marks it used.
///
/// A used token is `Reused`, which revokes its family, unless it is
/// presented again within `grace` of its first use while it, its family
/// and its grant are still live: then it rotates again (`again`). Several
/// processes of one client sharing a credential refresh at once when the
/// access token expires, and only the first can win the rotation; without
/// the window every loser would sign the client out. A used token in a
/// revoked family is `Reused` (revoking twice is harmless), never rotated:
/// a family revocation revokes its used tokens too.
///
/// # Arguments
/// * `tx` - a read-committed transaction, the one the successor is minted
///   in (commit it after; dropping it uncommitted rolls the mark back and
///   releases the family lock).
/// * `sha256` - the presented token's hash.
/// * `now` - the server clock.
/// * `grace` - `oauth.refresh_reuse_grace`; zero turns the window off.
///
/// # Returns
/// What was found.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.use_refresh"))]
pub async fn use_refresh(
    tx: &mut PgConnection,
    sha256: &[u8; 32],
    now: DateTime<Utc>,
    grace: TimeDelta,
) -> Result<RefreshUse, OkfError> {
    // The grant and the family first: a token's grant and family never
    // change, so reading them before the locks is safe; the token's state
    // is read only after them.
    let binding: Option<(String, String)> = sqlx::query_as(
        "SELECT grant_id, family_id FROM oauth_tokens WHERE sha256 = $1 AND kind = 'refresh'",
    )
    .bind(sha256.as_slice())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((grant, family)) = binding else {
        return Ok(RefreshUse::Unknown);
    };
    // Held until the successor is committed (or the transaction is
    // dropped), so a revocation, an idle one included, cannot slip between
    // this check and the mint. A revoked grant is answered below, after the
    // reuse check, from the row read under this lock.
    lock_grant(&mut *tx, &grant).await?;
    lock_family(&mut *tx, &family).await?;
    // A new statement, so (read committed) a new snapshot: it sees whatever
    // the family's previous lock holder committed, a revocation included.
    // `FOR UPDATE OF t` also locks the token row.
    let row: Option<RefreshRow> = sqlx::query_as(
        "SELECT t.grant_id, t.family_id, t.resource, t.used_at, t.revoked_at, t.expires_at, \
                k.revoked_at, k.expires_at, k.client_id \
         FROM oauth_tokens t JOIN api_keys k ON k.id = t.grant_id \
         WHERE t.sha256 = $1 AND t.kind = 'refresh' \
         FOR UPDATE OF t",
    )
    .bind(sha256.as_slice())
    .fetch_optional(&mut *tx)
    .await?;
    // `let ... else`: return early when there is no row.
    let Some((
        grant_id,
        family_id,
        resource,
        used_at,
        revoked_at,
        expires_at,
        grant_revoked_at,
        grant_expires_at,
        client_id,
    )) = row
    else {
        return Ok(RefreshUse::Unknown);
    };
    // An expired grant is refused as a revoked one is (the credential
    // cache refuses it the same way).
    let grant_expired = grant_expires_at.is_some_and(|at| at <= now);
    let live =
        revoked_at.is_none() && grant_revoked_at.is_none() && !grant_expired && expires_at > now;
    if let Some(first_use) = used_at {
        // `now - first_use` is a `TimeDelta`; a clock that stepped back
        // makes it negative, which is still inside an open window. A zero
        // grace closes the window even for a second use at the same instant.
        let within_grace = grace > TimeDelta::zero() && now - first_use <= grace;
        if !(live && within_grace) {
            return Ok(RefreshUse::Reused { family_id });
        }
        // `used_at` is left as it is, so the window never slides.
        return Ok(RefreshUse::Rotated {
            grant_id,
            family_id,
            resource,
            client_id,
            again: true,
        });
    }
    if !live {
        return Ok(RefreshUse::Unknown);
    }
    sqlx::query("UPDATE oauth_tokens SET used_at = $2 WHERE sha256 = $1")
        .bind(sha256.as_slice())
        .bind(now)
        .execute(&mut *tx)
        .await?;
    Ok(RefreshUse::Rotated {
        grant_id,
        family_id,
        resource,
        client_id,
        again: false,
    })
}

/// Revokes every live token of a family, under its lock ([`lock_family`]),
/// so a rotation or a mint in flight either commits first and is revoked
/// with the rest, or runs after and finds its parent revoked.
///
/// # Arguments
/// * `pool` - the database.
/// * `family_id` - the family.
/// * `now` - the server clock.
///
/// # Returns
/// The hashes it revoked (none if it was already revoked), for
/// `AccessTokens::forget`.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.revoke_family"))]
pub async fn revoke_family(
    pool: &PgPool,
    family_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<[u8; 32]>, OkfError> {
    let mut tx = pool.begin().await?;
    lock_family(&mut tx, family_id).await?;
    // Started after the lock was granted, so its snapshot holds every token
    // the family's previous lock holder committed.
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "UPDATE oauth_tokens SET revoked_at = $2 \
         WHERE family_id = $1 AND revoked_at IS NULL RETURNING sha256",
    )
    .bind(family_id)
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    // The schema checks `length(sha256) = 32`, so the conversion to a fixed
    // array cannot fail on a row the server wrote; `filter_map` skips any
    // other.
    Ok(rows
        .into_iter()
        .filter_map(|hash| <[u8; 32]>::try_from(hash.as_slice()).ok())
        .collect())
}

/// Deletes every token that expired before `expired_before`.
///
/// # Arguments
/// * `pool` - the database.
/// * `expired_before` - the cut-off (housekeeping passes a week ago).
///
/// # Returns
/// How many were deleted.
///
/// # Errors
/// [`OkfError::Database`].
#[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "tokens.prune"))]
pub async fn prune(pool: &PgPool, expired_before: DateTime<Utc>) -> Result<u64, OkfError> {
    let deleted = sqlx::query("DELETE FROM oauth_tokens WHERE expires_at < $1")
        .bind(expired_before)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, TimeZone};

    use crate::db::test_db;
    use crate::store::catalogs;
    use crate::store::keys::Access;
    use crate::store::oauth::testing::{insert_client, insert_grant};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap()
    }

    /// A database with one grant; returns it with its id.
    async fn with_grant() -> (crate::db::pg_support::TestDb, String) {
        let db = test_db().await;
        catalogs::create(
            &db.pool,
            &crate::store::catalogs::NewCatalog {
                name: "main",
                display_name: &crate::labels::DisplayName::default(),
                description: "",
                owner_team: crate::db::TEST_TEAM,
            },
            &crate::store::audit::AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();
        let client = insert_client(
            &db.pool,
            "https://client.example.com/client.json",
            "Example",
            now(),
        )
        .await;
        let grant = insert_grant(&db.pool, &client, Access::Write, &["main"], now()).await;
        (db, grant)
    }

    async fn store(db: &crate::db::pg_support::TestDb, new: &NewToken<'_>) {
        let mut conn = db.pool.acquire().await.unwrap();
        insert(&mut conn, new, now()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn find_access_names_the_state_of_an_access_token() {
        let (db, grant) = with_grant().await;
        let token = |byte: u8, kind: TokenKind, expires_at: DateTime<Utc>| NewToken {
            sha256: [byte; 32],
            grant_id: &grant,
            kind,
            family_id: "family-1",
            resource: "https://okf.example.com/mcp",
            expires_at,
        };
        let later = now() + TimeDelta::hours(1);
        store(&db, &token(1, TokenKind::Access, later)).await;
        store(&db, &token(2, TokenKind::Refresh, later)).await;
        store(
            &db,
            &token(3, TokenKind::Access, now() + TimeDelta::seconds(1)),
        )
        .await;
        store(&db, &token(4, TokenKind::Access, later)).await;
        revoke(&db.pool, &[4; 32], now()).await.unwrap();

        let live = find_access(&db.pool, &[1; 32], now()).await.unwrap();
        let refresh = find_access(&db.pool, &[2; 32], now()).await.unwrap();
        let expired = find_access(&db.pool, &[3; 32], now() + TimeDelta::seconds(1))
            .await
            .unwrap();
        let revoked = find_access(&db.pool, &[4; 32], now()).await.unwrap();
        let unknown = find_access(&db.pool, &[5; 32], now()).await.unwrap();

        assert_eq!(
            live,
            Some(AccessState::Live(LiveAccessToken {
                grant_id: grant.clone(),
                resource: "https://okf.example.com/mcp".to_string(),
                expires_at: later,
            }))
        );
        assert_eq!(
            (refresh, expired, revoked, unknown),
            (
                None,
                Some(AccessState::Expired),
                Some(AccessState::Revoked),
                None
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoke_reports_the_family_and_kind_and_is_idempotent() {
        let (db, grant) = with_grant().await;
        store(
            &db,
            &NewToken {
                sha256: [6; 32],
                grant_id: &grant,
                kind: TokenKind::Refresh,
                family_id: "family-2",
                resource: "https://okf.example.com/mcp",
                expires_at: now() + TimeDelta::days(30),
            },
        )
        .await;
        let expected = Some(RevokedToken {
            family_id: "family-2".to_string(),
            kind: TokenKind::Refresh,
        });

        let first = revoke(&db.pool, &[6; 32], now()).await.unwrap();
        let second = revoke(&db.pool, &[6; 32], now() + TimeDelta::hours(1))
            .await
            .unwrap();
        let unknown = revoke(&db.pool, &[7; 32], now()).await.unwrap();

        assert_eq!(first, expected);
        assert_eq!(second, expected);
        assert_eq!(unknown, None);
        let revoked_at: DateTime<Utc> =
            sqlx::query_scalar("SELECT revoked_at FROM oauth_tokens WHERE sha256 = $1")
                .bind([6u8; 32].as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(revoked_at, now(), "the first revocation time stays");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoke_answers_an_error_and_revokes_nothing_for_a_corrupt_kind() {
        let (db, grant) = with_grant().await;
        store(
            &db,
            &NewToken {
                sha256: [8; 32],
                grant_id: &grant,
                kind: TokenKind::Access,
                family_id: "family-corrupt",
                resource: "https://okf.example.com/mcp",
                expires_at: now() + TimeDelta::hours(1),
            },
        )
        .await;
        // The throwaway database's CHECK would refuse a bad kind, so drop it
        // (the constraint name is Postgres' default for an inline column CHECK).
        sqlx::query("ALTER TABLE oauth_tokens DROP CONSTRAINT oauth_tokens_kind_check")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE oauth_tokens SET kind = 'bogus' WHERE sha256 = $1")
            .bind([8u8; 32].as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        let result = revoke(&db.pool, &[8; 32], now()).await;

        assert!(
            matches!(result, Err(OkfError::Database { .. })),
            "got {result:?}"
        );
        let revoked_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM oauth_tokens WHERE sha256 = $1")
                .bind([8u8; 32].as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(revoked_at, None, "the failed revoke is rolled back");
    }

    /// A token of `grant` in `family`, live for 30 days.
    fn token<'a>(
        grant: &'a str,
        kind: TokenKind,
        family: &'a str,
        sha256: [u8; 32],
    ) -> NewToken<'a> {
        NewToken {
            sha256,
            grant_id: grant,
            kind,
            family_id: family,
            resource: "https://okf.example.com/mcp",
            expires_at: now() + TimeDelta::days(30),
        }
    }

    /// `use_refresh` in its own committed transaction.
    async fn present(db: &crate::db::pg_support::TestDb, sha256: &[u8; 32]) -> RefreshUse {
        let mut tx = db.pool.begin().await.unwrap();
        let found = use_refresh(&mut tx, sha256, now(), TimeDelta::zero())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        found
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_token_rotates_once_then_counts_as_reused() {
        let (db, grant) = with_grant().await;
        store(&db, &token(&grant, TokenKind::Refresh, "family-a", [8; 32])).await;

        let first = present(&db, &[8; 32]).await;
        let second = present(&db, &[8; 32]).await;

        assert_eq!(
            first,
            RefreshUse::Rotated {
                grant_id: grant.clone(),
                family_id: "family-a".to_string(),
                resource: "https://okf.example.com/mcp".to_string(),
                client_id: Some("https://client.example.com/client.json".to_string()),
                again: false,
            }
        );
        assert_eq!(
            second,
            RefreshUse::Reused {
                family_id: "family-a".to_string()
            }
        );
    }

    /// `use_refresh` at `at` with `grace`, in its own committed transaction.
    async fn present_with(
        db: &crate::db::pg_support::TestDb,
        sha256: &[u8; 32],
        at: DateTime<Utc>,
        grace: TimeDelta,
    ) -> RefreshUse {
        let mut tx = db.pool.begin().await.unwrap();
        let found = use_refresh(&mut tx, sha256, at, grace).await.unwrap();
        tx.commit().await.unwrap();
        found
    }

    /// Issue #884: processes sharing one credential refresh at once; a
    /// second use within the window rotates again, measured from the first
    /// use (it never slides), and past it the use is a reuse.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_use_within_the_grace_window_rotates_again() {
        let (db, grant) = with_grant().await;
        store(
            &db,
            &token(&grant, TokenKind::Refresh, "family-g", [30; 32]),
        )
        .await;
        let grace = TimeDelta::seconds(60);

        let first = present_with(&db, &[30; 32], now(), grace).await;
        let racing = present_with(&db, &[30; 32], now(), grace).await;
        let at_the_edge = present_with(&db, &[30; 32], now() + grace, grace).await;
        let past_it =
            present_with(&db, &[30; 32], now() + grace + TimeDelta::seconds(1), grace).await;

        assert!(
            matches!(first, RefreshUse::Rotated { again: false, .. }),
            "{first:?}"
        );
        assert_eq!(
            racing,
            RefreshUse::Rotated {
                grant_id: grant.clone(),
                family_id: "family-g".to_string(),
                resource: "https://okf.example.com/mcp".to_string(),
                client_id: Some("https://client.example.com/client.json".to_string()),
                again: true,
            }
        );
        assert!(
            matches!(at_the_edge, RefreshUse::Rotated { again: true, .. }),
            "the window's end is inside it: {at_the_edge:?}"
        );
        assert_eq!(
            past_it,
            RefreshUse::Reused {
                family_id: "family-g".to_string()
            },
            "uses inside the window did not slide it"
        );
    }

    /// The grace window never revives a revoked family or grant: the used
    /// token is a reuse, as without the window.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_grace_window_does_not_rotate_a_revoked_family_or_grant() {
        let (db, grant) = with_grant().await;
        let grace = TimeDelta::seconds(60);
        store(
            &db,
            &token(&grant, TokenKind::Refresh, "family-h", [31; 32]),
        )
        .await;
        store(
            &db,
            &token(&grant, TokenKind::Refresh, "family-i", [32; 32]),
        )
        .await;
        for hash in [[31; 32], [32; 32]] {
            assert!(matches!(
                present_with(&db, &hash, now(), grace).await,
                RefreshUse::Rotated { again: false, .. }
            ));
        }

        revoke_family(&db.pool, "family-h", now()).await.unwrap();
        let in_revoked_family = present_with(&db, &[31; 32], now(), grace).await;
        sqlx::query("UPDATE api_keys SET revoked_at = $2 WHERE id = $1")
            .bind(&grant)
            .bind(now())
            .execute(&db.pool)
            .await
            .unwrap();
        let in_revoked_grant = present_with(&db, &[32; 32], now(), grace).await;

        assert_eq!(
            in_revoked_family,
            RefreshUse::Reused {
                family_id: "family-h".to_string()
            }
        );
        assert_eq!(
            in_revoked_grant,
            RefreshUse::Reused {
                family_id: "family-i".to_string()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rolled_back_rotation_leaves_the_token_unused() {
        let (db, grant) = with_grant().await;
        store(&db, &token(&grant, TokenKind::Refresh, "family-b", [9; 32])).await;

        let mut tx = db.pool.begin().await.unwrap();
        let first = use_refresh(&mut tx, &[9; 32], now(), TimeDelta::zero())
            .await
            .unwrap();
        // Dropped without `commit`: sqlx rolls the transaction back.
        drop(tx);

        assert!(matches!(first, RefreshUse::Rotated { .. }));
        assert!(matches!(
            present(&db, &[9; 32]).await,
            RefreshUse::Rotated { .. }
        ));
    }

    /// The grant's own expiry, apart from its refresh token's: refresh reads
    /// no credential cache, so `use_refresh` is what refuses an expired
    /// grant (review of PR #713).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_refresh_token_of_an_expired_grant_is_unknown_and_unspent() {
        let (db, grant) = with_grant().await;
        // The grant expires an hour in; each token outlives it by 29 days.
        let expiry = now() + TimeDelta::hours(1);
        sqlx::query("UPDATE api_keys SET expires_at = $2 WHERE id = $1")
            .bind(&grant)
            .bind(expiry)
            .execute(&db.pool)
            .await
            .unwrap();
        for (byte, family) in [
            (21, "family-before"),
            (22, "family-at"),
            (23, "family-after"),
        ] {
            store(&db, &token(&grant, TokenKind::Refresh, family, [byte; 32])).await;
        }
        let present_at = async |byte: u8, at: DateTime<Utc>| {
            let mut tx = db.pool.begin().await.unwrap();
            let found = use_refresh(&mut tx, &[byte; 32], at, TimeDelta::zero())
                .await
                .unwrap();
            tx.commit().await.unwrap();
            found
        };
        let used_at = async |byte: u8| -> Option<DateTime<Utc>> {
            sqlx::query_scalar("SELECT used_at FROM oauth_tokens WHERE sha256 = $1")
                .bind([byte; 32].as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap()
        };

        let before = present_at(21, expiry - TimeDelta::seconds(1)).await;
        let at = present_at(22, expiry).await;
        let after = present_at(23, expiry + TimeDelta::minutes(1)).await;

        assert!(
            matches!(before, RefreshUse::Rotated { .. }),
            "a grant not yet expired rotates: {before:?}"
        );
        assert_eq!(at, RefreshUse::Unknown, "expired at its expiry");
        assert_eq!(after, RefreshUse::Unknown);
        assert!(used_at(21).await.is_some());
        assert_eq!(used_at(22).await, None, "the refused token is not spent");
        assert_eq!(used_at(23).await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_access_expired_revoked_and_revoked_grant_tokens_are_unknown() {
        let (db, grant) = with_grant().await;
        store(&db, &token(&grant, TokenKind::Access, "f", [10; 32])).await;
        store(
            &db,
            &NewToken {
                expires_at: now() + TimeDelta::seconds(1),
                ..token(&grant, TokenKind::Refresh, "f", [11; 32])
            },
        )
        .await;
        store(&db, &token(&grant, TokenKind::Refresh, "f", [12; 32])).await;
        revoke(&db.pool, &[12; 32], now()).await.unwrap();
        store(&db, &token(&grant, TokenKind::Refresh, "g", [13; 32])).await;
        let mut tx = db.pool.begin().await.unwrap();
        // One second on, the short-lived token has expired.
        let expired = use_refresh(
            &mut tx,
            &[11; 32],
            now() + TimeDelta::seconds(1),
            TimeDelta::zero(),
        )
        .await
        .unwrap();
        drop(tx);
        crate::store::keys::revoke_by_id(
            &db.pool,
            &grant,
            &crate::store::audit::AuditActor::cli(),
            now(),
        )
        .await
        .unwrap();

        assert_eq!(expired, RefreshUse::Unknown, "expired");
        for (hash, what) in [
            ([14u8; 32], "unknown"),
            ([10; 32], "an access token"),
            ([12; 32], "revoked"),
            ([13; 32], "its grant revoked"),
        ] {
            assert_eq!(present(&db, &hash).await, RefreshUse::Unknown, "{what}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoke_family_revokes_every_live_member_and_returns_their_hashes() {
        let (db, grant) = with_grant().await;
        store(&db, &token(&grant, TokenKind::Access, "fam", [15; 32])).await;
        store(&db, &token(&grant, TokenKind::Refresh, "fam", [16; 32])).await;
        store(&db, &token(&grant, TokenKind::Access, "else", [17; 32])).await;

        let mut revoked = revoke_family(&db.pool, "fam", now()).await.unwrap();
        revoked.sort();
        let again = revoke_family(&db.pool, "fam", now()).await.unwrap();

        assert_eq!(revoked, vec![[15; 32], [16; 32]]);
        assert!(again.is_empty(), "already revoked");
        assert_eq!(
            find_access(&db.pool, &[15; 32], now()).await.unwrap(),
            Some(AccessState::Revoked)
        );
        assert!(find_access(&db.pool, &[17; 32], now())
            .await
            .unwrap()
            .is_some());
    }

    /// Waits until another backend on this test's database is blocked on a
    /// lock (an advisory lock or a row lock): the step that makes the race
    /// below deterministic instead of a matter of scheduling.
    async fn until_another_backend_waits_on_a_lock(db: &crate::db::pg_support::TestDb) {
        for _ in 0..500 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND pid <> pg_backend_pid() \
                 AND wait_event_type = 'Lock'",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            if waiting > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the revocation never waited on a lock");
    }

    /// What revokes the family in [`race_a_revocation_with_a_rotation`].
    #[derive(Debug, Clone, Copy)]
    enum Trigger {
        /// R0 presented again at the token endpoint.
        Reuse,
        /// R0 revoked at `/oauth/revoke`.
        Revoke,
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reuse_racing_a_rotation_revokes_the_successors_too() {
        race_a_revocation_with_a_rotation(Trigger::Reuse).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revoke_racing_a_rotation_revokes_the_successors_too() {
        race_a_revocation_with_a_rotation(Trigger::Revoke).await;
    }

    /// PR #262 review: R0 has rotated to R1. A rotation of R1 inserts its
    /// successors (A2, R2) and stays open; meanwhile `trigger` on R0 revokes
    /// the family. When the rotation commits, its successors must be revoked
    /// too, not left live outside the revocation's snapshot.
    async fn race_a_revocation_with_a_rotation(trigger: Trigger) {
        let (db, grant) = with_grant().await;
        let family = "family-race";
        let (r0, a1, r1, a2, r2) = ([20u8; 32], [21; 32], [22; 32], [23; 32], [24; 32]);
        store(&db, &token(&grant, TokenKind::Refresh, family, r0)).await;
        // R0 -> (A1, R1), committed.
        let mut tx = db.pool.begin().await.unwrap();
        assert!(matches!(
            use_refresh(&mut tx, &r0, now(), TimeDelta::zero())
                .await
                .unwrap(),
            RefreshUse::Rotated { .. }
        ));
        insert(
            &mut tx,
            &token(&grant, TokenKind::Access, family, a1),
            now(),
        )
        .await
        .unwrap();
        insert(
            &mut tx,
            &token(&grant, TokenKind::Refresh, family, r1),
            now(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        // R1 -> (A2, R2), held open after inserting the successors.
        let mut rotation = db.pool.begin().await.unwrap();
        assert!(matches!(
            use_refresh(&mut rotation, &r1, now(), TimeDelta::zero())
                .await
                .unwrap(),
            RefreshUse::Rotated { .. }
        ));
        insert(
            &mut rotation,
            &token(&grant, TokenKind::Access, family, a2),
            now(),
        )
        .await
        .unwrap();
        insert(
            &mut rotation,
            &token(&grant, TokenKind::Refresh, family, r2),
            now(),
        )
        .await
        .unwrap();

        // On another connection, as the endpoints do it: the token endpoint
        // finds R0 reused, or `/oauth/revoke` revokes R0; then the family.
        let pool = db.pool.clone();
        let reuse = tokio::spawn(async move {
            let family_id = match trigger {
                Trigger::Reuse => {
                    let mut tx = pool.begin().await.unwrap();
                    let found = use_refresh(&mut tx, &r0, now(), TimeDelta::zero())
                        .await
                        .unwrap();
                    drop(tx);
                    let RefreshUse::Reused { family_id } = found else {
                        panic!("R0 is a reuse, found {found:?}");
                    };
                    family_id
                }
                Trigger::Revoke => {
                    revoke(&pool, &r0, now())
                        .await
                        .unwrap()
                        .expect("R0")
                        .family_id
                }
            };
            revoke_family(&pool, &family_id, now()).await.unwrap()
        });
        until_another_backend_waits_on_a_lock(&db).await;
        rotation.commit().await.unwrap();
        let mut revoked = reuse.await.unwrap();
        revoked.sort();

        assert!(
            find_access(&db.pool, &a2, now()).await.unwrap().is_none(),
            "the successor access token is revoked"
        );
        assert_eq!(present(&db, &r2).await, RefreshUse::Unknown, "so is R2");
        assert!(
            revoked.contains(&a2) && revoked.contains(&r2),
            "{revoked:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_deletes_only_tokens_expired_before_the_cut_off() {
        let (db, grant) = with_grant().await;
        for (byte, expires_at) in [
            (1u8, now() - TimeDelta::days(8)),
            (2, now() - TimeDelta::days(6)),
            (3, now() + TimeDelta::hours(1)),
        ] {
            sqlx::query(
                "INSERT INTO oauth_tokens \
                 (sha256, grant_id, kind, family_id, resource, created_at, expires_at) \
                 VALUES ($1, $2, 'access', 'f', 'https://okf.example.com/mcp', $3, $4)",
            )
            .bind([byte; 32].as_slice())
            .bind(&grant)
            .bind(expires_at - TimeDelta::hours(1))
            .bind(expires_at)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        let pruned = prune(&db.pool, now() - TimeDelta::days(7)).await.unwrap();

        assert_eq!(pruned, 1);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_tokens")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(left, 2);
    }
}
