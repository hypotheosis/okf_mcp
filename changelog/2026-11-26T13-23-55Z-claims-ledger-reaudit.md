# Claims ledger re-audit at b342157

`claims.json` had drifted after the M21 and burndown merges. `recheck` found 1,639 of 2,470
anchors not current on 1,659 active claims (963 only moved; 636 changed, 15 missing, 7
overturned, 18 unpinned). The migrations had moved to `migrations/accounts/` and
`migrations/catalog/` in 5cb1292.

## What was done

- Re-audited every claim with a changed, missing, overturned or unpinned anchor, plus the stale
  and dangling ones: 461 claims in 8 batches. The auditors were Sonnet subagents and are
  independent of the code's authors. Each claim got fresh evidence pinned to b342157 and a new
  verdict. Test evidence comes from three runs at b342157:
  - one full `tools/agentic/gate.sh test` run (3,659 tests in each store mode), cited by most
    test evidence;
  - one `node --test tests/read_worker/*.test.mjs` run by an auditor (115 passed, 0 failed),
    cited by the reader claims, for example EVD-3004 and EVD-3008 on CLM-0075, and CLM-1413;
  - one `bash tools/agentic/tests/run.sh` run by an auditor (109 passed, 0 failed), cited by the
    watchdog, lib.sh and gate.sh claims, for example EVD-3240 to EVD-3250 on CLM-0696, CLM-0697,
    CLM-0703, CLM-1356, CLM-1360 and CLM-1361. The gate does not run the shell suite.
- Retired 51 claims that described a past moment rather than the product: one PR's diff or run,
  interim task shims later removed by design (for example `refuse_host_stores_until_task_20`),
  plan-document edits, and a `.pyc` anchor. Where the lasting intent still mattered, a
  replacement claim states current behaviour. There are 19 replacements, CLM-1749 to CLM-1767,
  all supported.
- Re-pointed source anchors whose text had only moved. Relinked the claims whose parent had been
  retired, so `dangling_links` is now empty.
- Rolled up the parents: four become refuted because a child is now refuted (CLM-0432,
  CLM-0924, CLM-0985, CLM-1606). CLM-1548 stays `not_enough_info` because its 50 ms scale bound
  is measured only by the ignored `tests/scale.rs`.

## Result

`claims-mcp verify` passes (7,698 records). There are 1,627 active claims: 1,521 supported, 33
refuted, 60 not_enough_info and 13 unverified. The stale list is empty. Every anchored evidence item
that a verdict recorded in this pass cites is pinned to b342157. The rest are unanchored logs or
child roll-ups. Older evidence items stay in the record, since
history is append-only, so `recheck` still lists them.

## New findings (refuted, owner to decide)

- CLM-0937: `stores::add` holds an account transaction while the store connects
  (`src/store/stores.rs:317`), against the #968 requirement.
- CLM-0991: `catalogs::recover` commits in a different order from its doc comment.
- CLM-0438: the entitlement lock order differs from the claim's stated sequence.
- CLM-0261 and CLM-0262: docs/operations.md describes the image tag as `:sha-<short>` and lists
  only tag and manual-dispatch triggers. `image.yml` uses the full sha and also builds on pushes
  to main.
- CLM-0279: docs/operations.md says a replica "loaded its keys and settings"; only settings are
  loaded.
- CLM-0020: docs/operations.md says six histograms; the code has eight.

The UI end-to-end flake seen in the gate run is filed as #1101.
