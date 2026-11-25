//! Catalog generation announcements between two replicas (issue #659), and
//! the refresh on a changeset whose base is newer than a replica's snapshot.
//!
//! [`replica_pair`] loads two apps on one database and one Redis, and no
//! change-feed listener runs in a unit test: a replica's
//! snapshot moves only through what a test calls or the announcement
//! subscriber it starts.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{listen_catalogs, run, Events, Snapshots, Source};
use crate::app::App;
use crate::config::Config;
use crate::db::redis_support::TestRedis;
use crate::error::OkfError;
use crate::mcp::changesets::{pending_changes, publish, PendingParams, PublishParams};
use crate::mcp::context::ToolContext;
use crate::mcp::documents::{read_document, ReadDocumentParams};
use crate::mcp::testing::{app_with, open, principal_as, replica_pair, writer, TestApp};
use crate::mcp::writes::{edit_document, EditDocumentParams};
use crate::shared::SubscriptionEvent;
use crate::store::audit::AuditActor;
use crate::store::catalogs::{self, CatalogGeneration};
use crate::store::keys::Access;
use crate::telemetry::metrics::AnnouncementResult;

/// The longest any wait for a replica may take.
const WAIT: Duration = Duration::from_secs(10);

const INDEX: &str = "# PG\n\n# Sections\n\n* [Infra](/infra/index.md) - systems\n";
const INFRA: &str = "# Infra\n\n# Concepts\n\n* [X](x.md) - x\n";
const X: &str = "---\ntype: Note\ntitle: X\ndescription: X.\n---\n# X\n";

/// Two replicas sharing one catalog `pg`.
async fn pair() -> (TestApp, Arc<App>, TestRedis) {
    pair_with(Config::default()).await
}

/// A config whose cached catalog rows do not expire during a test, so the TTL
/// backstop cannot show a publish and hide a missing announcement or catch-up.
fn never_expiring() -> Config {
    Config {
        catalog_ttl: Duration::from_secs(3600),
        ..Config::default()
    }
}

/// Two replicas sharing one catalog `pg`, started with `config`.
async fn pair_with(config: Config) -> (TestApp, Arc<App>, TestRedis) {
    let files: &[(&str, &str)] = &[
        ("index.md", INDEX),
        ("infra/index.md", INFRA),
        ("infra/x.md", X),
    ];
    tokio::time::timeout(
        Duration::from_secs(120),
        replica_pair(&[("pg", files)], config),
    )
    .await
    .expect("starting two replicas within 120 s")
}

/// A running catalog subscriber.
struct Listener {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Listener {
    /// Starts `app`'s catalog subscriber and waits until Redis counts it.
    async fn start(app: &Arc<App>) -> Self {
        let (shutdown, task) = crate::shared::testing::listening_for_catalogs(app).await;
        Self { shutdown, task }
    }

    /// Stops the subscriber and waits for it.
    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("the subscriber stops")
            .expect("the subscriber's task");
    }
}

/// `alice`'s connection on `app`, the key `writer` stored on the first app.
fn alice_on(app: &Arc<App>) -> ToolContext {
    ToolContext::new(
        Arc::clone(app),
        principal_as("alice", Access::Write, &[], Some("pg")),
        None,
    )
}

/// Replaces `old` with `new` in `path` in the changeset `handle`.
async fn edit(ctx: &ToolContext, handle: &str, path: &str, old: &str, new: &str) {
    let params = EditDocumentParams {
        path: path.to_string(),
        old_str: old.to_string(),
        new_str: new.to_string(),
        replace_all: false,
        changeset: handle.to_string(),
    };
    edit_document(ctx, params).await.expect("the edit");
}

/// Publishes the changeset `handle`.
async fn publish_now(ctx: &ToolContext, handle: &str) {
    let params = PublishParams {
        message: "test".to_string(),
        changeset: handle.to_string(),
    };
    let published = publish(ctx, params).await.expect("the publish");
    assert!(published.report.published, "something was published");
}

/// One changeset that replaces `old` with `new` in `path`, published.
async fn published_edit(ctx: &ToolContext, path: &str, old: &str, new: &str) {
    let handle = open(ctx).await;
    edit(ctx, &handle, path, old, new).await;
    publish_now(ctx, &handle).await;
}

