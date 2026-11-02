# C1 gaps

C1 freezes the leftover denominator. Completeness gaps for C1 are closed:
inventory, status labels, and suite classification are in a terminal honest
state. C1 still does not claim full TiDB stmt coverage or integrationtest
pass rates.

Closed in this door:

- `unmapped-stmts`: implemented PlanBuilder kinds mapped to F*/C2–C5 owners;
  C6-deferred kinds tagged `C6`; remaining misc kinds frozen on
  `UNMAPPED_ALLOWLIST` and verified by leftover `--check`
- `stale-gap-labels`: F* and C1–C5 `gaps: []` in `status.json` match
  leftover inventory; obsolete frozen F* gap wording removed
- `leftover-suite-execution`: every leftover suite has a non-empty owner;
  `execute-leftover-suite-smoke` asserts classification plus C2
  `execute-explain*` / C5 `execute-leftover-collation-suites` MVB coverage

These stay out of the C1 door:

- Full TiDB PlanBuilder coverage beyond closed F*/C2–C5 owners (open stmts
  remain C6 + allowlisted unmapped in `project/features/leftover.md`)
- Full `tests/integrationtest` suite port (MVB / classification only)
- C6 items remain deferred: Cascades, TiFlash/MPP, BR/Lightning, plugins
