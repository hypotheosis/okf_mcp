# Issue 1227: the release build overflows rustc's query depth limit

`cargo build --release --locked --bin okf-mcp`, the Dockerfile's build step,
failed with `queries overflow the depth limit` while computing the layout of
`{async fn body of okf_mcp::invalidate::listen()}`. No image had built since
PR #1201, and v0.35.0 was tagged without one. The dev and test profiles never
compute that layout, so no gate saw it. It also depends on the compiler: the
image's rustc 1.99.1 fails, rustc 1.100.0 builds the same source, and
`cargo check --release` passes on both.

- `src/invalidate.rs`: `Feed::apply_payload` boxes the `App::apply` future
  (`Box::pin`). Every future above it (`run`, `listen`) then holds one
  pointer in place of the whole nested state machine, which cuts the layout
  recursion. The cost is one allocation per change-feed notification.
- `tools/agentic/gate.sh`: a new `build-release` gate runs the Dockerfile's
  command under the shared lock and a 40 minute limit, with `cargo +<version>`
  taken from the Dockerfile's `FROM rust:<version>-... AS chef` line, so the
  gate and the image cannot drift apart.
- `tools/agentic/brief-common.md`, `README.md`, `lessons.md`, `AGENTS.md`: the
  gate is listed, with when to run it, and the lesson is recorded.
- `tools/agentic/tests/run.sh`: two tests for the gate (arguments and timeout).
