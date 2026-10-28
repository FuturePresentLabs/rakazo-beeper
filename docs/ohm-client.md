# Chatting with the fleet orchestrator from your phone

Status: **design + offline skeleton.** Nothing in this document has sent a real
message, opened a real connection to a phone, or changed production. It explains
the path we are building and marks honestly what is proven, what is untested, and
what is blocked.

## The path

```
  your phone                    ohmOS messenger                 pris
 ┌──────────────┐   (1) app   ┌──────────────────┐   (2) HTTP  ┌────────────────┐
 │ WhatsApp /   │ ──────────▶ │ messages already │ ──────────▶ │ rakazo-beeper  │
 │ Telegram /   │             │ on the phone     │             │ (this repo)    │
 │ SMS thread   │ ◀────────── │                  │ ◀────────── │                │
 └──────────────┘   (4) shown └──────────────────┘   (4) POST  └───────┬────────┘
                                                                       │ (3)
                                                              ┌────────▼────────┐
                                                              │ fleet           │
                                                              │ orchestrator    │
                                                              │ (tmux pane)     │
                                                              └─────────────────┘
```

You write in a normal chat on your phone. ohmOS already holds that conversation.
rakazo-beeper reads it, hands the text to the fleet orchestrator, waits for the
orchestrator's answer, and puts the answer back into the same chat. You never
open a terminal.

## Each hop, and the exact call

| Hop | Who talks to whom | Exact call | Status |
| --- | --- | --- | --- |
| 1 | your chat app → ohmOS messenger | none, it is on the phone | **proven** (ohmOS exists and stores these) |
| 2 | ohmOS messenger → rakazo-beeper | `GET /health`, `GET /inbox`, `GET /thread/<id>`, `GET /events` (live feed), and `POST /send` back | **blocked** on the phone→pris transport (below); the endpoints themselves are proven locally |
| 3 | rakazo-beeper → orchestrator | queue the line with `pi-notify send` (a queued user message/handed row); never a tmux paste into `%0` | **proven primitive** (the fleet uses it daily), not yet composed here |
| 4 | orchestrator → ohmOS messenger → phone | `POST /send` with the reply text; the phone shows it in the same thread | **unproven** end to end |

The messenger daemon speaks four plain HTTP calls (`/inbox`, `/thread/<id>`,
`/send`, plus `/events` for live updates) and defaults to listening only on the
phone itself. The byte-for-byte contract above it — signed envelopes, compact
binary framing, the transport trait, and the message schemas — is ohmOS's
`ohmos-shared`. `dumb-bridge` is only a reference implementation, not the
contract.

## Proven / unproven / blocked

| | Item |
| --- | --- |
| **Proven** | The ohmOS messenger and its four HTTP calls exist and are testable locally. The fleet roster (orchestrator + five PMs) already exists in this repo as data. Injecting text into a tmux pane and reading the pane back is a proven fleet primitive. The offline parts of this repo build and test with no network. |
| **Unproven** | That rakazo-beeper can read a *real* phone conversation and write a reply into it. That the orchestrator's answer is captured reliably (one reply per question, no duplicates, no lost messages across a restart). How a phone-side conversation maps to a fleet entry (one thread per PM, or one fleet thread). |
| **Blocked** | **The phone → pris hop.** The messenger listens only on the phone. Nothing today carries a message from the phone to pris. This is a transport choice, not a missing password. |

## The one decision needed

How should a message travel from the phone to pris? Plain options:

1. **Phone → small authenticated relay on pris over the internet** (recommended).
   The phone pushes/pulls through one extra service we own; works anywhere with
   signal; needs a key the phone holds and a place to run the relay.
2. **ohmOS's own mesh/SMS/cellular transport.** No new server; works without
   internet; slower, and needs pairing and message-size discipline.
3. **A hosted relay/queue.** Least code, but someone else's server sees the
   message.

We do not pick this silently, and we are not asking for internal identifiers or
environment-variable names. When a real credential is unavoidable it will be
requested as "open this screen, copy this field".

## What this is not

- Not the ohmphone Iroh pairing/bridge path. That is out of scope.
- Not a new messaging protocol: we consume ohmOS's existing one.
- Not live: no production RPC, no database write, no bot, no deploy, no message
  send. Sends stay disabled by default until the round trip is proven.

## Where the work stands

- `src/ohm/` — an offline ohmOS messaging-client seam (daemon HTTP client, a
  `Transport` trait with a fake impl, and a pane bridge), with offline tests. It
  contains no live endpoint and `POST /send` is off unless configured.
- `docs/ohm-config.example.toml` — the documented config example (placeholders
  only; no secrets).
