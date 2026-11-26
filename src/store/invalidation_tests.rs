//! What each administrative store function sends on the change feed
//! (milestone 18 Task 2, spec Decision 10's table): one test per writer,
//! each listening through [`invalidations_of`] while the write commits. The
//! command line calls these same functions, so these tests cover it too.
//!
//! A write that changes nothing, or is refused, sends nothing: its
//! transaction rolls back, and Postgres drops a rolled-back `NOTIFY`.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;

use crate::db::{test_db, TEST_TEAM, TEST_USER};
use crate::invalidate::testing::invalidations_of;
use crate::invalidate::Invalidation;
use crate::labels::DisplayName;
use crate::store::audit::AuditActor;
use crate::store::catalogs::{self, DetailsChange, MaintenanceChange, NewCatalog, Switch};
use crate::store::ceiling::{self, MemberAccess, NewEntitlement};
use crate::store::import::{import, ImportOptions, ImportScope};
use crate::store::keys::{self, Access, CatalogGrant, Grants, NewKey, NewProvisioner};
use crate::store::oauth::clients::ClientKind;
use crate::store::oauth::codes::{self, insert_test_client, NewSignInCode};
use crate::store::oauth::grants::{self, NewSelfIssuedGrant};
use crate::store::oauth::testing::{insert_client, insert_grant};
use crate::store::repair::{repair, RepairOptions};
use crate::store::settings::{self, SettingKey};
use crate::store::teams::{self, Membership, NewTeam, Role, TeamDisplayNameChange};
use crate::store::unlocks::{self, Category, NewUnlock, UnlockPolicy};
use crate::store::user_sessions;
use crate::store::users::{self, AdminChange, DeleteOptions, NewUser};

const NOTE: &str = "---\ntype: Note\ntitle: A\ndescription: A note.\n---\n# A\n";

