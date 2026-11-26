# Property test of Bounded's epoch and fence (issue #744)

## Added
- `src/cache/bounded_property_tests.rs`, a child module of `cache::bounded`:
  a hegel property test that interleaves `get_or_load`, `get_or_revalidate`,
  `invalidate`, `invalidate_where`, `reweigh` and `mark_all_stale` (plus
  cancelled operations) against a model database. Loaders park on gates that
  generated `Release` steps open, and the operation futures are polled by hand,
  so completion order is fully controlled with no timers or threads.
- It asserts after every step that no cached value predates an invalidation
  of its key or a fence move, and that the cache weight stays within budget;
  after a final drain it asserts no epoch is left tracked.
- 1000 cases of up to 60 steps; about 1 second in the unit-test profile.

## Changed
- `src/cache/bounded.rs` declares the new test module (test-only).

## Notes
- Interleavings happen where a loader is parked. The windows inside the cache
  after a loader returns run within one poll, and stay covered by the
  deterministic tests in `bounded.rs`.
- Checked by mutation: removing the fence comparison in `Flight::current` or
  the epoch increment in `invalidate` makes the test fail. Disabling
  `too_heavy` does not, because moka itself refuses an entry above the
  budget; the weight property is largely moka's guarantee.
