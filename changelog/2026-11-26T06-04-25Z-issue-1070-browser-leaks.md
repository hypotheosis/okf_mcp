# Issue 1070: browser journeys no longer leak profile folders or browsers

## Added
- `tests/ui_e2e/browser_home.rs`: `BrowserHome`, one temporary folder per
  browser holding its profile and a launcher script. The launcher runs
  Chromium under `setpriv --pdeathsig KILL`, so the kernel kills the browser
  when the test process dies before `Drop` runs (a timeout, `kill`, Ctrl-C).
  Dropping the home waits for every process whose command line names its
  profile to exit (killing stragglers after 10 s), then deletes the folder.
- `tests/ui_e2e/leaks.rs`: a journey leaves no folder and no browser behind;
  a launched process dies with its parent; a home waits for, and kills, the
  processes using its profile before it is deleted.

## Changed
- `harness::drive` launches Chromium with the home's profile as
  `--user-data-dir` and its launcher as the executable, instead of letting
  rustwright make `/tmp/rustwright-profile-*` (which came back after
  rustwright deleted it, once a child flushed its HTTP cache index).

## Notes
- A test process killed with SIGKILL still leaves its small
  `okf-ui-browser-*` folder (no `Drop` ran); its browser no longer survives.
- Without a `setpriv` (util-linux) that supports `--pdeathsig` (the launcher probes it first; older versions lack it) the launcher starts the browser as before,
  without the parent-death protection.
- Reproduced before the fix: killing a journey's test binary with SIGKILL left
  its 7 browser processes running; after the fix none remain. A clean run left
  no browser before the fix either (plain `cargo test` and nextest), only the
  profile folders.
