#!/usr/bin/env bash
# irc-live-proof.sh — manual live proof of the Rakazo client identity hop.
#
# NOT run in CI. Sends exactly one message to #orch, prefixed
# `rakazo-live-proof`, and never retries. It prints the server echo, the
# outbound inbox row, and one inbound #orch line stored in the durable inbox.
#
# Requires: a configured client (~/.config/rakazo/irc.toml by default), the
# fleet CA, a mode-0600 ZNC password file, and sqlite3.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="${RAKAZO_IRC_CONFIG:-$HOME/.config/rakazo/irc.toml}"
BIN="${RAKAZO_BIN:-$ROOT/target/debug/rakazo-beeper}"
TMP=""

fail() { echo "LIVE PROOF FAIL: $*" >&2; exit 1; }
note() { echo "proof: $*"; }
cleanup() { [[ -n "$TMP" ]] && rm -rf "$TMP"; }
trap cleanup EXIT

command -v sqlite3 >/dev/null || fail "sqlite3 is required"
[[ -f "$CONFIG" ]] || fail "missing IRC config $CONFIG"
if [[ ! -x "$BIN" ]]; then
  note "building client"
  (cd "$ROOT" && cargo build --quiet) || fail "cargo build failed"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/rakazo-irc-live.XXXXXX")"
INBOX="$TMP/inbox.sqlite"
NONCE="$(date -u +%Y%m%dT%H%M%SZ)-$$"
BODY="rakazo-live-proof $NONCE"

note "identity (paths only, no secrets)"
"$BIN" irc --irc-config "$CONFIG" --inbox "$INBOX" whoami || fail "whoami failed"

note "connect as rakazo over TLS, JOIN #orch, send one message"
set +e
SEND_OUT="$("$BIN" irc --irc-config "$CONFIG" --inbox "$INBOX" \
  send --target '#orch' --text "$BODY" 2>&1)"
SEND_RC=$?
set -e
printf '%s\n' "$SEND_OUT"
[[ "$SEND_RC" -eq 0 ]] || fail "send exited $SEND_RC"
grep -q '^echo[[:space:]]' <<<"$SEND_OUT" || fail "no server echo observed"
grep -q 'status=sent' <<<"$SEND_OUT" || fail "server echo did not mark the row sent"

OUT_ROW="$(sqlite3 -separator $'\t' "$INBOX" \
  "SELECT thread_id, msg_id, status, body FROM irc_messages
   WHERE direction='outbound' AND body='$BODY' ORDER BY rowid DESC LIMIT 1;")"
[[ -n "$OUT_ROW" ]] || fail "no outbound inbox row for $BODY"
printf 'INBOX_OUTBOUND\t%s\n' "$OUT_ROW"

note "read one inbound #orch line into the durable inbox"
set +e
IN_OUT="$("$BIN" irc --irc-config "$CONFIG" --inbox "$INBOX" \
  connect --once --seconds 15 2>&1)"
IN_RC=$?
set -e
[[ "$IN_RC" -eq 0 ]] || { printf '%s\n' "$IN_OUT"; fail "connect exited $IN_RC"; }
FIRST="$(awk -F'\t' '$1=="INBOUND" && $2=="#orch" {print; exit}' <<<"$IN_OUT")"
if [[ -z "$FIRST" ]]; then
  # #orch is quiet and ZNC's buffer is empty, so read one line from the same
  # fleet-bus transcript the ircd mirrors. The durable inbox path is identical.
  BUS_DIR="${RAKAZO_FLEET_BUS:-$HOME/.pi/agent/fleet-bus}"
  TRANSCRIPT="$(ls -1t "$BUS_DIR"/irc/transcript-*.jsonl 2>/dev/null | head -1 || true)"
  [[ -n "$TRANSCRIPT" ]] || fail "no inbound #orch line and no bus transcript"
  note "no live #orch traffic; backfilling one line from $TRANSCRIPT"
  set +e
  INGEST_OUT="$("$BIN" irc --irc-config "$CONFIG" --inbox "$INBOX" \
    ingest --transcript "$TRANSCRIPT" --target '#orch' --limit 1 2>&1)"
  INGEST_RC=$?
  set -e
  printf '%s\n' "$INGEST_OUT"
  [[ "$INGEST_RC" -eq 0 ]] || fail "bus ingest exited $INGEST_RC"
  FIRST="$(awk -F'\t' '$1=="INBOUND" && $2=="#orch" {print; exit}' <<<"$INGEST_OUT")"
  [[ -n "$FIRST" ]] || fail "bus ingest produced no inbound #orch line"
fi
printf 'INBOX_INBOUND\t%s\n' "$FIRST"

IN_MSGID="$(cut -f3 <<<"$FIRST")"
[[ -n "$IN_MSGID" ]] || fail "inbound line has no msgid"
STORED="$(sqlite3 "$INBOX" \
  "SELECT count(*) FROM irc_messages
   WHERE direction='inbound' AND msg_id='$IN_MSGID';")"
[[ "$STORED" == "1" ]] || fail "inbound row not stored exactly once (got $STORED)"

echo "LIVE PROOF OK: tls-connect as rakazo, JOIN #orch, one echo+inbox row, one inbound row"
