# Issue 1207: changeset_by_handle skips stores the call found down

`ToolContext::changeset_by_handle` (`src/mcp/context.rs`) now answers
`store_unavailable` for a store already in the call's down set, without
connecting to it, as `open_in_every_store` does. A call that had found a store
down no longer spends a second connect timeout on it. The set is per call, so
a store that comes back is reached by the next call.

Tests (`src/mcp/unavailable_tests.rs`): a probe-counting test with the
stalled-store proxy, and a recovery test in which a later call reaches the
store again once its address is restored.
