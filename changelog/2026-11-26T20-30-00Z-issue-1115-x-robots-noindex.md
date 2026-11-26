# Issue 1115: X-Robots-Tag noindex on every response

Added one `map_response` layer, outermost on the router in `src/http/mod.rs`
(`no_index`), that sets `X-Robots-Tag: noindex, nofollow` on every response
whatever its status. Test: `every_response_says_noindex_whatever_its_status`
(UI, sign-in, OAuth page and metadata, `/mcp` 401, API 401, redirect, 404, 405).
`docs/operations.md` describes it. The separate `/metrics` listener is not
covered: it is internal and never routed by the ingress.
