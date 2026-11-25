//! [`CatalogDirectory::reachable_page`] and its cursor (M18 Task 10, spec
//! Decision 7).

use std::time::Duration;

use chrono::Utc;

use super::*;
use crate::db::{test_db, TEST_TEAM};
use crate::labels::DisplayName;
use crate::mcp::testing::principal;
use crate::store::audit::AuditActor;
use crate::store::catalogs::{create, NewCatalog};

const LONG: Duration = Duration::from_secs(3600);

/// A directory over a database holding catalogs `a` to `g`.
async fn seven() -> (crate::db::pg_support::TestDb, CatalogDirectory) {
    let db = test_db().await;
    for name in ["a", "b", "c", "d", "e", "f", "g"] {
        let new = NewCatalog {
            name,
            display_name: &DisplayName::default(),
            description: "",
            owner_team: TEST_TEAM,
        };
        create(&db.pool, &new, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
    }
    let directory = CatalogDirectory::for_tests(db.pool.clone(), 100, LONG, Metrics::global());
    (db, directory)
}

/// Every page of `connection` for `reach` (empty: every catalog), `limit`
/// at a time: each page's names and whether it had a cursor.
async fn walk(
    directory: &CatalogDirectory,
    reach: &[&str],
    connection: Option<&[String]>,
    limit: u32,
) -> Vec<(Vec<String>, bool)> {
    let principal = principal(reach, None);
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = directory
            .offered_page(&principal, connection, after.as_deref(), limit)
            .await
            .unwrap();
        let names = page.items.iter().map(|row| row.name.clone()).collect();
        pages.push((names, page.next.is_some()));
        match page.next {
            Some(next) => after = Some(next),
            None => return pages,
        }
        assert!(pages.len() < 20, "the pages never end");
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_every_catalog_key_pages_through_every_catalog_by_name() {
    let (_db, directory) = seven().await;

    let pages = walk(&directory, &[], None, 3).await;

    assert_eq!(
        pages,
        [
            (names(&["a", "b", "c"]), true),
            (names(&["d", "e", "f"]), true),
            (names(&["g"]), false),
        ]
    );
    // A page that ends exactly at the last catalog has no cursor.
    assert_eq!(
        walk(&directory, &[], None, 7).await,
        [(names(&["a", "b", "c", "d", "e", "f", "g"]), false)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_granted_key_gets_its_existing_catalogs_in_one_page() {
    let (_db, directory) = seven().await;
    let reach = ["f", "b", "gone", "d"];

    let whole = walk(&directory, &reach, None, DEFAULT_PAGE).await;
    let by_two = walk(&directory, &reach, None, 2).await;

    assert_eq!(whole, [(names(&["b", "d", "f"]), false)]);
    // A smaller `limit` is honoured for grants too.
    assert_eq!(by_two, [(names(&["b", "d"]), true), (names(&["f"]), false)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_list_narrows_the_page_to_what_the_key_reaches() {
    let (_db, directory) = seven().await;
    let list = names(&["e", "a", "nope", "c"]);

    let every = walk(&directory, &[], Some(&list), DEFAULT_PAGE).await;
    let granted = walk(&directory, &["a", "b", "e"], Some(&list), 1).await;
    let empty: Vec<String> = Vec::new();
    let no_list = walk(&directory, &["b"], Some(&empty), DEFAULT_PAGE).await;

    assert_eq!(every, [(names(&["a", "c", "e"]), false)]);
    assert_eq!(
        granted,
        [(names(&["a"]), true), (names(&["e"]), false)],
        "c is listed but not granted"
    );
    assert_eq!(no_list, [(names(&["b"]), false)], "an empty list is none");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_never_reveals_a_catalog_the_key_does_not_reach() {
    let (_db, directory) = seven().await;
    let key = principal(&["b", "f"], None);
    let after = |name: &str| cursor_of(name);
    let page = |cursor: String| {
        let key = Arc::clone(&key);
        let directory = &directory;
        async move {
            directory
                .reachable_page(&key, Some(&cursor), DEFAULT_PAGE)
                .await
                .unwrap()
                .items
                .iter()
                .map(|row| row.name.clone())
                .collect::<Vec<_>>()
        }
    };

    // `d` exists but is not granted; `dd` does not exist: the same answer.
    let after_hidden = page(after("d")).await;
    let after_missing = page(after("dd")).await;
    let after_all = page(after("z")).await;

    assert_eq!(after_hidden, ["f"]);
    assert_eq!(after_missing, after_hidden);
    assert!(after_all.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_cursor_is_refused_before_any_read() {
    let (_db, directory) = seven().await;
    let every = principal(&[], None);
    let granted = principal(&["a"], None);
    let malformed = [
        "!!".to_owned(),
        // Padded, so not what this server gives.
        "YQ==".to_owned(),
        URL_SAFE_NO_PAD.encode("A B"),
        URL_SAFE_NO_PAD.encode("a\0b"),
        URL_SAFE_NO_PAD.encode([0xff_u8, 0xfe]),
    ];

    for cursor in &malformed {
        for key in [&every, &granted] {
            let refused = directory
                .reachable_page(key, Some(cursor), DEFAULT_PAGE)
                .await
                .unwrap_err();
            assert!(
                matches!(&refused, OkfError::InvalidPath { path, .. } if path == "after"),
                "{cursor:?}: {refused:?}"
            );
        }
    }
}

#[test]
fn a_cursor_reads_back_to_its_name_and_an_empty_one_is_none() {
    assert_eq!(
        parse_cursor(Some(&cursor_of("work-notes"))).unwrap(),
        Some("work-notes".to_owned())
    );
    assert_eq!(
        parse_cursor(Some(&format!("  {}  ", cursor_of("pg")))).unwrap(),
        Some("pg".to_owned())
    );
    assert_eq!(parse_cursor(Some("")).unwrap(), None);
    assert_eq!(parse_cursor(Some("   ")).unwrap(), None);
    assert_eq!(parse_cursor(None).unwrap(), None);
}

#[test]
fn the_page_size_defaults_to_100_and_is_capped_at_1000() {
    assert_eq!(page_size(None), 100);
    assert_eq!(page_size(Some(0)), 100);
    assert_eq!(page_size(Some(3)), 3);
    assert_eq!(page_size(Some(1000)), 1000);
    assert_eq!(page_size(Some(u32::MAX)), 1000);
}

// ---- Issue #955: the readable listing ----

/// An admin key of a user with live unlocks on `unlocked` (names among `a`
/// to `g`), `require_unlock` on: it reaches every catalog and may read only
/// those.
async fn locked_admin(
    directory: &CatalogDirectory,
    unlocked: &[&str],
    now: chrono::DateTime<Utc>,
) -> Principal {
    let every = principal(&[], None);
    let all = directory
        .reachable_page(&every, None, DEFAULT_PAGE)
        .await
        .unwrap();
    let unlocks = all
        .items
        .iter()
        .filter(|row| unlocked.contains(&row.name.as_str()))
        .map(|row| crate::store::gates::UserUnlock {
            id: row.id,
            catalog_id: row.id,
            expires_at: now + chrono::TimeDelta::hours(1),
        })
        .collect();
    let gates = crate::store::gates::UserGates {
        is_admin: true,
        unlocks,
        ..Default::default()
    };
    Principal {
        access: crate::store::keys::Access::Admin,
        facts: Arc::new(crate::auth::AccessFacts {
            user: Some(Arc::new(gates)),
            credential: None,
            require_unlock: true,
        }),
        ..Arc::unwrap_or_clone(every)
    }
}

/// Every page of the readable listing, `limit` at a time, reading the reach
/// list two catalogs at a time so a run of locked catalogs crosses scans.
async fn walk_readable(
    directory: &CatalogDirectory,
    principal: &Principal,
    limit: u32,
    now: chrono::DateTime<Utc>,
) -> Vec<(Vec<String>, bool)> {
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = directory
            .readable_page_by(principal, after.as_deref(), limit, now, 2)
            .await
            .unwrap();
        let names = page.items.iter().map(|row| row.name.clone()).collect();
        pages.push((names, page.next.is_some()));
        match page.next {
            Some(next) => after = Some(next),
            None => return pages,
        }
        assert!(pages.len() < 20, "the pages never end");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_readable_listing_pages_through_locked_catalogs_without_gaps() {
    let (_db, directory) = seven().await;
    let now = Utc::now();
    let staff = locked_admin(&directory, &["b", "e", "g"], now).await;

    let by_two = walk_readable(&directory, &staff, 2, now).await;
    let by_one = walk_readable(&directory, &staff, 1, now).await;

    assert_eq!(by_two, [(names(&["b", "e"]), true), (names(&["g"]), false)]);
    assert_eq!(
        by_one,
        [
            (names(&["b"]), true),
            (names(&["e"]), true),
            (names(&["g"]), false)
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_of_only_locked_catalogs_is_skipped_and_none_readable_is_empty() {
    let (_db, directory) = seven().await;
    let now = Utc::now();
    // `a` to `f` are locked: three scans of two hold nothing readable.
    let one = locked_admin(&directory, &["g"], now).await;
    let none = locked_admin(&directory, &[], now).await;
    let every = locked_admin(&directory, &["a", "b", "c", "d", "e", "f", "g"], now).await;

    assert_eq!(
        walk_readable(&directory, &one, 2, now).await,
        [(names(&["g"]), false)]
    );
    assert_eq!(
        walk_readable(&directory, &none, 2, now).await,
        [(Vec::<String>::new(), false)]
    );
    assert_eq!(
        walk_readable(&directory, &every, 3, now).await,
        [
            (names(&["a", "b", "c"]), true),
            (names(&["d", "e", "f"]), true),
            (names(&["g"]), false)
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlock_that_expired_by_the_server_clock_is_not_readable() {
    let (_db, directory) = seven().await;
    let now = Utc::now();
    let staff = locked_admin(&directory, &["c"], now).await;

    let live = walk_readable(&directory, &staff, 5, now).await;
    let later = walk_readable(&directory, &staff, 5, now + chrono::TimeDelta::hours(2)).await;

    assert_eq!(live, [(names(&["c"]), false)]);
    assert_eq!(later, [(Vec::<String>::new(), false)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_require_unlock_the_readable_listing_is_the_reach_listing() {
    let (_db, directory) = seven().await;
    let now = Utc::now();
    let mut staff = locked_admin(&directory, &[], now).await;
    staff.facts = Arc::new(crate::auth::AccessFacts {
        require_unlock: false,
        ..(*staff.facts).clone()
    });

    let readable = walk_readable(&directory, &staff, 3, now).await;

    assert_eq!(
        readable,
        [
            (names(&["a", "b", "c"]), true),
            (names(&["d", "e", "f"]), true),
            (names(&["g"]), false)
        ]
    );
}
