# ADR-0002: Enforce runner-owned SQL execution

Date: 2026-07-18

## Status

Accepted

## Context

PostgreSQL migrations may need transactional or inherently nontransactional
operations. Zapadka must not let scripts escape its transaction boundary, infer
an outcome after an ambiguous failure, or silently undo a committed migration.

## Decision

Zapadka will own transaction boundaries and reject top-level transaction
control using a pinned PostgreSQL 18-derived parser behind a narrow internal
interface. Transactional scripts run as whole scripts in runner-owned
transactions. A nontransactional migration must declare that mode and contain
exactly one parsed statement. Its attempt is recorded before execution; an
ambiguous outcome blocks deployment until a migration-specific, audited
operator assertion resolves it. Zapadka never automatically reverts.

## Consequences

- Transactional failures roll back cleanly, while post-commit verification
  failures leave an accurate applied state.
- Nontransactional recovery is explicit and cannot be made fully automatic.
- PostgreSQL parser fidelity becomes a build and maintenance cost.
- Zapadka deliberately offers fewer execution modes than general SQL clients.
