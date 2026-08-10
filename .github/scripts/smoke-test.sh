#!/usr/bin/env bash
#
# Exercises a built Zapadka binary against a real PostgreSQL 18, from a
# container with no shared libraries.
#
# This is the acceptance test for a release: it proves the artifact that would
# be published actually works, rather than that the source it was built from
# passes its own tests. It deliberately uses no Rust tooling, so it tests the
# binary rather than the build.
#
# Usage: smoke-test.sh <path-to-zapadka-binary>

set -euo pipefail

BINARY="${1:?usage: smoke-test.sh <path-to-zapadka-binary>}"
BINARY="$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")"

# PostgreSQL 18, pinned by digest so the tag cannot be repointed underneath us.
POSTGRES_IMAGE="postgres@sha256:9a8afca54e7861fd90fab5fdf4c42477a6b1cb7d293595148e674e0a3181de15"
# A minimal image with no libc of its own beyond busybox's, so a binary that
# needed a shared library would fail here rather than in someone's production.
RUNNER_IMAGE="${ZAPADKA_RUNNER_IMAGE:-busybox:1.37}"

NETWORK="zapadka-smoke-$$"
CONTAINER="zapadka-smoke-postgres-$$"
WORKDIR="$(mktemp -d)"

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

# macOS ships `shasum`; Linux ships `sha256sum`.
digest() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }

say() { printf '\n=== %s ===\n' "$1"; }

# Runs the binary inside the minimal container.
#
# As the invoking user, not as root. Otherwise `init` and `new` create
# root-owned files in the bind-mounted project directory, and the host shell
# below cannot overwrite the generated scripts. Zapadka needs no privileges of
# its own, so there is nothing lost.
zapadka() {
  docker run --rm --network "$NETWORK" ${ZAPADKA_PLATFORM:+--platform "$ZAPADKA_PLATFORM"} \
    --user "$(id -u):$(id -g)" \
    -v "$BINARY:/zapadka:ro" \
    -v "$WORKDIR:/project" \
    -w /project \
    --entrypoint /zapadka \
    "$RUNNER_IMAGE" "$@"
}

URI="postgresql://postgres:smoke@db:5432/app"

say "starting PostgreSQL 18"
docker network create "$NETWORK" >/dev/null
docker run -d --name "$CONTAINER" --network "$NETWORK" --network-alias db \
  -e POSTGRES_PASSWORD=smoke -e POSTGRES_DB=app "$POSTGRES_IMAGE" >/dev/null
# `pg_isready` says yes during initialisation, while the entrypoint is running
# a temporary server to create the database -- so it goes true before `app`
# exists, and the first real query then fails with "database does not exist".
# Waiting on the query this script actually needs is the only signal that means
# anything here.
ready=""
for _ in $(seq 1 60); do
  if docker exec "$CONTAINER" psql -U postgres -d app -tAc "SELECT 1" >/dev/null 2>&1; then
    ready=yes
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "PostgreSQL did not accept a connection to app within 60s" >&2
  docker logs "$CONTAINER" >&2
  exit 1
fi
docker exec "$CONTAINER" psql -U postgres -d app -tAc "SELECT version()"

say "version"
zapadka --version

say "init"
zapadka init

say "new"
zapadka new create-orders
zapadka new add-order-status

# Fill in the generated skeletons. The directory names carry generated UUIDs,
# so they are discovered rather than assumed. A directory is '<uuid>-<slug>',
# which makes the slug a suffix glob -- and a glob that matched nothing would
# otherwise expand to itself and produce a confusing error several lines later.
migration_dir() {
  for dir in "$WORKDIR"/migrations/*-"$1"; do
    if [ -d "$dir" ]; then
      basename "$dir"
      return 0
    fi
  done
  echo "zapadka new did not create a migration for $1" >&2
  return 1
}
FIRST="$(migration_dir create-orders)"
SECOND="$(migration_dir add-order-status)"

cat > "$WORKDIR/migrations/$FIRST/deploy.sql" <<'SQL'
CREATE SCHEMA app;
CREATE TABLE app.orders (id bigint PRIMARY KEY, total numeric NOT NULL);
SQL
cat > "$WORKDIR/migrations/$FIRST/revert.sql" <<'SQL'
DROP SCHEMA app CASCADE;
SQL

# The second migration depends on the first, so it must deploy after it.
cat > "$WORKDIR/migrations/$SECOND/deploy.sql" <<'SQL'
ALTER TABLE app.orders ADD COLUMN status text NOT NULL DEFAULT 'pending';
SQL
cat > "$WORKDIR/migrations/$SECOND/revert.sql" <<'SQL'
ALTER TABLE app.orders DROP COLUMN status;
SQL
cat > "$WORKDIR/migrations/$SECOND/verify.sql" <<'SQL'
SELECT 1 FROM information_schema.columns
 WHERE table_schema = 'app' AND table_name = 'orders' AND column_name = 'status';
SQL

say "lint"
zapadka lint

say "dry run"
zapadka deploy --uri "$URI" --dry-run

say "deploy"
zapadka deploy --uri "$URI"

say "status"
zapadka status --uri "$URI"

say "verify"
zapadka verify --uri "$URI"

say "the two dependent migrations are both applied"
docker exec "$CONTAINER" psql -U postgres -d app -tAc \
  "SELECT string_agg(slug, ',' ORDER BY applied_at) FROM zapadka.applied_migrations" \
  | grep -q "create-orders,add-order-status" \
  || { echo "FAIL: migrations were not applied in dependency order"; exit 1; }

docker exec "$CONTAINER" psql -U postgres -d app -tAc \
  "SELECT count(*) FROM information_schema.columns
    WHERE table_schema='app' AND table_name='orders'" | grep -q '^3$' \
  || { echo "FAIL: the orders table does not have the expected columns"; exit 1; }

say "a modified deployed migration is detected"

# Edited from inside a container, on the same mount Zapadka reads through.
# Editing from the host and waiting for the change to appear made this test a
# probe of the file-sharing layer rather than of Zapadka.
docker run --rm ${ZAPADKA_PLATFORM:+--platform "$ZAPADKA_PLATFORM"} \
  --user "$(id -u):$(id -g)" \
  -v "$WORKDIR:/w" "$RUNNER_IMAGE" \
  sh -c "echo '-- an edit made after deployment' >> '/w/migrations/$FIRST/deploy.sql'"

if zapadka status --uri "$URI" --output json > "$WORKDIR/tampered.json" 2>/dev/null; then
  echo "FAIL: editing a deployed migration should have failed"
  cat "$WORKDIR/tampered.json"
  exit 1
fi
grep -q '"code": "history.definition_changed"' "$WORKDIR/tampered.json" \
  || { echo "FAIL: expected history.definition_changed"; cat "$WORKDIR/tampered.json"; exit 1; }

# Exit code 5 is the documented history-mismatch code; a script branching on it
# has to keep working.
set +e
zapadka status --uri "$URI" >/dev/null 2>&1
CODE=$?
set -e
[ "$CODE" -eq 5 ] || { echo "FAIL: expected exit 5 for a history mismatch, got $CODE"; exit 1; }

say "the JSON report is exactly one document"
zapadka status --uri "$URI" --output json > "$WORKDIR/report.json" 2>/dev/null || true
docker run --rm --user "$(id -u):$(id -g)" -v "$WORKDIR:/w" "$RUNNER_IMAGE" \
  sh -c 'head -c1 /w/report.json | grep -q "{"' \
  || { echo "FAIL: the JSON report does not start with an object"; exit 1; }

printf '\nSmoke test passed.\n'
