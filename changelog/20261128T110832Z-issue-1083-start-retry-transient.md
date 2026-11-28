# Container starts retry transient Docker errors more than once

Generated: 2026-11-28T11:08:32Z

The `tls` tests failed in the full test gate on about half of all runs. The
cause is not in `postgres_with_tls`: it asks testcontainers for an ordinary
random host port, as every other Postgres test does, and reads the mapped port
back after the start. The TLS tests start the container with the same shared
`start_with_retry`; Docker's own ephemeral host-port pick ("address already in
use", "failed to program external connectivity") fails when many containers
start at once, and `START_ATTEMPTS = 2` gave a single retry that could hit the
same condition again.

## Changed

- `tests/support/pg.rs`: `start_with_retry` makes up to 4 attempts, pausing
  0.5 s, 1 s and 1.5 s between them, but only when
  `is_transient_start_error` says so: a timeout of some stage, or an error
  containing "address already in use", "port is already allocated" or "failed
  to program external connectivity". Any other error panics on the first
  attempt. Each retried error is printed with `eprintln!` and logged with
  `tracing::warn!`. Every container start through the helper benefits.
- `tests/tls.rs`: `only_transient_docker_start_errors_are_retried` covers the
  classification without Docker.

Behaviour change: an error that is neither a timeout nor a known port refusal
was retried once before and now fails at once.
