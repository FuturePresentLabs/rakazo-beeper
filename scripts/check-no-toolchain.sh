#!/usr/bin/env bash
# Static guard: the runtime (final) Docker stage must carry the artifact and
# no toolchain. Build stages may compile; the final stage may not.
#
# Usage: scripts/check-no-toolchain.sh [Dockerfile]
# Prints exactly one "OK" or "FAIL <reason>" line.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ "${1:-}" = "--selftest" ]; then
    exec bash "$script_dir/check-no-toolchain.selftest.sh"
fi
dockerfile="${1:-$script_dir/../Dockerfile}"

# Standalone toolchain names. The boundary classes keep `cc` from matching
# `ca-certificates`, `gcc` from matching `gcc` differently, and `make` from
# matching `cmake`.
toolchain_re='(^|[^[:alnum:]_+.-])(cargo|rustc|rustup|gcc|g\+\+|clang|cc|make|cmake|node|npm|python3-dev|build-essential)([^[:alnum:]_+.-]|$)'
# Compiler/build dependencies pulled through apt (covers -dev and build packages).
apt_build_re='apt-get[^#]*(-dev|build)'
# Bases that are runtime-only by construction.
slim_base_re='(^|/)(scratch|busybox|.*-slim|.*distroless.*|.*alpine.*)$'
copy_from_re='^[[:space:]]*([Cc][Oo][Pp][Yy]|[Aa][Dd][Dd])[[:space:]].*--from='

fail() {
    printf 'FAIL %s\n' "$*"
    exit 1
}

[ -f "$dockerfile" ] || fail "Dockerfile not found: $dockerfile"

# Join continuation lines and drop comment/blank lines, recording logical
# instructions (1-based line number + text).
instructions=()
line_no=0
while IFS= read -r raw || [ -n "$raw" ]; do
    line_no=$((line_no + 1))
    if [[ "$raw" =~ ^[[:space:]]*# ]]; then
        continue
    fi
    start=$line_no
    buf="$raw"
    while [[ "$buf" =~ \\[[:space:]]*$ ]] && IFS= read -r next; do
        line_no=$((line_no + 1))
        buf="${buf%\\*}"
        buf="$buf $next"
    done
    if [[ -n "${buf//[[:space:]]/}" ]]; then
        instructions+=("$start|$buf")
    fi
done < "$dockerfile"

# The final stage begins at the last FROM instruction.
final_index=-1
for i in "${!instructions[@]}"; do
    text="${instructions[$i]#*|}"
    if [[ "$text" =~ ^[[:space:]]*[Ff][Rr][Oo][Mm][[:space:]] ]]; then
        final_index=$i
    fi
done
[ "$final_index" -ge 0 ] || fail "no FROM instruction in $dockerfile"

from_text="${instructions[$final_index]#*|}"
if [[ "$from_text" =~ ^[[:space:]]*[Ff][Rr][Oo][Mm][[:space:]]+(.*)$ ]]; then
    from_rest="${BASH_REMATCH[1]}"
else
    fail "cannot parse final FROM instruction"
fi

# First non-option token is the image (handles --platform=... and AS name).
base=""
for token in $from_rest; do
    if [[ "$token" == --* ]]; then
        continue
    fi
    base="$token"
    break
done
[ -n "$base" ] || fail "cannot parse final stage base image"
base_lc="$(printf '%s' "$base" | tr '[:upper:]' '[:lower:]')"

is_slim=0
if [[ "$base_lc" =~ $slim_base_re ]]; then
    is_slim=1
fi

# Inspect only instructions in the final stage; only RUN/COPY/ADD can install
# or copy a toolchain.
has_copy_from=0
for ((i = final_index; i < ${#instructions[@]}; i++)); do
    text="${instructions[$i]#*|}"
    line="${instructions[$i]%%|*}"
    if [[ "$text" =~ ^[[:space:]]*([Rr][Uu][Nn]|[Cc][Oo][Pp][Yy]|[Aa][Dd][Dd])[[:space:]] ]]; then
        if [[ "$text" =~ $toolchain_re ]]; then
            fail "final stage references toolchain '${BASH_REMATCH[2]}' at line $line: $text"
        fi
        if [[ "$text" =~ $apt_build_re ]]; then
            fail "final stage installs build/dev packages at line $line: $text"
        fi
        if [[ "$text" =~ ^[[:space:]]*([Cc][Oo][Pp][Yy]|[Aa][Dd][Dd])[[:space:]] ]] && [[ "$text" =~ $copy_from_re ]]; then
            has_copy_from=1
        fi
    fi
done

if [ "$is_slim" -eq 0 ] && [ "$has_copy_from" -eq 0 ]; then
    fail "final stage base '$base' is not a known slim/runtime image and no COPY --from= prebuilt artifact is present (compiles in place?)"
fi

printf 'OK final stage base %s, no toolchain installed or copied\n' "$base"
