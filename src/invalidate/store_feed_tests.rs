//! The change feed of each separate store (Task 17): a listener per opened
//! store, so a publish, import or repair on a store's database (which sends
//! its `NOTIFY` from the store's own transaction) reaches every replica.
//!
//! Every test registers its stores as `host` stores on databases of their own
//! (`TestDb::extra_store_database`), so it runs the same in both
//! `OKF_TEST_STORE` modes. Nothing here sleeps to wait: a test waits on the
//! listener's catch-up log, or on the cache state it asserts, with a bound.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::{listen_signalling, spawn_store_feed, Invalidation, StoreFeedStart};
use crate::app::App;
use crate::clock::SystemClock;
use crate::config::Config;
use crate::db::{test_db, TEST_TEAM};
use crate::labels::DisplayName;
use crate::store::audit::AuditActor;
use crate::store::catalogs::{self, NewCatalog};
use crate::store::changesets::{self, EntryRow};
use crate::store::import::{import, ImportOptions, ImportScope};
use crate::store::publish::{publish, PublishInput};
use crate::store::repair::{repair, RepairOptions};
use crate::store::teams;
use crate::stores::{PoolSettings, StoreName, Stores};

/// Longest a test waits for anything.
const WAIT: Duration = Duration::from_secs(30);

const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";
const NOTE_EDITED: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n\nEdited.\n";

/// Registers store `name` on a new database of its own.
pub(crate) async fn add_store(db: &crate::db::pg_support::TestDb, name: &str) -> String {
    let database = db.extra_store_database(&name.replace('-', "")).await;
    let options = db.pool.connect_options();
    sqlx::query(
        "INSERT INTO stores (name, location_kind, host, port, database, \
         credential_kind, credential_ref, created_at) \
         VALUES ($1, 'host', $2, $3, $4, 'env', $5, now())",
    )
    .bind(name)
    .bind(options.get_host())
    .bind(i32::from(options.get_port()))
    .bind(&database)
    .bind(crate::db::pg::STORE_PASSWORD_VAR)
    .execute(&db.pool)
    .await
    .unwrap();
    database
}

/// Creates catalog `name` on `store` (`None`: the default store) and imports
/// one note into it (generation 1).
pub(crate) async fn catalog_on(
    stores: &Stores,
    pool: &PgPool,
    name: &str,
    store: Option<&StoreName>,
) -> catalogs::Catalog {
    teams::set_store(pool, TEST_TEAM, store, &AuditActor::cli(), Utc::now())
        .await
        .unwrap();
    let new = NewCatalog {
        name,
        display_name: &DisplayName::default(),
        description: "",
        owner_team: TEST_TEAM,
    };
    let created = catalogs::create_in(stores, &new, &AuditActor::cli(), Utc::now())
        .await
        .unwrap();
    let store = store.cloned().unwrap_or_else(StoreName::main);
    let options = ImportOptions {
        replace: false,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "tester",
        audit: &AuditActor::cli(),
        now: Utc::now(),
    };
    import(
        &stores.get(&store).await.unwrap(),
        name,
        crate::archive::files(&[("a.md", NOTE)]),
        &options,
    )
    .await
    .unwrap();
    created
}

/// A replica: an app with its account listener and store listeners.
struct Replica {
    app: Arc<App>,
    shutdown: CancellationToken,
    account_listener: JoinHandle<()>,
}

impl Replica {
    /// Loads an app on `pool` (a long `catalog_ttl`, so only the change feed
    /// can refresh a cache within a test), starts its account listener and
    /// has its registry follow it.
    async fn start(pool: PgPool) -> Self {
        let config = Config {
            catalog_ttl: Duration::from_secs(3600),
            ..Config::default()
        };
        let app = App::load(pool, config, Arc::new(SystemClock))
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let caught_up = Arc::new(tokio::sync::Notify::new());
        let account_listener = tokio::spawn(listen_signalling(
            Arc::clone(&app),
            shutdown.clone(),
            Some(Arc::clone(&caught_up)),
        ));
        timeout(WAIT, caught_up.notified())
            .await
            .expect("the account listener subscribed within the wait");
        app.stores().follow(Arc::downgrade(&app), shutdown.clone());
        Self {
            app,
            shutdown,
            account_listener,
        }
    }

