//! The integration tests, built as one test binary.
//!
//! Every former `tests/*.rs` binary (and `tests/ui_e2e/`) is a module here, so the lib and every
//! dependency are linked once instead of 25 times and `tests/support/` is compiled once. Select a
//! former binary with a module filter: `cargo test --test it writes::`.
//!
//! Three tests stay their own `[[test]]` targets in `Cargo.toml`, because they change
//! process-wide state (the global tracing subscriber, the global meter provider, environment
//! variables): `telemetry_install`, `telemetry_sdk_disabled` and `cimd_no_proxy`. nextest runs
//! every test in a process of its own, so they would be safe under `just test`, but plain
//! `cargo test` runs a binary's tests as threads of one process. Run together with
//! `telemetry_privacy` in one process, the three made six of its tests fail (issue #1186), so they
//! keep the isolation of a process each.
#![recursion_limit = "256"]
#[path = "admin_parity.rs"]
mod admin_parity;
#[path = "grafana_dashboard.rs"]
mod grafana_dashboard;
#[path = "investigator.rs"]
mod investigator;
#[path = "investigator_cli.rs"]
mod investigator_cli;
#[path = "investigator_collection.rs"]
mod investigator_collection;
#[path = "maintenance.rs"]
mod maintenance;
#[path = "multi_replica.rs"]
mod multi_replica;
#[path = "parity.rs"]
mod parity;
#[path = "restructure.rs"]
mod restructure;
#[path = "roundtrip.rs"]
mod roundtrip;
#[path = "scale.rs"]
mod scale;
#[path = "server.rs"]
mod server;
#[path = "server_oauth.rs"]
mod server_oauth;
#[path = "skills.rs"]
mod skills;
#[path = "smoke.rs"]
mod smoke;
#[path = "snapshot_weight.rs"]
mod snapshot_weight;
#[path = "stores.rs"]
mod stores;
#[allow(dead_code, unused_imports)]
mod support;
#[path = "telemetry_privacy.rs"]
mod telemetry_privacy;
#[path = "test_database.rs"]
mod test_database;
#[path = "tls.rs"]
mod tls;
#[path = "ui_e2e/main.rs"]
mod ui_e2e;
#[path = "writes.rs"]
mod writes;