- `docs/ohm-client.md` (this page) — the shared description to agree on before
  the live hop is built.

## Agent conversations (Muse, self-hosted ohmOS)

Status: **offline-complete, live leg UNVERIFIED (phone 502).** The provider and
its tests are merged-ready; the real catalog/snapshot currently answers **502**
because the provisioning service cannot reach the paired phone over Iroh. That is
recorded as the named blocker, not papered over.

This is a *provider beside* the msgd client above, not a third bridge. It uses
the provisioning `agent-conversations` API (`GET /agent-conversations`,
`GET /agent-conversations/{id}?limit=N`,
`POST /agent-conversations/{id}/messages`), which forwards over the phone's
Iroh link. It does not touch `ohm-msgd` and it does not use the hosted Rakazo
`conversationAttachments` binding.

### Exactly one owner per WhatsApp message

| concern | owner |
| --- | --- |
| Agent delivery (wake the roster `worker`) | this bridge (`[[agent]] transport = "ohmos"`) |
| Outbound agent reply | this bridge, `POST …/messages` with a stable `request_id` |
| Rakazo UI display of the same thread | the existing hosted attachment binding (unchanged) |

The two paths do not run for the same message: this bridge never posts into the
Rakazo UI, and the UI binding is never asked to deliver to an agent.

### Config and commands

See the `[[agent]]` block in `docs/ohm-config.example.toml`. `transport` is
required and explicit; `msgd` is configured through the top-level
`base_url`/`[[routes]]`, so there is no implicit fallback either way. No
`base_url` means **offline: zero network calls**.

```sh
# Read-only: HTTP status + catalog/conversation ids only. No token, no text.
rakazo-beeper ohm --ohm-config ohm.toml agent-probe --agent muse

# Dry run of a reply: endpoint status only, nothing sent.
rakazo-beeper ohm --ohm-config ohm.toml agent-reply --agent muse --text "…" --dry-run

# Real outbound; the same text retries to the same stable request id.
rakazo-beeper ohm --ohm-config ohm.toml agent-reply --agent muse --text "…"
# An explicit key is honoured when identical text must be sent twice on purpose.
rakazo-beeper ohm --ohm-config ohm.toml agent-reply --agent muse --text "…" --request-id <id>

# Inbound poll loop; delivers once via `pi-notify --kind steer --urgent`.
rakazo-beeper ohm --ohm-config ohm.toml agent-serve --agent muse
rakazo-beeper ohm --ohm-config ohm.toml agent-serve --agent muse --once
```

Inbound rules: only `from_me=false`, non-deleted rows with a non-empty phone
`native_id` are delivered, once each, durably deduped by that **phone id** (never
text), with `--ref rakazo:<conversation>:<native_id>`. Our own request ids and
our own echoes are suppressed. A delivery failure fails closed and does not
advance the ledger, so the next poll retries.

Outbound rules: the request id is the phone's idempotency key; a retry with the
same id returns the same queued result and does not create a second native send.
`queued` is never reported as delivered.

### Status semantics (no silent success)

| result | meaning |
| --- | --- |
| `status=200` | catalog/snapshot read (or send queued) |
| `status=401` | typed `unauthorized`: bearer missing/wrong/revoked |
| `status=502`/`503`/`504` | typed `degraded`: provisioning is up, phone/Iroh is not |
| `offline` | no `base_url`; zero network calls were made |

`agent-probe` and `agent-reply --dry-run` print these statuses only — never a
token, never message text — and exit non-zero when the probe saw a 401/502. A
502 is a reported `degraded` result, not a silent success. The token is read from
`token_file`/`OHM_MUSE_TOKEN_FILE`, must be owner-only (`0600`), and is redacted
even in `Debug`.

If the token file is missing, the CLI prints a one-line, human-run acquisition
command and labels the live leg **UNVERIFIED** rather than scraping a container
environment silently.

### Rollback

Delete the `[[agent]]` block (or the state file) and stop `agent-serve`. No
daemon/unit was added in this slice, so rollback is: stop the command, remove the
block. The msgd provider, the Rakazo UI attachment binding and the single-orch
path are untouched and keep working throughout.

### Fleet-wide semantics this slice respects

- `relayOnly` applies to **every** fleet bot identity (orch, every PM child, and
  any relay/worker bot the bridge creates). New bots default to `relayOnly=true`
  unless explicitly overridden; the server implementation is pm-fpl's.
- `appendAssistant` remains the **only** bot reply path. This slice never uses
  `threads.send` for a bot reply, and never for relay-only inbound in a way that
  creates a run.
- The Fleet Orchestrator thread is not archived.
- No worker bots are created here; workers are Slice A with
  `hide_workers=true` by default.
