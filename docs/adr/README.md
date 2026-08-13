# Architecture Decision Records

Zapadka records only decisions that are architectural, long-lived, and costly
to reverse. ADRs use a concise Nygard format: context, decision, and
consequences.

These records describe the decisions that hold **today**. When one is reversed,
the record is rewritten to say what is now true and why the earlier choice was
abandoned — rather than layering a superseding ADR on top of a stale one.

That is a deliberate trade. Some fidelity to the original reasoning is lost;
what is gained is that a newcomer can read this set and believe it. An index
that lists a reversed decision as accepted is worse than no index, and the git
history keeps the earlier text for anyone who wants it.

| ADR | Status |
|---|---|
| [0001: Use SQL-first immutable migration graphs](0001-use-sql-first-immutable-migration-graphs.md) | Accepted |
| [0002: Enforce runner-owned SQL execution](0002-enforce-runner-owned-sql-execution.md) | Accepted |
| [0003: Serialize deployments and retain registry history](0003-serialize-deployments-and-retain-registry-history.md) | Accepted |
| [0004: Separate verification from tests, and own the assertion library](0004-separate-deployment-verification-from-database-tests.md) | Accepted |
| [0005: Ship self-contained PostgreSQL 18 Linux binaries](0005-ship-self-contained-postgresql-18-linux-binaries.md) | Accepted |
