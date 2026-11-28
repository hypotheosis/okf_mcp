//! Lock order between the changeset writers that record an audit entry
//! (publish, an admin's discard, an audited edit) and `users::delete`
//! (issue #418). Each writer takes the users its entry resolves before its
//! changeset, as the deletion does, so the two wait for each other instead
//! of deadlocking. Each forced test holds a row the writer needs after its
//! changeset lock, queues the deletion behind it, then releases the row.

use super::*;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::config::Config;
use crate::mcp::testing::{app_with, cached_principal, changeset_row, context, open, TestApp};
use crate::mcp::writes::{replace_body, ReplaceBodyParams};
use crate::store::audit::AuditActor;
use crate::store::changesets::ChangesetState;
use crate::store::keys::{self, Grants, NewKey};
use crate::store::users::{self, AdminChange, DeletedUser, NewUser};

/// The longest any one wait in these tests may take.
const LOCK_WAIT: Duration = Duration::from_secs(30);

const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";

async fn pg() -> TestApp {
    app_with(&[("pg", &[("a.md", NOTE)])], Config::default()).await
}

/// Creates user `name` (an admin user if `admin`) with an every-catalog
/// key at `access` defaulting to `pg`, and returns the
/// context of a connection with that key.
async fn user_context(test: &TestApp, name: &str, access: Access, admin: bool) -> ToolContext {
    let pool = test.app.accounts().executor();
    let cli = AuditActor::cli();
    let user = NewUser {
        name,
        display_name: &crate::labels::DisplayName::parse(name).unwrap(),
    };
    users::create(pool, &user, &cli, Utc::now())
        .await
        .expect("create the user");
    if admin {
        let change = AdminChange {
            on: true,
            allow_last: true,
        };
        users::set_admin(pool, name, change, &cli, Utc::now())
            .await
            .expect("make the user an admin");
    }
    let key_name = format!("{name}-key");
    let new = NewKey {
        name: &key_name,
        access,
        maintenance_report: false,
        grants: Grants::All,
        default_catalog: Some("pg"),
        actor: &key_name,
        user: name,
        self_issued: false,
        expires_at: None,
        levels: &[],
    };
    let secret = keys::create_with_id(pool, &new, &cli, Utc::now())
        .await
        .expect("create the key")
        .key;
    context(test, cached_principal(test, &secret).await, None)
}

async fn body(ctx: &ToolContext, handle: &str, text: &str) {
    let params = ReplaceBodyParams {
        path: "/a.md".to_string(),
        body: text.to_string(),
        changeset: handle.to_string(),
    };
    replace_body(ctx, params).await.expect("write a.md");
}

async fn publish_as(ctx: &ToolContext, handle: &str) -> Result<Published, OkfError> {
    let params = PublishParams {
        message: "race".to_string(),
        changeset: handle.to_string(),
    };
    publish(ctx, params).await
}

