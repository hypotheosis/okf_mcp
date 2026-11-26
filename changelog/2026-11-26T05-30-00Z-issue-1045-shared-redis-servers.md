# Issue 1045: one shared Redis and Valkey cluster per test run

## Added
- `scripts/test-redis.sh` (`up`, `down`, `run`): starts one Redis and one
  one-node Valkey cluster for a run, labelled with the owner pid so the
  stale-container sweep removes them once the run is gone, and passes their
  URLs as `TEST_REDIS_URL` and `TEST_REDIS_CLUSTER_URL`.
- `scripts/test-server-lib.sh`: the owner labels, shared by `test-pg.sh` and
  `test-redis.sh`.
- `TestRedis::dedicated()`: a container of its own for a test that needs the
  whole server.

## Changed
- `just test` runs through `scripts/test-redis.sh run` as well.
- The tests that stopped Redis, restricted or revoked the `default` user, or
  killed every pub/sub connection used to skip themselves on a shared server;
  they now call `dedicated()` and always run. `TestRedis::stop` and
  `start_again` return nothing and panic on a shared server.
- `scripts/test_release.py` stubs `test-redis.sh` beside `test-pg.sh`.

## Notes
- `tests/support/redis.rs` already honoured the two variables and gave every
  `TestRedis` a random key prefix; this issue adds the servers and moves the
  whole-server tests to dedicated containers.
- Without the variables (a plain `cargo test`) every test still starts its own
  container.
