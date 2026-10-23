#!/usr/bin/env bash
# irc-client-e2e.sh — local-only proof for the Rakazo IRC client.
#
# Uses a *private* loopback pi-bus-ircd with a throwaway PI_BUS_DIR. It never
# touches the real fleet bus, ports 6667/6697/6669, certificates, ZNC, or any
# production config.
#
# Proves: one live delivery; re-JOIN replay dedupe; restart dedupe; outbound
# appears once in the ircd transcript; over-long messages chunk and reassemble;
# reply/reaction plain-text fallbacks are emitted.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
IRCD="${PI_BUS_IRCD:-$HOME/.pi/agent/bin/pi-bus-ircd.lua}"
PI_BUS_BIN="${PI_BUS_BIN:-$HOME/.pi/agent/bin/pi-bus}"
BIN="${RAKAZO_BIN:-$ROOT/target/debug/rakazo-beeper}"
BROKER_PID=""
CLIENT_PID=""

fail() { echo "E2E FAIL: $*" >&2; exit 1; }
note() { echo "e2e: $*"; }

cleanup() {
  if [[ -n "${CLIENT_PID}" ]]; then kill "${CLIENT_PID}" 2>/dev/null || true; fi
  if [[ -n "${BROKER_PID}" ]]; then kill "${BROKER_PID}" 2>/dev/null || true; fi
  if [[ -n "${TMP:-}" ]]; then rm -rf "$TMP"; fi
}
trap cleanup EXIT

command -v lua5.4 >/dev/null || fail "lua5.4 is required"
command -v sqlite3 >/dev/null || fail "sqlite3 is required"
[[ -x "$IRCD" || -f "$IRCD" ]] || fail "missing $IRCD"
[[ -x "$PI_BUS_BIN" ]] || fail "missing $PI_BUS_BIN"

note "building client"
cargo build --quiet
[[ -x "$BIN" ]] || fail "missing $BIN"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/rakazo-irc-e2e.XXXXXX")"
BUS="$TMP/bus"
INBOX="$TMP/inbox.sqlite"
mkdir -p "$BUS"
ID="$(date +%s)-$$"

PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
[[ -n "$PORT" ]] || fail "could not allocate a loopback port"

note "starting private ircd on 127.0.0.1:$PORT (PI_BUS_DIR=$BUS, dry-run)"
# `--dry-run` is mandatory isolation, not a shortcut: without it, an inbound
# channel PRIVMSG makes the ircd call `pi-bus post` + `pi-bus inject`. `inject`
# registers from the *real* pi-tmux state regardless of PI_BUS_DIR, so a private
# rehearsal would fan test text into live worker panes. Dry-run still writes the
# ircd transcript, which is all this script asserts on.
PI_BUS_DIR="$BUS" PI_NOTIFY_DIR="$TMP/notify" PI_TMUX_STATE="$TMP/pi-tmux.state" \
  lua5.4 "$IRCD" serve --bind 127.0.0.1 --port "$PORT" --replay 30 --dry-run \
  >"$TMP/ircd.log" 2>&1 &
BROKER_PID=$!

READY=0
for _ in $(seq 1 100); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
    READY=1
    break
  fi
  sleep 0.1
done
[[ "$READY" == "1" ]] || { cat "$TMP/ircd.log"; fail "ircd did not accept connections"; }
kill -0 "$BROKER_PID" 2>/dev/null || { cat "$TMP/ircd.log"; fail "ircd exited"; }

client() {
  "$BIN" irc --host 127.0.0.1 --port "$PORT" --nick "$1" \
    --channel '#orch' --inbox "$INBOX" "${@:2}"
}

rows() {
  sqlite3 "$INBOX" \
    "select count(*) from irc_messages where part_index is null and direction='$1';"
}

