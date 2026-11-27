# serve waits for the housekeeping worker: test and a shared probe (issue #1096)

## Added
- `http::tests::serve_returns_only_after_the_housekeeping_worker_stopped`, mirroring the mail test.
- Test-only `app::WorkerProbe` and `WorkerProbes` (one exit hold and stopped flag per worker queue).
- A comment at the supervisor await in `serve_with` saying what it guarantees beyond `worker_pool.close()` (the worker task, and with it its LISTEN connection, has ended).

## Changed
- The #773 hook is no longer mail-specific: `App::mail_worker_exit_hold` and `mail_worker_stopped` are replaced by `App::worker_probes`, and the wrapper moved from `mail::queue::spawn_worker` to one `probe_worker` in `http/mod.rs` that wraps both workers. It is `cfg(test)`; the release build's `probe_worker` returns the task unchanged.
- The mail test and the new one share `assert_serve_waits_for_worker`.

## Notes
- Revert-checked: dropping each supervisor's await in turn made its own test fail; restored, not committed.
