# Rakazo as an IRC client (local rehearsal)

Status: **local only.** This increment connects rakazo-beeper as a real IRC client
to a private loopback `pi-bus-ircd`. It does not touch ports 6667/6697/6669, the
fleet bus, certificates, ZNC, production config, or SSO. The agent loop stays
unaware of Rakazo: there is no relay, no `pi-notify` tagging, and no extension.

## Shape

```
rakazo-beeper irc        ──TCP──▶  pi-bus-ircd (127.0.0.1:<private port>)
  src/irc/client.rs                  #orch replay + live bus entries
  src/irc/inbox.rs        SQLite      durable inbox
```

The later production hop is the existing stack, not a new one:

```
rakazo-beeper (CN=rakazo-beeper) ──TLS+mTLS──▶ ZNC ──▶ pi-irc-tls ──▶ pi-bus-ircd
```

Certificate paths are accepted in config but unused until that hop is approved.

## Commands

```bash
# Long-running client. Reconnects with capped backoff; never resends.
rakazo-beeper irc --inbox /tmp/irc.sqlite --nick rakazo \
  --channel '#orch' connect

# One pass for tests: JOIN, replay, then exit after the idle window.
rakazo-beeper irc --inbox /tmp/irc.sqlite --nick rakazo-test \
  --channel '#orch' connect --once

rakazo-beeper irc --inbox /tmp/irc.sqlite send  --target '#orch' --text 'hello'
rakazo-beeper irc --inbox /tmp/irc.sqlite reply --target '#orch' --msgid m1 --text 'hi'
rakazo-beeper irc --inbox /tmp/irc.sqlite react --target '#orch' --msgid m1 --emoji '👍'

# Offline proof of caps, chunk budgets, fallbacks and source identity.
rakazo-beeper irc selftest
```

`--host`, `--port`, `--nick`, `--channel`, `--inbox`, `--cert` and `--key` override
`irc.toml` (see `docs/irc-config.example.toml`). When the file is absent, the
client defaults to `127.0.0.1:6667`, nick `rakazo`, `#orch` and
`irc-inbox.sqlite`.

## Identity and dedupe

The fleet ircd currently advertises **no caps** and sends **no tags**. The client
therefore does not assume tags exist:

* `CAP LS` is parsed; an empty list is normal.
* When `msgid` is present it is the source identity: `msgid:<id>`.
* Otherwise the fallback is `src:<sha256(target|nick|ts|text|ordinal)>`, where
  `ordinal` is the per-connection receive index. JOIN replay uses the same
  ordinals, so a restart inserts nothing and prints nothing. The ircd's
  `(bus:file:line)` pointer is part of `text`, keeping the key stable when the
  replay window shifts.
* Chunk parts carrying a `msgid` are stored durably per part (`chunk:<msgid>:<i>`)
  and collapse into one logical row when all parts are present.
* When the fleet-owned ircd tag patch lands, the same column simply carries the
  server `msgid` unchanged. No schema change is required.

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

## Local rehearsal

`scripts/irc-client-e2e.sh` starts a private `pi-bus-ircd` under a throwaway
`PI_BUS_DIR` and asserts:

1. one bus post → exactly one inbox row and one printed notification;
2. a second client re-JOINs and replays the same line → zero new rows;
3. restart on the same inbox → zero new rows and zero notifications;
4. an outbound message appears once in the ircd transcript;
5. an over-long message is numbered, every line fits, and the chunks reassemble
   to the original both in the transcript and in the inbox row;
6. with no caps, reaction and reply use the plain-text fallbacks, and both are
   persisted locally first.
