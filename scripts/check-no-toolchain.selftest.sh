#!/usr/bin/env bash
# Self-test for scripts/check-no-toolchain.sh.
# Fixtures: this repo's Dockerfile (must pass) and a synthetic compile-in-guest
# Dockerfile (must fail with a clear reason). Also checks the non-slim/no-copy
# rule, since that is the other way a guest can silently compile in place.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
check="$script_dir/check-no-toolchain.sh"
repo_dockerfile="$script_dir/../Dockerfile"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

run_case() {
    local name="$1" dockerfile="$2" expect="$3"
    local out
    if out="$(bash "$check" "$dockerfile")"; then
        if [ "$expect" = pass ]; then
            printf 'PASS %s: %s\n' "$name" "$out"
        else
            printf 'FAIL selftest: %s was accepted: %s\n' "$name" "$out"
            exit 1
        fi
    else
        if [ "$expect" = fail ]; then
            printf 'PASS %s: %s\n' "$name" "$out"
        else
            printf 'FAIL selftest: %s was rejected: %s\n' "$name" "$out"
            exit 1
        fi
    fi
}

run_case "repo Dockerfile" "$repo_dockerfile" pass

cat > "$tmpdir/Dockerfile.compiles" <<'EOF'
FROM rust:1-bookworm AS build
RUN cargo build --release

FROM debian:bookworm-slim
RUN cargo build --release
EOF
out="$(bash "$check" "$tmpdir/Dockerfile.compiles" || true)"
if [[ "$out" != FAIL*cargo* ]]; then
    printf 'FAIL selftest: compile-in-guest fixture did not fail on cargo: %s\n' "$out"
    exit 1
fi
printf 'PASS synthetic compile-in-guest: %s\n' "$out"

cat > "$tmpdir/Dockerfile.builder-base" <<'EOF'
FROM rust:1-bookworm AS build
RUN cargo build --release

FROM rust:1-bookworm
ENTRYPOINT ["rakazo-beeper"]
EOF
out="$(bash "$check" "$tmpdir/Dockerfile.builder-base" || true)"
if [[ "$out" != FAIL*slim* ]]; then
    printf 'FAIL selftest: builder-base fixture did not fail on base image: %s\n' "$out"
    exit 1
fi
printf 'PASS synthetic builder final stage: %s\n' "$out"

printf 'OK selftest\n'