/// `path` as `read_document` answers it on `ctx`'s app, in a call of its
/// own (a fresh context: a call pins the snapshot it reads, M18 Decision 3).
async fn read(ctx: &ToolContext, path: &str, changeset: Option<&str>) -> String {
    let params = ReadDocumentParams {
        path: Some(path.to_string()),
        catalog: Some("pg".to_string()),
        changeset: changeset.map(str::to_string),
    };
    read_document(&ctx.fresh(), params)
        .await
        .expect("the read")
        .text
}

/// Waits (bounded by [`WAIT`]) until `read` of `path` on `ctx` contains
/// `text`. Propagation is asynchronous and best effort: the write's answer
/// does not wait for another replica's refresh, so these tests check that
/// the change arrives promptly, never that the first read is fresh.
async fn until_read(ctx: &ToolContext, path: &str, text: &str) -> bool {
    tokio::time::timeout(WAIT, async {
        while !read(ctx, path, None).await.contains(text) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// The catalog's generation in the database.
async fn stored_generation(app: &App, name: &str) -> CatalogGeneration {
    let row = catalogs::find(app.accounts().executor(), name)
        .await
        .expect("the catalog");
    CatalogGeneration {
        catalog_id: row.id,
        generation: catalogs::test_generation(app.accounts().executor(), name).await,
    }
}

/// The acceptance criterion: a publish on A propagates to B well within
/// `catalog_ttl`, with no poll (a bounded wait, not a first-read
/// guarantee). The first publish lets B's subscriber finish its catch-up on
/// subscribing, so the second can reach B only through its announcement.
#[tokio::test(flavor = "multi_thread")]
async fn a_publish_on_one_replica_propagates_to_the_other_promptly_without_a_poll() {
    let (a, b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    let listener = Listener::start(&b).await;

    published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
    let first = until_read(&on_b, "/index.md", "the systems").await;
    published_edit(&on_a, "/index.md", "- the systems", "- all systems").await;
    let second = until_read(&on_b, "/index.md", "all systems").await;
    listener.stop().await;

    assert!(first, "the first publish reached B within the wait");
    assert!(
        second,
        "the second publish reached B through its announcement"
    );
    let stored = stored_generation(&a.app, "pg").await;
    let state = b.catalog("pg").await.unwrap().expect("B holds pg");
    assert_eq!(state.current().generation(), stored.generation);
}

/// A lost announcement (B not subscribed when A published, and no change
/// feed in a unit test) is caught up once B's directory row expires
/// (`catalog_ttl`, the backstop of M18 Decision 13): the next use reads the
/// row again and refreshes the snapshot to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_announcement_is_caught_up_when_the_row_expires() {
    let files: &[(&str, &str)] = &[
        ("index.md", INDEX),
        ("infra/index.md", INFRA),
        ("infra/x.md", X),
    ];
    let config = Config {
        catalog_ttl: Duration::from_millis(500),
        ..Config::default()
    };
    let (a, b, _redis) = tokio::time::timeout(
        Duration::from_secs(120),
        replica_pair(&[("pg", files)], config),
    )
    .await
    .expect("starting two replicas within 120 s");
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    // B holds pg (its row and snapshot) before A publishes.
    read(&on_b, "/index.md", None).await;

    published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
    let before = read(&on_b, "/index.md", None).await;
    let after = until_read(&on_b, "/index.md", "the systems").await;

    assert!(!before.contains("the systems"), "no announcement reached B");
    assert!(after, "B caught up once its row expired");
}

/// A subscriber that starts after a publish catches up when it subscribes:
/// Redis does not replay the announcement it missed.
#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_catches_up_on_a_missed_publish_when_it_subscribes() {
    // An hour: B's cached row must not expire, or the TTL backstop would
    // show the publish and hide a missing catch-up.
    let (a, b, _redis) = pair_with(never_expiring()).await;
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    // B holds pg before A publishes.
    read(&on_b, "/index.md", None).await;
    published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
    assert!(!read(&on_b, "/index.md", None).await.contains("the systems"));

    let listener = Listener::start(&b).await;
    let caught_up = until_read(&on_b, "/index.md", "the systems").await;
    listener.stop().await;

    assert!(caught_up, "B caught up on subscribing");
}

/// A subscription that yields its one event, then waits forever.
struct OneEvent(Option<SubscriptionEvent>);

#[async_trait::async_trait]
impl Events for OneEvent {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "test double")
    )]
    async fn next(&mut self) -> Option<SubscriptionEvent> {
        match self.0.take() {
            Some(event) => Some(event),
            None => std::future::pending().await,
        }
    }
}

