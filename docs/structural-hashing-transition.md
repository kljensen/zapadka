# Transitioning migration hashes

Newly applied and baselined migrations use `structural-v1`. PostgreSQL's complete
parse tree determines their SQL identity: whitespace, line endings, and ordinary
source comments may change. Literal values, quoted names, statement/column order,
constraints, dependencies, and transaction mode remain significant. A `COMMENT
ON` statement stores a literal and remains substantive. Text inside dollar-quoted
functions and DO bodies remains significant, including its whitespace/comments.
This is structural equivalence under the supported parser, not proof of identical
effects in arbitrary database contexts.

## A new project

```sh
zapadka init
zapadka new create_widgets
# Write deploy.sql, then configure a target in zapadka.toml.
zapadka format --write
zapadka lint
zapadka deploy --target development
```

There is no rehash step for new migrations. Execution evidence still records the
SHA-256 of the exact bytes sent to PostgreSQL.

## An existing project with original files

Update team binaries and CI to Zapadka 0.6.0 or newer first. Registry format 3
requires at least that version. Keep the existing SQL checked out while
previewing and converting each target:

```sh
zapadka rehash --target staging --dry-run --output json
zapadka rehash --target staging
zapadka rehash --target production --dry-run
zapadka rehash --target production
zapadka format --write
```

Preview is read-only: it does not upgrade the registry, record events, or execute
migration SQL. Conversion verifies old hashes, then records the structural
definition and algorithm under the deployment lock in one transaction. Applied
migrations are converted; pending migrations stay pending. A successful repeated
conversion is a no-op. Original execution hashes and historical events remain.

Each target can transition independently. A mixed registry validates each
migration using its recorded algorithm. Continue using unchanged files until
every relevant target is converted; then commit broad formatting changes.
After registry upgrade, older binaries refuse the newer registry version.

## Files have already changed

If byte hashes no longer match, the stored digest cannot prove whether changes
were cosmetic. Review the preview and explicitly establish the current checked-
out SQL as the definition to enforce:

```sh
zapadka rehash --target production --dry-run
zapadka rehash --target production --accept-current \
  --reason "Adopt structural hashing after reviewing formatted migrations"
```

The audit distinguishes verified conversion from operator acceptance. Acceptance
does not claim to prove equivalence to the unavailable original script. It does
not execute SQL or make database contents match an edited script. Missing applied
migrations, changed graph dependencies or transaction mode, and unresolved
deployment attempts still block conversion. The acceptance flag cannot reset a
previously structural migration after a substantive edit. Such edits remain
history errors and require corrective migrations.

For an untouched project, normal verified rehash requires no reason. Explicit
acceptance requires a nonempty reason. Audit events retain old/new definition
hashes, algorithm versions, acceptance method, operator identity, and reason.
The registry version, definition algorithm, and structured rehash results are
exposed in versioned JSON reports; consumers should tolerate additive fields.
The generated [report schema](report-v1.schema.json) defines those fields.

## Formatting, restores, and compatibility

`format --write` checks the entire selection before writing and refuses a parser
or structural-equivalence failure without partially rewriting the selection.
Individual file replacement is atomic; this is not a filesystem-wide transaction
against I/O failures. `--allow-deploy-rewrite` remains a deprecated no-op for
existing scripts and cannot bypass structural validation. An informational
`format.legacy_targets` diagnostic reminds users that local equivalence cannot
prove targets have transitioned. Formatting neither connects nor rehashes.

A restored backup or another target with older registry state needs its own
preview and conversion. Retain the pre-formatting revision so verified conversion
remains possible, or explicitly accept current SQL after review. Replicas inherit
registry changes through normal replication; perform registry writes on the
writable primary and wait for replication before depending on the new state.

Algorithm names are permanent contracts. Fixed canonical/hash fixtures gate
parser upgrades; incompatible representation changes need a new version and
explicit transition, never an unnoticed hash change. See
[ADR-0006](adr/0006-version-structural-migration-hashes.md).

Original-source-directory or Git-revision recovery is deferred to an optional
follow-up. Neither is required to adopt structural hashing.
