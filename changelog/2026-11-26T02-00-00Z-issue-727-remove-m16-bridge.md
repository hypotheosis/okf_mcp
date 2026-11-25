# Issue 727: remove the M16 bridge and the deprecated settings

Release 0.23 carried a bridge so milestone 16 replicas (which poll
`admin_generation`) and milestone 18 replicas (which use the change feed)
agreed during a rolling deploy. Nothing older than 0.23 is supported any
more, so the bridge and the three settings it left behind are removed.

## Removed

- `App::bridge_to_m16` and its 29 call sites in the HTTP handlers and the
  OAuth consent step.
- `App::apply_m16_announcement`, `announce::{BRIDGE_PAYLOAD, publish_bridge,
  from_an_m16_replica, listen, AdminChanges, CHANNEL}` and the serve task
  that ran `listen`. The `admin-generation` Redis channel no longer exists.
- `announce_from_cli`'s `admin` argument. The command line announces only
  the catalog generations it committed (imports, `catalog repair-indexes`);
  a key, user, team or settings command publishes nothing.
- The settings `poll_interval`, `overlay_cache_size` and `mail.worker`, and
  `Config::deprecation_warnings` with its call in `serve` (nothing else was
  deprecated).
- The bridge tests: the `tests/multi_replica.rs` section "The M16 bridge",
  the `catch_up_tests` module in `shared/announce.rs` (the loop's reconnect
  and grace behaviour stays covered by `log_tests` and `catalog_tests`), and
  the CLI tests that expected a bridge message.

## Kept: the `admin_generation` writes

Every `admin_generation` bump stays. `App::revalidate_settings` reads the
counter to decide whether to reload the settings (spec Decision 9), and the
M18 invalidation tests assert it. Only the bridge's use of it went away, so
no writer "only fed the bridge".

## Operator-visible changes

- **Release-notes line:** `poll_interval`, `overlay_cache_size` and
  `mail.worker` are removed. A configuration that still sets them is NOT
  refused: neither the top-level `Config` nor `[mail]` uses
  `deny_unknown_fields`, so the keys are ignored silently and the server
  starts. The start-up warning is gone. Delete the keys.
- Upgrade from milestone 16 (0.22 or older) only through 0.23.0 or later: a
  milestone 16 replica no longer hears of administrative changes through
  Redis.
- A test (`removed_settings_are_ignored_as_unknown_keys`) pins the
  unknown-key behaviour. Making these keys an error would mean denying
  unknown keys on the whole configuration, which the design avoids so later
  milestones' keys pass through; that was not done here.

## Docs

`docs/operations.md`: the `{p}admin-generation` channel row, the bridge
paragraphs, the three settings rows and the CLI sentence are removed or
rewritten, and section 8 has a new upgrade entry. The M18 plan's Task 13
carries a dated `Removed (issue #727)` note; its other correction notes are
history and stay.
