//! Browser end-to-end tests of the web console (M9), driven by rustwright
//! against the real server. Each journey skips when no Chromium is found
//! (set RUSTWRIGHT_CHROMIUM); `just test-ui` runs them.

// The journeys' futures hold the whole router, whose middleware stack
// (settings check, own-writes capture, telemetry) nests deeply enough to
// overflow the default query depth (128) when their layout is computed, as
// in the library (`src/lib.rs`).
#![recursion_limit = "256"]

#[path = "../support/mod.rs"]
mod support;

// `oidc_mock` reaches this through `super::`.
use okf_mcp::config::OidcProviderConfig;

mod account;
mod audit;
mod browse;
mod catalogs;
mod changesets;
mod consent;
mod harness;
mod health;
mod help;
mod keys;
mod login;
mod maintenance;
mod nav;
// The library's in-process OIDC provider, compiled in from its own file.
// It names the provider config `super::OidcProviderConfig`, which the
// `use` above provides; the journeys use only part of it.
#[allow(dead_code)]
#[path = "../../src/oidc/testing.rs"]
mod oidc_mock;
mod read;
mod recent;
mod self_service;
mod settings;
mod signin;
mod smoke;
mod teams;
mod unlocks;
mod users;