- Muse is transport-backed in this slice: no Rakazo bot is created for it.

## Beeper Desktop local API (Muse/WhatsApp without ohmOS)

Status: **offline-complete, live leg BLOCKED on two human steps.** The provider,
durable ledger, typed errors and hermetic loopback tests are in place. The real
machine currently has **nothing listening on `127.0.0.1:23373`** and **no
`~/.config/rakazo/beeper-token`**, so the live probe reports the named blockers
`not-enabled` / `token-missing` instead of inventing a success. Both are human
actions; the bridge never enables the app and never creates the token.

Beeper Desktop ships a versioned local HTTP API (`Beeper Client API`, base
`http://127.0.0.1:23373`, bearer auth). This transport uses the plain HTTP API,
not MCP:

| call | use |
| --- | --- |
| `GET /v1/info` | is the local API enabled and is the bearer accepted? |
| `GET /v1/chats?limit=N` | resolve the chat id once; probe prints ids/names only |
| `GET /v1/chats/{chatID}` | configured chat status |
| `GET /v1/chats/{chatID}/messages?limit=N` | inbound poll |
| `POST /v1/chats/{chatID}/messages` | outbound reply → `pendingMessageID` |

### Rules this transport keeps

- **Offline by default.** No `base_url` means zero network calls.
- **Identity is the Beeper message id** (`id`), never text and never an index.
  The durable ledger is `muse-beeper.json`, separate from `muse-ohmos.json`.
- **One delivery per message, including across a restart.** `isSender = true`
  rows are suppressed. A delivery failure fails closed and does not advance the
  ledger.
- **Delivery still goes through `pi-notify`,** with
  `--ref rakazo:beeper:<chatID>:<messageID>`. No pane paste, no SSH.
- **Outbound records the returned id before reporting success.** Beeper has no
  idempotency key on POST; on a transport error the client does exactly one
  bounded "did my id land?" re-read of the newest 25 messages. If the exact text
  from us is there it reports queued with that id; otherwise it returns the
  original typed error and the caller retries.
- **Token hygiene.** The bridge reads the **path only**. Startup/status prints
  the path and a masked last-4 fingerprint (`token=/path (…a1b2)`), never the
  value. The file must be `0600`; group/world-readable is a typed refusal, not a
  warning. `Debug` is redacted.
- **Typed outcomes.** `401` → `unauthorized`, `404` on a chat route →
  `chat-not-found`, connection refused → `not-enabled` (reads as "Beeper local
  API is not enabled"), missing token → `token-missing`, no `base_url` →
  `offline`.

### Commands

```sh
# Read-only: live status + chat ids/names only. Never the token or message text.
rakazo-beeper ohm --ohm-config ohm.toml agent-probe --agent muse

# One poll (exits non-zero and prints the typed code on failure).
rakazo-beeper ohm --ohm-config ohm.toml agent-serve --agent muse --once

# Loop; delivers once via `pi-notify --kind steer --urgent`.
rakazo-beeper ohm --ohm-config ohm.toml agent-serve --agent muse

# Real outbound; retries with the same request id do not double-send.
rakazo-beeper ohm --ohm-config ohm.toml agent-reply --agent muse --text "…"
```

`bridge status --json` reads the optional `[bridge] agent_config` path and, for a
`transport = "beeper"` roster agent, adds `"transport": "beeper"` plus one live
`GET /v1/info`; `last_poll_ok` / `last_error` are therefore truthful. The text
status prints the same path + masked fingerprint line.

### Human steps to make the live leg real (not code)

1. Open Beeper Desktop → Settings → Integrations, enable the local API (listener
   on `127.0.0.1:23373`).
2. Write the token to `~/.config/rakazo/beeper-token` (`0600`):
   `mkdir -p ~/.config/rakazo && chmod 700 ~/.config/rakazo && printf '%s' '<token>' > ~/.config/rakazo/beeper-token && chmod 600 ~/.config/rakazo/beeper-token`

### Migration to agora `sh-whatsapp` (one config edit, later)

Option 2 stays a config change, not a rediscovery. Change only the `[[agent]]`
block:

```toml
transport = "sh-whatsapp"
base_url = "http://<agora-one>:<port>"
token_file = "/path/to/agora-token"
# keep chat_id / platform / worker / poll_seconds / limit
```

It needs **sshd/cert access on agora-one OR a reachable endpoint+token**.
**BLOCKED on host access:** agora-one answers ping but its sshd closes
connections, so no certificate can be installed and no endpoint can be verified.
`agent-probe` prints this same one-liner when a Beeper probe fails, so the path
is not lost.
