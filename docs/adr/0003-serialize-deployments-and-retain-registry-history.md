# ADR-0003: Serialize deployments and retain registry history

Date: 2026-07-18

## Status

Accepted

## Context

Concurrent deployers, process crashes, and registry evolution must not create
two plausible histories. Status queries also need efficient current state
without discarding the evidence needed to explain prior operations.

## Decision

Zapadka will hold a session-scoped PostgreSQL advisory lock from preflight
through default verification. One project per target database will use a
reserved registry schema containing versioned metadata, current applied
migrations, and append-only events. Mutating commands perform embedded,
ordered registry upgrades while holding the lock; older binaries refuse newer
registry formats.

## Consequences

- Zapadka deployers are serialized, including across commits and
  nontransactional statements; unrelated database work is not serialized.
- Current-state reads stay simple while deploy, verify, revert, baseline, and
  failure history remains auditable.
- Lock availability and registry privileges are operational requirements.
- Multi-project registries and destructive history repair are outside this
  design.
