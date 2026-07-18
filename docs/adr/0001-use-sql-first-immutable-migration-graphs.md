# ADR-0001: Use SQL-first immutable migration graphs

Date: 2026-07-18

## Status

Accepted

## Context

Zapadka must keep migrations reviewable as PostgreSQL SQL, detect edits to
deployed history, and order changes created on concurrent branches without
making the common linear workflow cumbersome.

## Decision

Each migration will be a directory with a typed `migration.toml`, `deploy.sql`,
and optional `revert.sql` and `verify.sql`. A UUIDv7 is its permanent identity;
declared dependencies form a DAG executed in deterministic topological order.
The canonical manifest and `deploy.sql` form the immutable deployment
definition and are SHA-256 hashed. Changes to deployed definitions are errors.
Mutable verification and revert artifacts are hashed when executed.

## Consequences

- Migrations remain ordinary, reviewable SQL rather than schema diffs or a DDL
  DSL.
- Parallel branches can converge explicitly, while `new` can depend on all
  current heads for normal authoring.
- Users cannot edit or delete deployed definitions without an integrity error;
  corrective work requires a new migration.
- Canonicalization, graph validation, and deterministic ordering become public
  compatibility contracts.
