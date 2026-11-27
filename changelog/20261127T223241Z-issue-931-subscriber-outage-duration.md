# Issue 931: the subscriber's recovery log reports the whole outage

The pub/sub subscriber's `pub/sub subscribed` line carried `disconnected_ms` only when the loss
happened inside the subscription that recovered. An outage that outlasted the grace, replaced the
subscription and went through failed subscribes logged the recovery with no duration, which is the
outage an operator most wants measured.

The loss timestamp (`outage_began` in `run`, `src/shared/announce.rs`) now lives next to
`subscribed_before`, outside the per-subscription loop. It is set by the first `Disconnected`, or,
while still unset, by a grace expiry, a stream that ended or a failed subscribe, and cleared by the
next `Subscribed`. The per-subscription `disconnected_at` stays: it times the grace and gates the
"connection lost" warning, so every log field and level is unchanged. The first subscribe of the
process never reports `disconnected_ms`, even after failed startup attempts; only a resubscribe does.

Decision: a stream that ends also starts the outage, since it is a loss too.

- Test: `log_tests::subscribe_disconnect_grace_failure_and_end_are_logged` now asserts both
  recoveries on the paused clock: the same-subscription one and the one after a grace expiry plus a
  failed subscribe.
- `docs/operations.md` describes `disconnected_ms` as the outage's length.
