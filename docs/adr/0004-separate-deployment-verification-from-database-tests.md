# ADR-0004: Separate deployment verification from database tests

Date: 2026-07-18

## Status

Accepted

## Context

Production-safe checks and development database tests have different trust,
installation, and output requirements. Requiring pgTAP on deployment targets
would expand production state and couple migration safety to a test harness.

## Decision

Migration-local `verify.sql` will be plain PostgreSQL SQL, run after commit in a
fresh transaction that is always rolled back. Separately, `zapadka test` will
require an explicit test target, install a pinned vendored pgTAP SQL artifact in
the reserved `zapadka_test` schema, and run each test serially in its own
rolled-back transaction. TAP is an internal adapter; the public automation
contract is versioned Zapadka JSON.

## Consequences

- Deployment targets need neither pgTAP nor an extension.
- Verification can observe committed state but cannot intentionally retain
  writes, and failure stops later migrations without automatic reversion.
- Test targets require explicit preparation and may gain a Zapadka-owned test
  schema.
- Zapadka must maintain pgTAP provenance, TAP parsing, and stable report models.
