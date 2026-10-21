#!/usr/bin/env bash
# Thin scheduler wrapper around healthcheck.sh: run one check, append exactly one
# line to a log file, and exit with the check's status. Checking logic lives only
# in healthcheck.sh; this script never retries, restarts or mutates anything.
set -u

base=${HEALTHCHECK_BASE:-https://agents.fpl.dev}
log=${HEALTHCHECK_LOG:-${XDG_STATE_HOME:-$HOME/.local/state}/rakazo-beeper/healthcheck.log}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

mkdir -p "$(dirname "$log")" || exit 1

out=$(bash "$here/healthcheck.sh" --base "$base" 2>&1)
rc=$?

printf '%s base=%s exit=%d %s\n' \
	"$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$base" "$rc" \
	"$(printf '%s' "$out" | tr '\n' ';')" >>"$log" || exit 1

printf '%s\n' "$out"
exit "$rc"
