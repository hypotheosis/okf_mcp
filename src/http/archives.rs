//! The archive routes (design "Process and routes"): a catalog's export as a
//! `.tar.gz`, and an import from one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, MethodRouter};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use crate::app::App;
use crate::archive::read_tarball;
use crate::auth::{Admission, CatalogScope, Principal};
use crate::error::OkfError;
use crate::http::requester::Requester;
use crate::http::{timeout_layer_after, AppError, REQUEST_TIMEOUT};
use crate::labels::directory::Wanted;
use crate::labels::export_stem;
use crate::mcp::blocking;
use crate::store::audit::{AccessKind, AuditActor};
use crate::store::catalogs::CatalogRow;
use crate::store::export::{export, ExportOptions};
use crate::store::import::{import, ImportOptions, ImportScope};
use crate::store::keys::Access;
use crate::telemetry::metrics::{Direction, Metrics, Outcome};
use crate::telemetry::subscriber::split_event;

/// The two routes, for the keyed part of the router.
///
/// # Arguments
/// * `app` - the shared state (for `max_import_bytes`).
///
/// # Returns
/// A router over `Arc<App>` state: the export behind the request timeout,
/// the import with its body cap (and its own deadline, see
/// [`import_catalog`]).
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "router builder: runs once at startup, outside any request, so a span would be an orphan root"
    )
)]
pub(crate) fn routes(app: &App) -> Router<Arc<App>> {
    Router::new()
        .route("/catalogs/{name}/export", export_route())
        .route("/catalogs/{name}/import", import_route(app))
}

/// The export route's method router: [`export_catalog`] behind the request
/// timeout (503 after [`REQUEST_TIMEOUT`], as for `/healthz`). Shared with
/// `/api/v1`, so both paths behave the same; mount it on a path with one
/// parameter, the catalog.
///
/// # Returns
/// The `GET` method router.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub(crate) fn export_route() -> MethodRouter<Arc<App>> {
    export_route_within(REQUEST_TIMEOUT)
}

/// [`export_route`] with its deadline given, so a test can pass a short one.
///
/// # Arguments
/// * `timeout` - how long the export may take before the 503.
///
/// # Returns
/// The `GET` method router.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub(crate) fn export_route_within(timeout: Duration) -> MethodRouter<Arc<App>> {
    // A layer on one method router applies to that route only.
    get(export_catalog).layer(timeout_layer_after(timeout))
}

/// The import route's method router: [`import_catalog`] with the body cap
/// [`body_limit`] computes from `max_import_bytes`, and no timeout layer.
/// One that fired after the import committed would answer 503 for an import
/// that happened (and a retry would get 409), so the handler bounds the
/// upload itself. Shared with `/api/v1`; mount it on a path with one
/// parameter, the catalog.
///
/// # Arguments
/// * `app` - the shared state (for `max_import_bytes`).
///
/// # Returns
/// The `POST` method router.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
pub(crate) fn import_route(app: &App) -> MethodRouter<Arc<App>> {
    let limit = body_limit(app.config().max_import_bytes.as_u64());
    post(import_catalog).layer(DefaultBodyLimit::max(limit))
}

/// The request-body cap for an import whose decompressed tar stream may be
/// `max_import_bytes`. gzip makes incompressible input slightly larger (5
/// bytes per stored 64 KiB block, plus an 18-byte header and trailer), so the
/// cap adds 0.1% and 64 KiB: a tarball the CLI would accept is never refused
/// here with 413. `read_tarball` still enforces the exact limit on the
/// decompressed stream.
///
/// # Arguments
/// * `max_import_bytes` - the configured limit.
///
/// # Returns
/// The body cap, saturating at `usize::MAX`.
#[cfg_attr(
    dylint_lib = "hyp_tracing",
    allow(
        uninstrumented_fn,
        reason = "trivial accessor or conversion: returns or maps a value, a span would record nothing new"
    )
)]
fn body_limit(max_import_bytes: u64) -> usize {
    let slack = max_import_bytes / 1000 + 64 * 1024;
    // `try_from` fails only for a `u64` that does not fit a `usize` (a
    // 32-bit target), which is larger than any body anyway.
    usize::try_from(max_import_bytes.saturating_add(slack)).unwrap_or(usize::MAX)
}

/// Counts an export or import under an unlock on that unlock (M14 Task 9b),
/// in the background. Any other admission records nothing here: the export
/// and import write their own entries.
///
/// # Arguments
/// * `app` - the shared state.
/// * `principal` - the caller.
/// * `requester` - its address and source.
/// * `row` - the catalog's directory row the admission was decided with.
/// * `admission` - how the caller was let in.
/// * `kind` - [`AccessKind::Export`] or [`AccessKind::Write`].
#[tracing::instrument(level = "trace", skip_all)]
fn note_unlocked(
    app: &App,
    principal: &Principal,
    requester: &Requester,
    row: &CatalogRow,
    admission: Admission,
    kind: AccessKind,
) {
    // `let ... else`: leave unless the admission is an unlock's.
    let Admission::Unlocked { .. } = admission else {
        return;
    };
    // The row that admitted it is of the catalog the unlock is on (an
    // unlock matches by `catalogs.id`); the tally needs only the unlock's id.
    let scope = CatalogScope::of(row);
    let record = principal.access_record(requester, &row.name, scope, app.clock().now());
    app.note_access(record, kind, admission);
}

/// Records the span's `okf.catalog` for a catalog the request has been
/// admitted to and that exists, and returns that catalog's own name.
/// A name that no catalog bears (an unknown or refused path segment) is
/// recorded nowhere: neither the span nor a metric label takes a raw path
/// value, only one validated against the catalog directory (a failed
/// lookup counts as none).
///
/// # Arguments
/// * `span` - the handler's span, captured before this function's own
///   trace-level span becomes the current one.
/// * `app` - the shared state.
/// * `name` - the catalog, from the path, already admitted.
///
/// # Returns
/// The catalog's name, or `None`.
#[tracing::instrument(level = "trace", skip_all)]
async fn resolved_catalog(span: &tracing::Span, app: &App, name: &str) -> Option<String> {
    let catalog = app.catalog_row(name).await.ok()??.name.clone();
    span.record("okf.catalog", catalog.as_str());
    Some(catalog)
}

