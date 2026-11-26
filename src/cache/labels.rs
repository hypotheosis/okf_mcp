//! [`LabelLookup`]: labels on demand (M18 spec Decision 9). A response asks
//! for the labels of the slugs it will show ([`Wanted`]); the cached ones
//! are served, and the rest are read together in one round trip. Nothing
//! holds every label: one bounded cache serves users, teams and catalogs,
//! keyed by slug.
//!
//! A label is a display name of a person or team, so it is privacy
//! sensitive: it never appears in an invalidation payload, a log line or a
//! metric label, and [`LabelLookup::labels_for`] returns only the slugs it
//! was asked for.
//!
//! Each cached label remembers the id of the row it came from, because the
//! change feed names ids, not slugs ([`LabelLookup::apply`]).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::cache::{Bounded, CacheName, Limit};
use crate::error::OkfError;
use crate::invalidate::Invalidation;
use crate::labels::directory::{CatalogEntry, Labels, Wanted};
use crate::labels::{team_label, user_label};
use crate::telemetry::metrics::Metrics;

/// The default capacity: label entries kept, by slug.
pub const DEFAULT_LABEL_CACHE_ENTRIES: u64 = 100_000;

/// What a cached label is for: a user and its personal team share a slug,
/// so the kind is part of the key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LabelKey {
    User(String),
    Team(String),
    Catalog(String),
}

/// A cached label with the ids the change feed refers to it by.
#[derive(Debug, Clone)]
enum LabelValue {
    User {
        /// `users.id`.
        id: Arc<str>,
        label: Arc<str>,
    },
    Team {
        /// `teams.id`.
        id: Arc<str>,
        /// The `users.id` whose personal team it is: that user's label is
        /// part of its own.
        personal_of: Option<Arc<str>>,
        label: Arc<str>,
    },
    Catalog(CatalogEntry),
}

/// A `users` row as the lookup reads it.
#[derive(sqlx::FromRow)]
struct UserLoad {
    id: String,
    name: String,
    display_name: String,
    email: Option<String>,
}

/// A `teams` row with its personal owner's display fields.
#[derive(sqlx::FromRow)]
struct TeamLoad {
    id: String,
    name: String,
    display_name: String,
    personal_of: Option<String>,
    owner_display_name: Option<String>,
    owner_email: Option<String>,
}

/// A `catalogs` row as the lookup reads it.
#[derive(sqlx::FromRow)]
struct CatalogLoad {
    name: String,
    display_name: String,
    description: String,
    owner_team: Option<String>,
}

/// The label cache and the reads behind it.
pub struct LabelLookup {
    pool: PgPool,
    cache: Bounded<LabelKey, LabelValue>,
}

impl std::fmt::Debug for LabelLookup {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a Debug impl")
    )]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LabelLookup")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

/// The slugs of `keys` of one kind, without one Postgres cannot take.
#[tracing::instrument(level = "trace", skip_all)]
fn slugs_of(keys: &[LabelKey], kind: fn(&LabelKey) -> Option<&String>) -> Vec<String> {
    keys.iter()
        .filter_map(kind)
        .filter(|slug| !slug.contains('\0'))
        .cloned()
        .collect()
}

