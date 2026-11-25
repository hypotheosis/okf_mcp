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
- Sibling tests: in src/cache/bounded.rs, `entries_expire_after_the_ttl` now
  proves reuse with a long lifetime and expiry with a short one, and
  `an_idle_entry_expires_but_a_used_one_stays` uses an 800 ms idle time with
  eight 150 ms sleeps (1.2 s of reads, longer than one idle lifetime, so it
  still proves reads refresh the idle timer). It still reads the real clock,
  so a scheduler stall of 800 ms or more between reads can fail it; the margin
  is wider than before (400 ms), not unlimited. moka offers no mock clock here.
- Left as is: the 200 ms join delay in
  `a_reader_that_joins_an_overtaken_read_...` (src/cache/directory.rs). It is
  also load-sensitive (too short a delay lets the follower miss the shared
  read), but it has no signal to wait on; a candidate follow-up.