fn cli() -> AuditActor {
    AuditActor::cli()
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Creates catalog `name`, owned by [`TEST_TEAM`].
async fn catalog(pool: &PgPool, name: &str) -> catalogs::Catalog {
    let new = NewCatalog {
        name,
        display_name: &DisplayName::default(),
        description: "",
        owner_team: TEST_TEAM,
    };
    catalogs::create(pool, &new, &cli(), now())
        .await
        .expect("create the catalog")
}

/// Creates user `name`, with its personal team.
async fn user(pool: &PgPool, name: &str) -> String {
    let new = NewUser {
        name,
        display_name: &DisplayName::default(),
    };
    users::create(pool, &new, &cli(), now())
        .await
        .expect("create the user");
    user_id(pool, name).await
}

async fn user_id(pool: &PgPool, name: &str) -> String {
    sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("the user's id")
}

async fn team_id(pool: &PgPool, name: &str) -> String {
    sqlx::query_scalar("SELECT id FROM teams WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("the team's id")
}

/// A key of [`TEST_USER`] on `catalogs`.
fn new_key<'a>(name: &'a str, catalogs: &'a [String]) -> NewKey<'a> {
    NewKey {
        name,
        access: Access::Write,
        maintenance_report: false,
        grants: Grants::Catalogs(catalogs),
        default_catalog: None,
        actor: "claude-code/test",
        user: TEST_USER,
        self_issued: false,
        expires_at: None,
        levels: &[],
    }
}

/// Creates a key of [`TEST_USER`] on `catalogs`; returns its id.
async fn key(pool: &PgPool, catalogs: &[String]) -> String {
    keys::create_with_id(pool, &new_key("laptop", catalogs), &cli(), now())
        .await
        .expect("create the key")
        .id
}

fn credential(id: &str) -> Invalidation {
    Invalidation::credential(id)
}

fn meta(catalog: &catalogs::Catalog) -> Invalidation {
    Invalidation::CatalogMeta {
        id: catalog.id,
        name: catalog.name.clone(),
    }
}

// ---- catalogs ----

#[tokio::test(flavor = "multi_thread")]
async fn creating_a_catalog_invalidates_it_and_its_owner() {
    let db = test_db().await;
    let owner = team_id(&db.pool, TEST_TEAM).await;
    let mut created = None;
    let got = invalidations_of(&db.pool, async {
        created = Some(catalog(&db.pool, "main").await);
    })
    .await;
    let main = created.expect("created");
    assert_eq!(got, [meta(&main), Invalidation::team(&owner)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_a_catalog_invalidates_it_and_both_owners() {
    let db = test_db().await;
    let main = catalog(&db.pool, "main").await;
    user(&db.pool, "bob").await;
    let (old, new) = (
        team_id(&db.pool, TEST_TEAM).await,
        team_id(&db.pool, "bob").await,
    );
    let got = invalidations_of(&db.pool, async {
        catalogs::set_owner(&db.pool, "main", "bob", &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        got,
        [
            meta(&main),
            Invalidation::team(&old),
            Invalidation::team(&new)
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_a_catalogs_details_invalidates_it_and_an_unchanged_one_sends_nothing() {
    let db = test_db().await;
    let main = catalog(&db.pool, "main").await;
    let change = |description| DetailsChange {
        catalog: "main",
        display_name: None,
        description: Some(description),
        owner: None,
    };
    let got = invalidations_of(&db.pool, async {
        catalogs::update_details(&db.pool, &change("Notes."), &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, [meta(&main)]);
    let unchanged = invalidations_of(&db.pool, async {
        catalogs::update_details(&db.pool, &change("Notes."), &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(unchanged, []);
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_maintenance_invalidates_the_catalog() {
    let db = test_db().await;
    let main = catalog(&db.pool, "main").await;
    let change = MaintenanceChange {
        catalog: "main",
        to: Switch::Off,
        owner: None,
    };
    let got = invalidations_of(&db.pool, async {
        catalogs::set_maintenance(&db.pool, &change, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, [meta(&main)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_catalog_invalidates_it_and_its_owner() {
    let db = test_db().await;
    let main = catalog(&db.pool, "main").await;
    let owner = team_id(&db.pool, TEST_TEAM).await;
    let got = invalidations_of(&db.pool, async {
        catalogs::delete_settled(&db.pool, "main", false, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, [meta(&main), Invalidation::team(&owner)]);
}

// ---- ceiling ----

#[tokio::test(flavor = "multi_thread")]
async fn changing_member_access_invalidates_the_catalog_and_its_owner() {
    let db = test_db().await;
    let main = catalog(&db.pool, "main").await;
    let owner = team_id(&db.pool, TEST_TEAM).await;
    let got = invalidations_of(&db.pool, async {
        ceiling::set_member_access(&db.pool, "main", MemberAccess::Read, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, [meta(&main), Invalidation::team(&owner)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn entitling_and_unentitling_invalidate_the_user_and_a_no_op_sends_nothing() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let alice = user(&db.pool, "alice").await;
    let entitlement = NewEntitlement {
        user: "alice",
        catalog: "main",
        access: Access::Read,
    };
    let entitled = invalidations_of(&db.pool, async {
        ceiling::entitle(&db.pool, &entitlement, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let again = invalidations_of(&db.pool, async {
        ceiling::entitle(&db.pool, &entitlement, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let removed = invalidations_of(&db.pool, async {
        ceiling::unentitle(&db.pool, "alice", "main", &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let not_given = invalidations_of(&db.pool, async {
        ceiling::unentitle(&db.pool, "alice", "main", &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(entitled, [Invalidation::user(&alice)]);
    assert_eq!(again, []);
    assert_eq!(removed, [Invalidation::user(&alice)]);
    assert_eq!(not_given, []);
}

// ---- settings ----

#[tokio::test(flavor = "multi_thread")]
async fn setting_and_resetting_a_setting_invalidate_the_settings() {
    let db = test_db().await;
    let set = invalidations_of(&db.pool, async {
        settings::set(&db.pool, SettingKey::MaxKeysPerUser, "7", &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let same = invalidations_of(&db.pool, async {
        settings::set(&db.pool, SettingKey::MaxKeysPerUser, "7", &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let reset = invalidations_of(&db.pool, async {
        settings::reset(&db.pool, SettingKey::MaxKeysPerUser, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(set, [Invalidation::Settings]);
    assert_eq!(same, [], "an unchanged value rolls back");
    assert_eq!(reset, [Invalidation::Settings]);
}

// ---- OAuth ----

#[tokio::test(flavor = "multi_thread")]
async fn redeeming_a_sign_in_code_invalidates_the_grant_it_becomes() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let main = ["main".to_string()];
    let new = NewSignInCode {
        name: "laptop",
        access: Access::Read,
        grants: Grants::Catalogs(&main),
        default_catalog: None,
        actor: "example-actor",
        expires_at: now() + TimeDelta::hours(1),
        user: TEST_USER,
    };
    let code = codes::create(&db.pool, &new, &cli(), now()).await.unwrap();
    let client = insert_test_client(
        &db.pool,
        "dcr-client",
        ClientKind::Dcr,
        "https://client.example.com/callback",
    )
    .await;
    let mut grant = String::new();
    let got = invalidations_of(&db.pool, async {
        grant = codes::redeem(&db.pool, &code.id, &client, now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, [credential(&grant)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_issued_grant_invalidates_itself() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let owner = user_id(&db.pool, TEST_USER).await;
    let session = user_sessions::create(
        &db.pool,
        &owner,
        None,
        "okf-test",
        Duration::from_secs(3600),
        now(),
    )
    .await
    .unwrap();
    let client = insert_test_client(
        &db.pool,
        "test-client",
        ClientKind::Dcr,
        "https://client.example.com/callback",
    )
    .await;
    let levels = BTreeMap::from([("main".to_string(), Access::Read)]);
    let mut audit = AuditActor::system("consent-test");
    audit.source = crate::store::audit::Source::Console;
    audit.via = crate::store::audit::Via::Session;
    audit.user_id = Some(owner.clone());
    let mut grant = String::new();
    let got = invalidations_of(&db.pool, async {
        let new = NewSelfIssuedGrant {
            user_id: &owner,
            session_id: &session.id,
            client: &client,
            levels: &levels,
            default_catalog: Some("main"),
        };
        grant = grants::create_self_issued(&db.pool, &new, &audit, now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, [credential(&grant)]);
}

// ---- keys ----

#[tokio::test(flavor = "multi_thread")]
async fn creating_keys_invalidates_each_new_credential() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let main = ["main".to_string()];
    let levels = [CatalogGrant {
        catalog: "main".to_string(),
        access: Some(Access::Read),
    }];
    let mut ids = Vec::new();
    let got = invalidations_of(&db.pool, async {
        let provisioner = NewProvisioner {
            name: "sync",
            actor: "claude-code/test",
            user: TEST_USER,
        };
        ids.push(
            keys::create_provisioner(&db.pool, &provisioner, &cli(), now())
                .await
                .unwrap()
                .id,
        );
        ids.push(key(&db.pool, &main).await);
        let own = NewKey {
            self_issued: true,
            levels: &levels,
            ..new_key("own", &main)
        };
        ids.push(
            keys::create_self_issued(&db.pool, &own, 5, &cli(), now())
                .await
                .unwrap()
                .id,
        );
    })
    .await;
    let expected: Vec<Invalidation> = ids.iter().map(|id| credential(id)).collect();
    assert_eq!(got, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_key_invalidates_that_credential() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let id = key(&db.pool, &["main".to_string()]).await;
    let got = invalidations_of(&db.pool, async {
        keys::revoke_by_id(&db.pool, &id, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(got, [credential(&id)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_write_sends_nothing() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let id = key(&db.pool, &["main".to_string()]).await;
    keys::revoke_by_id(&db.pool, &id, &cli(), now())
        .await
        .unwrap();
    let got = invalidations_of(&db.pool, async {
        // Already revoked, and unknown: both are refused after their locks.
        assert!(keys::revoke_by_id(&db.pool, &id, &cli(), now())
            .await
            .is_err());
        assert!(keys::revoke_by_id(&db.pool, "no-such-key", &cli(), now())
            .await
            .is_err());
    })
    .await;
    assert_eq!(got, []);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_grant_revocation_invalidates_the_grant() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let long_ago = now() - TimeDelta::days(60);
    let client = insert_client(
        &db.pool,
        "https://client.example.com/client.json",
        "Example",
        long_ago,
    )
    .await;
    let grant = insert_grant(&db.pool, &client, Access::Read, &["main"], long_ago).await;
    let got = invalidations_of(&db.pool, async {
        let revoked = keys::revoke_idle_grants(&db.pool, now() - TimeDelta::days(30), now())
            .await
            .unwrap();
        assert_eq!(revoked.len(), 1);
    })
    .await;
    assert_eq!(got, [credential(&grant)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_a_keys_report_flag_catalogs_or_user_invalidates_it() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    catalog(&db.pool, "other").await;
    user(&db.pool, "alice").await;
    let id = key(&db.pool, &["main".to_string()]).await;
    let got = invalidations_of(&db.pool, async {
        keys::set_maintenance_report_by_id(&db.pool, &id, true, &cli(), now())
            .await
            .unwrap();
        keys::grant_by_id(&db.pool, &id, "other", &cli(), now())
            .await
            .unwrap();
        // Already granted: a no-op, which sends nothing.
        keys::grant_by_id(&db.pool, &id, "other", &cli(), now())
            .await
            .unwrap();
        keys::ungrant_by_id(&db.pool, &id, "other", &cli(), now())
            .await
            .unwrap();
        keys::assign_user_by_id(&db.pool, &id, "alice", &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        got,
        [
            credential(&id),
            credential(&id),
            credential(&id),
            credential(&id)
        ]
    );
}

// ---- users ----

#[tokio::test(flavor = "multi_thread")]
async fn creating_a_user_invalidates_it_and_its_personal_team() {
    let db = test_db().await;
    let mut alice = String::new();
    let got = invalidations_of(&db.pool, async {
        alice = user(&db.pool, "alice").await;
    })
    .await;
    let personal = team_id(&db.pool, "alice").await;
    assert_eq!(
        got,
        [Invalidation::user(&alice), Invalidation::team(&personal)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_a_users_email_name_or_admin_flag_invalidates_it() {
    let db = test_db().await;
    let alice = user(&db.pool, "alice").await;
    let renamed = DisplayName::parse("Alice").unwrap();
    let on = AdminChange {
        on: true,
        allow_last: true,
    };
    let got = invalidations_of(&db.pool, async {
        users::set_email(&db.pool, "alice", Some("alice@example.com"), &cli(), now())
            .await
            .unwrap();
        // Unchanged: nothing.
        users::set_email(&db.pool, "alice", Some("alice@example.com"), &cli(), now())
            .await
            .unwrap();
        users::set_display_name(&db.pool, "alice", &renamed, &cli(), now())
            .await
            .unwrap();
        users::set_admin(&db.pool, "alice", on, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    assert_eq!(
        got,
        [
            Invalidation::user(&alice),
            Invalidation::user(&alice),
            Invalidation::user(&alice)
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_user_invalidates_it_its_credentials_and_its_teams() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let alice = user(&db.pool, "alice").await;
    let personal = team_id(&db.pool, "alice").await;
    let staff = NewTeam {
        name: "staff",
        display_name: &DisplayName::default(),
        owner: TEST_USER,
    };
    teams::create(&db.pool, &staff, &cli(), now())
        .await
        .unwrap();
    let membership = Membership {
        team: "staff",
        user: "alice",
        role: Role::Member,
    };
    teams::set_member(&db.pool, &membership, &cli(), now())
        .await
        .unwrap();
    let main = ["main".to_string()];
    ceiling::entitle(
        &db.pool,
        &NewEntitlement {
            user: "alice",
            catalog: "main",
            access: Access::Read,
        },
        &cli(),
        now(),
    )
    .await
    .unwrap();
    let laptop = keys::create_with_id(
        &db.pool,
        &NewKey {
            user: "alice",
            access: Access::Read,
            ..new_key("laptop", &main)
        },
        &cli(),
        now(),
    )
    .await
    .unwrap()
    .id;
    let staff_id = team_id(&db.pool, "staff").await;
    let got = invalidations_of(&db.pool, async {
        users::delete(&db.pool, "alice", DeleteOptions::OPERATOR, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let mut teams = [personal, staff_id];
    teams.sort();
    let expected: Vec<Invalidation> = [Invalidation::user(&alice), credential(&laptop)]
        .into_iter()
        .chain(teams.iter().map(|team| Invalidation::team(team)))
        .collect();
    assert_eq!(got, expected);
}

// ---- unlocks ----

#[tokio::test(flavor = "multi_thread")]
async fn an_unlock_and_its_end_invalidate_the_unlocked_admin() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    let staff = user(&db.pool, "staff").await;
    let on = AdminChange {
        on: true,
        allow_last: true,
    };
    users::set_admin(&db.pool, "staff", on, &cli(), now())
        .await
        .unwrap();
    let policy = UnlockPolicy {
        max_hours: 4,
        notify_owners: false,
        brand_defaults: crate::store::settings::Brand::default(),
    };
    let new = NewUnlock {
        catalog: "main",
        user: "staff",
        category: Category::Support,
        reason: "ticket 12",
        hours: 1,
    };
    let mut id = 0;
    let created = invalidations_of(&db.pool, async {
        id = unlocks::create(&db.pool, &new, &policy, &cli(), now())
            .await
            .unwrap()
            .id;
    })
    .await;
    let ended = invalidations_of(&db.pool, async {
        unlocks::end(&db.pool, id, &cli(), now()).await.unwrap();
    })
    .await;
    assert_eq!(created, [Invalidation::user(&staff)]);
    assert_eq!(ended, [Invalidation::user(&staff)]);
}

/// Ending an unlock whose admin was deleted (`unlocks.user_id` is then
/// NULL): it reaches nobody, so it sends nothing and leaves
/// `admin_generation` alone, but the unlock still ends.
#[tokio::test(flavor = "multi_thread")]
async fn ending_an_unlock_of_a_deleted_user_sends_nothing_and_does_not_bump() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    user(&db.pool, "staff").await;
    let on = AdminChange {
        on: true,
        allow_last: true,
    };
    users::set_admin(&db.pool, "staff", on, &cli(), now())
        .await
        .unwrap();
    let policy = UnlockPolicy {
        max_hours: 4,
        notify_owners: false,
        brand_defaults: crate::store::settings::Brand::default(),
    };
    let new = NewUnlock {
        catalog: "main",
        user: "staff",
        category: Category::Support,
        reason: "ticket 12",
        hours: 1,
    };
    let id = unlocks::create(&db.pool, &new, &policy, &cli(), now())
        .await
        .unwrap()
        .id;
    users::delete(&db.pool, "staff", DeleteOptions::OPERATOR, &cli(), now())
        .await
        .unwrap();
    let before = catalogs::admin_generation(&db.pool).await.unwrap();

    let got = invalidations_of(&db.pool, async {
        unlocks::end(&db.pool, id, &cli(), now()).await.unwrap();
    })
    .await;

    assert_eq!(got, []);
    assert_eq!(catalogs::admin_generation(&db.pool).await.unwrap(), before);
    let ended = unlocks::find(&db.pool, id).await.unwrap();
    assert!(ended.ended_at.is_some(), "the unlock ended");
    assert_eq!(ended.user, None, "its user was deleted");
}

// ---- teams ----

#[tokio::test(flavor = "multi_thread")]
async fn team_writes_invalidate_the_team_and_membership_writes_the_member() {
    let db = test_db().await;
    let alice = user(&db.pool, "alice").await;
    let new = NewTeam {
        name: "staff",
        display_name: &DisplayName::default(),
        owner: TEST_USER,
    };
    let renamed = DisplayName::parse("Staff").unwrap();
    let membership = Membership {
        team: "staff",
        user: "alice",
        role: Role::Member,
    };
    let mut staff = String::new();
    let got = invalidations_of(&db.pool, async {
        teams::create(&db.pool, &new, &cli(), now()).await.unwrap();
        staff = team_id(&db.pool, "staff").await;
        let change = TeamDisplayNameChange {
            team: "staff",
            display_name: &renamed,
            owner: None,
        };
        teams::set_display_name(&db.pool, &change, &cli(), now())
            .await
            .unwrap();
        teams::set_member(&db.pool, &membership, &cli(), now())
            .await
            .unwrap();
        teams::remove_member(&db.pool, "staff", "alice", &cli(), now())
            .await
            .unwrap();
        teams::delete(&db.pool, "staff", &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let team = Invalidation::team(&staff);
    let member = Invalidation::user(&alice);
    assert_eq!(
        got,
        [
            team.clone(),
            team.clone(),
            team.clone(),
            member.clone(),
            team.clone(),
            member,
            team
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn suspending_a_team_invalidates_it_and_its_catalogs_as_a_whole() {
    let db = test_db().await;
    catalog(&db.pool, "main").await;
    catalog(&db.pool, "other").await;
    let owner = team_id(&db.pool, TEST_TEAM).await;
    let suspended = invalidations_of(&db.pool, async {
        teams::suspend(&db.pool, TEST_TEAM, None, &cli(), now())
            .await
            .unwrap();
        // Already suspended with the same notice: nothing.
        teams::suspend(&db.pool, TEST_TEAM, None, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let lifted = invalidations_of(&db.pool, async {
        teams::unsuspend(&db.pool, TEST_TEAM, &cli(), now())
            .await
            .unwrap();
        // Not suspended: nothing.
        teams::unsuspend(&db.pool, TEST_TEAM, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let expected = [
        Invalidation::team(&owner),
        Invalidation::team_catalogs(&owner),
    ];
    assert_eq!(suspended, expected);
    assert_eq!(lifted, expected);
}

/// Catalogs the bulk test gives the team: enough that one message per
/// catalog would be plain in the count, few enough for the normal gate
/// (the issue's 50,000 take the one insert about a second, but the count
/// is the same at any size).
const MANY_CATALOGS: i64 = 5_000;

#[tokio::test(flavor = "multi_thread")]
async fn a_team_owning_thousands_of_catalogs_sends_two_messages_to_suspend_or_lift() {
    let db = test_db().await;
    let owner = team_id(&db.pool, TEST_TEAM).await;
    sqlx::query(
        "INSERT INTO catalogs (name, created_at, owner_team_id, provisioned_at) \
         SELECT 'bulk-' || n, now(), $1, now() FROM generate_series(1, $2::bigint) AS n",
    )
    .bind(&owner)
    .bind(MANY_CATALOGS)
    .execute(&db.pool)
    .await
    .unwrap();
    let owned: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogs WHERE owner_team_id = $1")
        .bind(&owner)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(owned >= MANY_CATALOGS, "the fixture: {owned} catalogs");

    let suspended = invalidations_of(&db.pool, async {
        teams::suspend(&db.pool, TEST_TEAM, None, &cli(), now())
            .await
            .unwrap();
    })
    .await;
    let lifted = invalidations_of(&db.pool, async {
        teams::unsuspend(&db.pool, TEST_TEAM, &cli(), now())
            .await
            .unwrap();
    })
    .await;

    assert_eq!(suspended.len(), 2, "counted, not timed: {suspended:?}");
    assert_eq!(lifted.len(), 2, "{lifted:?}");
}

// ---- catalog contents ----

#[tokio::test(flavor = "multi_thread")]
async fn an_import_and_a_repair_invalidate_the_catalog_at_their_generation() {
    let db = test_db().await;
    let main = catalog(&db.pool, "main").await;
    let audit = cli();
    let options = ImportOptions {
        replace: false,
        scope: ImportScope::Everything,
        expect_generation: None,
        expect_catalog_id: None,
        actor: "test/import",
        audit: &audit,
        now: now(),
    };
    let imported = invalidations_of(&db.pool, async {
        import(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "main",
            crate::archive::files(&[("a.md", NOTE)]),
            &options,
        )
        .await
        .unwrap();
    })
    .await;
    let repaired = invalidations_of(&db.pool, async {
        let options = RepairOptions {
            dry_run: false,
            actor: "okf-mcp/repair-indexes",
            audit: &audit,
            now: now(),
        };
        repair(
            &crate::stores::CatalogDb::main_in(&db.pool),
            "main",
            &options,
        )
        .await
        .unwrap();
    })
    .await;
    let at = |generation| Invalidation::Catalog {
        id: main.id,
        generation,
    };
    assert_eq!(imported, [at(1)]);
    assert_eq!(repaired, [at(2)]);
}
