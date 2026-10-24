#!/usr/bin/env bash
# irc-live-soak.sh — manual 90 s live soak of the Rakazo IRC client.
#
# NOT run in CI. It stays connected to ZNC as user `rakazo`, joins
# #rakazo-beeper, posts exactly ONE addressed repo-scope message, and asserts:
#
#   * the client logged exactly one CONNECTED and zero client-side RECONNECTs;
#   * the addressed message appears as exactly one durable inbound inbox row;
#   * ZNC status notices (the bouncer's own upstream churn) are printed as
#     STATUS lines and never stored as bus traffic.
#
# It NEVER posts to global, never sends probe/no-op text, and makes no ZNC/ircd
# changes. Use it to verify the outbound leg, not as a heartbeat.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="${RAKAZO_IRC_CONFIG:-$HOME/.config/rakazo/irc.toml}"
BIN="${RAKAZO_BIN:-$ROOT/target/debug/rakazo-beeper}"
PI_BUS_BIN="${PI_BUS_BIN:-$HOME/.pi/agent/bin/pi-bus}"
CHANNEL="${RAKAZO_SOAK_CHANNEL:-#rakazo-beeper}"
SCOPE="${RAKAZO_SOAK_SCOPE:-repo:FuturePresentLabs/rakazo-beeper}"
DURATION="${RAKAZO_SOAK_SECONDS:-90}"
TMP=""
CLIENT_PID=""

fail() { echo "LIVE SOAK FAIL: $*" >&2; exit 1; }
note() { echo "soak: $*"; }
cleanup() {
  if [[ -n "$CLIENT_PID" ]]; then kill "$CLIENT_PID" 2>/dev/null || true; fi
  if [[ -n "$TMP" ]]; then rm -rf "$TMP"; fi
}
trap cleanup EXIT

command -v sqlite3 >/dev/null || fail "sqlite3 is required"
[[ -x "$PI_BUS_BIN" ]] || fail "missing $PI_BUS_BIN"
[[ -f "$CONFIG" ]] || fail "missing IRC config $CONFIG"
case "$SCOPE" in
  repo:FuturePresentLabs/rakazo-beeper|dm:*) ;;
  *) fail "scope must be repo:FuturePresentLabs/rakazo-beeper or dm:* (got $SCOPE)" ;;
esac
[[ "$DURATION" =~ ^[0-9]+$ ]] && (( DURATION >= 30 )) \
  || fail "RAKAZO_SOAK_SECONDS must be an integer >= 30"

if [[ ! -x "$BIN" ]]; then
  note "building client"
  (cd "$ROOT" && cargo build --quiet) || fail "cargo build failed"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/rakazo-irc-soak.XXXXXX")"
INBOX="$TMP/inbox.sqlite"
NONCE="rakazo-soak-$(date -u +%Y%m%dT%H%M%SZ)-$$"
BODY="$NONCE outbound stability soak; replies now survive a connected window."
[[ "$BODY" != *probe* ]] || fail "refusing probe text"

note "identity (paths only, no secrets)"
"$BIN" irc --irc-config "$CONFIG" --inbox "$INBOX" whoami || fail "whoami failed"

note "connect as rakazo, JOIN $CHANNEL, stay connected for ${DURATION}s"
START="$(date +%s)"
"$BIN" irc --irc-config "$CONFIG" --inbox "$INBOX" \
  --channel "$CHANNEL" connect >"$TMP/client.out" 2>"$TMP/client.err" &
CLIENT_PID=$!

for _ in $(seq 1 150); do
  grep -q '^CONNECTED' "$TMP/client.out" 2>/dev/null && break
  kill -0 "$CLIENT_PID" 2>/dev/null || {
    cat "$TMP/client.err" >&2
    fail "client exited before CONNECTED"
  }
  sleep 0.1
done
grep -q '^CONNECTED' "$TMP/client.out" || {
  cat "$TMP/client.err" >&2
  fail "client did not reach CONNECTED"
}
CONNECTED_AT="$(date +%s)"

note "post exactly ONE repo-scope message: $BODY"
"$PI_BUS_BIN" post --scope "$SCOPE" --from orch --kind discovery \
  --text "$BODY" --quiet >"$TMP/post.out" 2>"$TMP/post.err" || {
  cat "$TMP/post.err" >&2
  fail "pi-bus post failed"
}

ROWS=0
for _ in $(seq 1 150); do
  ROWS="$(sqlite3 "$INBOX" \
    "SELECT count(*) FROM irc_messages
     WHERE direction='inbound' AND thread_id='$CHANNEL'
       AND body LIKE '%$NONCE%';" 2>/dev/null || echo 0)"
  [[ "$ROWS" == "1" ]] && break
  sleep 0.1
done

REMAINING=$(( CONNECTED_AT + DURATION - $(date +%s) ))
if (( REMAINING > 0 )); then sleep "$REMAINING"; fi

ELAPSED=$(( $(date +%s) - CONNECTED_AT ))
kill "$CLIENT_PID" 2>/dev/null || true
wait "$CLIENT_PID" 2>/dev/null || true
CLIENT_PID=""

RESETS="$(grep -c '^RECONNECT' "$TMP/client.err" || true)"
CONNECTED="$(grep -c '^CONNECTED' "$TMP/client.out" || true)"
STATUSES="$(grep -c '^STATUS' "$TMP/client.out" || true)"
KEEPALIVES="$(grep -c '^KEEPALIVE' "$TMP/client.out" || true)"
ROWS="$(sqlite3 "$INBOX" \
  "SELECT count(*) FROM irc_messages
   WHERE direction='inbound' AND thread_id='$CHANNEL'
     AND body LIKE '%$NONCE%';" 2>/dev/null || echo 0)"
STATUS_ROWS="$(sqlite3 "$INBOX" \
  "SELECT count(*) FROM irc_messages
   WHERE direction='inbound' AND thread_id='rakazo';" 2>/dev/null || echo 0)"

printf 'SOAK\tconnected_s=%s\tresets=%s\tconnects=%s\tkeepalives=%s\tznc_status_lines=%s\t%s_rows=%s\tstatus_rows=%s\n' \
  "$ELAPSED" "$RESETS" "$CONNECTED" "$KEEPALIVES" "$STATUSES" "$CHANNEL" "$ROWS" "$STATUS_ROWS"

[[ "$RESETS" == "0" ]] || {
  tail -20 "$TMP/client.err" >&2
  fail "client reset/reconnected $RESETS time(s)"
}
[[ "$CONNECTED" == "1" ]] || fail "expected one CONNECTED, got $CONNECTED"
[[ "$ROWS" == "1" ]] || fail "expected exactly one inbound row for $NONCE, got $ROWS"
[[ "$STATUS_ROWS" == "0" ]] || fail "ZNC status lines leaked into the bus inbox"

echo "LIVE SOAK OK: ${ELAPSED}s connected, resets=0, $CHANNEL rows=1, status_rows=0 (repo scope only; no global, no probe)"
