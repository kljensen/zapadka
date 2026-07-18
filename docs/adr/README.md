# Architecture Decision Records

Zapadka records only decisions that are architectural, long-lived, and costly
to reverse. ADRs use a concise Nygard format: context, decision, and
consequences.

Accepted ADRs are historical records. Change a decision by adding a new ADR and
marking the old one superseded; do not rewrite the original rationale.

| ADR | Status |
|---|---|
| [0001: Use SQL-first immutable migration graphs](0001-use-sql-first-immutable-migration-graphs.md) | Accepted |
| [0002: Enforce runner-owned SQL execution](0002-enforce-runner-owned-sql-execution.md) | Accepted |
| [0003: Serialize deployments and retain registry history](0003-serialize-deployments-and-retain-registry-history.md) | Accepted |
| [0004: Separate deployment verification from database tests](0004-separate-deployment-verification-from-database-tests.md) | Accepted |
| [0005: Ship self-contained PostgreSQL 18 Linux binaries](0005-ship-self-contained-postgresql-18-linux-binaries.md) | Accepted |
