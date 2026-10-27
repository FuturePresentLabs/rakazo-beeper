#!/usr/bin/env bash
# Resident supervisor for the AgentSmith bridge.
#
# Two-sided liveness: the binary is resident by default (`serve` loops; use
# `--once` for one cycle), and this wrapper restarts it on ANY exit including 0.
# A watchdog also catches the other failure shape: a process that is still alive
# but has stopped producing cycles (wedged on a hung roster, network, or lock).
#
# It never reads, writes or prints a token. The bridge reads its own service
# token from the path in RAKAZO_ORCH_TOKEN_FILE.
#
# Environment:
#   RAKAZO_BRIDGE_BIN          binary to run (default: <repo>/target/release, then
#                              <repo>/target/debug, then ./target/...)
#   RAKAZO_BRIDGE_CONFIG       bridge config (default ~/.config/rakazo/agentsmith.toml)
#   RAKAZO_BRIDGE_INTERVAL     seconds per cycle (default 10)
#   RAKAZO_BRIDGE_MAX_BACKOFF  restart backoff cap in seconds (default 300)
#   RAKAZO_BRIDGE_WATCHDOG_GRACE  extra seconds before a missing cycle is fatal
#                                 (default 5; threshold is 3*interval + grace)
#
# Stop with SIGTERM/SIGINT (Ctrl-C); it forwards the signal and exits 0.

set -uo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

CONFIG="${RAKAZO_BRIDGE_CONFIG:-$HOME/.config/rakazo/agentsmith.toml}"
INTERVAL="${RAKAZO_BRIDGE_INTERVAL:-10}"
MAX_BACKOFF="${RAKAZO_BRIDGE_MAX_BACKOFF:-300}"
WATCHDOG_GRACE="${RAKAZO_BRIDGE_WATCHDOG_GRACE:-5}"

log() {
    printf '%s supervisor: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

file_mtime() {
    if stat -f %m "$1" >/dev/null 2>&1; then
        stat -f %m "$1"
    else
        stat -c %Y "$1"
    fi
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
reader=""
backoff=1

stop_child() {
    if [[ -n "$child" ]]; then
        kill -TERM "$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
        child=""
    fi
    if [[ -n "$reader" ]]; then
        kill "$reader" 2>/dev/null || true
        wait "$reader" 2>/dev/null || true
        reader=""
    fi
}

on_signal() {
    stopping=1
    log "received SIG$1; stopping"
    stop_child
    exit 0
}
trap 'on_signal TERM' TERM
trap 'on_signal INT' INT

log "starting: $BIN bridge --bridge-config $CONFIG serve --interval-s $INTERVAL"
while [[ "$stopping" -eq 0 ]]; do
    started=$SECONDS
    rundir="$(mktemp -d "${TMPDIR:-/tmp}/rakazo-bridge.XXXXXX")"
    fifo="$rundir/out"
    heartbeat="$rundir/beat"
    if ! mkfifo "$fifo"; then
        log "mkfifo failed in $rundir"
        rm -rf "$rundir"
        exit 2
    fi
    : > "$heartbeat"

    "$BIN" bridge --bridge-config "$CONFIG" serve --interval-s "$INTERVAL" >"$fifo" 2>&1 &
    child=$!

    # Drain the child into our log while stamping every completed cycle. The
    # heartbeat is the only thing the watchdog trusts: process liveness alone is
    # not progress, and `kill -0` cannot tell a wedged daemon from a healthy one.
    (
        while IFS= read -r line; do
            printf '%s\n' "$line"
            case "$line" in
                # A failed cycle is still progress: the daemon is alive and
                # backing off. Only silence for 3 intervals + grace is wedged.
                cycle$'\t'*|cycle-error*) : > "$heartbeat" ;;
            esac
        done < "$fifo"
    ) &
    reader=$!

    wedged=0
    limit=$((INTERVAL * 3 + WATCHDOG_GRACE))
    while kill -0 "$child" 2>/dev/null; do
        sleep 1
        [[ "$stopping" -eq 1 ]] && break
        now=$(date +%s)
        last=$(file_mtime "$heartbeat" 2>/dev/null || printf '%s' "$now")
        age=$((now - last))
        if [[ "$age" -gt "$limit" ]]; then
            log "watchdog: no cycle in ${age}s (> ${limit}s); killing pid $child"
            kill -TERM "$child" 2>/dev/null || true
            wedged=1
            break
        fi
    done

    if [[ "$stopping" -eq 1 ]]; then
        stop_child
        rm -rf "$rundir"
        break
    fi

    wait "$child" 2>/dev/null
    rc=$?
    child=""
    kill "$reader" 2>/dev/null || true
    wait "$reader" 2>/dev/null || true
    reader=""
    rm -rf "$rundir"

    ran=$((SECONDS - started))
    if [[ "$wedged" -eq 1 ]]; then
        rc=124
        reason=watchdog
    else
        reason=exit
    fi
    # A run that survived a minute is healthy: reset the backoff so a later,
    # unrelated failure restarts promptly instead of waiting at the cap.
    if [[ "$ran" -ge 60 ]]; then
        backoff=1
    fi
    log "supervisor exited rc=$rc reason=$reason after ${ran}s; restarting in ${backoff}s"
    sleep "$backoff" &
    wait $! 2>/dev/null || true
    backoff=$((backoff * 2))
    if [[ "$backoff" -gt "$MAX_BACKOFF" ]]; then
        backoff="$MAX_BACKOFF"
    fi
done
log "stopped"
