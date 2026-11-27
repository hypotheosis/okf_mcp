# The bounded-cache property test asserts mark_all_stale (issue #1084)

`src/cache/bounded_property_tests.rs` generated `MarkAllStale` steps but never
checked their effect, so a `mark_all_stale` that did nothing still passed. The
model now tracks, per key, whether it is owed a refresh: it was resident when
`mark_all_stale` ran and has not been looked up or invalidated since.

* A `Load` or `Revalidate` of an owed key must still be pending after its first
  poll. Loaders and checks are parked, so finishing at once would mean the cache
  served the stale entry without reloading (`get_or_load`) or without calling
  the check (`get_or_revalidate`).
* After every step, no owed key may read as fresh (`peek_fresh`), which also
  catches a later step (a `reweigh`, a finishing older load) that wrongly
  refreshes a stale entry.

Mutation check: with the body of `mark_all_stale` replaced by a no-op, the test
fails ("key 0 is fresh though it was resident at mark_all_stale ..."); restored,
it passes. The mutation is not committed. No production code changed.
