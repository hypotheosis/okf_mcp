# Issue 756: cache weights no longer saturate at 4 GiB

## Added
- `bounded::unit_shift` and `units_up`: a byte-limited cache whose budget exceeds `u32::MAX`
  bytes gives moka weights and `max_capacity` in coarser units of `1 << shift` bytes (weights
  rounded up, so a count never understates). A budget that fits a `u32` keeps exact bytes.
- Tests: a value over a budget above 4 GiB is refused, rounding never undercounts, a small
  value still fits, eviction works by bytes. The weights are faked; nothing allocates GiB.

## Changed
- `Bounded::with_weigher`, `cache::catalogs::weight`, `weight_of` and the overlay weight are
  `u64` bytes, so `too_heavy` compares real bytes. The `okf_cache_bytes` gauge still reports
  bytes (moka's weighted size scaled back by the unit).
- The M18 spec and plan no longer say weights saturate at `u32::MAX`.

## Notes
- Chosen over rejecting large budgets in `Config::validate`: every budget accepted today still
  works. No budget can saturate a kept entry: the budget is at most `2^31` units, and an entry
  over the budget is refused by `too_heavy` before moka sees it. No `Config::validate` change.
