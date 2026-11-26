# Issue 792: reads through a changeset show stale entries as publish would write them

## Added
- `Overlay::read_snapshot`, `Overlay::read_lint`, `Overlay::replay_state` and `ReplayState`
  (`src/overlay.rs`): when a changeset has a stale, replayable entry (a `log.md` or index
  listing made on an older base), the overlay also holds a second snapshot with that entry
  redone on the current published text by `replayed_text`, the function publish calls. It is
  built once per overlay build, only when such an entry exists.
- `read_document` result fields `replayed` and `replay_error`, with the meaning they have in
  `pending_changes`; both are left out when they do not apply.

## Changed
- Read tools (`read_document`, `get_concept`, `search`, reports) reach the overlay through
  `View`, which now serves the replayed snapshot. Writes and publish's lint step keep
  `Overlay::snapshot` and `Overlay::lint`, the stored entries, unchanged.
- `cannot_replay` is `pub(crate)` so the read view and `pending_changes` word the reason alike.

## Notes
- The overlay cache key (published generation, changeset version) already covers the replayed
  view: it depends on the published bytes, and any publish bumps the generation.
- A replay whose text cannot be read back falls back to the stored text with a `replay_error`.