impl LabelLookup {
    /// An empty cache.
    ///
    /// # Arguments
    /// * `pool` - the database.
    /// * `entries` - the most labels kept.
    /// * `ttl` - how long a label is served without a message (the
    ///   backstop for a lost one).
    ///
    /// # Returns
    /// The lookup, with no metrics.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn new(pool: PgPool, entries: u64, ttl: Duration) -> Self {
        Self {
            pool,
            cache: Bounded::new(CacheName::Labels, Limit::Entries(entries), ttl),
        }
    }

    /// Records `okf.cache.*` for this cache.
    ///
    /// # Arguments
    /// * `metrics` - the instruments.
    ///
    /// # Returns
    /// The lookup.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.cache = self.cache.with_metrics(metrics);
        self
    }

    /// The labels of exactly the slugs in `wanted`. The cached ones are
    /// served; every other is read in one round trip.
    ///
    /// # Arguments
    /// * `wanted` - what the response will show.
    ///
    /// # Returns
    /// The labels. A slug that does not exist is absent.
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn labels_for(&self, wanted: &Wanted) -> Result<Labels, OkfError> {
        let mut labels = Labels::default();
        self.resolve(wanted, &mut labels).await?;
        Ok(labels)
    }

    /// [`LabelLookup::labels_for`], plus the owner teams of the catalogs in
    /// `wanted`: what [`Labels::catalog_choices`] needs to tell catalogs
    /// that share a label apart. A second round trip when a catalog names an
    /// owner team that was not asked for and is not cached.
    ///
    /// # Arguments
    /// * `wanted` - what the response will show.
    ///
    /// # Returns
    /// The labels, with the owner teams of the catalogs.
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn labels_with_owners(&self, wanted: &Wanted) -> Result<Labels, OkfError> {
        let mut labels = Labels::default();
        self.resolve(wanted, &mut labels).await?;
        let owners: BTreeSet<String> = wanted
            .catalogs
            .iter()
            .filter_map(|slug| labels.catalog_owner(slug))
            .filter(|team| !wanted.teams.contains(*team))
            .map(str::to_string)
            .collect();
        if !owners.is_empty() {
            let extra = Wanted {
                teams: owners,
                ..Wanted::default()
            };
            self.resolve(&extra, &mut labels).await?;
        }
        Ok(labels)
    }

    /// Adds the labels of `wanted` to `labels`.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn resolve(&self, wanted: &Wanted, labels: &mut Labels) -> Result<(), OkfError> {
        let keys: Vec<LabelKey> = wanted
            .users
            .iter()
            .map(|slug| LabelKey::User(slug.clone()))
            .chain(wanted.teams.iter().map(|slug| LabelKey::Team(slug.clone())))
            .chain(
                wanted
                    .catalogs
                    .iter()
                    .map(|slug| LabelKey::Catalog(slug.clone())),
            )
            .collect();
        let found: HashMap<LabelKey, LabelValue> = self
            .cache
            .get_or_load_many(keys, |missing| self.read(missing))
            .await?;
        for (key, value) in found {
            match (key, value) {
                (LabelKey::User(slug), LabelValue::User { label, .. }) => {
                    labels.insert_user(slug, label);
                }
                (LabelKey::Team(slug), LabelValue::Team { label, .. }) => {
                    labels.insert_team(slug, label);
                }
                (LabelKey::Catalog(slug), LabelValue::Catalog(entry)) => {
                    labels.insert_catalog(slug, entry);
                }
                // `read` pairs each key with its own kind of value.
                _ => {}
            }
        }
        Ok(())
    }

    /// Reads the labels of `missing`: one query per kind present, together.
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "labels.read"))]
    async fn read(&self, missing: Vec<LabelKey>) -> Result<Vec<(LabelKey, LabelValue)>, OkfError> {
        let users = slugs_of(&missing, |key| match key {
            LabelKey::User(slug) => Some(slug),
            LabelKey::Team(_) | LabelKey::Catalog(_) => None,
        });
        let teams = slugs_of(&missing, |key| match key {
            LabelKey::Team(slug) => Some(slug),
            LabelKey::User(_) | LabelKey::Catalog(_) => None,
        });
        let catalogs = slugs_of(&missing, |key| match key {
            LabelKey::Catalog(slug) => Some(slug),
            LabelKey::User(_) | LabelKey::Team(_) => None,
        });
        let (user_rows, team_rows, catalog_rows) = tokio::try_join!(
            async {
                if users.is_empty() {
                    return Ok(Vec::new());
                }
                sqlx::query_as::<_, UserLoad>(
                    "SELECT id, name, display_name, email::text AS email \
                     FROM users WHERE name = ANY($1)",
                )
                .bind(&users)
                .fetch_all(&self.pool)
                .await
            },
            async {
                if teams.is_empty() {
                    return Ok(Vec::new());
                }
                sqlx::query_as::<_, TeamLoad>(
                    "SELECT t.id, t.name, t.display_name, t.personal_of, \
                            u.display_name AS owner_display_name, \
                            u.email::text AS owner_email \
                     FROM teams t LEFT JOIN users u ON u.id = t.personal_of \
                     WHERE t.name = ANY($1)",
                )
                .bind(&teams)
                .fetch_all(&self.pool)
                .await
            },
            async {
                if catalogs.is_empty() {
                    return Ok(Vec::new());
                }
                sqlx::query_as::<_, CatalogLoad>(
                    "SELECT c.name, c.display_name, c.description, t.name AS owner_team \
                     FROM catalogs c LEFT JOIN teams t ON t.id = c.owner_team_id \
                     WHERE c.name = ANY($1) AND c.deleted_at IS NULL",
                )
                .bind(&catalogs)
                .fetch_all(&self.pool)
                .await
            },
        )?;
        let mut loaded = Vec::with_capacity(user_rows.len() + team_rows.len() + catalog_rows.len());
        for row in user_rows {
            let label = Arc::from(user_label(&row.display_name, row.email.as_deref()));
            loaded.push((
                LabelKey::User(row.name),
                LabelValue::User {
                    id: Arc::from(row.id),
                    label,
                },
            ));
        }
        for row in team_rows {
            let owner = row.personal_of.as_ref().map(|_| {
                user_label(
                    row.owner_display_name.as_deref().unwrap_or_default(),
                    row.owner_email.as_deref(),
                )
            });
            let label = Arc::from(team_label(&row.display_name, owner.as_deref()));
            loaded.push((
                LabelKey::Team(row.name),
                LabelValue::Team {
                    id: Arc::from(row.id),
                    personal_of: row.personal_of.map(Arc::from),
                    label,
                },
            ));
        }
        for row in catalog_rows {
            let entry = CatalogEntry::new(
                &row.display_name,
                &row.description,
                row.owner_team.as_deref(),
            );
            loaded.push((LabelKey::Catalog(row.name), LabelValue::Catalog(entry)));
        }
        Ok(loaded)
    }

    /// The slugs of the teams a user belongs to.
    ///
    /// # Arguments
    /// * `user` - the user's slug.
    ///
    /// # Returns
    /// The team slugs (empty for an unknown user).
    ///
    /// # Errors
    /// [`OkfError::Database`].
    #[tracing::instrument(level = "debug", skip_all, fields(db.system.name = "postgresql", db.operation.name = "labels.teams_of"))]
    pub async fn teams_of(&self, user: &str) -> Result<Vec<String>, OkfError> {
        if user.contains('\0') {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT t.name FROM team_members m \
             JOIN users u ON u.id = m.user_id JOIN teams t ON t.id = m.team_id \
             WHERE u.name = $1",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Applies one change-feed message: a user's change drops its label and
    /// its personal team's; a team's drops its label; a catalog's drops its
    /// label (and its owner). Messages carry ids, so the entries are found
    /// by the id they remember.
    ///
    /// # Arguments
    /// * `invalidation` - what changed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn apply(&self, invalidation: &Invalidation) {
        match invalidation {
            Invalidation::User { id } => {
                self.cache
                    .invalidate_where(|_, value| match value {
                        LabelValue::User { id: held, .. } => **held == **id,
                        LabelValue::Team { personal_of, .. } => {
                            personal_of.as_deref() == Some(id.as_str())
                        }
                        LabelValue::Catalog(_) => false,
                    })
                    .await;
            }
            Invalidation::Team { id } => {
                self.cache
                    .invalidate_where(|_, value| {
                        matches!(value, LabelValue::Team { id: held, .. } if **held == **id)
                    })
                    .await;
            }
            Invalidation::CatalogMeta { name, .. } => {
                self.cache
                    .invalidate(&LabelKey::Catalog(name.clone()))
                    .await;
            }
            // Content, credentials and settings carry no label.
            Invalidation::Catalog { .. }
            | Invalidation::TeamCatalogs { .. }
            | Invalidation::Credential { .. }
            | Invalidation::Settings
            | Invalidation::Store { .. } => {}
        }
    }

    /// After messages may have been missed: every label is read again on
    /// its next use.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn mark_all_stale(&self) {
        self.cache.mark_all_stale();
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sqlx::PgPool;

    use super::*;
    use crate::db::{test_db, TEST_TEAM, TEST_USER};
    use crate::invalidate::testing::invalidations_of;
    use crate::labels::DisplayName;
    use crate::mcp::testing::{local_metrics, render};
    use crate::store::audit::AuditActor;
    use crate::store::catalogs::{self, NewCatalog};
    use crate::store::users;

    const LONG: Duration = Duration::from_secs(3600);

    fn lookup(pool: &PgPool) -> LabelLookup {
        LabelLookup::new(pool.clone(), 100, LONG)
    }

    async fn catalog(pool: &PgPool, name: &str, display: &str) {
        let new = NewCatalog {
            name,
            display_name: &DisplayName::parse(display).unwrap(),
            description: "",
            owner_team: TEST_TEAM,
        };
        catalogs::create(pool, &new, &AuditActor::cli(), Utc::now())
            .await
            .unwrap();
    }

    fn wanted(users: &[&str], teams: &[&str], catalogs: &[&str]) -> Wanted {
        let mut wanted = Wanted::default();
        for slug in users {
            wanted.user(slug);
        }
        for slug in teams {
            wanted.team(slug);
        }
        for slug in catalogs {
            wanted.catalog(slug);
        }
        wanted
    }

    async fn user_id(pool: &PgPool, name: &str) -> String {
        sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The loads the labels cache recorded, from a rendered scrape.
    fn labels_loads(registry: &prometheus::Registry) -> u64 {
        render(registry)
            .lines()
            .find(|line| {
                line.starts_with("okf_cache_load_duration_seconds_count")
                    && line.contains("cache=\"labels\"")
            })
            .and_then(|line| line.rsplit(' ').next())
            .and_then(|count| count.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn labels_for_returns_only_the_slugs_asked_for() {
        let db = test_db().await;
        catalog(&db.pool, "notes", "Notes").await;
        catalog(&db.pool, "other", "Other").await;
        let labels = lookup(&db.pool);

        let got = labels
            .labels_for(&wanted(&[TEST_USER], &[TEST_TEAM], &["notes"]))
            .await
            .unwrap();

        assert!(got.user(TEST_USER).is_some());
        assert!(got.team(TEST_TEAM).is_some());
        assert_eq!(got.catalog("notes"), Some("Notes"));
        assert_eq!(got.catalog("other"), None, "a catalog not asked for");
        assert_eq!(got.user("nobody"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_response_reads_every_kind_in_one_load() {
        let db = test_db().await;
        for n in 0..50 {
            catalog(&db.pool, &format!("c-{n}"), &format!("Catalog {n}")).await;
        }
        let (_provider, metrics, registry) = local_metrics();
        let labels = lookup(&db.pool).with_metrics(metrics);
        let slugs: Vec<String> = (0..50).map(|n| format!("c-{n}")).collect();
        let refs: Vec<&str> = slugs.iter().map(String::as_str).collect();

        let got = labels
            .labels_for(&wanted(&[TEST_USER], &[TEST_TEAM], &refs))
            .await
            .unwrap();

        assert_eq!(got.catalog("c-49"), Some("Catalog 49"));
        // The catalogs' owner (the test team) was asked for: no second read.
        assert_eq!(labels_loads(&registry), 1, "users, teams and 50 catalogs");

        labels
            .labels_for(&wanted(&[TEST_USER], &[TEST_TEAM], &refs))
            .await
            .unwrap();
        assert_eq!(labels_loads(&registry), 1, "the second response is cached");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn labels_with_owners_adds_the_owner_teams() {
        let db = test_db().await;
        catalog(&db.pool, "notes", "Notes").await;
        let labels = lookup(&db.pool);

        let exact = labels
            .labels_for(&wanted(&[], &[], &["notes"]))
            .await
            .unwrap();
        let with_owner = labels
            .labels_with_owners(&wanted(&[], &[], &["notes"]))
            .await
            .unwrap();

        assert_eq!(exact.team(TEST_TEAM), None, "exact means exact");
        assert_eq!(with_owner.catalog_owner("notes"), Some(TEST_TEAM));
        assert!(with_owner.team(TEST_TEAM).is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_user_invalidation_drops_that_users_label() {
        let db = test_db().await;
        let labels = lookup(&db.pool);
        let ask = wanted(&[TEST_USER], &[TEST_TEAM], &[]);
        let before = labels.labels_for(&ask).await.unwrap();
        let name = DisplayName::parse("Renamed Person").unwrap();

        let sent = invalidations_of(&db.pool, async {
            users::set_display_name(&db.pool, TEST_USER, &name, &AuditActor::cli(), Utc::now())
                .await
                .unwrap();
        })
        .await;
        // Not applied yet: the cache still serves the old label.
        let stale = labels.labels_for(&ask).await.unwrap();
        assert_eq!(stale.user(TEST_USER), before.user(TEST_USER));
        for invalidation in &sent {
            labels.apply(invalidation).await;
        }

        let after = labels.labels_for(&ask).await.unwrap();
        assert_eq!(after.user(TEST_USER), Some("Renamed Person"));
        assert_ne!(after.team(TEST_TEAM), before.team(TEST_TEAM));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_catalog_meta_invalidation_drops_that_catalogs_label() {
        let db = test_db().await;
        catalog(&db.pool, "notes", "Notes").await;
        let labels = lookup(&db.pool);
        let ask = wanted(&[], &[], &["notes"]);
        labels.labels_for(&ask).await.unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM catalogs WHERE name = 'notes'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE catalogs SET display_name = 'Renamed' WHERE name = 'notes'")
            .execute(&db.pool)
            .await
            .unwrap();

        labels
            .apply(&Invalidation::CatalogMeta {
                id,
                name: "notes".to_string(),
            })
            .await;

        let after = labels.labels_for(&ask).await.unwrap();
        assert_eq!(after.catalog("notes"), Some("Renamed"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_cache_rereads_and_an_unknown_slug_is_never_cached() {
        let db = test_db().await;
        let (_provider, metrics, registry) = local_metrics();
        let labels = lookup(&db.pool).with_metrics(metrics);
        let ask = wanted(&[TEST_USER, "ghost"], &[], &[]);
        labels.labels_for(&ask).await.unwrap();
        assert_eq!(labels_loads(&registry), 1);

        // `ghost` was absent, so it is read again; the user is cached.
        labels.labels_for(&ask).await.unwrap();
        assert_eq!(labels_loads(&registry), 2);

        labels.mark_all_stale();
        labels
            .labels_for(&wanted(&[TEST_USER], &[], &[]))
            .await
            .unwrap();
        assert_eq!(labels_loads(&registry), 3, "stale entries are read again");
        let _ = user_id(&db.pool, TEST_USER).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn teams_of_reads_the_users_teams_but_does_not_fill_the_cache() {
        let db = test_db().await;
        let labels = lookup(&db.pool);

        let teams = labels.teams_of(TEST_USER).await.unwrap();

        assert_eq!(teams, [TEST_TEAM]);
        assert!(labels
            .cache
            .peek(&LabelKey::User(TEST_USER.to_string()))
            .await
            .is_none());
    }
}
