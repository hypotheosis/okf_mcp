# Issue #911: pending_changes says a stale free edit will be refused

## Changed
- `src/mcp/changesets.rs`: `PendingFile` has a new nullable `conflict_note`. A stale entry that is not replayable (a free edit such as `edit_document` or `replace_body` of a path published since) carries a note: publish would refuse the changeset with Conflict until the path is discarded and the edit made again on the current text, and `diff` is the entry's stored text against the text published now, not what publish would write. The tool description, the field docs and `docs/reference/tools.md` say so.
- Fresh entries and stale replayable ones serialize as before, with `conflict_note: null`.

## Why a new field, not `replay_error`
`replay_error` means "publish cannot replay this stale, replayable entry", and its `diff` is then replaced by a one-line note. A free edit has no replay to fail, and its `diff` stays a real diff worth reading, so reusing the field would make both meanings false. A separate nullable field keeps each one honest.

## Why
Agents read a free edit's diff as "publish would revert the newer changes" when publish would in fact refuse with Conflict.

## Added
- Unit tests in `src/mcp/changesets.rs`: a stale free edit carries the note and publish refuses it; a fresh entry, a replayed stale entry and a stale entry whose replay fails carry none.
- The MCPJam `editConflicts` scenario asserts the note before the Conflict.