/// Redis restart (issue #591): the first subscription is `Disconnected` and
/// never recovers; the second subscribe succeeds only after the test has
/// released it, which it does after the publish, so the announcement falls in
/// the window in which no subscription is live.
struct RestartWindow {
    attempts: std::sync::atomic::AtomicUsize,
    back: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl Source for RestartWindow {
    type Stream = OneEvent;

    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "test double")
    )]
    async fn subscribe(&self) -> redis::RedisResult<OneEvent> {
        let attempt = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt == 0 {
            return Ok(OneEvent(Some(SubscriptionEvent::Disconnected)));
        }
        self.back.notified().await;
        Ok(OneEvent(Some(SubscriptionEvent::Subscribed)))
    }
}

/// A publish announced while the subscriber is down, and a subscriber that
/// returns through its own fresh subscribe (the library never recovered, the
/// grace replaced the subscription) rather than the library's resubscribe,
/// is caught up by that subscribe's `Subscribed`: B reads the publish with
/// no further announcement and no wait for a cache TTL. This refutes the
/// issue's hypothesis for the current code: both the library's and the
/// code's own subscribe end in the same catch-up.
#[tokio::test(flavor = "multi_thread")]
async fn a_publish_during_a_resubscribe_window_is_seen_once_the_subscriber_returns() {
    let (a, b, _redis) = pair_with(never_expiring()).await;
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    read(&on_b, "/index.md", None).await;

    let source = RestartWindow {
        attempts: std::sync::atomic::AtomicUsize::new(0),
        back: tokio::sync::Notify::new(),
    };
    let shutdown = CancellationToken::new();
    let redis = b.redis().expect("B's redis").clone();
    let handler = Snapshots(&b);
    let looped = run(
        &handler,
        &redis,
        &source,
        Duration::from_millis(100),
        &shutdown,
    );
    let driver = async {
        // Wait until the grace replaced the subscription: the second
        // subscribe is then parked on `back`.
        while source.attempts.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
        assert!(
            !read(&on_b, "/index.md", None).await.contains("the systems"),
            "B was not told while it had no subscription"
        );
        source.back.notify_one();
        let caught_up = until_read(&on_b, "/index.md", "the systems").await;
        shutdown.cancel();
        caught_up
    };
    let (caught_up, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(driver, looped)
    })
    .await
    .expect("the window test finishes within 30 s");
    assert!(caught_up, "B caught up when its subscriber returned");
}

