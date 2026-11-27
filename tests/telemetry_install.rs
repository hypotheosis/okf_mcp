//! The start-up sequence leaves the configured subscriber, not the
//! start-up fallback, handling events on the thread that ran it.
//!
//! Its own integration-test binary: it sets the process's global
//! subscriber, which a second test in the same process could not.

mod support;

use std::io;
use std::sync::{Arc, Mutex};

use okf_mcp::telemetry::{install_with_writer, start_up};
use tracing_subscriber::fmt::writer::MakeWriter;

/// A writer that keeps what it is given, shared between clones.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture lock")).into_owned()
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(Arc::clone(&self.0))
    }
}

// Multi-thread: the OTLP batch processors spawn their workers on the runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_configured_subscriber_handles_events_after_start_up() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("okf.toml");
    // A Sentry DSN, so OTLP starts with its default signals (traces and logs).
    std::fs::write(
        &path,
        "[telemetry.stderr]\nformat = \"json\"\n[telemetry.otlp]\nsentry_dsn_env = \"DSN\"\n",
    )
    .expect("write config");
    let fallback = Capture::default();
    let configured = Capture::default();
    // Exporters may dial at flush or shutdown: a held, unlistened port refuses.
    let refusing = support::refused::Refused::new();
    let dsn = format!("http://public@{}/42", refusing.address());

    // The same sequence `cli::run` performs: start up under the fallback,
    // then install the configured subscriber.
    let started = start_up(
        Some(&path),
        true,
        &|name| (name == "DSN").then(|| dsn.clone()),
        fallback.clone(),
    )
    .expect("start up");
    install_with_writer(
        &started.config.telemetry,
        started.telemetry.as_ref(),
        None,
        configured.clone(),
    )
    .expect("install");

    // On the same thread, after `install`.
    tracing::warn!(target: "okf_mcp::test", "after install");

    let json = configured.text();
    assert!(
        json.contains("\"message\":\"after install\""),
        "configured got: {json:?}"
    );
    assert!(
        !fallback.text().contains("after install"),
        "the fallback must not see it: {:?}",
        fallback.text()
    );

    // `install` logs which signals are exported: the Sentry default is
    // traces and logs, with no metrics.
    assert!(
        json.contains("\"message\":\"OTLP export is on\"")
            && json.contains("\"signals\":\"traces,logs\""),
        "configured got: {json:?}"
    );

    // A second install is refused, not silently ignored.
    let again = install_with_writer(&started.config.telemetry, None, None, Capture::default());
    assert!(again.is_err());

    support::refused::bounded(
        "a telemetry shutdown",
        started
            .telemetry
            .expect("serve starts telemetry")
            .shutdown(),
    )
    .await;
}
