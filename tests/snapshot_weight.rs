//! Measures how many bytes a loaded catalog snapshot holds in memory per
//! byte of stored files: the factor the catalog cache's weigher multiplies
//! `Loaded::stored_bytes` by (M18 spec Decision 2, Task 8).
//!
//! It is its own test binary so the counting allocator below affects
//! nothing else. Ignored by default (it needs Docker and prints numbers to
//! read, rather than asserting a value); run it with:
//!
//! `cargo test --release --test snapshot_weight -- --ignored --nocapture`
//!
//! Method: build synthetic catalogs of 500 and 5,000 documents (frontmatter,
//! headings, prose and links to other documents), import each, load one
//! small catalog first so the pool's connections and every lazy static
//! exist, then count the bytes allocated minus the bytes freed across one
//! `CatalogState::load` whose result is still held. Temporary allocations
//! (rows read, the build's scratch) are freed by then, so the difference is
//! what the snapshot keeps resident.

use crate::support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::Utc;
use okf_mcp::archive::Files;
use okf_mcp::catalog::CatalogState;
use okf_mcp::labels::DisplayName;
use okf_mcp::okf::workspace::StoredFile;
use okf_mcp::store::audit::AuditActor;
use okf_mcp::store::catalogs::{self, NewCatalog};
use okf_mcp::store::import::{import, ImportOptions, ImportScope};
use sqlx::PgPool;

/// The system allocator, counting the bytes currently allocated.
struct Counting;

/// Bytes allocated and not yet freed, by every thread.
static LIVE: AtomicI64 = AtomicI64::new(0);

// SAFETY: every method forwards to `System` with the same arguments, so the
// allocator's contract is `System`'s; the counter is a side effect only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as i64, Ordering::Relaxed);
        // SAFETY: the caller upholds `alloc`'s contract for `layout`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        // SAFETY: `ptr` was allocated by `alloc` above with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
        // SAFETY: as for `dealloc`; `new_size` is the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// One synthetic document: frontmatter, a heading, prose and links to its
/// neighbours, about 850 bytes.
fn document(index: usize, total: usize) -> String {
    let next = (index + 1) % total;
    let previous = (index + total - 1) % total;
    format!(
        "---\ntype: Note\ntitle: Concept {index}\ndescription: A synthetic concept \
         number {index} for weighing snapshots.\ntags: [synthetic, weight]\n---\n\
         # Concept {index}\n\n\
         This concept links to [concept {next}](c{next}.md) and back to \
         [concept {previous}](c{previous}.md). {prose}\n\n\
         ## Details\n\n{prose}\n\n## See also\n\n- [Concept {next}](c{next}.md)\n",
        prose = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
                 tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim \
                 veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea \
                 commodo consequat. Duis aute irure dolor in reprehenderit in voluptate."
    )
}

/// Creates catalog `name` holding `count` synthetic documents.
async fn seed(pool: &PgPool, name: &str, count: usize) {
    let new = NewCatalog {
        name,
        display_name: &DisplayName::default(),
        description: "",
        owner_team: support::server::OWNER,
    };
    support::stores::create_catalog(pool, &new, &AuditActor::cli(), Utc::now())
        .await
        .unwrap();
    let files: Files = (0..count)
        .map(|index| {
            let file = StoredFile {
                content: document(index, count).into_bytes(),
                executable: false,
            };
            (format!("c{index}.md"), file)
        })
        .collect();
    let options = ImportOptions {
        replace: true,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "weight",
        audit: &AuditActor::cli(),
        now: Utc::now(),
    };
    import(&support::stores::main_store(pool), name, files, &options)
        .await
        .unwrap();
}

/// Creates catalog `name` holding `count` empty documents at paths shaped
/// like real ones (`notes/topic-12/concept-12345.md`).
async fn seed_empty(pool: &PgPool, name: &str, count: usize) {
    let new = NewCatalog {
        name,
        display_name: &DisplayName::default(),
        description: "",
        owner_team: support::server::OWNER,
    };
    support::stores::create_catalog(pool, &new, &AuditActor::cli(), Utc::now())
        .await
        .unwrap();
    let files: Files = (0..count)
        .map(|index| {
            let file = StoredFile {
                content: Vec::new(),
                executable: false,
            };
            (
                format!("notes/topic-{}/concept-{index:05}.md", index % 50),
                file,
            )
        })
        .collect();
    let options = ImportOptions {
        replace: true,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "weight",
        audit: &AuditActor::cli(),
        now: Utc::now(),
    };
    import(&support::stores::main_store(pool), name, files, &options)
        .await
        .unwrap();
}

/// Loads catalog `name` and returns `(resident bytes, stored bytes)`.
async fn weigh(pool: &PgPool, name: &str) -> (i64, u64) {
    let row = catalogs::row_by_name(pool, name).await.unwrap().unwrap();
    let before = LIVE.load(Ordering::SeqCst);
    let state = CatalogState::load(&support::stores::main_store(pool), &row)
        .await
        .unwrap();
    let resident = LIVE.load(Ordering::SeqCst) - before;
    let stored = state.current().stored_bytes();
    drop(state);
    (resident, stored)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; prints the measured factor"]
async fn measure_snapshot_overhead() {
    let db = support::pg::TestDb::start_migrated().await;
    support::server::migrate_with_owner(&db.pool).await;
    seed(&db.pool, "warm", 10).await;
    seed(&db.pool, "small", 500).await;
    seed(&db.pool, "large", 5_000).await;
    // Warm-up: pool connections, statement caches and lazy statics.
    weigh(&db.pool, "warm").await;
    weigh(&db.pool, "warm").await;

    for name in ["small", "large"] {
        let (resident, stored) = weigh(&db.pool, name).await;
        let factor = resident as f64 / stored as f64;
        eprintln!("{name}: resident {resident} B, stored {stored} B, factor {factor:.2}");
        assert!(stored > 0 && resident > 0);
    }
    // The fixed costs, which stored bytes do not see: a catalog of one
    // empty document, and one of 5,000. Per file is the slope between them;
    // per catalog is what one file leaves after it.
    seed_empty(&db.pool, "one", 1).await;
    seed_empty(&db.pool, "many", 5_000).await;
    let (one, _) = weigh(&db.pool, "one").await;
    let (many, _) = weigh(&db.pool, "many").await;
    let per_file = (many - one) as f64 / 4_999.0;
    let per_catalog = one as f64 - per_file;
    eprintln!(
        "empty: one {one} B, 5000 {many} B, per file {per_file:.0} B, per catalog {per_catalog:.0} B"
    );
    assert!(many > one);
}