/// The issue's four steps: a publish of `/index.md` (B not told), B still
/// reads the old revision, a new changeset edits `/index.md` on the newer
/// revision, and `pending_changes` is asked on B. B refreshes before
/// diffing: the base is current, not stale, and the published line is not
/// shown as added again.
#[tokio::test(flavor = "multi_thread")]
async fn pending_changes_on_a_lagging_replica_refreshes_and_never_reports_a_base_ahead() {
    let (a, b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    // B holds pg before A publishes.
    read(&on_b, "/index.md", None).await;
    // 1. Published on A; B has no subscriber.
    published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
    // 2. B still serves the older revision.
    assert!(!read(&on_b, "/index.md", None).await.contains("the systems"));
    // 3. An edit made on A's newer snapshot: its base is the new revision.
    let handle = open(&on_a).await;
    edit(
        &on_a,
        &handle,
        "/index.md",
        "- the systems",
        "- all systems",
    )
    .await;

    // 4. `pending_changes` on B.
    let params = PendingParams {
        changeset: handle.clone(),
    };
    let pending = pending_changes(&on_b, params)
        .await
        .expect("pending_changes");

    let file = &pending.files[0];
    assert_eq!(file.path, "/index.md");
    assert!(file.base_revision.is_some(), "{file:?}");
    assert_eq!(file.current_revision, file.base_revision, "{file:?}");
    assert!(!file.stale, "{file:?}");
    assert!(
        file.diff
            .contains("-* [Infra](/infra/index.md) - the systems"),
        "{}",
        file.diff
    );
    assert!(
        file.diff
            .contains("+* [Infra](/infra/index.md) - all systems"),
        "{}",
        file.diff
    );
    let stored = stored_generation(&a.app, "pg").await;
    let state = b.catalog("pg").await.unwrap().expect("B holds pg");
    assert_eq!(
        state.current().generation(),
        stored.generation,
        "B refreshed"
    );
}

/// A read through a changeset on a lagging replica refreshes first, so a
/// file the changeset does not touch shows the text the entries were made
/// on, not an older one.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_through_a_changeset_on_a_lagging_replica_sees_its_base() {
    let (a, b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    let both = open(&on_a).await;
    edit(&on_a, &both, "/index.md", "- systems", "- the systems").await;
    edit(&on_a, &both, "/infra/index.md", "- x", "- ex").await;
    publish_now(&on_a, &both).await;
    let handle = open(&on_a).await;
    edit(
        &on_a,
        &handle,
        "/index.md",
        "- the systems",
        "- all systems",
    )
    .await;

    let untouched = read(&on_b, "/infra/index.md", Some(&handle)).await;
    let touched = read(&on_b, "/index.md", Some(&handle)).await;

    assert!(untouched.contains("- ex"), "{untouched}");
    assert!(touched.contains("all systems"), "{touched}");
}

/// The same read after a refused write on the lagging replica: the write's
/// `get_in` builds and caches an overlay on B's old snapshot before the op
/// fails (and returns before `forget`). The read must not reuse it.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_through_a_changeset_after_a_refused_write_on_a_lagging_replica_sees_its_base() {
    let (a, b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    let both = open(&on_a).await;
    edit(&on_a, &both, "/index.md", "- systems", "- the systems").await;
    edit(&on_a, &both, "/infra/index.md", "- x", "- ex").await;
    publish_now(&on_a, &both).await;
    let handle = open(&on_a).await;
    edit(
        &on_a,
        &handle,
        "/index.md",
        "- the systems",
        "- all systems",
    )
    .await;
    let refused = edit_document(
        &on_b,
        EditDocumentParams {
            path: "/infra/index.md".to_string(),
            old_str: "not in the file".to_string(),
            new_str: "anything".to_string(),
            replace_all: false,
            changeset: handle.clone(),
        },
    )
    .await;

    let untouched = read(&on_b, "/infra/index.md", Some(&handle)).await;

    assert!(refused.is_err(), "the write on B is refused");
    assert!(untouched.contains("- ex"), "{untouched}");
}

/// The invariant in `pending_changes`' response builder: a snapshot that
/// is still behind an entry's base (a refresh that could not catch up, here
/// forced by a base no publish wrote) is refused as busy, never reported
/// with `current_revision < base_revision`.
#[tokio::test(flavor = "multi_thread")]
async fn pending_changes_refuses_a_snapshot_behind_a_base_it_cannot_catch_up_to() {
    let (a, _b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    let handle = open(&on_a).await;
    edit(&on_a, &handle, "/index.md", "- systems", "- the systems").await;
    sqlx::query("UPDATE catalog.changeset_entries SET base_revision = base_revision + 5")
        .execute(&crate::db::store_pool(a.app.accounts().executor()))
        .await
        .expect("move the base ahead");

    let params = PendingParams { changeset: handle };
    let refused = pending_changes(&on_a, params).await;

    assert!(
        matches!(refused, Err(OkfError::Busy { ref reason }) if reason.contains("older than")),
        "{refused:?}"
    );
}

/// An edit made on a lagging replica records the base revision of the
/// current published text, not of the older snapshot (issue #672), so a
/// publish of it neither conflicts nor replays. B holds an older snapshot of
/// `/index.md` because A's publish was never announced to it.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_on_a_lagging_replica_records_the_fresh_base_and_publishes_cleanly() {
    let (a, b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    let on_b = alice_on(&b);
    // B holds pg before A publishes, and is not told.
    read(&on_b, "/index.md", None).await;
    published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
    let fresh = a
        .app
        .catalog("pg")
        .await
        .unwrap()
        .expect("A holds pg")
        .current()
        .revision("index.md")
        .expect("index.md is published");
    let stale = b
        .catalog("pg")
        .await
        .unwrap()
        .expect("B holds pg")
        .current()
        .revision("index.md")
        .expect("index.md is published");
    assert!(stale < fresh, "B is behind: {stale} < {fresh}");

    // An edit of another line of the same file, made on B.
    let handle = open(&on_b).await;
    edit(&on_b, &handle, "/index.md", "# PG", "# Postgres").await;

    let base: Option<i32> = sqlx::query_scalar(
        "SELECT e.base_revision FROM catalog.changeset_entries e \
         JOIN catalog.changesets c ON c.id = e.changeset_id WHERE c.handle = $1",
    )
    .bind(&handle)
    .fetch_one(&crate::db::store_pool(a.app.accounts().executor()))
    .await
    .expect("the entry");
    assert_eq!(
        base,
        Some(fresh),
        "the entry's base is the current revision"
    );
    let params = PublishParams {
        message: "test".to_string(),
        changeset: handle.clone(),
    };
    let published = publish(&on_b, params).await.expect("no conflict");
    assert!(published.report.published);
    let text = read(&on_b, "/index.md", None).await;
    assert!(
        text.contains("# Postgres") && text.contains("the systems"),
        "{text}"
    );
}

/// Announcements racing direct refreshes of one catalog rebuild it once,
/// and an announcement of an older generation leaves it alone. Refreshes
/// are serialized by `CatalogState`'s rebuild lock and skip a generation
/// already reached.
#[tokio::test(flavor = "multi_thread")]
async fn racing_announcements_and_refreshes_rebuild_once_and_never_go_back() {
    let (a, b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    // B holds pg before A publishes.
    let state = b.catalog("pg").await.unwrap().expect("B holds pg");
    published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
    let change = stored_generation(&a.app, "pg").await;

    let raced = tokio::time::timeout(WAIT, async {
        let db = b.main_db().await;
        tokio::join!(
            b.apply_catalog_announcement(change),
            b.apply_catalog_announcement(change),
            b.apply_catalog_announcement(change),
            state.refresh(&db, b.rebuild_locks()),
            state.refresh(&db, b.rebuild_locks()),
        )
    })
    .await
    .expect("the race settles");
    let (one, two, three, four, five) = raced;
    let applied = [one, two, three]
        .iter()
        .filter(|result| **result == AnnouncementResult::Applied)
        .count();
    let refreshed = [four.expect("refresh"), five.expect("refresh")]
        .iter()
        .filter(|swapped| **swapped)
        .count();
    let after = state.current();
    let older = CatalogGeneration {
        generation: change.generation - 1,
        ..change
    };
    let stale = b.apply_catalog_announcement(older).await;
    let again = b.apply_catalog_announcement(change).await;

    assert_eq!(applied + refreshed, 1, "one rebuild");
    for result in [one, two, three] {
        assert!(
            matches!(
                result,
                AnnouncementResult::Applied | AnnouncementResult::Current
            ),
            "{result:?}"
        );
    }
    assert_eq!(after.generation(), change.generation);
    assert_eq!(stale, AnnouncementResult::Current);
    assert_eq!(again, AnnouncementResult::Current);
    assert!(
        Arc::ptr_eq(&after, &state.current()),
        "nothing rebuilt since"
    );
}

/// The same race with B's change-feed message for the publish in it: the
/// snapshot ends at the stored generation, whoever got there.
#[tokio::test(flavor = "multi_thread")]
async fn an_announcement_racing_the_change_feed_ends_at_the_stored_generation() {
    let (a, b, _redis) = pair().await;
    let on_a = writer(&a, "alice", "pg").await;
    // B holds pg before A publishes.
    b.catalog("pg").await.unwrap().expect("B holds pg");
    published_edit(&on_a, "/index.md", "- systems", "- the systems").await;
    let change = stored_generation(&a.app, "pg").await;

    let message = crate::invalidate::Message {
        invalidation: crate::invalidate::Invalidation::Catalog {
            id: change.catalog_id,
            generation: change.generation,
        },
        sent_at_ms: chrono::Utc::now().timestamp_millis(),
    };

    let (announced, ()) = tokio::time::timeout(WAIT, async {
        tokio::join!(b.apply_catalog_announcement(change), b.apply(&message))
    })
    .await
    .expect("the race settles");

    assert!(
        matches!(
            announced,
            AnnouncementResult::Applied | AnnouncementResult::Current
        ),
        "{announced:?}"
    );
    let state = b.catalog("pg").await.unwrap().expect("B holds pg");
    assert_eq!(state.current().generation(), change.generation);
}

/// An announcement of a catalog B has not cached (unknown, or created since)
/// loads nothing, and one of a catalog deleted since is answered without a
/// panic; catalogs load on their first use.
#[tokio::test(flavor = "multi_thread")]
async fn an_announcement_of_a_catalog_not_loaded_or_deleted_loads_nothing() {
    let (a, b, _redis) = pair().await;
    let pg = stored_generation(&a.app, "pg").await;
    let created = catalogs::create(
        a.app.accounts().executor(),
        &catalogs::NewCatalog {
            name: "work",
            display_name: &crate::labels::DisplayName::default(),
            description: "",
            owner_team: crate::db::TEST_TEAM,
        },
        &AuditActor::cli(),
        chrono::Utc::now(),
    )
    .await
    .expect("create work");
    let work = stored_generation(&a.app, "work").await;
    assert_eq!(work.catalog_id, created.id);
    let unknown = CatalogGeneration {
        catalog_id: i64::MAX,
        generation: 1,
    };

    let none = b.apply_catalog_announcement(unknown).await;
    let new = b.apply_catalog_announcement(work).await;
    catalogs::delete_settled(
        a.app.accounts().executor(),
        "pg",
        true,
        &AuditActor::cli(),
        chrono::Utc::now(),
    )
    .await
    .expect("delete pg");
    let gone = b
        .apply_catalog_announcement(CatalogGeneration {
            generation: pg.generation + 1,
            ..pg
        })
        .await;

    assert_eq!(none, AnnouncementResult::Unknown);
    assert_eq!(new, AnnouncementResult::Unknown);
    assert_eq!(gone, AnnouncementResult::Unknown);
    for id in [pg.catalog_id, work.catalog_id] {
        assert!(
            b.catalog_cache().resident(id).await.is_none(),
            "an announcement loads nothing"
        );
    }
    // Each use reads the directory (here marked stale, as after a
    // resubscribe) and loads on demand: work is served, pg is gone.
    b.catch_up_catalogs().await;
    assert!(b.catalog("work").await.unwrap().is_some());
    assert!(b.catalog("pg").await.unwrap().is_none());
}

/// Without `[redis]` nothing changes: there is no subscriber, a publish
/// announces nothing (and does not fail), and its own snapshot still moves
/// at once.
#[tokio::test(flavor = "multi_thread")]
async fn without_redis_a_publish_refreshes_only_its_own_replica() {
    let files: &[(&str, &str)] = &[
        ("index.md", INDEX),
        ("infra/index.md", INFRA),
        ("infra/x.md", X),
    ];
    let test = app_with(&[("pg", files)], Config::default()).await;
    assert!(test.app.redis().is_none());
    let ctx = writer(&test, "alice", "pg").await;

    let listened = tokio::time::timeout(
        WAIT,
        listen_catalogs(Arc::clone(&test.app), CancellationToken::new()),
    )
    .await;
    published_edit(&ctx, "/index.md", "- systems", "- the systems").await;

    assert!(listened.is_ok(), "the subscriber returns at once");
    assert!(read(&ctx, "/index.md", None).await.contains("the systems"));
}
