//! A tracing layer that counts the rows the database returns, for the
//! tests that prove a request reads a bounded number of rows (M18).

use std::future::Future;
use std::sync::Arc;

use tracing_subscriber::layer::SubscriberExt;
// `with_filter` is a method of the `Layer` trait.
use tracing_subscriber::Layer as _;

/// Sums the `rows_returned` of every `sqlx::query` event: how many rows the
/// database sent back to the code under test.
#[derive(Clone, Default)]
pub(crate) struct RowsReturned {
    /// Rows the database returned.
    pub(crate) rows: Arc<std::sync::atomic::AtomicU64>,
    /// Statements that returned them.
    pub(crate) statements: Arc<std::sync::atomic::AtomicU64>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RowsReturned {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Rows(u64);
        impl tracing::field::Visit for Rows {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                if field.name() == "rows_returned" {
                    self.0 = value;
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut rows = Rows(0);
        event.record(&mut rows);
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        self.rows.fetch_add(rows.0, relaxed);
        self.statements.fetch_add(1, relaxed);
    }
}

/// Runs `work` on this thread with a subscriber that counts its rows
/// (issue #736's size tests).
///
/// # Returns
/// `work`'s output, the rows returned and the statements run while it ran
/// (only this thread's events: the work must not hand its queries to
/// another task).
pub(crate) async fn counting<F: Future>(work: F) -> (F::Output, u64, u64) {
    let counter = RowsReturned::default();
    let subscriber = tracing_subscriber::registry().with(
        counter.clone().with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("sqlx::query", tracing::Level::TRACE),
        ),
    );
    let output = {
        let _guard = crate::test_tracing::set_default(subscriber);
        // Callsites other tests on other threads registered earlier, with
        // no subscriber or another one, can hold a cached "never" for
        // `sqlx::query`; recompute every cached answer against this
        // subscriber before the work runs (issues #481, #826).
        tracing::callsite::rebuild_interest_cache();
        work.await
    };
    let relaxed = std::sync::atomic::Ordering::Relaxed;
    (
        output,
        counter.rows.load(relaxed),
        counter.statements.load(relaxed),
    )
}
