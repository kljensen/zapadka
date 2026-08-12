# ADR-0006: Own a native SQL assertion library instead of vendoring pgTAP

Date: 2026-08-11

## Status

Accepted. Supersedes the transport and installation clauses of
[ADR-0004](0004-separate-deployment-verification-from-database-tests.md); the
rest of that record — verification separate from testing, an explicit test
target, a reserved schema — still stands.

## Context

`zapadka test` vendored pgTAP 1.3.4 and installed it into the reserved
`zapadka_test` schema. pgTAP's ~900 assertion functions all `RETURNS TEXT` and
return TAP: a line-oriented text format from 1980s Perl, in which
`ok 1 - orders exists` is the *only* place an assertion's outcome exists.
`add_result` receives the outcome, description, and directive and discards all
of it (`third_party/pgtap/sql/pgtap.sql.in:170`).

Zapadka therefore carried an 894-line parser, 29 unit tests, a fuzz target, and
property tests whose entire job was to undo that encoding. It was a genuine
source of defects: a test description reading `okay`, or a comment reading
`# TODO-list`, could be parsed as a passing assertion. Every one of those bugs
was possible only because a result had been flattened into prose that something
had to reconstruct.

Three designs were considered.

**Hook pgTAP.** Override `add_result`, `skip`, and `diag` to record structured
rows while leaving vendored files byte-identical. Cheapest, and it ends the
false-green class. But it depends on undocumented internals, cannot recover
typed values (`is()` stringifies both operands before any hook point at
`pgtap.sql.in:309`), and leaves TAP generation running underneath. An
architectural cul-de-sac if the goal is a good testing tool rather than a
tolerable one.

**Build a native layer over pgTAP's internal helpers.** Rejected. Public bodies
call 96 distinct `_`-prefixed names — effectively the whole internal library —
so it takes on full public responsibility while depending on *more* undocumented
surface than hooking does. The helpers are not uniformly clean either: `_rexists`
is a reusable boolean primitive, but `_docomp` mixes comparison, row
stringification, `diag()` and `ok()` in one body (`pgtap.sql.in:6913`). It is the
worst of both positions.

**Own the assertions.** More work, and the only design in which no TAP exists.

## Decision

Zapadka will ship its own SQL assertion library in the reserved `zapadka_test`
schema, and will not install pgTAP.

- **Assertions record typed rows.** A temporary table inside the runner-owned
  transaction holds outcome, number, description, and directive as columns with
  constraints. Family-specific detail goes in versioned `jsonb`. Nothing
  constructs, emits, or parses TAP.
- **Assertions return `boolean`**, so a test file remains runnable by hand in
  `psql`. The runner ignores return values entirely and reads the table.
- **pgTAP's public API is preserved** — same names, same argument types — because
  it is a good API that people already know. One capability has one name; there
  is no second, parallel assertion vocabulary.
- **PostgreSQL 18 only.** Zapadka already refuses older servers, so none of
  pgTAP's version-compatibility branching is written.
- **`plan()` and `finish()` are supported but never required.** `1..N` exists so
  a *text* consumer can detect a truncated stream; a runner reading a table
  already knows whether the file completed.
- **The TAP harness facilities are omitted rather than stubbed**: `runtests`,
  `do_tap`, `check_test`, `diag_test_name`, `findfuncs`, `pgtap_version`.
- **pgTAP becomes reference material, not an installed artifact.** It stays
  vendored for attribution and as a conformance oracle: upstream's own test
  suite is what new implementations are checked against.

Scope is roughly 80 assertion names across ten families, chosen to be
proportionate to pgTAP's range rather than to any one project's usage. Overloads
are generated from a checked-in manifest, so the generated SQL is reviewable.

## Consequences

- Zapadka owns a SQL library it must maintain, and correctness now rests on
  conformance testing rather than on pgTAP's years of edge cases. This is the
  real cost of the decision, and it is deliberate.
- Diagnostics can improve beyond pgTAP's. `results_eq` compares typed records
  correctly and then destroys the evidence with `have_rec::text`
  (`pgtap.sql.in:7237`); a native version reports which rows and which *columns*
  differed, with their types.
- Test files stop being constrained to single-column output. `collect_tap`
  required every result row to be exactly one text column, so a test could not
  contain an ordinary two-column `SELECT`. That restriction disappears.
- `tap.rs`, its fuzz target, and its property tests are deleted.
- Test files must not open their own transaction. pgTAP convention is
  `begin; ... rollback;` per file; Zapadka owns the transaction so that no test
  can escape rollback, and files carrying transaction control are rejected.
- Files written for stock pgTAP that use the harness facilities, call
  `_`-prefixed internals, or depend on exact diagnostic prose will not run. This
  is an intended break, reported explicitly rather than shimmed.
- The vendored pgTAP tree's `classification` stops describing an installed
  artifact. The PostgreSQL licence permits the derivative; attribution stays.
