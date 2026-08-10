# Zapadka

A static PostgreSQL migration and database-test tool, inspired by Sqitch and pgTAP.

One binary. No Perl, no `psql`, no libpq, no OpenSSL, no PostgreSQL client
installation, and no separately installed test framework.

> **Status: pre-alpha.** Every command below works and is tested against
> PostgreSQL 18. Nontransactional migrations are the significant gap. See
> [Limitations](#limitations).

## What it is

Zapadka deploys **authored SQL migrations** — reviewed source artifacts, not
schema diffs and not a DDL DSL — and records what it did in a registry inside
the database it changed.

```sh
zapadka init
zapadka new add-orders-table
# edit migrations/<id>-add-orders-table/deploy.sql
zapadka lint
zapadka deploy --target production
zapadka status --target production
```

The full command set is `init`, `new`, `lint`, `status`, `deploy`, `verify`,
`revert`, `baseline`, and `test`.

## What makes it different

**Migrations form a graph, not a list.** Each migration has a permanent UUIDv7
identity and declares which migrations must precede it. Two branches can add
migrations independently and converge without renumbering anything. Deployment
order is a deterministic topological sort, so the same graph always produces the
same plan.

**Zapadka owns every transaction boundary.** A migration script cannot `BEGIN`,
`COMMIT`, `ROLLBACK`, or `SAVEPOINT` — a pinned PostgreSQL 18 parser is compiled
into the binary and rejects it before anything runs. That is what makes the
applied state Zapadka records the same as the state the database is in.

**Deployed history is immutable.** Editing a migration that has already been
applied is a hard error, not a warning and not a silent re-run. Corrective work
is a new migration, which leaves both facts in the history.

**Verification is separate from testing.** `verify.sql` is plain,
production-safe SQL that runs after its migration commits, in a fresh
**read-only** transaction that is always rolled back. It observes committed
state and cannot change it. Database tests are a separate command against an
explicit test target.

Read-only as well as rolled back, because rollback alone is not enough:
PostgreSQL does not roll back `nextval()`, so a verification script that touched
a sequence would advance it permanently. The cost is that a read-only
transaction refuses every `CREATE`, including `CREATE TEMP TABLE` — build an
expected set with a CTE or a `VALUES` list instead:

```sql
WITH expected(id) AS (VALUES (1::bigint), (2::bigint))
SELECT 1 / (CASE WHEN (SELECT count(*) FROM app.orders)
                 = (SELECT count(*) FROM expected) THEN 1 ELSE 0 END);
```

**Database tests are isolated, with one documented exception.** Each test file
runs on a fresh connection in a transaction Zapadka always rolls back, so no
file can see another's data. PostgreSQL does not roll back `nextval()`, and
Zapadka will not rewind a sequence — its lock serializes Zapadka runs but not
application connections, so rewinding could hand out a key already issued.
A run that advances a sequence says so; assert on what a row contains rather
than on the id it was given.

**Nothing is reverted automatically.** If verification fails after a migration
committed, Zapadka records that and stops. It does not run unproven revert SQL
against an unexpected schema while nobody is watching.

**One report, two renderings.** Every command produces the same versioned
`ReportV1`. `--output json` writes exactly one document to stdout; human output
is a view over the same value, and never changes shape based on whether stdout
is a terminal.

## Project layout

```text
zapadka.toml
migrations/
  <uuidv7>-<slug>/
    migration.toml
    deploy.sql
    revert.sql      # when reversible
    verify.sql      # optional
tests/db/
  **/*.sql
```

`zapadka.toml` is checked in and holds **no credentials**. A target names where
to find its connection details — a PostgreSQL service entry or an environment
variable — and Zapadka resolves it at run time.

```toml
format_version = 1

[project]
id = "0198f5c0-0000-7000-8000-00000000000a"
registry_schema = "zapadka"

[targets.production]
pg_service = "app-production"

[targets.test]
uri_env = "TEST_DATABASE_URL"
application_schemas = ["app"]

[policy]
advisory_lock_timeout = "5s"
deny = ["lint.index_without_concurrently"]
```

## Safety checks

`zapadka lint` separates two kinds of finding, and the distinction is the
point.

**Errors** are provable invalidity — the script does not parse, it takes the
transaction boundary away from the runner, or PostgreSQL will certainly refuse
it in the declared mode. These always fail; there is no way to accept them,
because accepting them would just move the failure to production.

**Warnings** are intentional operational risks: dropping data, rewriting a
table, taking a lock that blocks writes. Zapadka cannot know whether one
matters — dropping a column is reckless on a hot table and routine on an empty
one — so it reports rather than refuses. A project promotes the ones it cares
about with `policy.deny`; a migration accepts a specific one with an `[[allow]]`
entry that states a reason.

```
warning: migrations/019.../deploy.sql:3: builds an index on app.orders without CONCURRENTLY
  [lint.index_without_concurrently]
  this blocks writes to the table until the index is built; on a table with
  existing rows, build it CONCURRENTLY in its own nontransactional migration
```

## Exit codes

Scripts branch on these; they are a stable contract.

| Code | Meaning |
|---|---|
| 0 | Success |
| 2 | Bad command line |
| 3 | Project, configuration, or filesystem unusable |
| 4 | Migration content, graph, or SQL is invalid |
| 5 | Deployed history and the checked-out project disagree |
| 6 | Another Zapadka run holds the deployment lock |
| 7 | Target unreachable or unsupported |
| 8 | Registry could not be read, created, or upgraded |
| 9 | User SQL failed |
| 70 | A bug in Zapadka |

The `error.code` field in the JSON report carries the specific reason. Match on
that and on the exit code, never on message text.

## Connecting

Zapadka reads PostgreSQL service files itself and speaks TLS with `rustls`.

**It verifies the server's identity whenever it encrypts.** There is no mode
that encrypts without checking who is on the other end, which makes Zapadka's
`require` stricter than libpq's: a server presenting a certificate Zapadka
cannot verify is refused rather than trusted. Supply a private CA with
`sslrootcert`. Running unencrypted is supported and normal on a private network,
but Zapadka says so in the report unless the target asked for it with
`sslmode=disable`.

**Connect as a role that owns the schema and nothing more.** Zapadka's scripts
are your SQL, run with your privileges; the runner decides *when* and *in what
transaction* they run, not what the server will let them do. A role holding
`SUPERUSER`, `pg_execute_server_program`, or `pg_write_server_files` can reach
outside the database — `COPY ... TO PROGRAM`, an untrusted-language function
writing a file — and no transaction, read-only or otherwise, rolls that back.
Zapadka does not deploy as a superuser, and neither should you.

## Requirements

- PostgreSQL 18 or newer. Zapadka analyses migrations with the PostgreSQL 18
  grammar, so it cannot make truthful safety decisions about an older server and
  refuses to try.
- Linux x86_64 or aarch64 for released binaries. Building from source works
  anywhere Rust and a C compiler do.

## Nontransactional migrations

`CREATE INDEX CONCURRENTLY` and its relatives refuse to run inside a
transaction. A migration can declare `transaction = "forbidden"` to run one —
exactly one statement, so that an interrupted run has a single possible
question rather than several.

The transactional guarantee is genuinely unavailable here, and Zapadka does not
pretend otherwise. What it does instead is **write down the attempt before the
statement runs**, and commit that. So a run killed mid-statement leaves evidence
naming what was in flight:

```console
$ zapadka deploy
error: the connection failed while running migrations/.../deploy.sql, so whether
       its statement took effect is unknown  [deploy.outcome_unknown]
```

The target is then **blocked**: every command that would act on it refuses,
because a plan computed from applied state would be built on a gap. `status`
still reports, since that is how you find out.

A statement the server *rejected* blocks the target too. An error is not proof
that nothing happened — a failed `CREATE INDEX CONCURRENTLY` leaves an invalid
index behind, and an automatic retry would fail on the name that now exists,
after you had been told the target was clean.

Zapadka will not retry and will not guess. A `CREATE INDEX CONCURRENTLY` can
finish after the client that asked for it is gone, and it can leave an invalid
index behind — so both "assume it worked" and "assume it didn't" are wrong some
of the time, expensively. Look at the database, then say what you found:

```sh
zapadka resolve <id> --applied      # it took effect; record it
zapadka resolve <id> --not-applied  # it did not; let a deploy try again
```

The assertion is written to the append-only history as `asserted_applied` or
`asserted_not_applied`, with the role that made it. A later reader can always
tell a migration Zapadka watched succeed from one a person vouched for.

`--not-applied` records a claim; it undoes nothing. If the statement half-ran,
clean that up yourself first — only you can see what is safe to drop.

## Limitations

Deliberately out of scope for v1: Sqitch or pgTAP CLI/metadata compatibility,
non-PostgreSQL databases, automatic rollback, declarative schema diffing,
repeatable migrations, callbacks, and multi-project registries.

## Documentation

- [Architecture decisions](docs/adr/) — the decisions that are costly to reverse
- [Code quality](docs/development/code-quality.md) — lints, complexity budgets,
  and the checks CI runs
- [`ReportV1` JSON Schema](docs/report-v1.schema.json)

## Development

```sh
cargo test --workspace     # unit and integration tests; needs Docker
cargo xtask quality        # the full static-analysis battery
cargo xtask metrics        # complexity report
```

## Licence

MIT. Zapadka embeds a pinned build of
[libpg_query](https://github.com/pganalyze/libpg_query) (BSD-3-Clause, with
PostgreSQL-licensed sources); see `third_party/libpg_query/`.
