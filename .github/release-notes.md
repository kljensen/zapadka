# Zapadka

A PostgreSQL 18 migration and database-testing tool, shipped as a static Linux
binary with no dynamic dependencies at all.

## Install

Download the binary for your architecture, verify it, and put it on your path.

```sh
sha256sum -c zapadka-x86_64-unknown-linux-musl.sha256
chmod +x zapadka-x86_64-unknown-linux-musl
sudo mv zapadka-x86_64-unknown-linux-musl /usr/local/bin/zapadka
```

No libpq, no OpenSSL, no PostgreSQL client installation. Every release is built
inside a container pinned by image digest and verified to have no ELF `NEEDED`
entries, so "static" is proven rather than intended. Each binary ships with its
SHA-256, a CycloneDX SBOM, and a list of the vendored third-party sources
compiled into it.

## What it does

`init`, `new`, `lint`, `status`, `deploy`, `verify`, `revert`, `baseline`,
`test`.

- Migrations are a dependency graph with UUIDv7 identities and a deterministic
  topological order — two people adding migrations on separate branches do not
  collide, and the deploy order does not depend on filenames or clocks.
- **Transactional deploys.** A migration's SQL and the record that it was
  applied commit together, so a crash leaves both or neither.
- **Runner-owned transactions.** A top-level `COMMIT`, `ROLLBACK`, or
  `SAVEPOINT` in a script is rejected by a PostgreSQL 18 parser compiled into
  the binary, before anything connects.
- **Deployed history is immutable.** Editing or deleting an applied migration
  is a hard error, not a warning and not a silent re-run.
- **Verification is separate from testing.** `verify.sql` runs after its
  migration commits, in a read-only transaction that is always rolled back. A
  failed verification stops the run and leaves the committed migration applied;
  Zapadka never reverts automatically.
- **Database tests** run against an explicitly named target with a vendored,
  pinned pgTAP installed into a reserved `zapadka_test` schema — no extension
  and no test framework on your production databases.
- Deployments are serialized by a session-scoped advisory lock, and contention
  reports who holds it.
- Every command emits one versioned `ReportV1`; `--output json` writes exactly
  one document to stdout.

- **Nontransactional migrations** (`transaction = "forbidden"`) run one
  statement outside a transaction, for `CREATE INDEX CONCURRENTLY` and its
  relatives. The attempt is recorded *before* the statement runs, so a run
  killed mid-statement leaves evidence rather than a mystery; the target then
  blocks until an operator records what actually happened with
  `zapadka resolve`. Zapadka never retries such a statement on its own.

## What does not work yet

Deliberately out of scope: Sqitch or pgTAP CLI compatibility,
non-PostgreSQL databases, automatic rollback, declarative schema diffing,
repeatable migrations, callbacks, and multi-project registries.

## Requirements

PostgreSQL 18 or newer. Zapadka analyses migrations with the PostgreSQL 18
grammar, so it cannot make truthful safety decisions about an older server and
refuses to try rather than guessing.

Connect as a role that owns the schema and no more. Zapadka runs verification
read-only so it cannot change committed state, but no transaction rolls back
what a superuser's `COPY ... TO PROGRAM` does outside the database — Zapadka
reports such a role rather than assuming it away.

## Compatibility promises

- `error.code` values and process exit codes are stable. Match on those, never
  on message text.
- `ReportV1` field names are stable; new optional fields may appear, so tolerate
  unknown ones.
- The migration definition hash is stable. If it ever has to change, the
  canonical form is versioned and a registry migration comes with it.

`zapadka.toml` and `migration.toml` both carry a `format_version`. A binary
refuses a format, or a registry, newer than it understands rather than guessing
what the unknown parts mean.

## Reporting problems

Please include the output of `zapadka <command> --output json`. It carries the
run id, the parser version, the exact hashes involved, and the PostgreSQL
`SQLSTATE` — which is usually everything needed to reproduce a problem.
