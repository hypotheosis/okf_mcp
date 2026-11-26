//! `users::delete_own` (issue #355): a signed-in person deletes their own
//! account through the one deletion path, with the person as the actor,
//! after re-authenticating. Covers what goes, each refusal of the deletion
//! rules, a proof or session that changed after it was checked, a rollback
//! when a step fails, and lock order against a session writer.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::tests::{seed_catalog_owned_by, seed_user};
use super::*;
use crate::db::test_db;
use crate::store::audit::{self, AuditActor};
use crate::store::ceiling::{self, NewEntitlement};
use crate::store::keys::{self as key_rows, Access, Grants, NewKey};
use crate::store::passkeys::{self, testing::passkey_for};
use crate::store::passwords::{self, SignInOutcome};
use crate::store::{catalogs, identities, teams};

/// How long a race test waits for a held lock or another task.
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// alice's password in these tests.
const PASSWORD: &str = "violet staple orbit lantern";

fn now() -> DateTime<Utc> {
    // A fixed instant: tests never read the wall clock.
    DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

/// alice as she signs in to the console: her user, the raw id of one live
/// session, the actor a request from it records, and her passkey's raw
/// credential id.
struct Alice {
    user: UserAuth,
    session: String,
    actor: AuditActor,
    passkey: Vec<u8>,
}

/// Seeds alice with a password, a console session, a passkey, a linked
/// identity, a key on `shared` (owned by the fixture team) and an
/// entitlement: every kind of row her own deletion must remove.
async fn alice(pool: &PgPool) -> Alice {
    let user = seed_user(pool, "alice", Some("alice@example.com")).await;
    passwords::set(pool, &user.id, PASSWORD, now())
        .await
        .unwrap();
    let session = user_sessions::create(
        pool,
        &user.id,
        None,
        "laptop browser",
        Duration::from_secs(86_400),
        now(),
    )
    .await
    .unwrap()
    .id;
    let passkey = passkey_for(&user.id);
    passkeys::add(pool, &user.id, &passkey, "laptop", now())
        .await
        .unwrap();
    identities::link(
        pool,
        &user.id,
        "example",
        "sub-1",
        "alice@example.com",
        now(),
    )
    .await
    .unwrap();
    seed_catalog_owned_by(pool, crate::db::TEST_TEAM).await;
    let catalogs = ["shared".to_string()];
    key_rows::create_with_id(
        pool,
        &NewKey {
            name: "alice-laptop",
            access: Access::Write,
            maintenance_report: false,
            grants: Grants::Catalogs(&catalogs),
            default_catalog: None,
            actor: "alice-laptop",
            user: "alice",
            self_issued: true,
            expires_at: None,
            levels: &[],
        },
        &AuditActor::cli(),
        now(),
    )
    .await
    .unwrap();
    ceiling::entitle(
        pool,
        &NewEntitlement {
            user: "alice",
            catalog: "shared",
            access: Access::Read,
        },
        &AuditActor::cli(),
        now(),
    )
    .await
    .unwrap();
    let actor = AuditActor {
        source: audit::Source::Console,
        user_id: Some(user.id.clone()),
        label: "alice".to_string(),
        via: audit::Via::Session,
        ..AuditActor::cli()
    };
    Alice {
        user,
        session,
        actor,
        passkey: passkey.cred_id().as_slice().to_vec(),
    }
}

/// The stamp of alice's current password, as `DELETE /me` gets it.
async fn stamp(pool: &PgPool, alice: &Alice) -> PasswordStamp {
    match passwords::verify_user(pool, &alice.user.id, PASSWORD, now())
        .await
        .unwrap()
    {
        SignInOutcome::Ok(verified) => verified.stamp,
        other => panic!("alice's password did not verify: {other:?}"),
    }
}

/// alice's own deletion with `proof`, from her session.
async fn delete_as_alice(
    pool: &PgPool,
    alice: &Alice,
    proof: Proof<'_>,
) -> Result<DeletedUser, OkfError> {
    let own = OwnDeletion {
        user_id: &alice.user.id,
        name: "alice",
        session_id: &alice.session,
        proof,
    };
    delete_own(pool, &own, &alice.actor, now()).await
}

/// The single number `sql` answers, for `alice`'s id as `$1`.
async fn count_for(pool: &PgPool, sql: &str, user_id: &str) -> i64 {
    // `AssertSqlSafe`: the text is a test's own constant, not input.
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// How many of each of alice's rows exist: user, keys, sessions,
/// passkeys, identities, entitlements, personal team.
async fn rows_of(pool: &PgPool, user_id: &str) -> [i64; 7] {
    let mut counts = [0; 7];
    let queries = [
        "SELECT count(*) FROM users WHERE id = $1",
        "SELECT count(*) FROM api_keys WHERE user_id = $1",
        "SELECT count(*) FROM user_sessions WHERE user_id = $1",
        "SELECT count(*) FROM user_passkeys WHERE user_id = $1",
        "SELECT count(*) FROM user_identities WHERE user_id = $1",
        "SELECT count(*) FROM user_catalogs WHERE user_id = $1",
        "SELECT count(*) FROM teams WHERE personal_of = $1",
    ];
    // `zip` pairs each slot with its query, like Python's `zip`.
    for (slot, sql) in counts.iter_mut().zip(queries) {
        *slot = count_for(pool, sql, user_id).await;
    }
    counts
}

/// What [`rows_of`] answers for a seeded alice that is still all there.
const ALL_THERE: [i64; 7] = [1, 1, 1, 1, 1, 1, 1];

/// The reason of a refused deletion.
fn refusal(result: Result<DeletedUser, OkfError>) -> String {
    match result {
        Err(OkfError::Refused { reason }) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_deletes_their_own_account_and_everything_only_theirs_goes() {
    // Arrange
    let db = test_db().await;
    let alice = alice(&db.pool).await;
    let stamp = stamp(&db.pool, &alice).await;

    // Act
    let deleted = delete_as_alice(&db.pool, &alice, Proof::Password(&stamp))
        .await
        .unwrap();

    // Assert: every row of hers is gone; the catalog and its team stay.
    assert_eq!(deleted.keys, 1);
    assert_eq!(rows_of(&db.pool, &alice.user.id).await, [0; 7]);
    assert!(catalogs::find(&db.pool, "shared").await.is_ok());
    // One entry, whose actor is the person herself (her audit row now
    // reads `deleted user #N`, as every deleted user's does).
    let (actor, target, name): (Option<i64>, Option<i64>, String) = sqlx::query_as(
        "SELECT e.actor_person_id, e.target_person_id, p.name FROM audit_entries e \
         JOIN audit_people p ON p.id = e.target_person_id WHERE e.action = 'user.deleted'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(actor.is_some(), "the entry has a person as its actor");
    assert_eq!(actor, target, "the person deleted their own account");
    assert!(name.starts_with("deleted user #"), "{name}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_passkey_proof_deletes_the_account_too() {
    let db = test_db().await;
    let alice = alice(&db.pool).await;

    let deleted = delete_as_alice(&db.pool, &alice, Proof::Passkey(&alice.passkey)).await;

    assert!(deleted.is_ok(), "{deleted:?}");
    assert_eq!(rows_of(&db.pool, &alice.user.id).await, [0; 7]);
}

#[tokio::test(flavor = "multi_thread")]
async fn each_deletion_rule_refuses_with_its_reason_and_changes_nothing() {
    let db = test_db().await;
    let alice = alice(&db.pool).await;
    let stamp = stamp(&db.pool, &alice).await;
    let proof = Proof::Password(&stamp);

    // Her personal team owns a catalog.
    sqlx::query(
        "UPDATE catalogs SET owner_team_id = (SELECT id FROM teams WHERE name = 'alice') \
         WHERE name = 'shared'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let owns = delete_as_alice(&db.pool, &alice, proof).await;
    sqlx::query(
        "UPDATE catalogs SET owner_team_id = (SELECT id FROM teams WHERE name = $1) \
         WHERE name = 'shared'",
    )
    .bind(crate::db::TEST_TEAM)
    .execute(&db.pool)
    .await
    .unwrap();
    // She is the only owner of a shared team.
    teams::create(
        &db.pool,
        &teams::NewTeam {
            name: "family",
            display_name: &DisplayName::default(),
            owner: "alice",
        },
        &AuditActor::cli(),
        now(),
    )
    .await
    .unwrap();
    let sole = delete_as_alice(&db.pool, &alice, proof).await;
    teams::set_member(
        &db.pool,
        &teams::Membership {
            team: "family",
            user: crate::db::TEST_USER,
            role: teams::Role::Owner,
        },
        &AuditActor::cli(),
        now(),
    )
    .await
    .unwrap();
    // She is the last admin user (the fixture has none).
    set_admin(
        &db.pool,
        "alice",
        AdminChange {
            on: true,
            allow_last: true,
        },
        &AuditActor::cli(),
        now(),
    )
    .await
    .unwrap();
    // Setting the admin flag ends her sessions; she signs in again.
    let session = user_sessions::create(
        &db.pool,
        &alice.user.id,
        None,
        "laptop browser",
        Duration::from_secs(86_400),
        now(),
    )
    .await
    .unwrap()
    .id;
    let alice = Alice { session, ..alice };
    let last_admin = delete_as_alice(&db.pool, &alice, proof).await;

    assert!(
        refusal(owns).contains("its personal team owns catalog(s) \"shared\""),
        "owned catalogs"
    );
    assert!(
        refusal(sole).contains("it is the last owner of team(s) \"family\""),
        "sole owner"
    );
    assert!(
        refusal(last_admin).contains("it is the last admin user"),
        "last admin"
    );
    assert_eq!(rows_of(&db.pool, &alice.user.id).await, ALL_THERE);
    assert_eq!(
        count_for(
            &db.pool,
            "SELECT count(*) FROM audit_entries WHERE action = 'user.deleted' AND $1 <> ''",
            &alice.user.id
        )
        .await,
        0,
        "a refusal records nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_admin_may_delete_their_own_account() {
    let db = test_db().await;
    let alice = alice(&db.pool).await;
    for name in ["alice", crate::db::TEST_USER] {
        let change = AdminChange {
            on: true,
            allow_last: true,
        };
        set_admin(&db.pool, name, change, &AuditActor::cli(), now())
            .await
            .unwrap();
    }
    let session = user_sessions::create(
        &db.pool,
        &alice.user.id,
        None,
        "laptop browser",
        Duration::from_secs(86_400),
        now(),
    )
    .await
    .unwrap()
    .id;
    let alice = Alice { session, ..alice };
    let stamp = stamp(&db.pool, &alice).await;

    let deleted = delete_as_alice(&db.pool, &alice, Proof::Password(&stamp)).await;

    assert!(deleted.is_ok(), "{deleted:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proof_or_session_changed_since_the_check_refuses_and_changes_nothing() {
    let db = test_db().await;
    let alice = alice(&db.pool).await;
    let stale = stamp(&db.pool, &alice).await;

    // The password changed after it was checked (the same text: a new salt).
    passwords::set(&db.pool, &alice.user.id, PASSWORD, now())
        .await
        .unwrap();
    let changed = delete_as_alice(&db.pool, &alice, Proof::Password(&stale)).await;
    // A passkey that is not hers (or was removed).
    let removed = delete_as_alice(&db.pool, &alice, Proof::Passkey(b"not-a-passkey")).await;
    // Another user's session, with alice's id.
    let bob = seed_user(&db.pool, "bob", None).await;
    let bobs = user_sessions::create(
        &db.pool,
        &bob.id,
        None,
        "",
        Duration::from_secs(86_400),
        now(),
    )
    .await
    .unwrap()
    .id;
    let fresh = stamp(&db.pool, &alice).await;
    let other_session = delete_own(
        &db.pool,
        &OwnDeletion {
            user_id: &alice.user.id,
            name: "alice",
            session_id: &bobs,
            proof: Proof::Password(&fresh),
        },
        &alice.actor,
        now(),
    )
    .await;
    // A name that is not the session's user.
    let other_name = delete_own(
        &db.pool,
        &OwnDeletion {
            user_id: &alice.user.id,
            name: "bob",
            session_id: &alice.session,
            proof: Proof::Password(&fresh),
        },
        &alice.actor,
        now(),
    )
    .await;
    // Her session ended (signed out elsewhere).
    user_sessions::revoke(&db.pool, &alice.session)
        .await
        .unwrap();
    let ended = delete_as_alice(&db.pool, &alice, Proof::Password(&fresh)).await;

    assert!(
        matches!(&changed, Err(OkfError::Forbidden { reason }) if reason.contains("password changed")),
        "{changed:?}"
    );
    assert!(
        matches!(&removed, Err(OkfError::Forbidden { reason }) if reason.contains("passkey was removed")),
        "{removed:?}"
    );
    for result in [&other_session, &other_name, &ended] {
        assert!(
            matches!(result, Err(OkfError::Unauthenticated { .. })),
            "{result:?}"
        );
    }
    assert_eq!(rows_of(&db.pool, &alice.user.id).await[..2], [1, 1]);
    assert_eq!(rows_of(&db.pool, &alice.user.id).await[3..], [1, 1, 1, 1]);
    assert!(find(&db.pool, "bob").await.is_ok(), "bob stays");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_step_rolls_the_whole_deletion_back() {
    // Arrange: the audit write, after the discards and before the deletes,
    // fails.
    let db = test_db().await;
    let alice = alice(&db.pool).await;
    let stamp = stamp(&db.pool, &alice).await;
    crate::http::api::testing::fail_on(&db.pool, "BEFORE INSERT ON audit_entries FOR EACH ROW")
        .await;

    // Act
    let failed = delete_as_alice(&db.pool, &alice, Proof::Password(&stamp)).await;

    // Assert: a database error, and every row is still there.
    assert!(
        matches!(failed, Err(OkfError::Database { .. })),
        "{failed:?}"
    );
    assert_eq!(rows_of(&db.pool, &alice.user.id).await, ALL_THERE);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_waiting_on_its_session_and_a_passkey_removal_both_finish() {
    // Arrange: a holder of alice's session row (as a sign-out of it would
    // hold it) stops the deletion at its session check, after it locked
    // alice; a passkey removal, which locks alice first, then queues
    // behind the deletion. Lock order is the user first, then the session
    // or passkey, on both sides, so neither deadlocks.
    let db = test_db().await;
    let alice = alice(&db.pool).await;
    let stamp = stamp(&db.pool, &alice).await;
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM user_sessions WHERE user_id = $1 FOR UPDATE")
        .bind(&alice.user.id)
        .execute(&mut *holder)
        .await
        .unwrap();
    let deleting = tokio::spawn({
        let pool = db.pool.clone();
        let (user_id, session, actor) = (
            alice.user.id.clone(),
            alice.session.clone(),
            alice.actor.clone(),
        );
        let stamp = stamp.clone();
        async move {
            let own = OwnDeletion {
                user_id: &user_id,
                name: "alice",
                session_id: &session,
                proof: Proof::Password(&stamp),
            };
            delete_own(&pool, &own, &actor, now()).await
        }
    });
    tokio::time::timeout(
        LOCK_WAIT,
        crate::http::api::testing::wait_for_lock_waiters(&db.pool, 1),
    )
    .await
    .expect("the deletion never queued on the session row");
    let passkey_id = passkeys::list(&db.pool, &alice.user.id).await.unwrap()[0]
        .id
        .clone();
    let removing = tokio::spawn({
        let pool = db.pool.clone();
        let user_id = alice.user.id.clone();
        async move { passkeys::remove(&pool, &user_id, &passkey_id).await }
    });
    tokio::time::timeout(
        LOCK_WAIT,
        crate::http::api::testing::wait_for_lock_waiters(&db.pool, 2),
    )
    .await
    .expect("the removal never queued on alice");

    // Act
    holder.rollback().await.unwrap();
    let deleted = tokio::time::timeout(LOCK_WAIT, deleting)
        .await
        .expect("the deletion finished within 30 s")
        .expect("the deletion did not panic");
    let removed = tokio::time::timeout(LOCK_WAIT, removing)
        .await
        .expect("the removal finished within 30 s")
        .expect("the removal did not panic");

    // Assert: the deletion went first (it held alice); the removal then
    // found no passkey.
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(
        matches!(removed, Err(OkfError::NotFound { .. })),
        "{removed:?}"
    );
    assert_eq!(rows_of(&db.pool, &alice.user.id).await, [0; 7]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_reauthentication_deletes_the_account_too() {
    let db = test_db().await;
    let alice = alice(&db.pool).await;

    let deleted = delete_as_alice(
        &db.pool,
        &alice,
        Proof::Oidc {
            provider: "example",
            subject: "sub-1",
        },
    )
    .await;

    assert!(deleted.is_ok(), "{deleted:?}");
    assert_eq!(rows_of(&db.pool, &alice.user.id).await, [0; 7]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_identity_that_is_not_linked_to_the_user_any_more_refuses_the_deletion() {
    let db = test_db().await;
    let alice = alice(&db.pool).await;

    // Another subject at the provider, another provider, and the identity
    // after it was unlinked.
    let other_subject = delete_as_alice(
        &db.pool,
        &alice,
        Proof::Oidc {
            provider: "example",
            subject: "sub-2",
        },
    )
    .await;
    let other_provider = delete_as_alice(
        &db.pool,
        &alice,
        Proof::Oidc {
            provider: "elsewhere",
            subject: "sub-1",
        },
    )
    .await;
    identities::unlink(&db.pool, &alice.user.id, "example")
        .await
        .unwrap();
    let unlinked = delete_as_alice(
        &db.pool,
        &alice,
        Proof::Oidc {
            provider: "example",
            subject: "sub-1",
        },
    )
    .await;

    for result in [&other_subject, &other_provider, &unlinked] {
        assert!(
            matches!(result, Err(OkfError::Forbidden { reason }) if reason.contains("unlinked")),
            "{result:?}"
        );
    }
    assert_eq!(rows_of(&db.pool, &alice.user.id).await[..3], [1, 1, 1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_waiting_on_its_session_and_an_unlink_both_finish() {
    // Arrange: as for a passkey removal above, with the provider
    // re-authentication as the proof and an unlink of that identity (which
    // also locks alice first) queued behind the deletion.
    let db = test_db().await;
    let alice = alice(&db.pool).await;
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM user_sessions WHERE user_id = $1 FOR UPDATE")
        .bind(&alice.user.id)
        .execute(&mut *holder)
        .await
        .unwrap();
    let deleting = tokio::spawn({
        let pool = db.pool.clone();
        let (user_id, session, actor) = (
            alice.user.id.clone(),
            alice.session.clone(),
            alice.actor.clone(),
        );
        async move {
            let own = OwnDeletion {
                user_id: &user_id,
                name: "alice",
                session_id: &session,
                proof: Proof::Oidc {
                    provider: "example",
                    subject: "sub-1",
                },
            };
            delete_own(&pool, &own, &actor, now()).await
        }
    });
    tokio::time::timeout(
        LOCK_WAIT,
        crate::http::api::testing::wait_for_lock_waiters(&db.pool, 1),
    )
    .await
    .expect("the deletion never queued on the session row");
    let unlinking = tokio::spawn({
        let pool = db.pool.clone();
        let user_id = alice.user.id.clone();
        async move { identities::unlink(&pool, &user_id, "example").await }
    });
    tokio::time::timeout(
        LOCK_WAIT,
        crate::http::api::testing::wait_for_lock_waiters(&db.pool, 2),
    )
    .await
    .expect("the unlink never queued on alice");

    // Act
    holder.rollback().await.unwrap();
    let deleted = tokio::time::timeout(LOCK_WAIT, deleting)
        .await
        .expect("the deletion finished within 30 s")
        .expect("the deletion did not panic");
    let unlinked = tokio::time::timeout(LOCK_WAIT, unlinking)
        .await
        .expect("the unlink finished within 30 s")
        .expect("the unlink did not panic");

    // Assert: the deletion went first (it held alice); the unlink then
    // found no user.
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(
        matches!(unlinked, Err(OkfError::NotFound { .. })),
        "{unlinked:?}"
    );
    assert_eq!(rows_of(&db.pool, &alice.user.id).await, [0; 7]);
}
