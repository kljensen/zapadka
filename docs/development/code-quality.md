# Code quality

Everything here runs from one command:

```sh
cargo xtask quality
```

CI runs exactly the same command, so a green local run means a green pipeline.
Use `--fast` to skip the checks that need a full rebuild while iterating.

## Why these checks

Zapadka runs against production databases with migration privileges. A defect
here is not a crashed process; it is a schema change that half-happened, or a
report that says something untrue about what a database contains. The checks
below are chosen for that risk profile rather than for generic tidiness.

| Check | Tool | What it protects against |
|---|---|---|
| Formatting | `cargo fmt` | Diff noise that hides real changes in review |
| Lints | `cargo clippy` | Correctness and clarity defects the compiler allows |
| Tests | `cargo test` | Behaviour contracts regressing silently |
| Documentation | `cargo doc` | Intra-doc links that stopped resolving |
| Unused dependencies | `cargo-machete` | Build time and attack surface bought for nothing |
| Advisories, licences, sources | `cargo-deny` | Known-vulnerable or unexpectedly licensed code |
| Complexity budgets | `rust-code-analysis` | Functions growing past what a reviewer can follow |
| Fixture provenance | `cargo xtask verify-fixtures` | Vendored upstream code being edited in place |

## Tools

```sh
cargo install cargo-deny cargo-machete rust-code-analysis-cli --locked
```

`cargo xtask quality` reports a missing optional tool and continues, so a
partial local setup still gives useful output. CI installs all of them, so
nothing is skipped where it counts.

## The toolchain is pinned

`rust-toolchain.toml` names the exact compiler, and rustup selects it for
anyone working in this checkout. That is deliberate: clippy gains lints between
releases, so a floating `stable` means CI eventually fails on warnings a
developer cannot reproduce locally — and the only way to read them is a CI log.

The pin is the same version the release image ships, so the compiler that
checks the code is the compiler that builds the binary. Bumping it is a commit
that updates `rust-toolchain.toml`, `rust-version` in the root `Cargo.toml`, and
the image digests in `.github/scripts/build-musl.sh` together, with any new
lints fixed in the same change.

## Lint policy

Lints are configured once, in `[workspace.lints]` in the root `Cargo.toml`.
Each crate opts in with `[lints] workspace = true`.

The set is stricter than the Rust default in three ways that matter:

- **`clippy::pedantic`** is on. Most of it is about clarity, and the cost of
  reading a suggestion and rejecting it is low.
- **Numeric casts that can change a value are denied**, not warned:
  `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`. Zapadka
  reports durations, exit codes, version numbers, and lock keys. A silent
  truncation in any of those produces a plausible wrong answer rather than an
  obvious failure. Every conversion is therefore an explicit `try_from` with a
  documented decision about what to do when it does not fit.
- **`unsafe_code` is denied workspace-wide.** Exactly one module opts back in:
  `zapadka-parser::ffi`, the boundary to the vendored C parser. That opt-in is
  the complete list of places where memory safety rests on review.

A handful of lints are allowed with reasons stated inline in `Cargo.toml`.
`missing_errors_doc`, for example, is off because error conditions are
described in prose where they are interesting, and a mandatory `# Errors`
heading on several hundred fallible functions would be filler.

## Complexity budgets

`cargo xtask metrics` reports per-function complexity and fails when a function
exceeds a budget:

| Budget | Limit |
|---|---|
| Cognitive complexity | 15 |
| Function length | 120 source lines |

**Cognitive complexity is enforced; cyclomatic complexity is only reported.**
Cyclomatic complexity counts branches, so it charges the same for a flat
forty-arm `match` mapping error codes to strings as for four levels of nested
conditionals. `ErrorCode::as_str` scores 39 cyclomatic and 0 cognitive, which is
exactly right — there is nothing to hold in your head. Cognitive complexity
charges for nesting and for breaks in linear flow, which is much closer to
"can a reviewer follow this".

Metrics come from Mozilla's `rust-code-analysis`, a different implementation
from clippy's own `cognitive_complexity` lint. Both are configured to the same
threshold, so a function has to satisfy two independent measurements.

Test modules are excluded. A test should be obvious and repetitive, and holding
tests to a production complexity budget pushes people toward clever tests.

Raising a budget is allowed. Do it in a commit that says why, rather than by
adding a suppression to the function that failed.

### Current baseline

```
functions analysed   648
cognitive complexity max 14, p90 2, median 0
```

A median of 0 and a p90 of 2 is the shape to preserve: nearly every function is
straight-line code, and the small number of genuinely branchy ones are where
the domain is genuinely branchy — dependency ordering, lint classification, and
the deploy loop.

## Dependency policy

`deny.toml` states what is allowed into the binary. Two rules are worth calling
out:

- **Permissive licences only.** Zapadka is MIT and statically links everything
  it depends on, so a copyleft dependency would change what downstream users may
  do with the binary. The allow-list is kept to licences actually present, so
  adding a dependency with a new licence is a decision someone makes rather than
  one already made.
- **No PostgreSQL client libraries.** `openssl`, `native-tls`, `pq-sys` and
  friends are explicitly banned. ADR-0005 requires a binary with no dynamic
  database-client dependency, and a transitive dependency is exactly how that
  requirement would erode.

## Fixture provenance

Zapadka compiles a copy of PostgreSQL's parser into its binary and will ship a
copy of pgTAP. Both are safety-relevant: the parser decides whether a migration
may run, and pgTAP decides whether a test passed.

`cargo xtask verify-fixtures` re-hashes every vendored file and fails on a
modified file, a missing file, or an **unrecorded** file — because a file nobody
recorded is a file nobody reviewed. Vendored files are never edited in place.

## The report schema

`docs/report-v1.schema.json` is generated from the Rust model:

```sh
cargo xtask schema          # regenerate
cargo xtask schema --check  # fail if out of date (CI)
```

It cannot drift from the model by hand, but it can drift by someone changing the
model and forgetting to regenerate. `--check` is what catches that.
