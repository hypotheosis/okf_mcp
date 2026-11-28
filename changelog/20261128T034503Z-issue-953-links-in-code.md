# Links written as code are not links (#953)

Generated: 2026-11-28T03:45:03Z

`find_links` read link syntax inside inline code spans and fenced or indented
code blocks as links, so an example such as `[x](<a b.md>)` in backticks got a
`broken-link` finding, was counted in the link graph and backlinks, and could
be rewritten by relink or unlinked by `delete_concept`.

Code ranges now come from pulldown-cmark's offset iterator (matching backtick
runs, nested fences and indented blocks follow CommonMark), parsed once per
call, in linear time, and skipped altogether for text with no backtick, tilde,
tab or four-space run. A match is dropped when its `[`, its `]` or its
closing `)` is inside code; code wholly inside the link text (`[\`x\`](a.md)`)
is still a link. A leading `---` frontmatter block is read as metadata so its
backticks cannot pair with the body.

Line-based callers (the snapshot's link graph, `rewrite_text` for moves and
`unlink_text` for deletes) use `link_lines`, which takes the code ranges from
the whole text, so a block or span covering several lines hides the links on
each. `slugify` keeps a link written as code literal in a heading. Bullet
helpers for index and log lines still call `find_links` on one line, so they
see inline code only.

Tests: markdown.rs, relink.rs and lint.rs unit tests; an MCPJam scenario
(careful-fact-curator) that publishes a concept with an example link in code.

## Review round 1

`unlink_text` reparsed each shortened line alone on its later passes, so a code
span over a line break lost its protection ("`start\n[a](alpha.md)` [b](alpha.md)"
unlinked both links). It now reparses the whole updated text on each pass.
`delete_concept`'s StillLinked guard (`links_into`) and replay's `listing_line`
use `link_lines`, so a link written as code no longer refuses a delete, and a
bullet inside a fenced block is not a listing. `links_to` is gone;
`into_links_to` replaces it. `leading_link_to` and `relinked_bullet` (the
okf.py mirrors) still read one line.

## Review round 2

A match that started at a `[` inside code was dropped whole, and the scan
resumed after it, so it swallowed a real link after the span
(`` `[` [x](a.md) ``). `find_links`, `link_lines` and `slugify` now run LINK and
SLUG_LINK on a copy of the text with code bytes (not line breaks) blanked to
`_`, same length, so offsets and lines are unchanged. Code can no longer open,
close or swallow a link; link text and target are read from the original. It is
still one parse and one linear regex pass, with no rescans. A `]` or `)` inside
code no longer ends a link early either (`[a `]` b](x.md)` is a link).

## Review round 3

Blanking code to `_` could make a link: `` [x](`a b.md`) `` is no CommonMark
link (a bare destination can't hold a space), so the backticked part is code,
but the masked `[x](________)` matched LINK and gave the target `` `a b.md` ``.
Code is now masked to bytes that never occur in UTF-8 (`0xFF` for a code span,
`0xFE` for a code block), and LINK and SLUG_LINK run as byte regexes that
accept a span byte only in the link text, and either byte only as the char
before the `[`. A destination, a title and the space before one can never hold
a mask byte, so a match's `(...)` is always the original bytes: masking can
hide a link but can never make one `okf.py` would not see. CommonMark reads a
real link's destination as raw text, so `` [x](`a.md`) `` has no code and is
still the link `` `a.md` ``, as in `okf.py`. A code block is now never part of
a link's text. It is still one parse and one linear regex pass.
