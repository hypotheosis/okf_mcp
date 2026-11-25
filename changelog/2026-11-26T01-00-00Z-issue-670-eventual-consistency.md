# Issue #670: document cross-replica read consistency for clients

Decision (issue #670): keep eventual consistency (option a). No behavior change.

- `publish`'s tool description and `docs/reference/tools.md` now say that a
  read on another replica may briefly show the previous revision, and that
  reads through a changeset and `pending_changes` are always current for its
  own edits.
- `docs/operations.md` ("Catalog reads are eventually consistent across
  replicas") tells clients to read through the changeset before publishing or
  retry briefly.
- `tests/mcpjam/README.md` gets a "Reads after a publish" note for scenario
  authors.