/// One archive transfer being measured. [`Transfer::finish`] records how it
/// ended; if the handler's future is dropped first (the export route's
/// timeout, or a client that disconnects mid-import), `Drop` records an
/// error instead, so the slowest transfers never vanish from the spans and
/// metrics.
struct Transfer {
    // The handler's span, captured before any trace-level helper span
    // becomes the current one (see [`resolved_catalog`]).
    span: tracing::Span,
    metrics: Metrics,
    direction: Direction,
    // The loaded catalog, as a gate: the size and the metrics are recorded
    // only for `Some`.
    catalog: Option<String>,
    started: Instant,
    finished: bool,
}

impl Transfer {
    /// Starts measuring a transfer.
    ///
    /// # Arguments
    /// * `span` - the handler's span.
    /// * `app` - the shared state (for its metrics).
    /// * `direction` - export or import.
    ///
    /// It starts before the permission and unlock checks, so a refusal
    /// there (an early `?`) still closes the span with an error outcome. It
    /// has no catalog until [`Transfer::admit`]: nothing but the closed
    /// outcome is recorded for a refused request.
    #[tracing::instrument(level = "trace", skip_all)]
    fn begin(span: tracing::Span, app: &App, direction: Direction) -> Self {
        Self {
            span,
            metrics: app.metrics().clone(),
            direction,
            catalog: None,
            started: Instant::now(),
            finished: false,
        }
    }

    /// Names the catalog once the request is admitted to it.
    ///
    /// # Arguments
    /// * `catalog` - [`resolved_catalog`]'s answer.
    #[tracing::instrument(level = "trace", skip_all)]
    fn admit(&mut self, catalog: Option<String>) {
        self.catalog = catalog;
    }

    /// Records the transfer's end: ok with its size, or an error for `None`.
    ///
    /// # Arguments
    /// * `bytes` - the archive's size when the transfer succeeded.
    #[tracing::instrument(level = "trace", skip_all)]
    fn finish(mut self, bytes: Option<usize>) {
        self.record(bytes);
    }

    /// Records the outcome once (a second call does nothing).
    #[tracing::instrument(level = "trace", skip_all)]
    fn record(&mut self, bytes: Option<usize>) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        // `usize` to `u64` is lossless on every platform Rust supports.
        let bytes = bytes.map(|bytes| bytes as u64);
        let outcome = if bytes.is_some() {
            Outcome::Ok
        } else {
            Outcome::Error
        };
        // The outcome is a closed value, so it is recorded even for a name
        // no loaded catalog bears; the size and the metrics need a resolved
        // catalog.
        self.span.record("okf.outcome", outcome.as_str());
        if outcome == Outcome::Error {
            self.span.record("otel.status_code", "error");
        }
        // The metrics carry no catalog name, but a request that resolved
        // no catalog is still not counted.
        if self.catalog.is_none() {
            return;
        }
        if let Some(bytes) = bytes {
            // Spans take integers as `i64`; a size past `i64::MAX` is not
            // real.
            self.span
                .record("okf.archive.size", i64::try_from(bytes).unwrap_or(i64::MAX));
        }
        self.metrics
            .archive(self.direction, outcome, self.started.elapsed(), bytes);
    }
}

// `Drop` runs when the value goes out of scope, like a Python context
// manager's `__exit__`, and also when a cancelled future is dropped.
impl Drop for Transfer {
    #[cfg_attr(
        dylint_lib = "hyp_tracing",
        allow(uninstrumented_fn, reason = "a drop hook that calls the recorder")
    )]
    fn drop(&mut self) {
        self.record(None);
    }
}

/// `GET /catalogs/{name}/export`: the published documents as a `.tar.gz`,
/// for a key with `read` on the catalog.
///
/// # Arguments
/// * `app` - the shared state.
/// * `principal` - the caller, from the auth layer.
/// * `requester` - its address and source, for the `catalog.exported` entry.
/// * `name` - the catalog, from the path.
///
/// # Returns
/// The tarball, as `application/gzip` with a download file name.
///
/// # Errors
/// 404 ([`OkfError::CatalogNotSelected`]) for a catalog that does not exist
/// or that the key does not reach; 403 ([`OkfError::UnlockRequired`]) for
/// operator staff without an unlock while `require_unlock` is on, or
/// ([`OkfError::MaintenanceOff`]) for the maintenance runner on a catalog
/// that opted out; 500 for a database or archive failure.
// Each argument is an extractor: axum fills it from the request (the state,
// the auth layer's principal, the `{name}` path segment), much as a Python
// web framework injects arguments by their annotations.
#[tracing::instrument(
    name = "okf.archive.export",
    level = "info",
    skip_all,
    fields(
        okf.catalog = tracing::field::Empty,
        okf.archive.size = tracing::field::Empty,
        okf.outcome = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    )
)]
async fn export_catalog(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    requester: Requester,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    // `?` returns early with the error, converted to `AppError` by `From`.
    // Export stays open while the owner team is suspended (M13 spec), so
    // members can take their data; reach and read access still apply.
    // Before any check, so a refusal still ends the span with an error.
    let mut transfer = Transfer::begin(tracing::Span::current(), &app, Direction::Export);
    // `reached_row` checks reach and read access (the whole of
    // `require_export`) before it reads the catalog's row.
    let row = principal
        .reached_row(app.directory(), Access::Read, &name)
        .await?;
    // Then the unlock (M14 Task 9b), before anything is read or recorded.
    let now = app.clock().now();
    let admission = principal.admission(&row, Access::Read, now)?;
    // M3's export builds the whole tarball in memory (a `Vec<u8>`).
    // The generation is for the command line's `import --expect-generation`.
    let actor = AuditActor::of(&principal, &requester);
    let options = ExportOptions {
        audit: &actor,
        now,
        // The incarnation the admission was decided for: the store refuses
        // a catalog created again under this name since its row was read.
        expect_catalog_id: Some(row.id),
    };
    // Only now is the name admitted: the span and the metrics take the
    // loaded catalog's own name, never the path segment.
    transfer.admit(resolved_catalog(&transfer.span, &app, &name).await);
    // A store that cannot be reached is an error of the export, which
    // `finish` records like any other.
    let exported = match app.catalog_db(&row).await {
        Ok(db) => export(&db, &name, Vec::new(), &options).await,
        Err(error) => Err(error),
    };
    transfer.finish(
        exported
            .as_ref()
            .ok()
            .map(|(bytes, _generation)| bytes.len()),
    );
    let (bytes, _generation) = exported?;
    note_unlocked(
        &app,
        &principal,
        &requester,
        &row,
        admission,
        AccessKind::Export,
    );
    // The stem is ASCII letters, digits and '-', and `name` is `[a-z0-9-]+`:
    // both are safe in a header.
    let mut wanted = Wanted::default();
    wanted.catalog(&name);
    let stem = app
        .labels()
        .labels_for(&wanted)
        .await?
        .catalog(&name)
        .map(export_stem)
        .unwrap_or_default();
    let file = if stem.is_empty() {
        name.clone()
    } else {
        format!("{stem}-{name}")
    };
    let disposition = format!("attachment; filename=\"{file}.tar.gz\"");
    let headers = [
        (header::CONTENT_TYPE, "application/gzip".to_string()),
        (header::CONTENT_DISPOSITION, disposition),
    ];
    Ok((headers, bytes).into_response())
}

