# ADR-0004: Separate deployment verification from database tests, and own the assertion library

Date: 2026-07-18, revised 2026-08-12

## Status

Accepted. Supersedes the pgTAP-vendoring decision this record originally
carried; the reasoning for that reversal is preserved below rather than in a
separate ADR, so a reader gets one account instead of an archaeology exercise.

## Context

Production-safe checks and development database tests have different trust,
installation, and output requirements. Requiring a test framework on deployment
targets would expand production state and couple migration safety to it.

The original decision was to vendor pgTAP and parse its output. pgTAP's ~900
assertion functions all `RETURNS TEXT` and return TAP: a line-oriented format
from 1980s Perl in which `ok 1 - orders exists` is the *only* place an
assertion's outcome exists. `add_result` receives the outcome, description and
directive and discards all of it.

That required an 894-line parser whose sole job was to undo the encoding, and it
was a genuine source of defects: a description reading `okay`, or a comment
reading `# TODO-list`, could be parsed as a passing assertion. Every one of
those bugs was possible only because a result had been flattened into prose that
something had to reconstruct.

## Decision

**Verification.** Migration-local `verify.sql` is plain PostgreSQL SQL, run
after commit in a read-only transaction that is always rolled back.

**Tests.** `zapadka test` requires an explicitly named target and runs each file
serially in its own rolled-back transaction, on a fresh connection.

**The assertion library is Zapadka's own**, installed into the reserved
`zapadka_test` schema. pgTAP is not installed.

- Assertions record typed rows in temporary tables and return `boolean`. Nothing
  constructs, emits, or parses TAP.
- The API is **pgTAP-inspired and deliberately divergent**. Its names and
  argument types are pgTAP's, because that is a good API people already know,
  and many pgTAP files port unchanged. It is not a compatibility contract:
  where pgTAP is showing its age, this library improves on it and documents the
  difference.
- PostgreSQL 18 only, so none of pgTAP's version-compatibility branching exists.
- `plan()` and `finish()` are supported but never required. `1..N` existed so a
  *text* consumer could detect a truncated stream; a runner reading a table
  already knows. A declared plan is enforced.
- The TAP harness facilities are omitted rather than stubbed: `runtests`,
  `do_tap`, `check_test`, `diag_test_name`, `findfuncs`, `pgtap_version`.
- pgTAP remains vendored as attribution for a derivative work and as a
  reference, not as an installed artifact.

Two cheaper options were considered and rejected. **Hooking pgTAP** —
overriding `add_result`, `skip` and `diag` to record structured rows — would
have ended the parsing defects, but it depends on undocumented internals, cannot
recover typed values (`is()` stringifies both operands before any hook point),
and leaves TAP generation running underneath. **Building over pgTAP's internal
helpers** is worse than either: public bodies call 96 distinct `_`-prefixed
names, so it takes on full public responsibility while depending on more
undocumented surface than hooking does.

## Consequences

- Zapadka owns a SQL library it must maintain. Correctness rests on its own
  tests rather than on pgTAP's years of edge cases. This is the real cost, and
  it is deliberate.
- Diagnostics improve beyond pgTAP's. `results_eq` compares typed records
  correctly and then destroys the evidence with `have_rec::text`; here a failure
  reports which rows and which columns differed, with their types.
- Test files are not constrained to single-column output, as they were when
  every result row had to be a TAP line.
- Files written for stock pgTAP that use the harness facilities, call
  `_`-prefixed internals, or depend on exact diagnostic prose will not run. This
  is an intended break, reported explicitly rather than shimmed.
- Test files must not open their own transaction; the runner owns it so no test
  can escape rollback.
- Deployment targets need no test framework at all.
