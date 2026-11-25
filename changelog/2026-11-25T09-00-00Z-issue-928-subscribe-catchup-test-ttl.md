# Issue 928: the subscribe catch-up test no longer passes on the TTL backstop

`a_subscriber_catches_up_on_a_missed_publish_when_it_subscribes` used the
default `catalog_ttl`, so B's cached row could expire and show the publish
without the `subscribed()` catch-up in `run`. It now starts its replicas with a
one-hour `catalog_ttl`. The config is shared with the resubscribe-window test
through `never_expiring()` and `pair_with()` in
`src/shared/announce/catalog_tests.rs`. Test-only change.
