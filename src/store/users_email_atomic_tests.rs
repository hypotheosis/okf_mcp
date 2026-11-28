//! Issue #980: `users::create_with_email` creates a user and its address in
//! one transaction, and `DeleteOptions::unless_has_email` deletes only a user
//! with no address, decided under the user's row lock.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::tests::seed_user;
use super::*;
use crate::db::test_db;
use crate::store::audit::{self, AuditActor};

/// How long a race test waits for a task that a held lock delays.
const LOCK_WAIT: Duration = Duration::from_secs(30);

const UNLESS_EMAIL: DeleteOptions = DeleteOptions {
    allow_last_admin: false,
    unless_shared_member: false,
    unless_has_email: true,
};

fn now() -> DateTime<Utc> {
    // A fixed instant: tests never read the wall clock.
    DateTime::parse_from_rfc3339("2026-11-28T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn provisioner() -> AuditActor {
    AuditActor {
        provisioner: true,
        ..AuditActor::cli()
    }
}

/// A user to create, named `name`, with the default display name.
fn named(name: &str) -> NewUser<'_> {
    static UNSET: std::sync::LazyLock<DisplayName> = std::sync::LazyLock::new(DisplayName::default);
    NewUser {
        name,
        display_name: &UNSET,
    }
}

/// Rows that a created user `name` leaves: its user, personal team and
/// audit entries.
async fn rows_of(pool: &PgPool, name: &str) -> (i64, i64, usize) {
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    let teams: i64 = sqlx::query_scalar("SELECT count(*) FROM teams WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    let mut entries = 0;
    for action in ["user.created", "user.email_changed"] {
        entries += audit::testing::entries(pool, action)
            .await
            .into_iter()
            .filter(|entry| entry.person.as_deref() == Some(name))
            .count();
    }
    (users, teams, entries)
}

/// The refusal text of a failed call.
fn refusal<T: std::fmt::Debug>(result: Result<T, OkfError>) -> String {
    match result {
        Err(OkfError::Refused { reason }) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn create_with_email_stores_the_address_and_records_what_create_then_set_email_would() {
    let db = test_db().await;

    let created = create_with_email(
        &db.pool,
        &named("alice"),
        Some("Alice@Example.com"),
        &AuditActor::cli(),
        now(),
    )
    .await
    .unwrap();
    create(&db.pool, &named("bob"), &AuditActor::cli(), now())
        .await
        .unwrap();
    set_email(
        &db.pool,
        "bob",
        Some("Alice2@Example.com"),
        &AuditActor::cli(),
        now(),
    )
    .await
    .unwrap();

    assert_eq!(created.email.as_deref(), Some("Alice@Example.com"));
    assert_eq!(
        find(&db.pool, "alice").await.unwrap().email,
        created.email,
        "the answer is what a read finds"
    );
    // The same recording as create then set_email: one of each, no address.
    for name in ["alice", "bob"] {
        let made = audit::testing::entries(&db.pool, "user.created").await;
        let made: Vec<_> = made
            .iter()
            .filter(|entry| entry.person.as_deref() == Some(name))
            .collect();
        assert_eq!(made.len(), 1, "{name}");
        assert_eq!(made[0].details, json!({}));
        let changed = audit::testing::entries(&db.pool, "user.email_changed").await;
        let changed: Vec<_> = changed
            .iter()
            .filter(|entry| entry.person.as_deref() == Some(name))
            .collect();
        assert_eq!(changed.len(), 1, "{name}");
        assert_eq!(changed[0].details, json!({"cleared": false}));
        assert!(!changed[0].details.to_string().contains('@'));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn create_without_an_email_records_no_email_entry() {
    let db = test_db().await;

    let created = create_with_email(&db.pool, &named("alice"), None, &AuditActor::cli(), now())
        .await
        .unwrap();

    assert_eq!(created.email, None);
    assert!(audit::testing::entries(&db.pool, "user.email_changed")
        .await
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provisioner_creates_with_an_email_like_it_sets_one() {
    let db = test_db().await;

    let created = create_with_email(
        &db.pool,
        &named("alice"),
        Some("alice@example.com"),
        &provisioner(),
        now(),
    )
    .await
    .unwrap();

    assert_eq!(created.email.as_deref(), Some("alice@example.com"));
    let changed = audit::testing::entries(&db.pool, "user.email_changed").await;
    assert_eq!(changed.len(), 1);
    assert!(!changed[0].actor_label.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_taken_address_in_any_letter_case_creates_nothing_and_names_no_one() {
    let db = test_db().await;
    seed_user(&db.pool, "holder", Some("held@example.com")).await;
    // What set_email answers for the same taken address.
    seed_user(&db.pool, "other", None).await;
    let from_set_email = refusal(
        set_email(
            &db.pool,
            "other",
            Some("held@example.com"),
            &AuditActor::cli(),
            now(),
        )
        .await,
    );

    for address in ["held@example.com", "HELD@Example.COM"] {
        let refused = create_with_email(
            &db.pool,
            &named("newcomer"),
            Some(address),
            &AuditActor::cli(),
            now(),
        )
        .await;

        let reason = refusal(refused);
        assert_eq!(reason, EMAIL_TAKEN_REFUSAL);
        assert_eq!(reason, from_set_email, "the two paths answer alike");
        assert!(!reason.contains("holder"), "{reason}");
        assert_eq!(rows_of(&db.pool, "newcomer").await, (0, 0, 0), "{address}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_address_creates_nothing() {
    let db = test_db().await;

    let refused = create_with_email(
        &db.pool,
        &named("alice"),
        Some("not an address"),
        &AuditActor::cli(),
        now(),
    )
    .await;

    assert!(
        matches!(refused, Err(OkfError::LimitExceeded { limit: "email", .. })),
        "{refused:?}"
    );
    assert_eq!(rows_of(&db.pool, "alice").await, (0, 0, 0));
}

/// The address is in flight in another transaction that has not committed:
/// the create waits on it (a real lock wait on the unique index), then loses
/// when it commits, and has created nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_that_waited_on_an_uncommitted_holder_of_the_address_loses_cleanly() {
    let db = test_db().await;
    let mut holding = db.pool.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holding)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (name, email, created_at) VALUES ('winner', 'x@example.com', $1)",
    )
    .bind(now())
    .execute(&mut *holding)
    .await
    .unwrap();
    let pool = db.pool.clone();
    let contender = tokio::spawn(async move {
        create_with_email(
            &pool,
            &named("loser"),
            Some("X@example.com"),
            &AuditActor::cli(),
            now(),
        )
        .await
    });
    let mut probe = db.pool.acquire().await.unwrap();
    crate::test_locks::wait_waiter(&mut probe, holder).await;

    holding.commit().await.unwrap();

    let outcome = tokio::time::timeout(LOCK_WAIT, contender)
        .await
        .expect("the create waited for the holder for too long")
        .unwrap();
    assert_eq!(refusal(outcome), EMAIL_TAKEN_REFUSAL);
    assert_eq!(rows_of(&db.pool, "loser").await, (0, 0, 0));
}

/// The same wait, but the holder rolls back: the waiting create gets the
/// address.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_that_waited_on_a_holder_that_rolled_back_gets_the_address() {
    let db = test_db().await;
    let mut holding = db.pool.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holding)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (name, email, created_at) VALUES ('gone', 'x@example.com', $1)")
        .bind(now())
        .execute(&mut *holding)
        .await
        .unwrap();
    let pool = db.pool.clone();
    let contender = tokio::spawn(async move {
        create_with_email(
            &pool,
            &named("heir"),
            Some("x@example.com"),
            &AuditActor::cli(),
            now(),
        )
        .await
    });
    let mut probe = db.pool.acquire().await.unwrap();
    crate::test_locks::wait_waiter(&mut probe, holder).await;

    holding.rollback().await.unwrap();

    let created = tokio::time::timeout(LOCK_WAIT, contender)
        .await
        .expect("the create waited for the holder for too long")
        .unwrap()
        .unwrap();
    assert_eq!(created.email.as_deref(), Some("x@example.com"));
}

/// Two real creates of one address, whatever their interleaving: exactly one
/// wins, the other is refused, and the loser leaves no user, team or entry.
#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_creates_of_one_address_have_one_winner_and_no_leftovers() {
    let db = test_db().await;
    let spawn_create = |name: &'static str| {
        let pool = db.pool.clone();
        tokio::spawn(async move {
            create_with_email(
                &pool,
                &named(name),
                Some("same@example.com"),
                &AuditActor::cli(),
                now(),
            )
            .await
        })
    };

    let (first, second) = (spawn_create("first"), spawn_create("second"));
    let (first, second) = (first.await.unwrap(), second.await.unwrap());

    let (winner, loser, refused) = match (first, second) {
        (Ok(_), refused) => ("first", "second", refused),
        (refused, Ok(_)) => ("second", "first", refused),
        both => panic!("one create must win: {both:?}"),
    };
    assert_eq!(refusal(refused), EMAIL_TAKEN_REFUSAL);
    assert_eq!(rows_of(&db.pool, loser).await, (0, 0, 0));
    assert_eq!(rows_of(&db.pool, winner).await, (1, 1, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn unless_has_email_deletes_a_user_with_no_address() {
    let db = test_db().await;
    seed_user(&db.pool, "alice", None).await;

    let deleted = delete(&db.pool, "alice", UNLESS_EMAIL, &AuditActor::cli(), now()).await;

    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(find(&db.pool, "alice").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn unless_has_email_keeps_a_user_with_an_address_and_names_nothing() {
    let db = test_db().await;
    seed_user(&db.pool, "alice", Some("alice@example.com")).await;
    seed_user(&db.pool, "bob", Some("bob@example.com")).await;

    let refused = delete(&db.pool, "alice", UNLESS_EMAIL, &AuditActor::cli(), now()).await;

    let reason = refusal(refused);
    assert_eq!(reason, HAS_EMAIL_REFUSAL);
    assert!(!reason.contains('@') && !reason.contains("bob"), "{reason}");
    assert_eq!(
        find(&db.pool, "alice").await.unwrap().email.as_deref(),
        Some("alice@example.com")
    );
    assert!(find(&db.pool, "bob").await.is_ok());
    // Without the option the same user is deleted, as before.
    let plain = delete(
        &db.pool,
        "alice",
        DeleteOptions::default(),
        &AuditActor::cli(),
        now(),
    )
    .await;
    assert!(plain.is_ok(), "{plain:?}");
}

/// An address being set (its transaction holds the user row, as `set_email`
/// does) when the conditional delete starts: the delete waits, then sees the
/// committed address and keeps the user.
#[tokio::test(flavor = "multi_thread")]
async fn a_set_email_in_flight_makes_the_conditional_delete_keep_the_user() {
    let db = test_db().await;
    seed_user(&db.pool, "alice", None).await;
    let mut setting = db.pool.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *setting)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET email = 'alice@example.com' WHERE name = 'alice'")
        .execute(&mut *setting)
        .await
        .unwrap();
    let pool = db.pool.clone();
    let deletion = tokio::spawn(async move {
        delete(&pool, "alice", UNLESS_EMAIL, &AuditActor::cli(), now()).await
    });
    let mut probe = db.pool.acquire().await.unwrap();
    crate::test_locks::wait_waiter(&mut probe, holder).await;

    setting.commit().await.unwrap();

    let outcome = tokio::time::timeout(LOCK_WAIT, deletion)
        .await
        .expect("the delete waited for the set for too long")
        .unwrap();
    assert_eq!(refusal(outcome), HAS_EMAIL_REFUSAL);
    assert_eq!(
        find(&db.pool, "alice").await.unwrap().email.as_deref(),
        Some("alice@example.com"),
        "the address is not lost"
    );
}

/// The conditional delete holds the user row when `set_email` arrives: the
/// set waits and then finds the user gone (it never lands on a user the
/// delete has already decided to remove).
#[tokio::test(flavor = "multi_thread")]
async fn a_set_email_that_waited_on_the_conditional_delete_finds_the_user_gone() {
    let db = test_db().await;
    seed_user(&db.pool, "alice", None).await;
    let mut deleting = db.pool.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *deleting)
        .await
        .unwrap();
    let user_id: String =
        sqlx::query_scalar("SELECT id FROM users WHERE name = 'alice' FOR UPDATE")
            .fetch_one(&mut *deleting)
            .await
            .unwrap();
    let pool = db.pool.clone();
    let setting = tokio::spawn(async move {
        set_email(
            &pool,
            "alice",
            Some("alice@example.com"),
            &AuditActor::cli(),
            now(),
        )
        .await
    });
    let mut probe = db.pool.acquire().await.unwrap();
    crate::test_locks::wait_waiter(&mut probe, holder).await;
    sqlx::query("DELETE FROM teams WHERE personal_of = $1")
        .bind(&user_id)
        .execute(&mut *deleting)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&user_id)
        .execute(&mut *deleting)
        .await
        .unwrap();
    deleting.commit().await.unwrap();

    let outcome = tokio::time::timeout(LOCK_WAIT, setting)
        .await
        .expect("the set waited for the delete for too long")
        .unwrap();
    assert!(
        matches!(outcome, Err(OkfError::UnknownName { .. })),
        "{outcome:?}"
    );
}

/// Real set-email and conditional deletes racing, in whatever order: the
/// user is never deleted after it was given an address.
#[tokio::test(flavor = "multi_thread")]
async fn a_racing_set_email_never_leads_to_a_deleted_user_that_had_one() {
    let db = test_db().await;
    for round in 0..8 {
        let name = format!("user-{round}");
        seed_user(&db.pool, &name, None).await;
        let address = format!("{name}@example.com");
        let (pool, who) = (db.pool.clone(), name.clone());
        let setting = tokio::spawn(async move {
            set_email(&pool, &who, Some(&address), &AuditActor::cli(), now()).await
        });
        let (pool, who) = (db.pool.clone(), name.clone());
        let deletion = tokio::spawn(async move {
            delete(&pool, &who, UNLESS_EMAIL, &AuditActor::cli(), now()).await
        });

        let (set, deleted) = (setting.await.unwrap(), deletion.await.unwrap());

        match (set, deleted) {
            // The set came first: the delete refused and the user keeps it.
            (Ok(()), Err(OkfError::Refused { reason })) => {
                assert_eq!(reason, HAS_EMAIL_REFUSAL);
                assert!(find(&db.pool, &name).await.unwrap().email.is_some());
            }
            // The delete came first: the set found nobody.
            (Err(OkfError::UnknownName { .. }), Ok(_)) => {
                assert!(find(&db.pool, &name).await.is_err());
            }
            other => panic!("a set and a conditional delete cannot both succeed: {other:?}"),
        }
    }
}

/// Waits until at least `count` backends of this test's database are
/// queued for a lock, whichever lock it is (a row, a transaction or an
/// index entry), so the race below holds in either lock order.
async fn until_waiting(pool: &PgPool, count: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(DISTINCT l.pid) FROM pg_locks l \
                 JOIN pg_stat_activity a ON a.pid = l.pid \
                 WHERE NOT l.granted AND a.datname = current_database()",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the two writers never both waited");
}

/// Review of PR #1197: a create that wrote its address after the
/// `server_state` singleton, racing the real `set_email` moving an address
/// onto the new user, deadlocked (the create waited on the address, the
/// set on the singleton) and Postgres aborted one with a database error.
/// The singleton is held here, so both writers queue behind it; when it is
/// released, each finishes with a success or the 409, never a database
/// error. (The holder tests above insert rows directly and never take the
/// singleton, so they could not show this.)
#[tokio::test(flavor = "multi_thread")]
async fn a_create_racing_the_real_set_email_for_one_address_never_deadlocks() {
    let db = test_db().await;
    seed_user(&db.pool, "mover", None).await;
    let mut holding = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE server_state SET admin_generation = admin_generation + 1")
        .execute(&mut *holding)
        .await
        .unwrap();
    let pool = db.pool.clone();
    let creating = tokio::spawn(async move {
        create_with_email(
            &pool,
            &named("newcomer"),
            Some("race@example.com"),
            &AuditActor::cli(),
            now(),
        )
        .await
        .map(|_| ())
    });
    // The create is first in line for the singleton, or (written address
    // first) is queued behind it already.
    until_waiting(&db.pool, 1).await;
    let pool = db.pool.clone();
    let setting = tokio::spawn(async move {
        set_email(
            &pool,
            "mover",
            Some("race@example.com"),
            &AuditActor::cli(),
            now(),
        )
        .await
    });
    until_waiting(&db.pool, 2).await;

    holding.commit().await.unwrap();

    let created = tokio::time::timeout(LOCK_WAIT, creating)
        .await
        .expect("the create never finished")
        .unwrap();
    let set = tokio::time::timeout(LOCK_WAIT, setting)
        .await
        .expect("the set never finished")
        .unwrap();
    // Exactly one writer got the address; the other was refused cleanly.
    match (created, set) {
        (Ok(()), Err(OkfError::Refused { reason }))
        | (Err(OkfError::Refused { reason }), Ok(())) => {
            assert_eq!(reason, EMAIL_TAKEN_REFUSAL);
        }
        other => panic!("one writer wins and the other is refused, never an error: {other:?}"),
    }
    let holders: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'race@example.com'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(holders, 1);
}
