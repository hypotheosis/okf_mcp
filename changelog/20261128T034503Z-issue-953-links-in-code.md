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
