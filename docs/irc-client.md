# Rakazo as an IRC client

Status: **live-capable.** The client connects as the ZNC user `rakazo` over TLS
to `100.108.9.93:6669`, verifies the listener against the private fleet CA, and
JOINs `#orch`. The same binary still supports the private loopback
`pi-bus-ircd` rehearsal used by tests. The agent loop stays unaware of Rakazo:
there is no relay, no `pi-notify` tagging, and no extension.

```
rakazo-beeper ──TLS (ca.pem, no client cert)──▶ ZNC (user rakazo)
  src/irc/client.rs        ──▶ pi-irc-tls ──▶ pi-bus-ircd
  src/irc/tls.rs           #orch replay + live bus entries
  src/irc/inbox.rs         SQLite durable inbox
```

No client certificate is presented. `CN=rakazo-beeper` is ZNC's **upstream**
identity to `pi-irc-tls`; it is configured in the ZNC network, not in this
client. Entering the IRC side as that CN would be a different identity.

## Config

`--config` (alias `--irc-config`) selects the file; the default is
`~/.config/rakazo/irc.toml`. See `irc.example.toml`. Secrets are paths only:

```toml
host = "100.108.9.93"
port = 6669
tls = true
cafile = "~/.config/fleet/pki/ca.pem"
password_file = "~/.config/rakazo/znc-password"   # mode 0600
nick = "rakazo"
user = "rakazo"
channels = ["#orch"]
inbox = "~/.local/share/rakazo/irc-inbox.sqlite"
verify_hostname = true
```

TLS is fail-closed: with `tls = true`, a missing/unreadable CA, a missing or
empty password file, or group/other-readable password permissions abort before
any socket is opened. Errors name paths, never contents. The password is read
once, sent as `PASS <user>:<password>`, and never printed, logged, or placed in
an error. `verify_hostname` only controls the name check; chain verification
against `cafile` always happens. ZNC's certificate carries `IP:100.108.9.93`,
so the IP host verifies normally.

## Commands

```bash
# Long-running client. Reconnects with capped backoff; never resends.
rakazo-beeper irc connect

# One pass: JOIN, replay/read until the quiet window, then exit.
rakazo-beeper irc connect --once --seconds 15

rakazo-beeper irc send  --target '#orch' --text 'rakazo-live-proof <nonce>'
rakazo-beeper irc reply --target '#orch' --msgid m1 --text 'hi'
rakazo-beeper irc react --target '#orch' --msgid m1 --emoji '👍'
rakazo-beeper irc whoami       # nick/user/channels/transport; no secrets
rakazo-beeper irc selftest     # offline caps/chunk/fallback proof

# Backfill the inbox from the fleet-bus IRC transcript (no network).
rakazo-beeper irc ingest --transcript ~/.pi/agent/fleet-bus/irc/transcript-<date>.jsonl \
  --target '#orch' --limit 1
```

`--host`, `--port`, `--nick`, `--channel`, `--inbox`, `--tls`, `--cafile`,
`--password-file`, `--user` and `--no-verify-hostname` override the file. With
no file at all the client falls back to `127.0.0.1:6667`, nick `rakazo`,
`#orch`, and `irc-inbox.sqlite`, which keeps the local rehearsal working.

The server advertises `echo-message`, so the client requests it (plus
`message-tags` and `server-time`): a successful send is acknowledged by the
server's own echo and only then marked `sent`. `CAP END` is mandatory on ZNC.

## Identity and dedupe

The fleet ircd advertises **no caps** and sends **no tags**. The client therefore
does not assume tags exist:

* `CAP LS` is parsed; an empty list is normal.
* When `msgid` is present it is the source identity: `msgid:<id>`.
* Otherwise the fallback is `src:<sha256(target|nick|ts|text|ordinal)>`, where
  `ordinal` is the per-connection receive index. JOIN replay uses the same
  ordinals, so a restart inserts nothing and prints nothing. The ircd's
  `(bus:file:line)` pointer is part of `text`, keeping the key stable when the
  replay window shifts.
* Chunk parts carrying a `msgid` are stored durably per part (`chunk:<msgid>:<i>`)
  and collapse into one logical row when all parts are present.
* `irc ingest` uses the same keys, so re-running it over the same transcript
  inserts nothing.

