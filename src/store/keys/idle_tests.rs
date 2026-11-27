//! `revoke_idle_grants` (issue #269): which grants count as idle, how they
//! are revoked and audited, and that passes running at once revoke each
//! grant once; and (issue #652) the grant lock protocol against user
//! deletion. Each test has an isolated database.
use super::*;
use crate::db::{test_db, TEST_TEAM, TEST_USER};
use crate::store::audit::testing::entries;
use crate::store::oauth::testing::{insert_client, insert_grant};
use crate::store::oauth::tokens::{self, RefreshUse};
use crate::test_locks::expect_row_lock_waiter;
use chrono::TimeDelta;
use std::time::Duration;

/// The pass's time. Fixed, so the ages below are exact.
fn now() -> DateTime<Utc> {
    "2026-11-18T12:00:00Z".parse().expect("a time")
}

/// The cut-off a 30-day `oauth.idle_grant_revoke_after` gives at [`now`].
fn idle_before() -> DateTime<Utc> {
    now() - TimeDelta::days(30)
}

/// The catalog `main` and the test client.
///
/// # Returns
/// The client's id.
async fn setup(pool: &PgPool) -> String {
    catalogs::create(
        pool,
        &catalogs::NewCatalog {
            name: "main",
            display_name: &crate::labels::DisplayName::default(),
            description: "",
            owner_team: TEST_TEAM,
        },
        &AuditActor::cli(),
        now(),
    )
    .await
    .expect("create main");
    insert_client(
        pool,
        "https://client.example.com/client.json",
        "Example",
        now() - TimeDelta::days(60),
    )
    .await
}

/// Stores a token for `grant` issued at `created_at`, lasting `lifetime`.
async fn token(
    pool: &PgPool,
    grant: &str,
    byte: u8,
    kind: tokens::TokenKind,
    created_at: DateTime<Utc>,
    lifetime: TimeDelta,
) {
    let mut conn = pool.acquire().await.expect("a connection");
    tokens::insert(
        &mut conn,
        &tokens::NewToken {
            sha256: [byte; 32],
            grant_id: grant,
            kind,
            family_id: grant,
            resource: "https://okf.example.com/mcp",
            expires_at: created_at + lifetime,
        },
        created_at,
    )
    .await
    .expect("store a token");
}

/// When `id` was revoked, if it was.
async fn revoked_at(pool: &PgPool, id: &str) -> Option<DateTime<Utc>> {
    sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read the row")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_grant_is_revoked_like_an_operator_revoke_and_its_tokens_stop_working() {
    let db = test_db().await;
    let client = setup(&db.pool).await;
    let grant = insert_grant(
        &db.pool,
        &client,
        Access::Write,
        &["main"],
        now() - TimeDelta::days(50),
    )
    .await;
    // Last refreshed 35 days ago: its refresh token expired 5 days ago.
    let last = now() - TimeDelta::days(35);
    token(
        &db.pool,
        &grant,
        1,
        tokens::TokenKind::Refresh,
        now() - TimeDelta::days(40),
        TimeDelta::days(30),
    )
    .await;
    token(
        &db.pool,
        &grant,
        2,
        tokens::TokenKind::Refresh,
        last,
        TimeDelta::days(30),
    )
    .await;
    let generation_before: i64 = sqlx::query_scalar("SELECT admin_generation FROM server_state")
        .fetch_one(&db.pool)
        .await
        .expect("the generation");

    let revoked = revoke_idle_grants(&db.pool, idle_before(), now())
        .await
        .expect("the pass");

    assert_eq!(
        revoked,
        [IdleRevocation {
            id: grant.clone(),
            last_activity: last,
        }]
    );
    assert_eq!(revoked_at(&db.pool, &grant).await, Some(now()));
    let generation_after: i64 = sqlx::query_scalar("SELECT admin_generation FROM server_state")
        .fetch_one(&db.pool)
        .await
        .expect("the generation");
    assert!(
        generation_after > generation_before,
        "the key cache reloads"
    );
    // The same audit entry an operator's revoke records, with a system
    // actor and an idle reason.
    let recorded = entries(&db.pool, "oauth.grant_revoked").await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].source, "system");
    assert_eq!(recorded[0].actor_label, "system:idle-grant-revocation");
    assert!(!recorded[0].actor_operator);
    assert_eq!(recorded[0].target.as_deref(), Some(grant.as_str()));
    assert_eq!(
        recorded[0].details,
        serde_json::json!({"reason": "idle", "last_activity": "2026-10-14T12:00:00Z"})
    );
    // Its refresh token is refused, even at a time it had not expired yet.
    let mut tx = db.pool.begin().await.expect("begin");
    let presented = tokens::use_refresh(
        &mut tx,
        &[2; 32],
        last + TimeDelta::days(1),
        TimeDelta::zero(),
    )
    .await
    .expect("present the token");
    assert_eq!(presented, RefreshUse::Unknown);
}