/// An import's query: `?replace=true&only_machinery=true&expect_generation=N`,
/// the HTTP spelling of the CLI's `--replace`, `--only-machinery` and
/// `--expect-generation N`. Booleans are `true` or `false`; anything else is
/// axum's 400.
#[derive(Debug, Default, Deserialize)]
struct ImportQuery {
    /// Replace a non-empty catalog's documents (not needed at generation 0).
    #[serde(default)]
    replace: bool,
    /// Replace only repository machinery ([`ImportScope::Machinery`]); the
    /// archive's other files are ignored and the catalog's concepts are
    /// kept. A non-empty catalog past generation 0 also needs `replace`, as
    /// with the CLI.
    #[serde(default)]
    only_machinery: bool,
    /// Refuse (409) unless the catalog is still at this generation, the one
    /// its export had. A missing `Option` field deserializes as `None`.
    expect_generation: Option<i64>,
}

/// `POST /catalogs/{name}/import[?replace=true&only_machinery=true&expect_generation=N]`:
/// imports the uploaded `.tar.gz` as the key's actor, for a key with `write`
/// (or `admin`) on the catalog, and swaps the new snapshot in before answering, so the
/// next read sees it.
///
/// The key is checked before the body is read, so a key that may not import
/// cannot make the server buffer an upload. Reading and checking the upload
/// must finish within [`REQUEST_TIMEOUT`] (else 503, nothing imported); the
/// import and the refresh after it have no deadline, so a committed import
/// is never answered with a timeout.
///
/// # Arguments
/// * `app` - the shared state.
/// * `principal` - the caller, from the auth layer.
/// * `actor` - the caller, as the audit log's `catalog.imported` records it.
/// * `requester` - its address and source, for an unlock's tally.
/// * `name` - the catalog, from the path.
/// * `query` - the import options.
/// * `request` - the rest of the request: its body is the tarball.
///
/// # Returns
/// The import's report as JSON; 413 past [`body_limit`]; 503 if the upload
/// is not read and checked within [`REQUEST_TIMEOUT`].
///
/// # Errors
/// 404 for a catalog that does not exist or that the key does not reach;
/// 403 ([`OkfError::Forbidden`]) for a key below `write`,
/// ([`OkfError::Suspended`]) for a non-admin while the catalog's owner team
/// is suspended, or ([`OkfError::UnlockRequired`]) for operator staff
/// without an unlock while `require_unlock` is on; 400
/// ([`OkfError::ImportRefused`]) for an unreadable or rule-breaking archive;
/// 409 ([`OkfError::Refused`]) for a non-empty catalog past generation 0
/// without `?replace=true`, or one no longer at `expect_generation`; 500 for a
/// database failure. A refresh that fails after the import committed is
/// logged, not returned.
#[tracing::instrument(
    name = "okf.archive.import",
    level = "info",
    skip_all,
    fields(
        okf.catalog = tracing::field::Empty,
        okf.archive.size = tracing::field::Empty,
        okf.outcome = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    )
)]
async fn import_catalog(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<Arc<Principal>>,
    actor: AuditActor,
    requester: Requester,
    Path(name): Path<String>,
    Query(query): Query<ImportQuery>,
    // The whole request, unread, must come last: reading its body consumes
    // it. Taking `Request` rather than `Bytes` defers the read until after
    // the key check.
    request: Request,
) -> Result<Response, AppError> {
    // Before any check, so a refusal still ends the span with an error.
    let mut transfer = Transfer::begin(tracing::Span::current(), &app, Direction::Import);
    // Reach, then level, then suspension (`Principal::require`): a key that
    // does not reach the catalog gets 404, one that reaches it read-only
    // gets 403, and a non-admin cannot import into a suspended catalog.
    // Import is a write, so `write` is enough (issue #260).
    let row = principal
        .reached_row(app.directory(), Access::Write, &name)
        .await?;
    principal.require(Access::Write, &row)?;
    // An import rewrites the catalog's contents: a write, which needs the
    // unlock a write does (M14 Task 9b). Checked before the body is read.
    let admission = principal.admission(&row, Access::Write, app.clock().now())?;
    // Only now is the name admitted: the span and the metrics take the
    // loaded catalog's own name, never the path segment.
    transfer.admit(resolved_catalog(&transfer.span, &app, &name).await);
    let mut uploaded = None;
    // An `async` block, so every early return below (a refusal, a timeout)
    // still passes through the measurement after it.
    let answer: Result<Response, AppError> = async {
        let max_bytes = app.config().max_import_bytes.as_u64();
        // An `async` block is a future that runs only when awaited (here, under
        // the deadline). Its error is the response to send instead.
        let upload = async {
            // `Bytes::from_request` is what a `Bytes` handler argument runs: it
            // honours the route's `DefaultBodyLimit`, and its rejection is a
            // ready-made response (413 past the cap).
            let body = Bytes::from_request(request, &())
                .await
                .map_err(IntoResponse::into_response)?;
            // Decompressing and checking every entry is CPU work, so it runs on
            // a blocking thread; `move` hands `body` (a cheap reference-counted
            // buffer) to the closure.
            let size = body.len();
            blocking(move || read_tarball(body.as_ref(), max_bytes))
                .await
                .map(|files| (files, size))
                .map_err(|error| AppError(error).into_response())
        };
        // `timeout` gives `Err(Elapsed)` if the deadline passes first; dropping
        // the unfinished upload then cancels it, and nothing is imported.
        let files = match tokio::time::timeout(REQUEST_TIMEOUT, upload).await {
            Ok(Ok((files, size))) => {
                uploaded = Some(size);
                files
            }
            Ok(Err(refusal)) => return Ok(refusal),
            Err(_elapsed) => return Ok(upload_timed_out()),
        };
        let options = ImportOptions {
            replace: query.replace,
            scope: if query.only_machinery {
                ImportScope::Machinery
            } else {
                ImportScope::Everything
            },
            expect_generation: query.expect_generation,
            // As for export: the store checks it under the catalog's lock.
            expect_catalog_id: Some(row.id),
            actor: &principal.actor,
            audit: &actor,
            now: app.clock().now(),
        };
        let db = app.catalog_db(&row).await?;
        let report = import(&db, &name, files, &options)
            .await
            // `store::import`'s "not empty" refusal names the CLI flag: say what
            // HTTP needs instead. Its generation refusal ("... export it again")
            // already fits and passes through unchanged.
            .map_err(|error| match error {
                OkfError::Refused { reason } => OkfError::Refused {
                    reason: reason.replace("pass --replace", "add ?replace=true"),
                },
                other => other,
            })?;
        // Swap the new snapshot in now, not when the change message arrives.
        // The import has committed, so a failed refresh must not turn it into
        // an error (a retry would then get 409): log it and leave it to the
        // next use, which checks the snapshot against the catalog's row. A
        // catalog this replica has not cached loads on its first use. So does
        // a committed import whose client disconnects (dropping this future)
        // during the refresh; a disconnect during `import` itself drops its
        // transaction, which rolls back, and nothing is imported.
        // Committed: tell the other replicas, on its own task (issue #659).
        if let Some(change) = report.changed {
            app.announce_catalog(change);
            // This replica's own write, applied before the answer (in
            // process, no I/O): the directory drops its row behind the new
            // generation, so the next read loads the import even when the
            // catalog is not cached (one remembered as broken at the old
            // generation included). The change feed delivers the same
            // message later; applying it twice is harmless.
            app.apply_own_writes(&[crate::invalidate::Invalidation::Catalog {
                id: change.catalog_id,
                generation: change.generation,
            }])
            .await;
            let caught_up = app
                .catalog_cache()
                .catch_up(change.catalog_id, change.generation, app.rebuild_locks())
                .await;
            if let Err(error) = caught_up {
                split_event!(
                    error,
                    { catalog = %name, kind = error.kind(), },
                    { %error, },
                    "refresh after import failed; the next use retries"
                );
            }
        }
        note_unlocked(
            &app,
            &principal,
            &requester,
            &row,
            admission,
            AccessKind::Write,
        );
        // The key's id, never the key itself.
        split_event!(
            info,
            { catalog = %name, },
            { key = %principal.key_id, },
            "imported"
        );
        Ok(Json(report).into_response())
    }
    .await;
    let succeeded = answer
        .as_ref()
        .is_ok_and(|response| response.status().is_success());
    transfer.finish(uploaded.filter(|_| succeeded));
    answer
}

