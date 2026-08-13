# Zapadka development tasks.
#
# `cargo test` works and is not going anywhere. What this adds is cleanup that
# happens when the tests *finish*, which the test binary itself cannot do: the
# harness holds its container in a `static`, and Rust never drops statics at
# process exit. Without an outer wrapper the only available hook is the *next*
# run's startup, which bounds the leak but always leaves one container behind.
#
# Every recipe that removes anything requires two independent conditions -- the
# label and the name prefix. Every destructive bug in this project so far came
# from matching on a single identifier that turned out not to be unique, so
# `docker rm` is never handed a filter that could mean somebody else's database.

label := "dev.zapadka.test-harness"
prefix := "zapadka-testharness-"

# List the recipes.
default:
    @just --list

# Cleanup runs whether the tests passed or failed -- a failing suite is exactly
# when you re-run, and exactly when leftovers accumulate fastest.
#
# Run every test, then remove the containers it created.
test:
    #!/usr/bin/env bash
    set -uo pipefail
    trap 'just _sweep' EXIT
    cargo test --workspace

# The PostgreSQL integration tests only.
test-db:
    #!/usr/bin/env bash
    set -uo pipefail
    trap 'just _sweep' EXIT
    cargo test --workspace --test postgres

# Everything CI runs, in the order CI runs it.
ci: fmt-check lint test quality schema-check

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

quality:
    cargo xtask quality

metrics:
    cargo xtask metrics

schema-check:
    cargo xtask schema --check

# Show the containers this harness owns -- run before `clean` to see what goes.
containers:
    @docker ps -a --filter "label={{label}}" --filter "name={{prefix}}" \
        --format 'table {{{{.Names}}}}\t{{{{.Status}}}}\t{{{{.Image}}}}'

# `-v` matters as much as the removal: each container owns an anonymous volume
# holding a PostgreSQL data directory, and dropping the container without it
# just moves the leak somewhere less visible.
#
# Remove this harness's containers and their volumes.
clean:
    @just _sweep
    @echo "removed Zapadka test containers"

_sweep:
    #!/usr/bin/env bash
    set -uo pipefail
    ids=$(docker ps -aq --filter "label={{label}}" --filter "name={{prefix}}" 2>/dev/null || true)
    if [ -n "$ids" ]; then
        docker rm -f -v $ids >/dev/null 2>&1 || true
    fi
