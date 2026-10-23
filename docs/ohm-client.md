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
