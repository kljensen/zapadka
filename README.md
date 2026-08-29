# Zapadka

Zapadka is a [PostgreSQL](https://www.postgresql.org/) migration, testing, and formatting tool packaged
as single static binary. It's basically a convenient Frankenstein's monster mashup of
[Sqitch](https://github.com/sqitchers/sqitch),
[pgTAP](https://github.com/theory/pgtap),
and [pg_format](https://github.com/darold/pgformatter).

I built it for myself and it is what I use for most of my projects
using Postgres.

## TLDR

```sh
zapadka init
zapadka new add-orders-table
# edit migrations/<id>-add-orders-table/deploy.sql
zapadka lint
zapadka format --check
zapadka deploy --target production
zapadka status --target production
```

The whole command set is `init`, `new`, `lint`, `format`, `status`, `deploy`,
`verify`, `revert`, `baseline`, and `test`.

## The basic idea

Zapadka keeps authored SQL migrations. There is no schema-diff magic and no DSL
to learn: the SQL you review is the SQL that runs. It keeps a registry in the
database, so it can tell what happened there instead of guessing from the files
in your checkout.

Migrations are a graph rather than a numbered list. Each one has a permanent
UUIDv7 identity and says what needs to come before it. That means two branches
can add migrations independently and later meet without a renumbering ritual.
When it deploys, Zapadka topologically sorts the graph deterministically.

I also wanted transaction boundaries to be boring. Zapadka owns them: a normal
migration cannot contain `BEGIN`, `COMMIT`, `ROLLBACK`, or `SAVEPOINT`. The
PostgreSQL 18 parser inside the binary catches that before anything runs. A
migration's SQL and its “applied” registry row therefore commit together: after
a crash, you get both or neither.

That only promises what PostgreSQL promises. For example, `nextval()` is not
transactional, so a failed migration can leave a sequence advanced even though
the migration was not recorded as applied.

Already-deployed migrations are immutable. Editing one is a hard error, not a
warning and definitely not an invitation to silently run it again. Make a new
migration for a correction; then the history says what actually happened.

## Formatting, verification, and tests

`zapadka format --check` checks migration scripts and `tests/db/**/*.sql` (or
just the paths you give it) with that same PostgreSQL-aware parser. `--write`
rewrites selected files atomically. It refuses to rewrite a `deploy.sql` unless
you explicitly add `--allow-deploy-rewrite`, because formatting a deployed
migration changes its definition. Comments survive formatting; code inside a
dollar-quoted function body is left alone.

Each migration can have a `verify.sql`. Zapadka runs it after the migration has
committed, on a fresh read-only transaction that it always rolls back. This is
the small, production-safe check that answers “did that migration leave the
database in the state I meant?” It is deliberately not continuous monitoring:
changes made outside Zapadka are noticed next time you verify or test.

Read-only matters as well as rollback. PostgreSQL will not roll back `nextval()`,
so verification cannot touch a sequence by accident. The tradeoff is that it
also cannot do `CREATE TEMP TABLE`; use a CTE or `VALUES` list for expected data
instead.

```sql
WITH expected(id) AS (VALUES (1::bigint), (2::bigint))
SELECT 1 / (CASE WHEN (SELECT count(*) FROM app.orders)
                 = (SELECT count(*) FROM expected) THEN 1 ELSE 0 END);
```

`zapadka test` is the bigger, separate database-test runner. Every test file
gets a fresh connection and a transaction Zapadka rolls back, so tests cannot
see one another's data. The exception is sequences: Zapadka will tell you when
one advanced, but will not rewind it and risk handing an application connection
an id it has already seen.

Tests use a bundled SQL assertion library. It is inspired by pgTAP, but it is
not pgTAP and it does not emit TAP. There is no extension to install and nothing
to put on the server filesystem. Assertions record typed result rows that
Zapadka reads directly, which gives useful differences instead of two strings
to squint at.

```sql
SELECT has_table_in('app', 'orders');
SELECT col_is_pk_in('app', 'orders', 'id');
SELECT set_eq(
    'SELECT status FROM app.orders',
    ARRAY['paid', 'pending'],
    'only these statuses occur');
SELECT throws_ok($$INSERT INTO app.orders VALUES (1)$$, '23505');
```

For example, a failed set comparison can carry the actual missing and extra
rows, including their PostgreSQL types:

```json
{ "columns": [{"name": "id", "type": "bigint"}],
  "missing": [[2]], "extra": [[3]], "missing_count": 1, "extra_count": 1 }
```

`plan()` and `finish()` work if you want them, but you do not need them.
Assertions return `boolean`, so the SQL is still pleasant to read in `psql`.

### A few intentional pgTAP differences

| Difference | Why |
|---|---|
| `throws_ok`'s third argument is the description | pgTAP treats it as an expected message whenever the second argument happens to be five bytes long. That is surprising enough to be a footgun. Use `throws_sqlstate(sql, code, description)` when you want to be explicit. A file in pgTAP's ambiguous order is refused, not guessed at. |
| `has_table_in('app', 'orders')` and friends | Two untyped string literals are not reliably interpreted as schema and table by PostgreSQL. The `_in` forms always are. The pgTAP spellings still work. |
| `has_view` includes materialized views | A materialized view exists; it should count. |
| No `runtests`, `do_tap`, `check_test`, or `pgtap_version` | They are TAP-harness plumbing and there is no TAP harness here. Leaving them out makes accidental use obvious. |
| Test files cannot open their own transaction | The runner owns it so a test cannot escape rollback. Remove the `begin;` / `rollback;` from a pgTAP file. |

## What a project looks like

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

`zapadka.toml` goes in the repository and has no credentials. A target only
says where to find connection details: a PostgreSQL service entry or an
environment variable.

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

Every command produces the same versioned `ReportV1`. `--output json` writes
one document to stdout; human output is just another view of that same result.

## The checks that are supposed to save you

`zapadka lint` has errors and warnings, and I think the distinction is useful.
Errors are things Zapadka can prove are wrong: invalid SQL, a script taking over
transaction control, or PostgreSQL certainly refusing it in the declared mode.
They always fail.

Warnings are operational risks such as dropping data, rewriting a table, or
building an index that blocks writes. Zapadka cannot know whether a particular
one is fine for *your* database, so it tells you rather than pretending it can
decide. Put rules you care about in `policy.deny`; allow a specific warning in a
migration with an `[[allow]]` entry and a reason.

```
warning: migrations/019.../deploy.sql:3: builds an index on app.orders without CONCURRENTLY
  [lint.index_without_concurrently]
  this blocks writes to the table until the index is built; on a table with
  existing rows, build it CONCURRENTLY in its own nontransactional migration
```

There is no automatic revert. If verification fails after a migration committed,
Zapadka records that and stops. Running unproven revert SQL against an unknown
schema while nobody is looking is not a useful kind of automation.

`baseline` and `resolve` can write applied rows on an operator's word. They
require an explicit acknowledgement and record an assertion, rather than
pretending Zapadka observed something it did not.

## Nontransactional migrations

Some PostgreSQL commands, notably `CREATE INDEX CONCURRENTLY`, cannot run in a
transaction. Declare `transaction = "forbidden"` for one of those migrations.
Zapadka permits exactly one statement, which gives an interrupted run one clear
question instead of several.

The usual all-or-nothing guarantee simply does not exist here. Zapadka writes
down the attempt *before* it sends the statement, so if the connection dies it
does not pretend to know the answer:

```console
$ zapadka deploy
error: the connection failed while running migrations/.../deploy.sql, so whether
       its statement took effect is unknown  [deploy.outcome_unknown]
```

That target is then blocked. `status` still works, but commands that would make
changes refuse to build a plan on top of a gap. A server-side error blocks it
too: `CREATE INDEX CONCURRENTLY` can leave an invalid index behind, so “it
failed” is not proof that nothing happened.

Look at the database, clean up if needed, then tell Zapadka what you found:

```sh
zapadka resolve <id> --applied      # it took effect; record it
zapadka resolve <id> --not-applied  # it did not; let a deploy try again
```

Those become `asserted_applied` or `asserted_not_applied` history entries with
the role that made the call. `--not-applied` only records that claim; it cannot
clean up a partial statement for you.

## Connecting safely

Zapadka reads PostgreSQL service files itself and uses `rustls` for TLS. If it
encrypts, it verifies the server identity. In other words, its `require` mode
is stricter than libpq's: an untrusted certificate is refused rather than
quietly accepted. Provide a private CA with `sslrootcert`. Unencrypted
connections are fine on a private network, but Zapadka says so in the report
unless the target explicitly uses `sslmode=disable`.

Connect as a role that owns the schema, and no more. The runner controls when
your SQL runs, not what that SQL is allowed to do. A role with `SUPERUSER`,
`pg_execute_server_program`, or `pg_write_server_files` can reach outside the
database; no transaction can make that safe. Zapadka does not deploy as a
superuser, and neither should you.

## Exit codes

Scripts can rely on these.

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

For a script, use the exit code and `error.code` in the JSON report—not a
human-readable message.

## Requirements and limitations

- PostgreSQL 18 or newer. Zapadka uses the PostgreSQL 18 grammar to make its
  safety calls and will not bluff about older servers.
- Released binaries cover Linux x86_64 and aarch64, macOS Intel and Apple
  Silicon, and Windows x86_64. From source, it works wherever Rust and a C
  compiler do.

V1 deliberately does not try to be Sqitch or pgTAP compatible at the
CLI/metadata level, support other databases, do automatic rollback, diff a
declarative schema, offer repeatable migrations or callbacks, or manage
multiple projects from one registry.

## Development

```sh
just test          # every test, then clean up the containers it started
just test-db       # the PostgreSQL integration tests only
just ci            # everything CI runs, in CI's order
just containers    # show the containers this harness owns
just clean         # remove them
```

`cargo test --workspace` also works with nothing else installed. The only
difference is cleanup: a bare Cargo run can leave its PostgreSQL test container
behind until the next run. `just test` removes it at the end, pass or fail.

The cleanup code is deliberately paranoid. A container must have both the
`dev.zapadka.test-harness` label and the `zapadka-testharness-` name prefix
before Zapadka will remove it. That is a little fussy because deleting somebody
else's database would be much worse.

## Documentation

- [Architecture decisions](docs/adr/) — choices that would be expensive to undo
- [Code quality](docs/development/code-quality.md) — lints, complexity budgets,
  and the checks CI runs
- [`ReportV1` JSON Schema](docs/report-v1.schema.json)

## Licence

MIT. Zapadka embeds a pinned build of
[libpg_query](https://github.com/pganalyze/libpg_query) (BSD-3-Clause, with
PostgreSQL-licensed sources); see `third_party/libpg_query/`.
