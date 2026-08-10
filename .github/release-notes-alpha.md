# Zapadka alpha

A transactional PostgreSQL 18 migration slice, shipped as a static Linux binary.

This is a **prerelease**. It is deliberately narrow: the goal was not a broad
demo but a small core whose state model, reports, and failure semantics are
trustworthy enough to build on without redesigning.

## Install

Download the binary for your architecture, verify it, and put it on your path.

```sh
sha256sum -c zapadka-x86_64-unknown-linux-musl.sha256
chmod +x zapadka-x86_64-unknown-linux-musl
sudo mv zapadka-x86_64-unknown-linux-musl /usr/local/bin/zapadka
```

The binary is statically linked and has no dynamic dependencies at all — no
libpq, no OpenSSL, no PostgreSQL client installation. Every release is built
inside a pinned container and verified to have no ELF `NEEDED` entries.

## What works

`init`, `new`, `lint`, `status`, `deploy`, `verify`.

- Migrations are a dependency graph with UUIDv7 identities and a deterministic
  topological order.
- Transactional deploys: a migration's SQL and the record that it was applied
  commit together, so a crash leaves both or neither.
- Top-level transaction control in a script is rejected by a PostgreSQL 18
  parser compiled into the binary, before anything runs.
- Editing or deleting a migration that has already been applied is a hard
  error.
- `verify.sql` runs after its migration commits, in a transaction that is always
  rolled back.
- A failed verification stops the run and leaves the committed migration
  applied. Zapadka never reverts automatically.
- Deployments are serialized by a session-scoped advisory lock, and contention
  reports who holds it.
- Every command emits one versioned `ReportV1`; `--output json` writes exactly
  one document to stdout.

## What does not work yet

- **`revert` and `baseline`** are not implemented.
- **`zapadka test`** and the vendored pgTAP runner are not implemented.
- **`transaction = "forbidden"`** — nontransactional migrations such as
  `CREATE INDEX CONCURRENTLY` — is rejected during validation with an
  explanation. The audited recovery workflow it needs ships with the next
  milestone, and shipping the execution mode without that recovery path would be
  worse than not shipping it.
- **aarch64 binaries and an SBOM** come with v1.

## Requirements

PostgreSQL 18 or newer. Zapadka analyses migrations with the PostgreSQL 18
grammar, so it cannot make truthful safety decisions about an older server and
refuses to try rather than guessing.

## Compatibility promises

Within this prerelease series:

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
