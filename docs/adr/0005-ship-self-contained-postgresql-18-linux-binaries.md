# ADR-0005: Ship self-contained PostgreSQL 18 Linux binaries

Date: 2026-07-18

## Status

Accepted

## Context

Zapadka should run in deployment and CI environments without requiring libpq,
OpenSSL, a PostgreSQL installation, or a separately installed test framework.
Its safety checks must match the PostgreSQL version it supports.

## Decision

Zapadka will be a Rust application targeting PostgreSQL 18 and distributed as
static musl Linux binaries for x86_64 and aarch64. It will use native Rust TLS,
embed the pinned PostgreSQL-derived parser and Zapadka's SQL assertion library,
and avoid runtime plug-ins and dynamically linked database client libraries.
The pinned pgTAP source is retained for attribution and conformance reference,
not compiled or installed; see ADR-0004.

## Consequences

- Installation is a single-file operation with reproducible supporting
  checksums and an SBOM.
- Cross-compilation, static FFI, and minimal-container testing are required.
- PostgreSQL parser and assertion-library upgrades are deliberate release work.
- Other operating systems and PostgreSQL major versions are not v1 release
  targets.