wait_rows() {
  local want="$1" direction="$2" timeout="${3:-10}"
  for _ in $(seq 1 $((timeout * 10))); do
    if [[ -f "$INBOX" ]] && [[ "$(rows "$direction" 2>/dev/null || echo 0)" == "$want" ]]; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

transcript_count() {
  grep -F -c "$1" "$TMP"/bus/irc/transcript-*.jsonl 2>/dev/null || true
}

note "start long-running client as rakazo-test"
"$BIN" irc --host 127.0.0.1 --port "$PORT" --nick rakazo-test \
  --channel '#orch' --inbox "$INBOX" connect \
  >"$TMP/client1.out" 2>"$TMP/client1.err" &
CLIENT_PID=$!

for _ in $(seq 1 100); do
  grep -q "join rakazo-test #orch" "$TMP/ircd.log" 2>/dev/null && break
  sleep 0.1
done
grep -q "join rakazo-test #orch" "$TMP/ircd.log" \
  || { cat "$TMP/ircd.log"; cat "$TMP/client1.err"; fail "client did not JOIN #orch"; }

MESSAGE="hello-e2e-$ID"
note "post one bus message and expect exactly one inbox row"
PI_BUS_DIR="$BUS" "$PI_BUS_BIN" post --scope global --from e2e \
  --kind steer --text "$MESSAGE" --quiet

wait_rows 1 inbound 10 || {
  cat "$TMP/client1.err"
  echo "ircd log:"; tail -40 "$TMP/ircd.log"
  fail "expected one inbound row, got $(rows inbound 2>/dev/null || echo missing)"
}
[[ "$(grep -c '^INBOUND' "$TMP/client1.out" || true)" == "1" ]] \
  || { cat "$TMP/client1.out"; fail "client did not print the message exactly once"; }
grep -qF "$MESSAGE" "$TMP/client1.out" || fail "printed message body missing"

note "duplicate inject: a second client re-JOINs and replays the same line"
client rakazo-dup connect --once >"$TMP/dup.out" 2>"$TMP/dup.err" || {
  cat "$TMP/dup.err"; fail "duplicate-inject client failed"
}
[[ "$(rows inbound)" == "1" ]] || fail "replay inserted extra rows: $(rows inbound)"
[[ "$(grep -c '^INBOUND' "$TMP/dup.out" || true)" == "0" ]] \
  || { cat "$TMP/dup.out"; fail "replay printed a duplicate notification"; }

note "restart: stop the client, reopen the same inbox, re-JOIN, expect zero new rows"
kill "$CLIENT_PID" 2>/dev/null || true
wait "$CLIENT_PID" 2>/dev/null || true
CLIENT_PID=""
client rakazo-restart connect --once >"$TMP/restart.out" 2>"$TMP/restart.err" || {
  cat "$TMP/restart.err"; fail "restart client failed"
}
[[ "$(rows inbound)" == "1" ]] || fail "restart inserted extra rows: $(rows inbound)"
[[ "$(grep -c '^INBOUND' "$TMP/restart.out" || true)" == "0" ]] \
  || { cat "$TMP/restart.out"; fail "restart re-delivered a message"; }

note "outbound: one message must appear once in the ircd transcript"
OUT="outbound-e2e-$ID"
client rakazo-test send --target '#orch' --text "$OUT" >"$TMP/send.out" 2>"$TMP/send.err" || {
  cat "$TMP/send.err"; fail "send failed"
}
[[ "$(transcript_count "$OUT")" == "1" ]] \
  || { cat "$TMP/send.out"; fail "outbound expected once in transcript, got $(transcript_count "$OUT")"; }

note "limits: an over-long message is numbered, capped at 512 bytes, and reassembles"
LONG="chunk-e2e-$ID-$(python3 -c 'print("A"*1200)')"
client rakazo-test send --target '#orch' --text "$LONG" >"$TMP/long.out" 2>"$TMP/long.err" || {
  cat "$TMP/long.err"; fail "long send failed"
}
grep -q "NumberedChunks" "$TMP/long.out" \
  || { cat "$TMP/long.out"; fail "long send did not use numbered chunks"; }
python3 - "$TMP" "$LONG" <<'PY'
import glob, json, sys
tmp, expected = sys.argv[1], sys.argv[2]
parts = []
for path in glob.glob(tmp + "/bus/irc/transcript-*.jsonl"):
    for line in open(path, encoding="utf-8"):
        if not line.strip():
            continue
        entry = json.loads(line)
        text = entry.get("text", "")
        if entry.get("nick") == "rakazo-test" and text.startswith("["):
            head, _, body = text.partition("] ")
            index, total = head.strip("[").split("/")
            parts.append((int(index), int(total), body))
if not parts:
    raise SystemExit("no numbered chunks in transcript")
parts.sort()
total = parts[0][1]
if len(parts) != total:
    raise SystemExit(f"expected {total} chunks, found {len(parts)}")
if [p[0] for p in parts] != list(range(1, total + 1)):
    raise SystemExit(f"chunk indexes not contiguous: {[p[0] for p in parts]}")
assembled = "".join(p[2] for p in parts)
if assembled != expected:
    raise SystemExit(f"reassembled body mismatch ({len(assembled)} != {len(expected)})")
print(f"e2e: reassembled {total} chunks into {len(assembled)} bytes")
PY
INBOX_BODY="$(sqlite3 "$INBOX" \
  "select body from irc_messages where direction='outbound' and body like 'chunk-e2e-$ID-%' order by rowid desc limit 1;")"
[[ "$INBOX_BODY" == "$LONG" ]] || fail "outbound inbox row does not hold the original body"
STATUS="$(sqlite3 "$INBOX" \
  "select status from irc_messages where direction='outbound' and body like 'chunk-e2e-$ID-%' order by rowid desc limit 1;")"
case "$STATUS" in
  pending|sent|unknown) ;;
  *) fail "invalid outbound status: $STATUS" ;;
