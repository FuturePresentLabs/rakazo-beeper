#!/usr/bin/env bash
# Resident supervisor for the AgentSmith bridge.
#
# `rakazo-beeper bridge serve` without --interval-s is a one-shot: it runs one
# cycle and exits 0, which is indistinguishable from a crash to a naive wrapper.
# This script always passes --interval-s, so the binary stays resident, and it
# restarts it on ANY exit (including rc=0) with capped exponential backoff.
#
# It never reads, writes or prints a token. The bridge reads its own service
# token from the path in RAKAZO_ORCH_TOKEN_FILE.
#
# Environment:
#   RAKAZO_BRIDGE_BIN       binary to run (default: <repo>/target/release, then
#                           <repo>/target/debug, then ./target/...)
#   RAKAZO_BRIDGE_CONFIG    bridge config (default ~/.config/rakazo/agentsmith.toml)
#   RAKAZO_BRIDGE_INTERVAL  seconds per cycle (default 10)
#   RAKAZO_BRIDGE_MAX_BACKOFF  restart backoff cap in seconds (default 300)
#
# Stop with SIGTERM/SIGINT (Ctrl-C); it forwards the signal and exits 0.

set -uo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

CONFIG="${RAKAZO_BRIDGE_CONFIG:-$HOME/.config/rakazo/agentsmith.toml}"
INTERVAL="${RAKAZO_BRIDGE_INTERVAL:-10}"
MAX_BACKOFF="${RAKAZO_BRIDGE_MAX_BACKOFF:-300}"

log() {
    printf '%s supervisor: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

resolve_bin() {
    if [[ -n "${RAKAZO_BRIDGE_BIN:-}" ]]; then
        printf '%s\n' "$RAKAZO_BRIDGE_BIN"
        return 0
    fi
    local candidate
    for candidate in \
        "$REPO_ROOT/target/release/rakazo-beeper" \
        "$REPO_ROOT/target/debug/rakazo-beeper" \
        "$PWD/target/release/rakazo-beeper" \
        "$PWD/target/debug/rakazo-beeper"; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

if ! BIN="$(resolve_bin)"; then
    log "no binary found: set RAKAZO_BRIDGE_BIN or build target/release/rakazo-beeper"
    exit 2
fi

if [[ ! -r "$CONFIG" ]]; then
    log "bridge config $CONFIG is not readable"
    exit 2
fi

if [[ -z "${RAKAZO_ORCH_TOKEN_FILE:-}" ]]; then
    log "warning: RAKAZO_ORCH_TOKEN_FILE is unset; the bridge will fail to authenticate"
fi

stopping=0
child=""
backoff=1

on_signal() {
    stopping=1
    log "received SIG$1; stopping"
    if [[ -n "$child" ]]; then
        kill -TERM "$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
    fi
    exit 0
}
trap 'on_signal TERM' TERM
trap 'on_signal INT' INT

log "starting: $BIN bridge --bridge-config $CONFIG serve --interval-s $INTERVAL"
while [[ "$stopping" -eq 0 ]]; do
    started=$SECONDS
    "$BIN" bridge --bridge-config "$CONFIG" serve --interval-s "$INTERVAL" &
    child=$!
    wait "$child"
    rc=$?
    child=""

    if [[ "$stopping" -eq 1 ]]; then
        break
    fi

    ran=$((SECONDS - started))
    # A run that survived a minute is healthy: reset the backoff so a later,
    # unrelated failure restarts promptly instead of waiting at the cap.
    if [[ "$ran" -ge 60 ]]; then
        backoff=1
    fi
    log "supervisor exited rc=$rc after ${ran}s; restarting in ${backoff}s"
    sleep "$backoff" &
    wait $! 2>/dev/null || true
    backoff=$((backoff * 2))
    if [[ "$backoff" -gt "$MAX_BACKOFF" ]]; then
        backoff="$MAX_BACKOFF"
    fi
done
log "stopped"
