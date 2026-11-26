//! okf_mcp: an MCP server for Open Knowledge Format (OKF) catalogs.
//!
//! The library holds everything except process startup: the OKF document
//! model (`okf`), catalog archives (`archive`), the per-catalog in-memory
//! snapshots (`catalog`) and the per-connection overlays (`overlay`), the
//! database (`db`), API keys (`keys`) and the requests they authenticate
//! (`auth`), which catalogs a call acts on (`selection`), the catalog store
//! (`store`), the database handles that tell the account database from a
//! catalog store (`stores`), the write path every write tool goes through (`write`), the
//! configuration file (`config`), the server's shared state and change-feed handlers
//! (`app`), the bounded caches (`cache`), per-changeset async locks
//! (`keyed_lock`), outgoing mail and its
//! job queue (`mail`), the MCP handler and its tools (`mcp`), the SNTP client (`ntp`),
//! OIDC providers (`oidc`) and the clock check that uses it (`clock_check`), the HTTP router and
//! server (`http`), the OAuth authorization server (`oauth`), the
//! command line (`cli`), errors, and the clock.
//! `main.rs` only starts it.

// The async bodies that `#[instrument]` wraps nest deeply enough that the nightly
// toolchain `cargo dylint` builds with overflows its default query depth (128)
// when it computes the layout of `app::load`'s future.
#![recursion_limit = "256"]

pub mod app;
pub mod archive;
pub mod auth;
pub mod backoff;
pub mod cache;
pub mod catalog;
pub mod cli;
pub mod clock;
pub mod clock_check;
pub mod config;
pub mod db;
pub mod error;
pub mod housekeeping;
pub mod http;
pub mod invalidate;
pub mod investigator;
pub mod keyed_lock;
pub mod keys;
pub mod labels;
pub mod maintenance;
#[cfg(test)]
mod maintenance_contract_tests {
    #[test]
    fn new_id_is_random_hex() {
        let first = crate::maintenance::types::new_id().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(first, crate::maintenance::types::new_id().unwrap());
    }
}
pub mod mail;
pub mod mcp;
pub mod ntp;
pub mod oauth;
pub mod oidc;
pub mod okf;
pub mod overlay;
pub mod render;
#[cfg(test)]
pub(crate) mod rows_returned;
pub mod selection;
pub mod shared;
pub mod store;
pub mod stores;
pub mod telemetry;
#[cfg(test)]
pub(crate) mod test_locks;
#[cfg(test)]
pub(crate) mod test_net;
#[cfg(test)]
pub(crate) mod test_tracing;
pub mod write;