esac
[[ "$STATUS" != "delivered" ]] || fail "outbound must never be reported delivered"

note "reaction: with no caps, the plain-text fallback is emitted"
MSGID="$(sqlite3 "$INBOX" \
  "select msg_id from irc_messages where direction='inbound' order by rowid limit 1;")"
[[ -n "$MSGID" ]] || fail "no inbound msgid to react to"
client rakazo-test react --target '#orch' --msgid "$MSGID" --emoji '👍' \
  >"$TMP/react.out" 2>"$TMP/react.err" || { cat "$TMP/react.err"; fail "react failed"; }
grep -q "Fallback" "$TMP/react.out" || { cat "$TMP/react.out"; fail "reaction did not use fallback"; }
grep -qF "reaction $MSGID 👍" "$TMP"/bus/irc/transcript-*.jsonl \
  || fail "plain-text reaction fallback missing from transcript"
[[ "$(sqlite3 "$INBOX" "select reaction from irc_messages where msg_id='$MSGID';")" == "👍" ]] \
  || fail "reaction was not persisted locally before/with the wire send"

note "reply: with no caps, the quote-line fallback is emitted"
REPLY="reply-e2e-$ID"
client rakazo-test reply --target '#orch' --msgid "$MSGID" --text "$REPLY" \
  >"$TMP/reply.out" 2>"$TMP/reply.err" || { cat "$TMP/reply.err"; fail "reply failed"; }
grep -qF "> $MSGID" "$TMP"/bus/irc/transcript-*.jsonl \
  || fail "quote-line reply fallback missing from transcript"
grep -qF "$REPLY" "$TMP"/bus/irc/transcript-*.jsonl \
  || fail "reply body missing from transcript"
[[ "$(sqlite3 "$INBOX" "select replied from irc_messages where msg_id='$MSGID';")" == "1" ]] \
  || fail "reply was not persisted locally"

note "all assertions passed"
echo "E2E OK irc-client: live=1 replay-dupe=0 restart-new=0 outbound=1 chunks=reassembled fallbacks=plain"
