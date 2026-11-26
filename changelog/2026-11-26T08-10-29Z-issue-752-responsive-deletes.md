# Issue 752: the console stays responsive during a catalog delete

## Added
- `catalogs.js`: the catalog Delete button and force box are a pending write keyed by the
  catalog (`catalog:<name>:delete`, `core/pending.js`), so a second submit is ignored in the
  drawer it started in and in any drawer drawn again meanwhile. A status note in the drawer
  says that the delete is running, or why it failed.
- `tests/ui_e2e/catalogs.rs`: a delayed-DELETE journey (pending state, no duplicate, navigation
  away and back, another catalog's drawer left alone, success only after the answer) and a
  refused-DELETE journey.
- `src/http/api/catalogs.rs`: a test that DELETE answers while the catalog's documents and
  revisions are still on the store, with the cleanup owed, an unrelated read working and a
  repeat answering 404.

## Changed
- A delete that settles after the user left the Catalogs page and came back now reloads the
  page shown, not the one the delete started on (which was gone, so the list kept the deleted
  catalog).
- `docs/operations.md`: the DELETE row says the answer does not wait for the data to go.

## Notes
- Measured (disposable catalog, 20000 documents, 160000 revisions): DELETE 15 ms, the
  background cleanup 3.2 s, unrelated requests unaffected. The handler does no `settle`/poll.
  The two-step delete of issue 972 already moves the bulk work out of the request, so no
  Apalis job was added.
