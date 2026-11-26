# Issue 758: tell clients to sequence dependent calls

Swarm runs showed models sending dependent calls (create then add_source, edit
then publish, publish then read) in one parallel turn. Calls in flight together
have no defined order, and nothing said so.

## Added
- One sentence in the server `INSTRUCTIONS` and the matching bullet of the tool
  reference: a call that depends on an earlier one must wait for its result.
- `publish` returns an optional `reason` when the changeset held no entries
  (`published: false`), naming the edits-in-the-same-turn case. `published` and
  `revisions` are unchanged; a no-op publish (`published: true`) has no reason.

## Changed
- `docs/reference/tools.md` regenerated.

## Notes
- The optional `NotFound` hint on changeset writes was not added: the error is
  built in the shared path-resolution code, so a hint needs more than one short
  clause and touches several call sites.
- The swarm-wave acceptance criterion cannot be checked locally.
