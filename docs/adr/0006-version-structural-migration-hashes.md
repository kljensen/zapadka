# ADR-0006: Version structural migration hashes

Date: 2026-09-05

## Status

Accepted

## Context

Exact-byte migration definitions reject harmless SQL formatting and source
comments. Projects need an explicit transition without losing the evidence of
what ran. PostgreSQL query fingerprints discard constants and reorder some
lists, making them unsuitable for migration integrity.

## Decision

Keep `raw-v1` byte-for-byte compatible with the original algorithm. Its UTF-8
envelope is `zapadka.migration.v1\nid=<uuid>\ntransaction=<mode>\ndepends=<ids>\n`
followed by `deploy_sha256=<hex>\n`. IDs are lowercase UUIDs, dependencies are
sorted lexicographically and comma-separated, and mode is `required` or
`forbidden`. SHA-256 hashes this envelope; the inner hash covers exact SQL bytes.

For `structural-v1`, parse with the pinned PostgreSQL 18 parser and retain the
complete `stmts` JSON array. The root parser `version` is deliberately excluded.
Recursively remove only integer-valued source-position fields named `location`,
`name_location`, `stmt_location`, `stmt_len`, `list_start`, `list_end`,
`rexpr_list_start`, or `rexpr_list_end`. This list was audited against the
vendored `parsenodes.h`, `primnodes.h`, and `pg_query_outfuncs_defs.c`: it covers
the ParseLoc fields, including the deparser's list-boundary extensions.
`CreateTableSpaceStmt.location` is a string containing the actual directory
and MUST remain. `limitOffset`, window offsets, and all other fields remain.
Anonymous code `source_text` and string values are substantive and remain.

Recursively sort object keys lexicographically; preserve array order and all
remaining values and node tags. Serialize as compact serde_json UTF-8 JSON
with no trailing newline. The stable payload has no parser-build metadata.
The envelope is exactly:

```text
zapadka.migration.v2
algorithm=structural-v1
id=<uuid>
transaction=<mode>
depends=<sorted comma-separated ids>
deploy_structure_sha256=<lowercase hex SHA-256 of canonical JSON>
```

Every line, including the last, ends in LF. SHA-256 of its UTF-8 bytes is the
definition hash. Reversibility, annotations, and verification/revert artifacts
remain outside the deployment definition. Parsing failures are errors, never
fallbacks to byte hashing or a weaker fingerprint. Empty/comment-only input
canonicalizes to `[]`; existing role-specific lint policy is unchanged (empty
deployments warn, while empty verification scripts fail).

Store each definition's algorithm explicitly. New definitions use structural
hashing; legacy definitions retain exact-byte validation until an explicit
`rehash`. Preserve original execution hashes and append an audit event for
each conversion. Verified conversions prove the old byte hash first; explicit
acceptance establishes current SQL as the operator-asserted definition without
claiming to prove equivalence to unavailable original SQL.

## Consequences

Whitespace, ordinary source comments, and equivalent parser spellings can
change without a structural mismatch. Quoted identifiers, literals, dollar-
quoted procedural bodies, COMMENT ON payloads, and ordered lists remain
significant. This guarantees structural equivalence under the supported
parser, not identical effects in arbitrary database contexts.

Canonical representations and expected hashes are fixed regression fixtures.
Parser/deparser upgrades must run the compatibility corpus. Hash drift must be
explained and resolved without silently refreshing fixtures; incompatible
representations require a new algorithm and explicit audited transition.
Formatter output must pass structural equality before files are rewritten.
