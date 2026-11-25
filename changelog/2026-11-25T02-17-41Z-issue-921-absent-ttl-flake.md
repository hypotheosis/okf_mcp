# Issue #921: absent-name expiry test no longer races its lifetime

## Added
Nothing.

## Changed
- `an_absent_name_is_forgotten_after_its_lifetime` (src/cache/directory.rs) no
  longer asserts the name is still absent between the seed and the sleep. That
  assertion needed the seed to finish inside the 100 ms lifetime and failed on
  a loaded machine. The expiry assertion stays.

## Notes
- "Remembered while the lifetime runs" is covered with a long lifetime by
  `an_unknown_name_is_remembered_briefly_and_forgotten_by_create`.
- Sibling tests in the module were checked: the only other real-time wait is a
  200 ms join delay in `a_reader_that_joins_an_overtaken_read_...`, which can
  only fail by being too short, not by a slow machine of the kind seen here.