Inbound is written **before** it is printed or handed to a UI. Outbound is written
`pending` **before** the first socket byte. It becomes `sent` only on an echo;
otherwise it becomes `unknown` after the wait. No row is ever `delivered`, and an
unknown send is never automatically retried. Rows left `pending` by a crash are
reconciled to `unknown` on the next open.

## Wire limits and fallbacks

| Need | Advertised | Wire form | Fallback |
| --- | --- | --- | --- |
| long message | `draft/multiline` | `BATCH` + continuation tags | numbered `[i/n]` chunks, shared `msgid` when tags exist |
| reply | `draft/reply` | `+draft/reply=<msgid>` tag | separate line `> <msgid>` before the body |
| reaction | `draft/react` | `TAGMSG target +draft/react=…;+draft/reply=…` | `PRIVMSG target :reaction <msgid> <emoji>` |

Every line is checked against IRC's 512-byte limit **including tags and CRLF**
before it reaches the socket. A body containing newlines becomes separate IRC
lines; a wire line never contains CR/LF.

## Live proof

`scripts/irc-live-proof.sh` is manual, not CI. It sends **one** message to
`#orch` prefixed `rakazo-live-proof`, prints the server echo and the outbound
inbox row, then reads one inbound `#orch` line into the durable inbox. It has no
retries and exits non-zero on any failure. When `#orch` has no live traffic and
ZNC has no buffer to play, it backfills the inbound row from the fleet-bus
transcript through `irc ingest` (still the same durable-inbox path).

## Connection stability and reconnect diagnosis

The client is a client of ZNC, not of the ircd. ZNC's own upstream link
(ZNC → `pi-irc-tls` → `pi-bus-ircd`) is independent: when that link drops, ZNC
sends itself a status notice shaped like

```
:*rakazo!znc@znc.in PRIVMSG rakazo :Disconnected from IRC (Connection reset by peer). Reconnecting...
```

Those are **bouncer status lines**, not this client reconnecting, and they are
not bus traffic. The client prints them as `STATUS	<prefix>	<body>` and never
stores them in the durable inbox (the old behaviour put them in a `rakazo`
thread and made a healthy connection look unstable). A real client reconnect is
the client's own `RECONNECT	...` line on stderr; it also never resends.

The read loop is a state machine, not a timeout race:

* the socket read timeout is one second and only means "try again"; it never
  closes a healthy connection;
* the client sends `PING :rzk-keepalive-<n>` after 60 s with no traffic in
  either direction, under ZNC's 180 s `NoTrafficTimeout`;
* any inbound line (`PONG`, channel traffic, or a ZNC status line) is liveness;
* only 120 s with no traffic in either direction is a dead link;
* server `PING` is answered with `PONG` on every read path;
* reconnect backoff is 1 s, 2 s, 4 s … capped at `max_backoff_seconds`, and
  resets only after a connection has stayed up for 60 s. A server that accepts
  JOIN and immediately drops cannot make the client hammer it;
* `JOIN` is idempotent per socket; a new socket re-joins and replay dedupe
  handles the rest.

Manual 90 s soak (not CI): connects to ZNC, joins `#rakazo-beeper`, posts one
real `repo:FuturePresentLabs/rakazo-beeper` message, and asserts zero client
reconnects and exactly one durable inbound row. It never uses global scope and
never sends probe/no-op text.

```bash
scripts/irc-live-soak.sh
RAKAZO_SOAK_SECONDS=120 scripts/irc-live-soak.sh
```

## Local rehearsal

`scripts/irc-client-e2e.sh` starts a private `pi-bus-ircd` under a throwaway
`PI_BUS_DIR` with `--dry-run` (mandatory isolation: otherwise the ircd's
human-`PRIVMSG` path calls `pi-bus post` + `inject`, and `inject` registers from
the real `pi-tmux.state` regardless of `PI_BUS_DIR`, fanning rehearsal text into
live worker panes). `PI_NOTIFY_DIR` and `PI_TMUX_STATE` are also redirected. It
asserts:

1. one bus post → exactly one inbox row and one printed notification;
2. a second client re-JOINs and replays the same line → zero new rows;
3. restart on the same inbox → zero new rows and zero notifications;
4. an outbound message appears once in the ircd transcript;
5. an over-long message is numbered, every line fits, and the chunks reassemble
   to the original both in the transcript and in the inbox row;
6. with no caps, reaction and reply use the plain-text fallbacks, and both are
   persisted locally first.
