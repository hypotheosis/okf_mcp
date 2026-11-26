# Issue 769: tests no longer make an "unreachable" address by dropping a port

## Changed
- New test helper `src/test_net.rs`: `Refused` holds a loopback socket that is bound but never
  listening. On Linux a connect to it is refused at once (`ECONNREFUSED`, checked for three
  connects), and the port cannot be handed to another bind while it is held. `bounded` wraps a
  step in `tokio::time::timeout` (30 s) and names what it waited for.
- `TestApp::make_unreachable` (`src/mcp/testing.rs`, used by `mcp::unavailable_tests`) now keeps
  a `Refused` on the app for the test's lifetime.
- Replaced the bind, read the port, drop pattern with `Refused` in: the investigator client's
  init refusal test, the Postmark and SES unreachable cases, the three store-registration tests
  (`store::stores`, `cli::stores` twice, `http::api::stores`), `stores::registry`'s unavailable
  test and the CIMD refused-fetch test in `oauth::clients`. Each call that connects is bounded.
- The investigator HTTPS test starts `openssl s_server -accept 127.0.0.1:0` and reads the port
  from its `ACCEPT <address>` line, instead of reserving a port and dropping it.

## Notes
- `tests/support/redis.rs::free_port` also reserves and drops a port, but for a container's host
  port, and it already retries a failed start with a new port; left as it is.