/// Deletes `name` with the CLI's actor, in its own task.
fn delete_in_task(
    test: &TestApp,
    name: &str,
) -> tokio::task::JoinHandle<Result<DeletedUser, OkfError>> {
    let pool = test.app.accounts().executor().clone();
    let name = name.to_string();
    tokio::spawn(async move {
        users::delete(
            &pool,
            &name,
            users::DeleteOptions::default(),
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
    })
}

/// Waits until `count` backends of this database wait on a lock.
async fn lock_waiters(test: &TestApp, count: i64) {
    let waited = tokio::time::timeout(LOCK_WAIT, async {
        loop {
            let waiting: i64 = sqlx::query_scalar(concat!(
                "SELECT count(*) FROM pg_stat_activity WHERE ",
                crate::test_locks::own_databases!("datname"),
                " AND wait_event_type = 'Lock'"
            ))
            .fetch_one(test.app.accounts().executor())
            .await
            .expect("read pg_stat_activity");
            if waiting >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{count} lock waiter(s) never appeared");
}

/// Awaits a spawned task under [`LOCK_WAIT`], naming `what` if it hangs.
async fn finish<T>(task: tokio::task::JoinHandle<T>, what: &str) -> T {
    tokio::time::timeout(LOCK_WAIT, task)
        .await
        .unwrap_or_else(|_| panic!("{what} did not finish: a lock cycle Postgres cannot see?"))
        .expect("the task panicked")
}

/// Runs the store cleanup the deletions owe, as the housekeeping task does.
async fn settle(test: &TestApp) {
    crate::store::cleanups::settle_for_tests(test.app.accounts().executor()).await;
}

async fn state_of(test: &TestApp, handle: &str) -> ChangesetState {
    changeset_row(test, handle).await.state
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_holding_its_changeset_finishes_before_its_users_deletion() {
    // Arrange: alice's changeset, and a holder of the catalog state row, so the
    // publish stops at its generation bump, after its changeset lock.
    let test = pg().await;
    let alice = Arc::new(user_context(&test, "alice", Access::Write, false).await);
    let handle = open(&alice).await;
    body(&alice, &handle, "# A\n\nAlice.\n").await;
    let catalog_id = test
        .app
        .catalog("pg")
        .await
        .unwrap()
        .expect("catalog pg")
        .id();
    let mut holder = test
        .app
        .main_db()
        .await
        .begin()
        .await
        .expect("begin the holder");
    sqlx::query("SELECT id FROM catalog_state WHERE id = $1 FOR NO KEY UPDATE")
        .bind(catalog_id)
        .execute(&mut *holder)
        .await
        .expect("hold the catalog row");
    let publisher = Arc::clone(&alice);
    let to_publish = handle.clone();
    let publishing = tokio::spawn(async move { publish_as(&publisher, &to_publish).await });
    lock_waiters(&test, 1).await;
    let deleting = delete_in_task(&test, "alice");
    lock_waiters(&test, 2).await;

    // Act
    holder.rollback().await.expect("release the catalog row");
    let published = finish(publishing, "the publish").await;
    let deleted = finish(deleting, "the deletion").await;

    // Assert: the publish held alice before its changeset, so the deletion
    // waited for it and found nothing open; neither side is `Busy`.
    assert!(published.is_ok(), "{published:?}");
    deleted.expect("the deletion");
    assert_eq!(state_of(&test, &handle).await, ChangesetState::Published);
}

/// Gives `name` a password and a console session, and answers a start of
/// the account's deletion as `DELETE /api/v1/me` does it (issue #355), in
/// its own task: the same deletion path, with the person as the actor and
/// the re-authentication checked under the user's lock. Prepared before
/// any lock is held, so only the deletion itself queues.
async fn own_deletion(
    test: &TestApp,
    name: &str,
) -> impl FnOnce() -> tokio::task::JoinHandle<Result<DeletedUser, OkfError>> {
    use crate::store::{audit, passwords, user_sessions};
    let pool = test.app.accounts().executor().clone();
    let user_id = users::id_by_name(&pool, name).await.expect("the user's id");
    let password = "violet staple orbit lantern";
    passwords::set(&pool, &user_id, password, Utc::now())
        .await
        .expect("set a password");
    let session = user_sessions::create(
        &pool,
        &user_id,
        None,
        "",
        Duration::from_secs(3600),
        Utc::now(),
    )
    .await
    .expect("a session")
    .id;
    let passwords::SignInOutcome::Ok(verified) =
        passwords::verify_user(&pool, &user_id, password, Utc::now())
            .await
            .expect("check the password")
    else {
        panic!("the password did not verify");
    };
    let actor = AuditActor {
        source: audit::Source::Console,
        user_id: Some(user_id.clone()),
        label: name.to_string(),
        via: audit::Via::Session,
        ..AuditActor::cli()
    };
    let name = name.to_string();
    // `move ||`: a closure that owns what it captured, called once later.
    move || {
        tokio::spawn(async move {
            let own = users::OwnDeletion {
                user_id: &user_id,
                name: &name,
                session_id: &session,
                proof: users::Proof::Password(&verified.stamp),
            };
            users::delete_own(&pool, &own, &actor, Utc::now()).await
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_holding_its_changeset_finishes_before_its_users_own_deletion() {
    // Arrange: as the CLI deletion's test above, with the person deleting
    // their own account (issue #355), whose extra checks take the session
    // row after the user and before any team, catalog or changeset.
    let test = pg().await;
    let alice = Arc::new(user_context(&test, "alice", Access::Write, false).await);
    let start_deleting = own_deletion(&test, "alice").await;
    let handle = open(&alice).await;
    body(&alice, &handle, "# A\n\nAlice.\n").await;
    let catalog_id = test
        .app
        .catalog("pg")
        .await
        .unwrap()
        .expect("catalog pg")
        .id();
    let mut holder = test
        .app
        .main_db()
        .await
        .begin()
        .await
        .expect("begin the holder");
    sqlx::query("SELECT id FROM catalog_state WHERE id = $1 FOR NO KEY UPDATE")
        .bind(catalog_id)
        .execute(&mut *holder)
        .await
        .expect("hold the catalog row");
    let publisher = Arc::clone(&alice);
    let to_publish = handle.clone();
    let publishing = tokio::spawn(async move { publish_as(&publisher, &to_publish).await });
    lock_waiters(&test, 1).await;
    let deleting = start_deleting();
    lock_waiters(&test, 2).await;

    // Act
    holder.rollback().await.expect("release the catalog row");
    let published = finish(publishing, "the publish").await;
    let deleted = finish(deleting, "the own deletion").await;

    // Assert: neither deadlocked nor was `Busy`; the deletion waited for
    // the publish and found nothing open.
    assert!(published.is_ok(), "{published:?}");
    deleted.expect("the own deletion");
    assert_eq!(state_of(&test, &handle).await, ChangesetState::Published);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admins_discard_holding_a_changeset_finishes_before_its_owners_deletion() {
    // Arrange: an admin user's key discards alice's changeset, and a holder
    // of its entry stops the discard after its changeset lock. The deletion
    // takes every admin row first, the discard's actor included.
    let test = pg().await;
    let alice = user_context(&test, "alice", Access::Write, false).await;
    let boss = user_context(&test, "boss", Access::Admin, true).await;
    let handle = open(&alice).await;
    body(&alice, &handle, "# A\n\nAlice.\n").await;
    let changeset_id = changeset_row(&test, &handle).await.id;
    let mut holder = test
        .app
        .main_db()
        .await
        .begin()
        .await
        .expect("begin the holder");
    sqlx::query("SELECT path FROM catalog.changeset_entries WHERE changeset_id = $1 FOR UPDATE")
        .bind(changeset_id)
        .execute(&mut *holder)
        .await
        .expect("hold the entry");
    let to_discard = handle.clone();
    let discarding = tokio::spawn(async move {
        let params = DiscardParams {
            paths: None,
            changeset: to_discard,
        };
        discard(&boss, params).await
    });
    lock_waiters(&test, 1).await;
    let deleting = delete_in_task(&test, "alice");
    lock_waiters(&test, 2).await;

    // Act
    holder.rollback().await.expect("release the entry");
    let discarded = finish(discarding, "the discard").await;
    let deleted = finish(deleting, "the deletion").await;

    // Assert
    assert!(discarded.expect("the discard").closed);
    deleted.expect("the deletion");
    assert_eq!(state_of(&test, &handle).await, ChangesetState::Discarded);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_holding_its_changeset_finishes_before_its_users_deletion() {
    // Arrange: alice reverts a.md in her changeset (an audited edit), and a
    // holder of the entry stops the revert after its changeset lock.
    let test = pg().await;
    let alice = Arc::new(user_context(&test, "alice", Access::Write, false).await);
    let handle = open(&alice).await;
    body(&alice, &handle, "# A\n\nAlice.\n").await;
    let changeset_id = changeset_row(&test, &handle).await.id;
    let mut holder = test
        .app
        .main_db()
        .await
        .begin()
        .await
        .expect("begin the holder");
    sqlx::query("SELECT path FROM catalog.changeset_entries WHERE changeset_id = $1 FOR UPDATE")
        .bind(changeset_id)
        .execute(&mut *holder)
        .await
        .expect("hold the entry");
    let reverter = Arc::clone(&alice);
    let to_revert = handle.clone();
    let reverting = tokio::spawn(async move {
        let params = RevertParams {
            path: "/a.md".to_string(),
            revision: 1,
            expected_revision: None,
            changeset: to_revert,
        };
        revert(&reverter, params).await
    });
    lock_waiters(&test, 1).await;
    let deleting = delete_in_task(&test, "alice");
    lock_waiters(&test, 2).await;

    // Act
    holder.rollback().await.expect("release the entry");
    let reverted = finish(reverting, "the revert").await;
    let deleted = finish(deleting, "the deletion").await;

    // Assert: the revert committed first, and left the changeset open; the
    // deletion owed its discard, which the store cleanup then does.
    assert!(reverted.is_ok(), "{reverted:?}");
    deleted.expect("the deletion");
    assert_eq!(state_of(&test, &handle).await, ChangesetState::Open);
    settle(&test).await;
    assert_eq!(state_of(&test, &handle).await, ChangesetState::Discarded);
}

/// Issue #970, Brooke's review of #1001: a change that audits holds an
/// account transaction and a store transaction, and a user deletion holds
/// the user's rows and then needs a store connection. If the change took the
/// store's only connection BEFORE it locked the users, the deletion (holding
/// the rows) and the change (holding the connection) would wait on each
/// other with one edge a pool wait, which PostgreSQL's deadlock detector
/// does not see. Account locks first, store connection second, removes it.
///
/// The order is forced, not raced: a holder keeps alice's row, the deletion
/// queues for it first, the revert queues second, and the holder lets go. The
/// store pool has ONE connection, the account pool several, and both sides
/// use the app's own registry.
#[tokio::test(flavor = "multi_thread")]
async fn an_audited_revert_and_its_users_deletion_share_one_store_connection() {
    // Arrange
    let config = Config {
        store_max_connections: 1,
        ..Config::default()
    };
    let test = app_with(&[("pg", &[("a.md", NOTE)])], config).await;
    let alice = Arc::new(user_context(&test, "alice", Access::Write, false).await);
    let handle = open(&alice).await;
    body(&alice, &handle, "# A\n\nAlice.\n").await;
    let store = test.app.main_db().await;
    assert!(
        test.app
            .accounts()
            .executor()
            .options()
            .get_max_connections()
            >= 3,
        "the account pool must have connections to spare"
    );
    assert_eq!(store.pool().options().get_max_connections(), 1);
    let mut holder = test.app.accounts().begin().await.expect("begin the holder");
    sqlx::query("SELECT id FROM users WHERE name = 'alice' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .expect("hold alice's row");
    let pool = test.app.accounts().executor().clone();
    let deleting = tokio::spawn(async move {
        users::delete_on(
            &pool,
            "alice",
            users::DeleteOptions::default(),
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
    });
    lock_waiters(&test, 1).await;
    let reverter = Arc::clone(&alice);
    let to_revert = handle.clone();
    let reverting = tokio::spawn(async move {
        let params = RevertParams {
            path: "/a.md".to_string(),
            revision: 1,
            expected_revision: None,
            changeset: to_revert,
        };
        revert(&reverter, params).await
    });
    lock_waiters(&test, 2).await;

    // Act: the deletion is first in the queue, so it gets the rows first.
    holder.rollback().await.expect("release alice's row");

    // Assert: both finish (a hang here is the invisible cycle). The deletion
    // wins; the revert then finds its user gone and is refused, or, if it
    // got in first, commits.
    let deleted = finish(deleting, "the deletion").await;
    let _ = finish(reverting, "the revert").await;
    deleted.expect("the deletion");
}

/// Issue #970, Brooke's review of #1001 (the `catalogs.rs` comment): a forced
/// catalog deletion locks the open changesets on its store transaction and
/// then clears the default of the keys that name the catalog on its account
/// transaction; a user deletion locks its key and then waits for its
/// changeset. Taken in that order the two wait on each other across the two
/// databases. The catalog deletion now locks those keys before its store
/// transaction. Forced order: a holder keeps the changeset, the catalog
/// deletion queues first, the user deletion second, the holder lets go.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_catalog_deletion_and_a_users_deletion_never_wait_on_each_other() {
    // Arrange
    let config = Config {
        store_max_connections: 3,
        ..Config::default()
    };
    let test = app_with(&[("pg", &[("a.md", NOTE)])], config).await;
    let bob = Arc::new(user_context(&test, "bob", Access::Write, false).await);
    let handle = open(&bob).await;
    body(&bob, &handle, "# A\n\nBob.\n").await;
    let changeset_id = changeset_row(&test, &handle).await.id;
    let store = test.app.main_db().await;
    let mut holder = store.begin().await.expect("begin the holder");
    sqlx::query("SELECT id FROM changesets WHERE id = $1 FOR UPDATE")
        .bind(changeset_id)
        .execute(&mut *holder)
        .await
        .expect("hold the changeset");
    let catalog_store = store.clone();
    let dropping = tokio::spawn(async move {
        crate::store::catalogs::delete_on(
            &catalog_store,
            "pg",
            true,
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
    });
    lock_waiters(&test, 1).await;
    let pool = test.app.accounts().executor().clone();
    let deleting = tokio::spawn(async move {
        users::delete_on(
            &pool,
            "bob",
            users::DeleteOptions::default(),
            &AuditActor::cli(),
            Utc::now(),
        )
        .await
    });
    lock_waiters(&test, 2).await;

    // Act
    holder.rollback().await.expect("release the changeset");

    // Assert: both finish, and neither is the victim of a deadlock.
    finish(dropping, "the catalog deletion")
        .await
        .expect("the catalog deletion");
    let deleted = finish(deleting, "the user deletion").await;
    deleted.expect("the user deletion");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_racing_its_users_deletion_never_deadlocks() {
    // The issue's race, unforced and repeated: one side wins cleanly every
    // time, and the losing publish is refused, never `Busy`.
    let test = pg().await;
    for round in 0..20 {
        // Arrange
        let name = format!("racer{round}");
        let racer = Arc::new(user_context(&test, &name, Access::Write, false).await);
        let handle = open(&racer).await;
        body(&racer, &handle, &format!("# A\n\nRound {round}.\n")).await;
        let publisher = Arc::clone(&racer);
        let to_publish = handle.clone();

        // Act: both start at once.
        let publishing = tokio::spawn(async move { publish_as(&publisher, &to_publish).await });
        let deleting = delete_in_task(&test, &name);
        let published = finish(publishing, "the publish").await;
        let deleted = finish(deleting, "the deletion").await;

        // Assert
        deleted.unwrap_or_else(|error| panic!("round {round}: {error:?}"));
        let state = state_of(&test, &handle).await;
        match published {
            Ok(_) => {
                assert_eq!(state, ChangesetState::Published, "round {round}");
            }
            // The deletion won. A publish that resolved the handle first and
            // then waited finds the changeset closed under its lock
            // (`Refused`, "no longer open"); one that resolved it after the
            // commit finds its key gone, so the handle names nothing it may
            // use (`UnknownChangeset`). Never `Busy`.
            Err(error) => {
                let refused_under_the_lock = matches!(
                    &error,
                    OkfError::Refused { reason } if reason.contains("is no longer open")
                );
                assert!(
                    refused_under_the_lock || matches!(error, OkfError::UnknownChangeset { .. }),
                    "round {round}: {error:?}"
                );
                // Left open by the deletion, which owes its discard.
                assert_eq!(state, ChangesetState::Open, "round {round}");
                settle(&test).await;
                assert_eq!(
                    state_of(&test, &handle).await,
                    ChangesetState::Discarded,
                    "round {round}"
                );
            }
        }
    }
}

/// Issue #972: a catalog deletion racing an audited edit by an admin whose
/// user row is locked by a concurrent user deletion (a holder stands in for
/// that deletion's lock). With one store connection, the admin's discard must
/// wait for the user row holding no store connection and no store lock; the
/// forced catalog deletion then runs to the end first. A discard that took
/// the store before the user row would hold the only store connection while
/// it waited, and the two would wait on each other, where Postgres cannot
/// see the cycle: the bounded waits turn that into a failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_deletion_racing_an_admins_audited_edit_behind_a_user_deletion_finishes() {
    // Arrange
    let config = Config {
        store_max_connections: 1,
        ..Config::default()
    };
    let test = app_with(&[("pg", &[("a.md", NOTE)])], config).await;
    let alice = user_context(&test, "alice", Access::Write, false).await;
    let boss = user_context(&test, "boss", Access::Admin, true).await;
    let handle = open(&alice).await;
    body(&alice, &handle, "# A\n\nAlice.\n").await;
    let store = test.app.main_db().await;
    assert_eq!(store.pool().options().get_max_connections(), 1);
    let mut holder = test.app.accounts().begin().await.expect("begin the holder");
    sqlx::query("SELECT id FROM users WHERE name = 'boss' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .expect("hold boss's row");
    let to_discard = handle.clone();
    let discarding = tokio::spawn(async move {
        let params = DiscardParams {
            paths: None,
            changeset: to_discard,
        };
        discard(&boss, params).await
    });
    lock_waiters(&test, 1).await;

    // Act: the forced catalog deletion, while the discard waits for the row.
    let deleting = tokio::spawn(async move {
        crate::store::catalogs::delete_on(&store, "pg", true, &AuditActor::cli(), Utc::now()).await
    });
    let deleted = finish(deleting, "the catalog deletion").await;
    holder.rollback().await.expect("release boss's row");
    let discarded = finish(discarding, "the discard").await;

    // Assert: the deletion finished with the discard still waiting; the
    // discard then found its changeset closed or its catalog gone.
    deleted.expect("the catalog deletion");
    assert!(discarded.is_err(), "{discarded:?}");
    assert_eq!(state_of(&test, &handle).await, ChangesetState::Discarded);
}

/// Issue #972, Task 11's hand-off: a key's deletion and an `open_changeset`
/// of that key are serialised on the key's row, as the foreign key that went
/// in 0041 did. A holder stands in for the deletion's lock on the key row; the
/// open waits for it, holding no store connection, and once the key is gone
/// it is refused and leaves no changeset under the deleted key.
#[tokio::test(flavor = "multi_thread")]
async fn a_changeset_opened_while_its_key_is_deleted_is_refused_and_leaves_nothing() {
    // Arrange
    let config = Config {
        store_max_connections: 1,
        ..Config::default()
    };
    let test = app_with(&[("pg", &[("a.md", NOTE)])], config).await;
    let alice = user_context(&test, "alice", Access::Write, false).await;
    let key_id: String = sqlx::query_scalar("SELECT id FROM api_keys WHERE name = 'alice-key'")
        .fetch_one(test.app.accounts().executor())
        .await
        .expect("alice's key");
    let mut holder = test.app.accounts().begin().await.expect("begin the holder");
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(&key_id)
        .execute(&mut *holder)
        .await
        .expect("hold the key");
    let opening = tokio::spawn(async move {
        crate::mcp::changesets::open_changeset(
            &alice,
            crate::mcp::changesets::OpenParams { catalog: None },
        )
        .await
    });
    lock_waiters(&test, 1).await;

    // Act: the deletion's effect on the key, then its commit.
    sqlx::query("DELETE FROM api_keys WHERE id = $1")
        .bind(&key_id)
        .execute(&mut *holder)
        .await
        .expect("delete the key");
    holder.commit().await.expect("commit the deletion");
    let opened = finish(opening, "the open").await;

    // Assert
    assert!(
        matches!(&opened, Err(OkfError::Forbidden { reason }) if reason.contains("no longer exists")),
        "{opened:?}"
    );
    let under_key: i64 =
        sqlx::query_scalar("SELECT count(*) FROM catalog.changesets WHERE key_id = $1")
            .bind(&key_id)
            .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
            .await
            .expect("count the changesets");
    assert_eq!(under_key, 0);
}
