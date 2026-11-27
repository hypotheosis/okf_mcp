# Tests dial a held refusing port, not a fixed dead one (issue #1095)

Tests that connected (or might connect) to `127.0.0.1:1` or `127.0.0.1:9` assumed nothing listens there. Port 9 is `discard`, which some hosts run. Each such site now holds a `test_net::Refused` (a bound, never-listened loopback socket) for the whole call, builds its URL from `refusing.address()`, and wraps the call in `test_net::bounded`.

Integration tests cannot import the `pub(crate)` `test_net`, so `tests/support/refused.rs` adds the same two items (`Refused`, `bounded`) for `tests/`.

Sites changed: `src/mail/feedback.rs` (unreachable feedback endpoint), `src/shared/mod.rs` (three Redis connects), `src/cli.rs` (the unreachable Redis announce), `src/investigator/client_tests.rs` (proxy env vars of the child test), `src/telemetry/mod.rs` (DSN and OTLP endpoint), `tests/telemetry_install.rs` (DSN), `tests/telemetry_privacy.rs` (CIMD client id URL).

Left alone, because nothing dials them: the `connect_lazy` pools in `src/stores/credentials.rs` and `src/stores/mod.rs`, the parsed keeper URL, the endpoint validation cases in `src/investigator/client_tests.rs` (rejected before any connect), the redirect URI in `tests/telemetry_privacy.rs` (sent, never fetched) and `tests/server_oauth.rs` (only compared).
