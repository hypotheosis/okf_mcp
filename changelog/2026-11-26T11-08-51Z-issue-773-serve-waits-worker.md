# Issue 773: a test that `serve` returns only after the mail worker stopped

## Added
- `http::tests::serve_returns_only_after_the_mail_worker_stopped`. The test holds the mail
  worker's task open (`App::mail_worker_exit_hold`), cancels `serve`, and checks that `serve` is
  still running 3 s later and that the worker's flag is unset. It then lets go, and checks
  that `serve` returns `Ok` and that `App::mail_worker_stopped` is already set when it does,
  read right after `serve` returns with no wait for anything to settle.
- Test-only hooks (`#[cfg(test)]`, nothing in production builds): the two `App` fields above, and
  a wrapper task in `mail::queue::spawn_worker` that waits for the hold and sets the flag as its
  last act. The shape follows `App::access_hold`.

## Notes
- The issue suggested polling `pg_stat_activity` after `serve` returns. That cannot prove the
  ordering. With the supervisor await removed (`.take(0)`) a `pg_stat_activity` check made at the
  moment `serve` returned still passed 3 of 3 runs, because the worker had stopped by then anyway.
  A job in hand did not help either: `worker_pool.close()` waits for the pool's connections, so it
  kept `serve` waiting even without the supervisor await. Holding the worker's task end is the
  only way found to make the supervisor await the only thing `serve` waits on.
- Revert check: with `for supervisor in supervisors.into_iter().flatten().take(0)` the test fails
  with "serve returned while the mail worker's task was held".