#[tokio::test(flavor = "multi_thread")]
async fn recent_live_and_static_credentials_of_the_same_age_are_kept() {
    let db = test_db().await;
    let client = setup(&db.pool).await;
    let old = now() - TimeDelta::days(50);
    // Refreshed 10 days ago; its tokens since expired or not, it was used.
    let recent = insert_grant(&db.pool, &client, Access::Read, &["main"], old).await;
    token(
        &db.pool,
        &recent,
        1,
        tokens::TokenKind::Access,
        now() - TimeDelta::days(10),
        TimeDelta::hours(1),
    )
    .await;
    // Issued 40 days ago with a 90-day refresh token (a longer
    // `oauth.refresh_token_ttl`): the client may still come back with it.
    let live = insert_grant(&db.pool, &client, Access::Read, &["main"], old).await;
    token(
        &db.pool,
        &live,
        2,
        tokens::TokenKind::Refresh,
        now() - TimeDelta::days(40),
        TimeDelta::days(90),
    )
    .await;
    // Made a day ago and never used yet.
    let fresh = insert_grant(
        &db.pool,
        &client,
        Access::Read,
        &["main"],
        now() - TimeDelta::days(1),
    )
    .await;
    // Static keys as old as the idle grants, never used by any token.
    let names = ["main".to_string()];
    let mut static_keys = Vec::new();
    for (name, self_issued) in [("old-key", false), ("old-self-issued", true)] {
        let created = create_with_id(
            &db.pool,
            &NewKey {
                name,
                access: Access::Read,
                maintenance_report: false,
                grants: Grants::Catalogs(&names),
                default_catalog: None,
                actor: "example-agent",
                user: TEST_USER,
                self_issued,
                expires_at: None,
                levels: &[],
            },
            &AuditActor::cli(),
            old,
        )
        .await
        .expect("create a key");
        static_keys.push(created.id);
    }
    // The control: a grant as old with no token, which is idle.
    let idle = insert_grant(&db.pool, &client, Access::Read, &["main"], old).await;

    let revoked = revoke_idle_grants(&db.pool, idle_before(), now())
        .await
        .expect("the pass");

    // Never used, so its last activity is its creation.
    assert_eq!(
        revoked,
        [IdleRevocation {
            id: idle.clone(),
            last_activity: old,
        }]
    );
    for kept in [&recent, &live, &fresh]
        .into_iter()
        .chain(static_keys.iter())
    {
        assert_eq!(revoked_at(&db.pool, kept).await, None, "{kept} is kept");
    }
    assert_eq!(entries(&db.pool, "key.revoked").await.len(), 0);
    assert_eq!(entries(&db.pool, "oauth.grant_revoked").await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn passes_run_twice_or_at_once_revoke_and_audit_each_grant_once() {
    let db = test_db().await;
    let client = setup(&db.pool).await;
    let old = now() - TimeDelta::days(50);
    let mut grants = Vec::new();
    for _ in 0..3 {
        grants.push(insert_grant(&db.pool, &client, Access::Read, &["main"], old).await);
    }
    grants.sort();

    // Several replicas' passes at once: each grant is revoked by one.
    let passes: Vec<_> = (0..4)
        .map(|_| {
            let pool = db.pool.clone();
            tokio::spawn(async move { revoke_idle_grants(&pool, idle_before(), now()).await })
        })
        .collect();
    let mut revoked: Vec<String> = Vec::new();
    for pass in passes {
        let ids = tokio::time::timeout(Duration::from_secs(30), pass)
            .await
            .expect("a pass finished within 30 s (waiting on a grant's row lock)")
            .expect("the pass's task")
            .expect("the pass");
        revoked.extend(ids.into_iter().map(|revocation| revocation.id));
    }
    revoked.sort();
    assert_eq!(revoked, grants);
    assert_eq!(entries(&db.pool, "oauth.grant_revoked").await.len(), 3);

    // A later pass finds nothing left to do.
    let again = revoke_idle_grants(&db.pool, idle_before(), now() + TimeDelta::hours(1))
        .await
        .expect("the pass");
    assert!(again.is_empty());
    assert_eq!(entries(&db.pool, "oauth.grant_revoked").await.len(), 3);
    for grant in &grants {
        assert_eq!(revoked_at(&db.pool, grant).await, Some(now()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grant_used_or_revoked_while_the_pass_waits_is_left_alone() {
    let db = test_db().await;
    let client = setup(&db.pool).await;
    let old = now() - TimeDelta::days(50);
    let used = insert_grant(&db.pool, &client, Access::Read, &["main"], old).await;
    let revoked = insert_grant(&db.pool, &client, Access::Read, &["main"], old).await;
    // Hold both rows, as a refresh or an operator's revoke would, so the
    // pass lists them idle and then waits for their locks.
    let mut holder = db.pool.begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM api_keys WHERE id = ANY($1) FOR UPDATE")
        .bind([used.clone(), revoked.clone()].as_slice())
        .execute(&mut *holder)
        .await
        .expect("hold the rows");
    let pool = db.pool.clone();
    let pass = tokio::spawn(async move { revoke_idle_grants(&pool, idle_before(), now()).await });
    // Wait until the pass is blocked on a row lock the holder keeps.
    expect_row_lock_waiter(&db.pool, backend_pid(&mut *holder).await, "api_keys").await;
    // Meanwhile: one grant gets a token, the other is revoked. In the
    // holder's transaction: the token's foreign key would wait for the row.
    tokens::insert(
        &mut holder,
        &tokens::NewToken {
            sha256: [1; 32],
            grant_id: &used,
            kind: tokens::TokenKind::Access,
            family_id: &used,
            resource: "https://okf.example.com/mcp",
            expires_at: now() + TimeDelta::hours(1),
        },
        now(),
    )
    .await
    .expect("store a token");
    let earlier = now() - TimeDelta::minutes(5);
    sqlx::query("UPDATE api_keys SET revoked_at = $2 WHERE id = $1")
        .bind(&revoked)
        .bind(earlier)
        .execute(&mut *holder)
        .await
        .expect("revoke it");
    holder.commit().await.expect("release the rows");

    let result = tokio::time::timeout(Duration::from_secs(30), pass)
        .await
        .expect("the pass finished within 30 s of the release")
        .expect("the pass's task")
        .expect("the pass");

    assert!(result.is_empty(), "{result:?}");
    assert_eq!(revoked_at(&db.pool, &used).await, None);
    assert_eq!(revoked_at(&db.pool, &revoked).await, Some(earlier));
    assert_eq!(entries(&db.pool, "oauth.grant_revoked").await.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_admitted_before_expiry_is_never_undone_by_the_idle_pass() {
    let db = test_db().await;
    let client = setup(&db.pool).await;
    let created = now() - TimeDelta::days(50);
    let grant = insert_grant(&db.pool, &client, Access::Read, &["main"], created).await;
    // Its only refresh token was issued 30 days before `now()`, so it
    // expires at `now()`: the default lifetimes' boundary.
    let issued = now() - TimeDelta::days(30);
    token(
        &db.pool,
        &grant,
        1,
        tokens::TokenKind::Refresh,
        issued,
        TimeDelta::days(30),
    )
    .await;

    // A real rotation, as `/oauth/token` runs it: presented a second
    // before expiry and admitted, then paused before minting.
    let presented_at = now() - TimeDelta::seconds(1);
    let mut rotation = db.pool.begin().await.expect("begin");
    let admitted = tokens::use_refresh(&mut rotation, &[1; 32], presented_at, TimeDelta::zero())
        .await
        .expect("present the token");
    assert!(
        matches!(admitted, RefreshUse::Rotated { .. }),
        "{admitted:?}"
    );

    // The idle pass runs just past expiry, while the rotation is paused.
    let after_expiry = now() + TimeDelta::seconds(1);
    let pool = db.pool.clone();
    let pass = tokio::spawn(async move {
        revoke_idle_grants(&pool, after_expiry - TimeDelta::days(30), after_expiry).await
    });
    // The rotation holds the grant's row, so the pass must block on it.
    expect_row_lock_waiter(&db.pool, backend_pid(&mut *rotation).await, "api_keys").await;

    // The rotation resumes: it mints its successor pair, as `mint` does,
    // and commits.
    for (byte, kind, lifetime) in [
        (2_u8, tokens::TokenKind::Access, TimeDelta::hours(1)),
        (3_u8, tokens::TokenKind::Refresh, TimeDelta::days(30)),
    ] {
        tokens::insert(
            &mut rotation,
            &tokens::NewToken {
                sha256: [byte; 32],
                grant_id: &grant,
                kind,
                family_id: &grant,
                resource: "https://okf.example.com/mcp",
                expires_at: presented_at + lifetime,
            },
            presented_at,
        )
        .await
        .expect("mint the successor");
    }
    rotation.commit().await.expect("commit the rotation");
    let revoked = tokio::time::timeout(Duration::from_secs(30), pass)
        .await
        .expect("the pass finished within 30 s of the rotation's commit")
        .expect("the pass's task")
        .expect("the pass");

    // The refresh succeeded, so its grant must still be live.
    assert!(revoked.is_empty(), "{revoked:?}");
    assert_eq!(revoked_at(&db.pool, &grant).await, None);
    assert_eq!(entries(&db.pool, "oauth.grant_revoked").await.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rotation_after_a_revocation_is_refused() {
    let db = test_db().await;
    let client = setup(&db.pool).await;
    let grant = insert_grant(
        &db.pool,
        &client,
        Access::Read,
        &["main"],
        now() - TimeDelta::days(50),
    )
    .await;
    token(
        &db.pool,
        &grant,
        1,
        tokens::TokenKind::Refresh,
        now() - TimeDelta::days(30),
        TimeDelta::days(30),
    )
    .await;
    // The idle pass holds the grant's row, revoking it.
    let mut revocation = db.pool.begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(&grant)
        .execute(&mut *revocation)
        .await
        .expect("lock the grant");
    sqlx::query("UPDATE api_keys SET revoked_at = $2 WHERE id = $1")
        .bind(&grant)
        .bind(now())
        .execute(&mut *revocation)
        .await
        .expect("revoke it");
    // A refresh presented a second before expiry waits on it ...
    let pool = db.pool.clone();
    let refresh = tokio::spawn(async move {
        let mut tx = pool.begin().await.expect("begin");
        tokens::use_refresh(
            &mut tx,
            &[1; 32],
            now() - TimeDelta::seconds(1),
            TimeDelta::zero(),
        )
        .await
    });
    expect_row_lock_waiter(&db.pool, backend_pid(&mut *revocation).await, "api_keys").await;
    revocation.commit().await.expect("commit the revocation");
    // ... and, once it commits, is refused: nothing is minted.
    let found = tokio::time::timeout(Duration::from_secs(30), refresh)
        .await
        .expect("the refresh finished within 30 s of the revocation")
        .expect("the refresh's task")
        .expect("present the token");
    assert_eq!(found, RefreshUse::Unknown);
}

// Refresh rotation against user deletion (issue #652). The old order could
// deadlock: a rotation held the token row and then needed the grant (the
// foreign key's `FOR KEY SHARE`), while the deletion held the grant and
// then needed the token row (the cascade). `use_refresh` now takes the
// grant first (`lock_grant`), as the deletion does, so the two queue. A
// deadlock shows only when each side holds its first lock before it asks
// for the second, so each test parks one side there.

/// How long any wait on a spawned task may take; longer than Postgres's
/// default `deadlock_timeout` (1 s) many times over, and longer than the
/// lock-wait helpers' own 10 s bound.
const RACE_LIMIT: Duration = Duration::from_secs(30);

/// Creates user `alice` and a grant of hers with an access token made a day
/// ago (byte 1) and a refresh token made an hour ago (byte 2), so a
/// cascade that scans the grant's tokens by age reaches the access token
/// first.
///
/// # Returns
/// The grant's id.
async fn alices_grant(pool: &PgPool) -> String {
    let client = setup(pool).await;
    users::create(
        pool,
        &users::NewUser {
            name: "alice",
            display_name: &crate::labels::DisplayName::default(),
        },
        &AuditActor::cli(),
        now(),
    )
    .await
    .expect("create alice");
    let grant = insert_grant(
        pool,
        &client,
        Access::Read,
        &["main"],
        now() - TimeDelta::days(2),
    )
    .await;
    sqlx::query(
        "UPDATE api_keys SET user_id = (SELECT id FROM users WHERE name = 'alice') WHERE id = $1",
    )
    .bind(&grant)
    .execute(pool)
    .await
    .expect("give the grant to alice");
    token(
        pool,
        &grant,
        1,
        tokens::TokenKind::Access,
        now() - TimeDelta::days(1),
        TimeDelta::days(30),
    )
    .await;
    token(
        pool,
        &grant,
        2,
        tokens::TokenKind::Refresh,
        now() - TimeDelta::hours(1),
        TimeDelta::days(30),
    )
    .await;
    grant
}

/// Starts deleting `alice` in its own task.
fn delete_alice(pool: &PgPool) -> tokio::task::JoinHandle<Result<users::DeletedUser, OkfError>> {
    let pool = pool.clone();
    tokio::spawn(async move {
        users::delete(
            &pool,
            "alice",
            users::DeleteOptions::default(),
            &AuditActor::cli(),
            now(),
        )
        .await
    })
}

/// `conn`'s backend pid.
async fn backend_pid(conn: &mut PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(conn)
        .await
        .expect("read the backend pid")
}

/// What the one-value count query `sql` answers.
async fn rows_in(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count rows")
}

/// A refresh that is admitted (it holds its grant, family and token) before
/// the deletion starts: the deletion queues for the grant behind it, the
/// refresh mints its successors and commits, and the deletion then cascades
/// away the grant and every token, successors included.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_deletion_queues_behind_an_admitted_refresh_and_removes_what_it_minted() {
    let db = test_db().await;
    let grant = alices_grant(&db.pool).await;
    let presented_at = now();
    let mut rotation = db.pool.begin().await.expect("begin");
    let rotation_pid = backend_pid(&mut *rotation).await;
    let admitted = tokens::use_refresh(&mut rotation, &[2; 32], presented_at, TimeDelta::zero())
        .await
        .expect("present the token");
    assert!(
        matches!(admitted, RefreshUse::Rotated { .. }),
        "{admitted:?}"
    );

    let deletion = delete_alice(&db.pool);
    // The deletion has locked alice and now waits for the grant's row,
    // which the rotation holds `FOR SHARE`.
    let waiter = expect_row_lock_waiter(&db.pool, rotation_pid, "api_keys").await;
    assert_ne!(waiter, rotation_pid);

    // The rotation mints its successor pair and commits; neither step may
    // wait for the deletion.
    for (byte, kind) in [
        (3_u8, tokens::TokenKind::Access),
        (4_u8, tokens::TokenKind::Refresh),
    ] {
        tokens::insert(
            &mut rotation,
            &tokens::NewToken {
                sha256: [byte; 32],
                grant_id: &grant,
                kind,
                family_id: &grant,
                resource: "https://okf.example.com/mcp",
                expires_at: presented_at + TimeDelta::days(30),
            },
            presented_at,
        )
        .await
        .expect("mint a successor (no deadlock with the deletion)");
    }
    tokio::time::timeout(RACE_LIMIT, rotation.commit())
        .await
        .expect("the rotation committed within 30 s")
        .expect("commit the rotation (not a deadlock, SQLSTATE 40P01)");
    let deleted = tokio::time::timeout(RACE_LIMIT, deletion)
        .await
        .expect("the deletion finished within 30 s of the rotation's commit")
        .expect("the deletion's task")
        .expect("delete alice (not a deadlock, SQLSTATE 40P01)");

    assert_eq!(deleted.keys, 1);
    assert_eq!(
        rows_in(&db.pool, "SELECT count(*) FROM users WHERE name = 'alice'").await,
        0
    );
    assert_eq!(
        rows_in(
            &db.pool,
            "SELECT count(*) FROM api_keys WHERE kind = 'oauth'"
        )
        .await,
        0
    );
    assert_eq!(
        rows_in(&db.pool, "SELECT count(*) FROM oauth_tokens").await,
        0,
        "the cascade removed the successors too"
    );
}

/// A deletion that holds the grant (parked on a token row a third
/// transaction holds, so it has not yet cascaded) before the refresh starts:
/// the refresh queues for the grant behind it, and once the deletion
/// finishes it finds no grant and is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_queues_behind_a_user_deletion_and_is_refused() {
    let db = test_db().await;
    let _grant = alices_grant(&db.pool).await;
    // The parking transaction holds the access token's row, the oldest of
    // the grant's tokens, which the deletion's cascade reaches first.
    let mut parking = db.pool.begin().await.expect("begin");
    let parking_pid = backend_pid(&mut parking).await;
    sqlx::query("SELECT 1 FROM oauth_tokens WHERE sha256 = $1 FOR UPDATE")
        .bind([1_u8; 32].as_slice())
        .execute(&mut *parking)
        .await
        .expect("hold the access token");

    let deletion = delete_alice(&db.pool);
    // The deletion holds the grant `FOR UPDATE` (its keys-and-grants lock)
    // and waits to cascade onto the parked token row.
    let deletion_pid = expect_row_lock_waiter(&db.pool, parking_pid, "oauth_tokens").await;

    let pool = db.pool.clone();
    let refresh = tokio::spawn(async move {
        let mut tx = pool.begin().await.expect("begin");
        tokens::use_refresh(&mut tx, &[2; 32], now(), TimeDelta::zero()).await
    });
    // The refresh waits for the grant's row behind the deletion, not the
    // other way round.
    let refresh_pid = expect_row_lock_waiter(&db.pool, deletion_pid, "api_keys").await;
    assert_ne!(refresh_pid, deletion_pid);

    parking.rollback().await.expect("release the access token");
    let deleted = tokio::time::timeout(RACE_LIMIT, deletion)
        .await
        .expect("the deletion finished within 30 s")
        .expect("the deletion's task")
        .expect("delete alice (not a deadlock, SQLSTATE 40P01)");
    let found = tokio::time::timeout(RACE_LIMIT, refresh)
        .await
        .expect("the refresh finished within 30 s of the deletion's commit")
        .expect("the refresh's task")
        .expect("present the token (not a deadlock, SQLSTATE 40P01)");

    assert_eq!(deleted.keys, 1);
    assert_eq!(found, RefreshUse::Unknown, "the grant is gone");
    assert_eq!(
        rows_in(
            &db.pool,
            "SELECT count(*) FROM api_keys WHERE kind = 'oauth'"
        )
        .await,
        0
    );
    assert_eq!(
        rows_in(&db.pool, "SELECT count(*) FROM oauth_tokens").await,
        0
    );
}