/// The answer to an upload not read and checked within [`REQUEST_TIMEOUT`]:
/// 503 with the `{error, message}` JSON every other refusal of the route
/// has. Built here rather than as an [`OkfError`] variant, which only this
/// route would ever return.
///
/// # Returns
/// The response.
#[tracing::instrument(level = "trace", skip_all)]
fn upload_timed_out() -> Response {
    let body = serde_json::json!({
        "error": "Timeout",
        "message": format!(
            "the upload was not received and checked within {} s; nothing was imported",
            REQUEST_TIMEOUT.as_secs()
        ),
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

/// A valid `.tar.gz` of `entries` (`(path, text)` pairs, none executable),
/// for the tests here and in `http::api`.
///
/// # Panics
/// If the archive cannot be written (it is built in memory, so it can't).
#[cfg(test)]
pub(crate) fn tarball_of(entries: &[(&str, &str)]) -> Vec<u8> {
    use crate::archive::{write_tarball, ExportFile};
    let modified = chrono::Utc::now();
    let files: Vec<ExportFile<'_>> = entries
        .iter()
        // `&(path, text)` destructures each borrowed pair, copying out the
        // two `&str`s (which are `Copy`).
        .map(|&(path, text)| ExportFile {
            path,
            content: text.as_bytes(),
            executable: false,
            modified,
        })
        .collect();
    write_tarball(Vec::new(), &files).expect("an in-memory tarball")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use axum::body::Body;
    use tokio::io::AsyncWriteExt;
    use tokio_util::io::ReaderStream;
    // `oneshot` sends one request through a router without a socket.
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::mcp::testing::{app_with, principal, TestApp};
    use crate::store::catalogs;

    /// A key with `admin` on every catalog.
    fn admin() -> Arc<Principal> {
        Arc::new(Principal {
            access: Access::Admin,
            ..(*principal(&[], None)).clone()
        })
    }

    /// The import routes over `test`'s app, with `caller` as the auth
    /// layer's principal (added by hand).
    fn router(test: &TestApp, caller: Arc<Principal>) -> Router {
        routes(&test.app)
            .layer(Extension(caller))
            .with_state(Arc::clone(&test.app))
    }

    /// A valid archive of one document, `index.md`.
    fn tarball() -> Vec<u8> {
        tarball_of(&[("index.md", "# Index\n")])
    }

    /// A response's status and its body as JSON.
    async fn status_and_json(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// `pg`'s generation and live document count in the database.
    async fn stored(test: &TestApp) -> (i64, i64) {
        let catalog = catalogs::find(test.app.accounts().executor(), "pg")
            .await
            .unwrap();
        let generation = catalogs::test_generation(test.app.accounts().executor(), "pg").await;
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM catalog.documents WHERE catalog_id = $1 AND deleted_at IS NULL",
        )
        .bind(catalog.id)
        .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();
        (generation, live)
    }

    /// An import of `size` zero bytes into `pg` by `caller`, through the
    /// routes alone (the auth layer's principal added by hand), with a 1 KiB
    /// `max_import_bytes`.
    async fn import_status(caller: Arc<Principal>, size: usize) -> StatusCode {
        import_status_into("pg", caller, size).await
    }

    /// [`import_status`] into `catalog`, on an app holding `pg` and `other`.
    async fn import_status_into(catalog: &str, caller: Arc<Principal>, size: usize) -> StatusCode {
        let config = Config {
            max_import_bytes: bytesize::ByteSize::b(1024),
            ..Config::default()
        };
        let test = app_with(&[("pg", &[]), ("other", &[])], config).await;
        let request = Request::post(format!("/catalogs/{catalog}/import"))
            .body(Body::from(vec![0u8; size]))
            .expect("request");
        router(&test, caller)
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn an_upload_timeout_is_503_with_the_error_json() {
        let response = upload_timed_out();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "Timeout");
        assert_eq!(
            body["message"],
            "the upload was not received and checked within 120 s; nothing was imported"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_key_is_checked_before_the_body_is_read() {
        use crate::mcp::testing::principal_as;
        let reader = principal(&[], None);
        let writer = principal_as("writer", Access::Write, &["pg"], None);
        // A writer granted only `other`: `pg` is outside its reach.
        let stranger = principal_as("stranger", Access::Write, &["other"], None);
        let oversized = body_limit(1024) + 1;

        let as_reader = import_status(reader, oversized).await;
        let as_stranger = import_status(Arc::clone(&stranger), oversized).await;
        // A named-grant key, for which an unknown catalog is outside reach.
        // An every-catalog key reaches every name, so it gets past the check
        // and is refused 404 by the import itself, as before this change;
        // it may write any catalog anyway, so the body it could make the
        // server read is no larger than for a real import.
        let unknown = import_status_into("nope", Arc::clone(&stranger), oversized).await;
        let as_writer = import_status(writer, oversized).await;
        let as_admin = import_status(admin(), oversized).await;

        // A body read first would have made every one of them 413.
        assert_eq!(as_reader, StatusCode::FORBIDDEN, "read-only");
        assert_eq!(as_stranger, StatusCode::NOT_FOUND, "outside the grants");
        assert_eq!(unknown, StatusCode::NOT_FOUND, "no such catalog");
        // Past the check, the body is read and its size refused.
        assert_eq!(as_writer, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(as_admin, StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// Issue #260: `write` on the catalog is enough to import, and the
    /// import is read at once; a read-only key's valid archive is refused
    /// and changes nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_writer_imports_and_a_reader_changes_nothing() {
        use crate::mcp::testing::principal_as;
        let test = app_with(&[("pg", &[("index.md", "# Index\n")])], Config::default()).await;
        let reader = principal_as("reader", Access::Read, &["pg"], None);
        let writer = principal_as("writer", Access::Write, &["pg"], None);
        let import = || {
            Request::post("/catalogs/pg/import?replace=true")
                .body(Body::from(tarball_of(&[
                    ("index.md", "# Index\n\nImported.\n"),
                    ("a.md", "# A\n"),
                ])))
                .unwrap()
        };
        let before = stored(&test).await;

        let refused = status_and_json(router(&test, reader).oneshot(import()).await.unwrap()).await;
        let after_refusal = stored(&test).await;
        let imported =
            status_and_json(router(&test, writer).oneshot(import()).await.unwrap()).await;
        let after_import = stored(&test).await;

        assert_eq!(refused.0, StatusCode::FORBIDDEN, "{}", refused.1);
        assert_eq!(refused.1["error"], "Forbidden");
        assert_eq!(after_refusal, before, "a refused import writes nothing");
        assert_eq!(imported.0, StatusCode::OK, "{}", imported.1);
        assert_eq!(imported.1["added"], serde_json::json!(["a.md"]));
        assert_eq!(imported.1["updated"], serde_json::json!(["index.md"]));
        assert_eq!(after_import, (before.0 + 1, 2));
        // The route swapped the snapshot in before answering.
        let current = test.app.catalog("pg").await.unwrap().expect("pg").current();
        assert_eq!(current.generation(), before.0 + 1);
    }

    // Paused time (after the database is up, which needs real time): with
    // nothing else to do, the runtime jumps the clock to the next timer, so
    // the 120-second deadline passes at once. A paused clock needs the
    // single-threaded runtime, `#[tokio::test]`'s default.
    #[tokio::test]
    async fn a_slow_upload_is_503_and_imports_nothing() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let before = stored(&test).await;
        let files_before = test
            .app
            .catalog("pg")
            .await
            .unwrap()
            .unwrap()
            .current()
            .files();
        let archive = tarball();
        let (head, tail) = archive.split_at(archive.len() / 2);
        // An in-memory pipe: the request body reads `reader` as `writer`
        // sends, so the upload arrives as slowly as the writer makes it.
        let (mut writer, reader) = tokio::io::duplex(archive.len());
        writer.write_all(head).await.unwrap();
        // `tail` is copied into the task (`to_vec`): a spawned task must own
        // what it uses (`'static`), as it may outlive this function.
        let tail = tail.to_vec();
        // The catalog's row is read (and cached) before time is paused: a
        // pooled query under a paused clock would time out at once.
        test.app.directory().resolve("pg").await.unwrap();
        tokio::time::pause();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(REQUEST_TIMEOUT + Duration::from_secs(1)).await;
            writer.write_all(&tail).await
        });
        let body = Body::from_stream(ReaderStream::new(reader));
        let request = Request::post("/catalogs/pg/import")
            .body(body)
            .expect("request");

        let response = router(&test, admin()).oneshot(request).await.unwrap();
        let (status, json) = status_and_json(response).await;
        // The rest of the upload, a second after the deadline, finds the
        // body dropped: the route stopped reading it.
        let late = sender.await.unwrap();
        tokio::time::resume();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["error"], "Timeout");
        assert!(late.is_err(), "the late bytes were read: {late:?}");
        assert_eq!(stored(&test).await, before);
        assert_eq!(
            test.app
                .catalog("pg")
                .await
                .unwrap()
                .unwrap()
                .current()
                .files(),
            files_before
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_failing_after_a_committed_import_still_reports_the_import() {
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let state = test.app.catalog("pg").await.unwrap().unwrap();
        let loaded_at = state.current().generation();
        // The fault: the import's generation bump (its last write, in its
        // transaction) also stores a row no workspace accepts, so the import
        // commits and the refresh after it fails to build the snapshot.
        sqlx::raw_sql(
            "CREATE FUNCTION break_the_refresh() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
                 INSERT INTO catalog.documents (catalog_id, path, content, executable, revision, \
                                        updated_at, updated_by) \
                 VALUES (NEW.id, '../escape.md', '', false, 1, now(), 'test'); \
                 RETURN NULL; \
             END $$; \
             CREATE TRIGGER break_the_refresh AFTER UPDATE OF generation ON catalog.catalog_state \
                 FOR EACH ROW EXECUTE FUNCTION break_the_refresh();",
        )
        .execute(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();
        let import = || {
            let request = Request::post("/catalogs/pg/import")
                .body(Body::from(tarball()))
                .expect("request");
            router(&test, admin()).oneshot(request)
        };

        let (status, report) = status_and_json(import().await.unwrap()).await;
        let after_import = stored(&test).await;
        let snapshot_after_import = state.current().generation();
        // A retry of the same upload: the catalog is no longer empty.
        let (retry_status, retry) = status_and_json(import().await.unwrap()).await;
        // Repair the data by hand; the next use reads the catalog's row
        // (here marked stale, as the import's change message would) and
        // retries the refresh.
        sqlx::raw_sql(
            "DROP TRIGGER break_the_refresh ON catalog.catalog_state; \
             DELETE FROM catalog.documents WHERE path = '../escape.md';",
        )
        .execute(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();
        test.app.catch_up_catalogs().await;
        test.app.catalog("pg").await.unwrap().expect("pg");

        // Reported as the import it was, not as a failure.
        assert_eq!(status, StatusCode::OK);
        assert_eq!(report["added"], serde_json::json!(["index.md"]));
        // Committed: one generation on, with the imported document and the
        // trigger's row live; the snapshot was not swapped in.
        assert_eq!(after_import, (loaded_at + 1, 2));
        assert_eq!(snapshot_after_import, loaded_at);
        assert_eq!(retry_status, StatusCode::CONFLICT);
        assert_eq!(
            retry["message"],
            "catalog \"pg\" is not empty; add ?replace=true to replace its contents"
        );
        // The next use swapped the import in, in place.
        let current = state.current();
        assert_eq!(current.generation(), loaded_at + 1);
        assert_eq!(current.files(), 1);
    }

    /// A catalog remembered as broken at its generation, repaired by a
    /// replacing import through this route, is readable on this replica at
    /// once: no listener runs in a unit test, and the route's router here
    /// has no other layer, so only the route's own write can tell the
    /// directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_broken_catalog_repaired_by_an_import_is_readable_at_once() {
        use crate::okf::workspace::Workspace as _;
        let test = app_with(&[("pg", &[])], Config::default()).await;
        let id = test.app.catalog_row("pg").await.unwrap().expect("pg").id;
        // A row no workspace accepts, at a new generation.
        sqlx::query(
            "INSERT INTO catalog.documents (catalog_id, path, content, executable, revision, \
                                    updated_at, updated_by) \
             VALUES ($1, '../escape.md', '', false, 1, now(), 'test')",
        )
        .bind(id)
        .execute(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();
        sqlx::query("UPDATE catalog.catalog_state SET generation = generation + 1 WHERE id = $1")
            .bind(id)
            .execute(&crate::db::store_pool(test.app.accounts().executor()))
            .await
            .unwrap();
        test.app.catch_up_catalogs().await;
        // Its cold load fails and is remembered as broken at that generation.
        assert!(test.app.catalog("pg").await.unwrap().is_none());

        let request = Request::post("/catalogs/pg/import?replace=true")
            .body(Body::from(tarball()))
            .expect("request");
        let (status, _) =
            status_and_json(router(&test, admin()).oneshot(request).await.unwrap()).await;
        let state = test.app.catalog("pg").await.unwrap();

        assert_eq!(status, StatusCode::OK);
        let state = state.expect("readable at once");
        assert_eq!(
            state
                .current()
                .workspace()
                .read_to_string("index.md")
                .unwrap(),
            "# Index\n"
        );
    }

    /// An admin's import into `test`'s `pg`, with `query` ("" or "?...").
    async fn import_with(
        test: &TestApp,
        query: &str,
        archive: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::post(format!("/catalogs/pg/import{query}"))
            .body(Body::from(archive))
            .expect("request");
        status_and_json(router(test, admin()).oneshot(request).await.unwrap()).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_machinery_replaces_machinery_and_keeps_the_concepts() {
        let test = app_with(
            &[(
                "pg",
                &[
                    ("index.md", "# Index\n"),
                    ("AGENTS.md", "# Old\n"),
                    ("a.md", "# A\n"),
                ],
            )],
            Config::default(),
        )
        .await;
        let archive = tarball_of(&[("AGENTS.md", "# New\n"), ("a.md", "# Overwritten\n")]);

        let (status, report) =
            import_with(&test, "?replace=true&only_machinery=true", archive).await;
        let catalog_id = crate::db::catalog_id(test.app.accounts().executor(), "pg").await;
        let kept: Vec<u8> = sqlx::query_scalar(
            "SELECT content FROM catalog.documents WHERE path = 'a.md' AND catalog_id = $1",
        )
        .bind(catalog_id)
        .fetch_one(&crate::db::store_pool(test.app.accounts().executor()))
        .await
        .unwrap();

        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["updated"], serde_json::json!(["AGENTS.md"]));
        assert_eq!(report["added"], serde_json::json!([]));
        assert_eq!(report["deleted"], serde_json::json!([]));
        assert_eq!(kept, b"# A\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_import_expecting_another_generation_is_409_and_imports_nothing() {
        let test = app_with(&[("pg", &[("index.md", "# Index\n")])], Config::default()).await;
        let before = stored(&test).await;
        let stale = before.0 + 1;
        let archive = || tarball_of(&[("index.md", "# Changed\n")]);

        let (status, refused) = import_with(
            &test,
            &format!("?replace=true&expect_generation={stale}"),
            archive(),
        )
        .await;
        let after_refusal = stored(&test).await;
        let (current_status, report) = import_with(
            &test,
            &format!("?replace=true&expect_generation={}", before.0),
            archive(),
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(refused["error"], "Refused");
        let message = refused["message"].as_str().unwrap();
        // The generation refusal keeps its own words; only the "not empty"
        // refusal is reworded for HTTP.
        assert!(
            message.contains(&format!("generation {}, not {stale}", before.0)),
            "{message}"
        );
        assert!(message.contains("export it again"), "{message}");
        assert_eq!(after_refusal, before);
        assert_eq!(current_status, StatusCode::OK);
        assert_eq!(report["updated"], serde_json::json!(["index.md"]));
    }

    /// M13 Task 3: export stays open to every credential that reaches a
    /// suspended catalog, and only an admin (exempt from suspension) imports
    /// into it: a writer, who may import an active catalog (issue #260), is
    /// refused as `Suspended`, and a reader as `Forbidden` (the level check
    /// comes first); a key that does not reach it gets 404.
    #[tokio::test(flavor = "multi_thread")]
    async fn export_stays_open_and_only_an_admin_imports_while_suspended() {
        use crate::mcp::testing::{cached_principal, stored_secret, suspend_catalogs};
        let test = app_with(
            &[("pg", &[("index.md", "# Index\n")]), ("other", &[])],
            Config::default(),
        )
        .await;
        let reader = stored_secret(&test, "reader", Access::Read, &["pg"], false).await;
        let writer = stored_secret(&test, "writer", Access::Write, &["pg"], false).await;
        let stranger = stored_secret(&test, "stranger", Access::Read, &["other"], false).await;
        let root = stored_secret(&test, "root", Access::Admin, &[], false).await;
        suspend_catalogs(&test, &["pg"]).await;
        // Each request goes through the routes with the principal the key
        // cache holds now, as the auth layer would add it.
        let call = async |secret: &str, request: Request<Body>| {
            let caller = cached_principal(&test, secret).await;
            router(&test, caller).oneshot(request).await
        };
        let export = || {
            Request::get("/catalogs/pg/export")
                .body(Body::empty())
                .unwrap()
        };
        let import = || {
            Request::post("/catalogs/pg/import?replace=true")
                .body(Body::from(tarball_of(&[("index.md", "# Imported\n")])))
                .unwrap()
        };

        let exported = call(&reader, export()).await.unwrap();
        let hidden = status_and_json(call(&stranger, export()).await.unwrap()).await;
        let refused = status_and_json(call(&writer, import()).await.unwrap()).await;
        let below = status_and_json(call(&reader, import()).await.unwrap()).await;
        let imported = status_and_json(call(&root, import()).await.unwrap()).await;

        assert_eq!(
            exported.status(),
            StatusCode::OK,
            "a suspended catalog still exports"
        );
        assert_eq!(hidden.0, StatusCode::NOT_FOUND, "{}", hidden.1);
        assert_eq!(hidden.1["error"], "CatalogNotSelected");
        assert_eq!(refused.0, StatusCode::FORBIDDEN, "{}", refused.1);
        assert_eq!(refused.1["error"], "Suspended", "a writer is not exempt");
        assert_eq!(below.0, StatusCode::FORBIDDEN, "{}", below.1);
        assert_eq!(below.1["error"], "Forbidden", "the level check comes first");
        assert_eq!(
            imported.0,
            StatusCode::OK,
            "an admin is exempt: {}",
            imported.1
        );
    }

    #[test]
    fn the_body_limit_leaves_room_for_gzip_overhead() {
        let zero = body_limit(0);
        let default = body_limit(64 << 20);
        let huge = body_limit(u64::MAX);

        assert_eq!(zero, 64 * 1024);
        assert_eq!(default, (64 << 20) + (64 << 20) / 1000 + 64 * 1024);
        assert_eq!(huge, usize::MAX);
    }

    /// A value that must never appear in a span or a metric.
    const SENTINEL: &str = "zq-sentinel-9e3a";

    /// The body of a response as bytes.
    async fn body_of(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body")
            .to_vec()
    }

    /// The `Content-Disposition` of `pg`'s export after its display name is
    /// set to `display_name`, once the key cache has reloaded.
    async fn export_disposition(display_name: &str) -> String {
        let test = crate::mcp::testing::app_with(&[("pg", &[])], Config::default()).await;
        let name = crate::labels::DisplayName::parse(display_name).unwrap();
        let change = catalogs::DetailsChange {
            catalog: "pg",
            display_name: Some(&name),
            description: None,
            owner: None,
        };
        catalogs::update_details(
            test.app.accounts().executor(),
            &change,
            &AuditActor::cli(),
            test.app.clock().now(),
        )
        .await
        .unwrap();
        let exported = router(&test, admin())
            .oneshot(
                Request::get("/catalogs/pg/export")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(exported.status(), StatusCode::OK);
        exported.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_export_filename_carries_the_label_stem_and_the_slug() {
        assert_eq!(
            export_disposition("Work notes").await,
            "attachment; filename=\"work-notes-pg.tar.gz\""
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_label_with_no_ascii_letters_leaves_the_slug_alone() {
        assert_eq!(
            export_disposition("\u{30ce}\u{30fc}\u{30c8}").await,
            "attachment; filename=\"pg.tar.gz\""
        );
    }

    #[tokio::test]
    async fn archive_export_and_import_are_measured() {
        let (test, registry) =
            crate::mcp::testing::app_with_metrics(&[("pg", &[])], Config::default()).await;
        let exported = router(&test, admin())
            .oneshot(
                Request::get("/catalogs/pg/export")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(exported.status(), StatusCode::OK);
        let exported_len = body_of(exported).await.len();
        let upload = tarball();
        let imported = router(&test, admin())
            .oneshot(
                Request::post("/catalogs/pg/import")
                    .body(Body::from(upload.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(imported.status(), StatusCode::OK);

        let text = crate::mcp::testing::render(&registry);

        let sample = |series: &str| crate::mcp::testing::sample_of(&text, series);
        assert_eq!(
            sample(r#"okf_archive_size_bytes_total{direction="export"}"#),
            Some(exported_len as f64),
            "{text}"
        );
        assert_eq!(
            sample(r#"okf_archive_size_bytes_total{direction="import"}"#),
            Some(upload.len() as f64),
            "{text}"
        );
        for direction in ["export", "import"] {
            let series = format!(
                r#"okf_archive_duration_seconds_count{{direction="{direction}",outcome="ok"}}"#
            );
            assert_eq!(sample(&series), Some(1.0), "{direction}: {text}");
        }
    }

    #[tokio::test]
    async fn an_unknown_catalog_leaves_no_span_attribute_and_no_series() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};
        let (test, registry) =
            crate::mcp::testing::app_with_metrics(&[("pg", &[])], Config::default()).await;
        for filter in ["info", "trace"] {
            let observer = Observer::start("off", filter, "off", LogFormat::Text);
            let export = router(&test, admin())
                .oneshot(
                    Request::get(format!("/catalogs/{SENTINEL}/export"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let import = router(&test, admin())
                .oneshot(
                    Request::post(format!("/catalogs/{SENTINEL}/import"))
                        .body(Body::from(tarball()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let known = router(&test, admin())
                .oneshot(
                    Request::get("/catalogs/pg/export")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let observed = observer.finish();

            assert_eq!(export.status(), StatusCode::NOT_FOUND, "{filter}");
            assert_eq!(import.status(), StatusCode::NOT_FOUND, "{filter}");
            assert_eq!(known.status(), StatusCode::OK, "{filter}");
            // Each route's span for the unknown name has no catalog at all.
            for name in ["okf.archive.export", "okf.archive.import"] {
                let spans = observed.spans_named(name);
                let unnamed = spans
                    .iter()
                    .filter(|span| attribute(span, "okf.catalog").is_none())
                    .count();
                assert_eq!(unnamed, 1, "{filter} {name}: {spans:?}");
            }
            // The known catalog's export span names it and carries the size.
            let named: Vec<_> = observed
                .spans_named("okf.archive.export")
                .into_iter()
                .filter(|span| attribute(span, "okf.catalog").is_some())
                .collect();
            assert_eq!(named.len(), 1, "{filter}");
            assert_eq!(
                attribute(named[0], "okf.catalog")
                    .map(ToString::to_string)
                    .as_deref(),
                Some("pg"),
                "{filter}"
            );
            assert!(
                attribute(named[0], "okf.archive.size").is_some(),
                "{filter}"
            );
            assert!(!observed.exported_text().contains(SENTINEL), "{filter}");
        }
        let text = crate::mcp::testing::render(&registry);
        assert!(!text.contains(SENTINEL), "{text}");
    }

    #[tokio::test]
    async fn a_refused_import_of_an_admitted_catalog_is_an_error_span_and_adds_no_bytes() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};
        let (test, registry) =
            crate::mcp::testing::app_with_metrics(&[("pg", &[])], Config::default()).await;
        for filter in ["info", "trace"] {
            let observer = Observer::start("off", filter, "off", LogFormat::Text);
            // A generation that is not the catalog's: 409 after admission.
            let refused = router(&test, admin())
                .oneshot(
                    Request::post("/catalogs/pg/import?replace=true&expect_generation=999")
                        .body(Body::from(tarball()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let succeeded = router(&test, admin())
                .oneshot(
                    Request::get("/catalogs/pg/export")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let observed = observer.finish();

            assert_eq!(refused.status(), StatusCode::CONFLICT, "{filter}");
            assert_eq!(succeeded.status(), StatusCode::OK, "{filter}");
            let imports = observed.spans_named("okf.archive.import");
            assert_eq!(imports.len(), 1, "{filter}");
            let outcome = |span| attribute(span, "okf.outcome").map(ToString::to_string);
            assert_eq!(outcome(imports[0]).as_deref(), Some("error"), "{filter}");
            assert!(
                matches!(
                    imports[0].status,
                    opentelemetry::trace::Status::Error { .. }
                ),
                "{filter}"
            );
            assert_eq!(
                attribute(imports[0], "okf.catalog")
                    .map(ToString::to_string)
                    .as_deref(),
                Some("pg")
            );
            assert!(
                attribute(imports[0], "okf.archive.size").is_none(),
                "{filter}"
            );
            let exports = observed.spans_named("okf.archive.export");
            assert_eq!(outcome(exports[0]).as_deref(), Some("ok"), "{filter}");
            assert!(
                !matches!(
                    exports[0].status,
                    opentelemetry::trace::Status::Error { .. }
                ),
                "{filter}"
            );
        }
        let text = crate::mcp::testing::render(&registry);
        let sample = |series: &str| crate::mcp::testing::sample_of(&text, series);
        // One refused import per filter: errors counted, no bytes added.
        assert_eq!(
            sample(r#"okf_archive_duration_seconds_count{direction="import",outcome="error"}"#),
            Some(2.0),
            "{text}"
        );
        assert!(
            !text.contains(r#"okf_archive_size_bytes_total{direction="import"}"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_export_past_the_route_timeout_is_one_error_observation_and_an_error_span() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};
        let (test, registry) =
            crate::mcp::testing::app_with_metrics(&[("pg", &[])], Config::default()).await;
        let pool = test.app.accounts().executor().clone();
        // The catalog's directory row is cached first, as for any catalog in
        // use: the wait below is the export's own.
        test.app.directory().resolve("pg").await.unwrap();
        // Hold every connection, so the export waits for one until the
        // route's deadline drops it.
        let mut held = Vec::new();
        for _ in 0..pool.options().get_max_connections() {
            held.push(pool.acquire().await.expect("a connection"));
        }
        let slow = Router::new()
            .route(
                "/catalogs/{name}/export",
                export_route_within(Duration::from_millis(300)),
            )
            .layer(Extension(admin()))
            .with_state(Arc::clone(&test.app));
        let observer = Observer::start("off", "info", "off", LogFormat::Text);

        let response = tokio::time::timeout(
            Duration::from_secs(20),
            slow.oneshot(
                Request::get("/catalogs/pg/export")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("the route answers at its deadline")
        .unwrap();
        let observed = observer.finish();
        drop(held);

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let spans = observed.spans_named("okf.archive.export");
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(
            attribute(spans[0], "okf.outcome")
                .map(ToString::to_string)
                .as_deref(),
            Some("error")
        );
        assert!(
            matches!(spans[0].status, opentelemetry::trace::Status::Error { .. }),
            "{:?}",
            spans[0].status
        );
        let text = crate::mcp::testing::render(&registry);
        assert_eq!(
            crate::mcp::testing::sample_of(
                &text,
                r#"okf_archive_duration_seconds_count{direction="export",outcome="error"}"#
            ),
            Some(1.0),
            "{text}"
        );
        assert!(!text.contains("okf_archive_size_bytes_total"), "{text}");
    }

    #[tokio::test]
    async fn a_refused_request_still_ends_the_span_with_an_error_and_records_no_series() {
        use crate::telemetry::config::LogFormat;
        use crate::telemetry::subscriber::capture::{attribute, Observer};
        let (test, registry) =
            crate::mcp::testing::app_with_metrics(&[("pg", &[])], Config::default()).await;
        // A key that may read but not write.
        let reader = Arc::new(Principal {
            access: Access::Read,
            ..(*principal(&[], None)).clone()
        });
        for filter in ["info", "trace"] {
            let observer = Observer::start("off", filter, "off", LogFormat::Text);
            let import = router(&test, Arc::clone(&reader))
                .oneshot(
                    Request::post("/catalogs/pg/import")
                        .body(Body::from(tarball()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let observed = observer.finish();

            assert_eq!(import.status(), StatusCode::FORBIDDEN, "{filter}");
            let spans = observed.spans_named("okf.archive.import");
            assert_eq!(spans.len(), 1, "{filter}");
            assert_eq!(
                attribute(spans[0], "okf.outcome")
                    .map(ToString::to_string)
                    .as_deref(),
                Some("error"),
                "{filter}"
            );
            assert!(
                matches!(spans[0].status, opentelemetry::trace::Status::Error { .. }),
                "{filter}"
            );
            assert!(attribute(spans[0], "okf.catalog").is_none(), "{filter}");
        }
        let text = crate::mcp::testing::render(&registry);
        assert!(!text.contains("okf_archive_"), "{text}");
    }
}