    /// Opens `store`'s pool (which starts its listener) and waits until the
    /// listener has subscribed and caught up `times` times.
    async fn follow_store(&self, store: &StoreName, times: usize) {
        let mut log = self.app.stores().catch_ups();
        self.app.stores().get(store).await.unwrap();
        timeout(
            WAIT,
            log.wait_for(|log| log.iter().filter(|name| *name == store).count() >= times),
        )
        .await
        .expect("the store's listener caught up within the wait")
        .unwrap();
    }

    async fn stop(self) {
        self.shutdown.cancel();
        timeout(WAIT, self.account_listener)
            .await
            .expect("the account listener stopped within the wait")
            .unwrap();
    }
}

/// The pids of the listening backends on `database`.
async fn listeners_on(pool: &PgPool, database: &str) -> Vec<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_stat_activity WHERE datname = $1 AND query LIKE 'LISTEN%' \
         AND pid <> pg_backend_pid() ORDER BY pid",
    )
    .bind(database)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Edits `a.md` of `catalog` on `store` and publishes it through `app`. The
/// changeset is made on the catalog's store directly: the tools find a
/// changeset by handle on `main` only until Task 18.
async fn edit_and_publish(app: &Arc<App>, store: &StoreName, catalog: &str) {
    let state = app.catalog(catalog).await.unwrap().unwrap();
    let db = app.stores().get(store).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    changesets::lock_catalog(&mut tx, state.id()).await.unwrap();
    let changeset = changesets::open(&mut tx, state.id(), "writer-a", "writer-a", Utc::now())
        .await
        .unwrap();
    changesets::upsert_entry(
        &mut tx,
        changeset.id,
        &EntryRow {
            path: "a.md".to_owned(),
            base_revision: Some(1),
            content: Some(NOTE_EDITED.as_bytes().to_vec()),
            executable: false,
            replay: None,
        },
        Utc::now(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let report = publish(
        app,
        &state,
        &PublishInput {
            changeset: &changeset,
            actor: "writer-a",
            message: "edit a",
        },
        &AuditActor::cli(),
    )
    .await
    .unwrap();
    assert!(report.published);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_on_a_separate_store_reaches_the_other_replica() {
    let db = test_db().await;
    add_store(&db, "store-b").await;
    let store_b = StoreName::parse("store-b").unwrap();
    let stores = Stores::new(
        db.pool.clone(),
        PoolSettings::from_config(&Config::default()),
    );
    catalog_on(&stores, &db.pool, "on-b", Some(&store_b)).await;
    stores.forget_all().await;

    // Two replicas of one account database; each has its own pools and
    // listeners, and neither is told what the other writes except by the feed.
    let writer = Replica::start(db.pool.clone()).await;
    let second_pool = crate::db::connect(&db.url, 4).await.unwrap();
    let reader = Replica::start(second_pool).await;
    writer.follow_store(&store_b, 1).await;
    reader.follow_store(&store_b, 1).await;

    // The reader caches the catalog at its current generation.
    let before = reader
        .app
        .catalog("on-b")
        .await
        .unwrap()
        .unwrap()
        .current()
        .generation();

    edit_and_publish(&writer.app, &store_b, "on-b").await;

    // The reader's directory row lives for an hour, so only the store's
    // message can have moved it: it drops the row, the next read loads the
    // new generation, and the snapshot is brought up to it.
    timeout(WAIT, async {
        loop {
            let seen = reader
                .app
                .catalog("on-b")
                .await
                .unwrap()
                .unwrap()
                .current()
                .generation();
            if seen > before {
                return seen;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the other replica saw the publish within the wait");

    writer.stop().await;
    reader.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_store_listener_drops_only_that_stores_catalogs() {
    let db = test_db().await;
    let database_b = add_store(&db, "store-b").await;
    add_store(&db, "store-c").await;
    let (store_b, store_c) = (
        StoreName::parse("store-b").unwrap(),
        StoreName::parse("store-c").unwrap(),
    );
    let stores = Stores::new(
        db.pool.clone(),
        PoolSettings::from_config(&Config::default()),
    );
    catalog_on(&stores, &db.pool, "on-main", None).await;
    catalog_on(&stores, &db.pool, "on-b", Some(&store_b)).await;
    catalog_on(&stores, &db.pool, "on-c", Some(&store_c)).await;
    stores.forget_all().await;

    let replica = Replica::start(db.pool.clone()).await;
    for store in [&StoreName::main(), &store_b, &store_c] {
        replica.app.stores().get(store).await.unwrap();
    }
    replica.follow_store(&store_b, 1).await;
    replica.follow_store(&store_c, 1).await;
    // Every catalog's directory row is cached.
    for name in ["on-main", "on-b", "on-c"] {
        replica.app.catalog_row(name).await.unwrap().unwrap();
        assert!(replica.app.directory().is_cached(name).await, "{name}");
    }

    // End the backend that listens on store b, by its pid. sqlx reconnects,
    // and the listener catches that store up.
    let pids = listeners_on(&db.pool, &database_b).await;
    assert_eq!(pids.len(), 1, "one listener on store b: {pids:?}");
    let ended: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pids[0])
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(ended);
    replica.follow_store(&store_b, 2).await;

    let directory = replica.app.directory();
    assert!(!directory.is_cached("on-b").await, "store b's catalog");
    assert!(directory.is_cached("on-c").await, "store c's catalog");
    assert!(directory.is_cached("on-main").await, "the default store's");
    // The listener is back, on a new connection.
    let after = listeners_on(&db.pool, &database_b).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_ne!(after, pids);

    replica.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forgetting_a_store_ends_its_listener_and_closes_its_connection() {
    let db = test_db().await;
    let database_b = add_store(&db, "store-b").await;
    let store_b = StoreName::parse("store-b").unwrap();
    let replica = Replica::start(db.pool.clone()).await;
    replica.follow_store(&store_b, 1).await;
    assert_eq!(listeners_on(&db.pool, &database_b).await.len(), 1);

    replica.app.stores().forget(&store_b).await;

    timeout(WAIT, async {
        while !listeners_on(&db.pool, &database_b).await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the listener's connection closed within the wait");
    // No pool is open for it, so none runs; the next use opens both again.
    assert_eq!(replica.app.stores().feed_finished(&store_b).await, None);
    replica.follow_store(&store_b, 2).await;
    assert_eq!(listeners_on(&db.pool, &database_b).await.len(), 1);

    replica.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_that_cannot_be_listened_to_does_not_hold_up_another() {
    let db = test_db().await;
    add_store(&db, "store-b").await;
    let store_b = StoreName::parse("store-b").unwrap();
    let replica = Replica::start(db.pool.clone()).await;

    // A listener whose database is not there: its connect fails, and it
    // keeps trying on its own task.
    let options = db
        .pool
        .connect_options()
        .as_ref()
        .clone()
        .port(1)
        .host("127.0.0.1");
    let unreachable = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(options);
    let bad = spawn_store_feed(StoreFeedStart {
        app: Arc::downgrade(&replica.app),
        name: StoreName::parse("store-c").unwrap(),
        pool: unreachable,
        connect_timeout: Duration::from_secs(1),
        shutdown: replica.shutdown.clone(),
        caught_up: None,
    });

    // Another store's listener, and the account's, work meanwhile.
    replica.follow_store(&store_b, 1).await;
    assert!(!bad.is_finished());
    assert_eq!(
        replica.app.stores().feed_finished(&store_b).await,
        Some(false)
    );

    // Dropping it ends it (it would otherwise retry until the shutdown).
    let task = bad.into_task();
    timeout(WAIT, task)
        .await
        .expect("the listener ended within the wait")
        .unwrap();
    replica.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shutdown_ends_a_store_listener() {
    let db = test_db().await;
    let database_b = add_store(&db, "store-b").await;
    let store_b = StoreName::parse("store-b").unwrap();
    let replica = Replica::start(db.pool.clone()).await;
    replica.follow_store(&store_b, 1).await;

    replica.shutdown.cancel();

    timeout(WAIT, async {
        while !replica.app.stores().feed_finished(&store_b).await.unwrap() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the listener ended within the wait");
    timeout(WAIT, async {
        while !listeners_on(&db.pool, &database_b).await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("its connection closed within the wait");
    replica.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_in_the_account_database_gets_no_listener_of_its_own() {
    let db = test_db().await;
    let replica = Replica::start(db.pool.clone()).await;
    let main = StoreName::main();
    replica.app.stores().get(&main).await.unwrap();
    let account = db.pool.connect_options().get_database().unwrap().to_owned();

    // `main` is the account database in `same` mode and another database in
    // `separate` mode: the test checks whichever it is.
    let inside = crate::stores::same_database(&db.pool, &crate::db::store_pool(&db.pool));
    let feed = replica.app.stores().feed_finished(&main).await;
    if inside {
        assert_eq!(
            feed, None,
            "no listener for a store in the account database"
        );
        // Only the account listener listens there.
        assert_eq!(listeners_on(&db.pool, &account).await.len(), 1);
    } else {
        assert_eq!(feed, Some(false), "a listener for a separate database");
        replica.follow_store(&main, 1).await;
        assert_eq!(listeners_on(&db.pool, &account).await.len(), 1);
        let store = db
            .store()
            .connect_options()
            .get_database()
            .unwrap()
            .to_owned();
        assert_eq!(listeners_on(&db.pool, &store).await.len(), 1);
    }
    replica.stop().await;
}

/// The invalidations a write sent on the account database and on `store`'s.
async fn sent_by<T>(
    account: &PgPool,
    store: &PgPool,
    write: impl std::future::Future<Output = T>,
) -> (Vec<Invalidation>, Vec<Invalidation>) {
    let (_, mut by_database) =
        crate::invalidate::testing::messages_by_database(&[account, store], write).await;
    let on_store = by_database.pop().unwrap();
    let on_account = by_database.pop().unwrap();
    (on_account, on_store)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_an_import_and_a_repair_send_their_catalog_message_on_the_store() {
    let db = test_db().await;
    add_store(&db, "store-b").await;
    let store_b = StoreName::parse("store-b").unwrap();
    let stores = Stores::new(
        db.pool.clone(),
        PoolSettings::from_config(&Config::default()),
    );
    let created = catalog_on(&stores, &db.pool, "on-b", Some(&store_b)).await;
    let replica = Replica::start(db.pool.clone()).await;
    let store = replica.app.stores().get(&store_b).await.unwrap();
    let store = store.pool();
    let audit = AuditActor::cli();
    let expected = |sent: &[Invalidation]| matches!(sent, [Invalidation::Catalog { id, .. }] if *id == created.id);

    let published = sent_by(&db.pool, store, async {
        edit_and_publish(&replica.app, &store_b, "on-b").await;
    })
    .await;
    let imported = sent_by(&db.pool, store, async {
        let options = ImportOptions {
            replace: true,
            scope: ImportScope::Everything,
            expect_generation: None,
            expect_catalog_id: None,
            actor: "tester",
            audit: &audit,
            now: Utc::now(),
        };
        import(
            &replica.app.stores().get(&store_b).await.unwrap(),
            "on-b",
            crate::archive::files(&[("b.md", NOTE)]),
            &options,
        )
        .await
        .unwrap();
    })
    .await;
    let repaired = sent_by(&db.pool, store, async {
        let options = RepairOptions {
            dry_run: false,
            actor: "tester",
            audit: &audit,
            now: Utc::now(),
        };
        repair(
            &replica.app.stores().get(&store_b).await.unwrap(),
            "on-b",
            &options,
        )
        .await
        .unwrap();
    })
    .await;

    // Each writer's message went out on the store's database, and nothing
    // went out on the account database: a sender moved back onto the account
    // transaction fails here (and loses the message if that commit failed).
    for (writer, (on_account, on_store)) in [
        ("publish", published),
        ("import", imported),
        ("repair", repaired),
    ] {
        assert!(expected(&on_store), "{writer}: on the store: {on_store:?}");
        assert_eq!(on_account, [], "{writer}: on the account database");
    }
    replica.stop().await;
}
