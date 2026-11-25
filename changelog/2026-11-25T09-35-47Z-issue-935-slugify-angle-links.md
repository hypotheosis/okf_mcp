# Issue 935: slugify strips angle-bracket link destinations

`SLUG_LINK` ended a link destination at the first `)`, so a heading such as
`## See [x](<a)b.md>)` kept a stray `b.md>)` tail in its slug, while GitHub and
other CommonMark renderers slug it as `see-x`. `SLUG_LINK` now accepts an
angle-bracket destination (with an optional title) before falling back to the
old "up to the first `)`" rule. The angle-destination text comes from one
`angle_destination!()` macro shared with `LINK` (#633), so the two cannot
drift apart again. New `slugify` rows cover `[x](<a)b.md>)`,
`[x](<a b.md> "t")` and the image forms.
