# Issue 829: the console browse route carries its section as a parameter

## Changed
- `browse_route` (src/render.rs) writes a section as `?s=<slug>`:
  `#/browse/<catalog>/<path>.md?s=<slug>`. The old trailing `/<slug>`
  segment could not be told from a folder named `x.md`.
- The console router (`core/router.js`) splits the hash at `?`, passes the
  parameters to a page as a third argument, and `browseHash` writes `?s=`.
- `pages/browse.js` `locate` ends a document at the LAST `.md` segment (it
  was the first), and takes the section from `?s=`, else from the segments
  after the document (old-form links already in rendered HTML or bookmarks).
- The reader's `fromBrowse` (src/http/read/js/route.js) reads `?s=` too and
  still reads the old form.

## Added
- Tests: `browse_route` unit tests, `fromBrowse` worker tests, and a console
  journey for a folder named `archive.md`.

## Notes
- Links are produced at render time, so only bookmarks and cached pages
  carry the old form.
- No parity fixture (tests/parity.rs) mentions the browse route.
