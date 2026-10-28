# ADR: one `irc-bridge` core for Rakazo, Muse and the pi fleet

Status: **Proposed — evidence and decision only.** No bridge, daemon, transcript
or production path was added by this document. Repo placement is a human
decision and is not recorded as accepted here.

Reconciled against `origin/main` `41a2144` (after PR #19 "auto-add orchestrator,
PMs and Muse" and PR #20 "Muse via the self-hosted ohmOS agent-conversation
API"). This revision only corrects statements that became stale when those two
changes merged; no claim was added beyond code that exists on that commit.

Companion evidence: [`irc-bridge-inventory.md`](irc-bridge-inventory.md).
Related: [`irc-client.md`](irc-client.md),
[`fleet-contract.md`](fleet-contract.md),
[`orchestrator-in-rakazo.md`](orchestrator-in-rakazo.md).

## 1. Context

There are currently three overlapping ways a message reaches a human or an
agent:

1. **Beeper/Matrix ↔ Rakazo agent** — the Rust `rakazo-beeper` appservice.
2. **Rakazo UI ↔ an existing ohmOS conversation (the visible Muse chat)** — a
   selected WhatsApp/Telegram thread imported into Rakazo as an
   attachment-backed bot by `fpl-agents` for **display**, while a second,
   separate consumer in `rakazo-beeper`
   (`[[agent]] transport = "ohmos"`) owns **agent delivery and outbound** for
   the same native conversation.
3. **Fleet IRC / pi bus** — `pi-bus`, `pi-bus-ircd.lua`, `pi-irc-tls`,
   `pi-bus-unibus`, `pi-note`, `pi-notify`, plus the Rust IRC client in
   `rakazo-beeper`.

Each path has its own identity, dedupe, durable ledger, retry rule and
transcript. The result is not just duplication: the same logical peer (“Muse”)
has at least two unrelated identities, and a message can be delivered by one
path while a different path waits for a poll. The Muse conversation is now read
by two consumers with disjoint jobs (UI display vs agent delivery); that split
is deliberate and must be preserved, not "deduplicated" by merging accounts.

The first deliverable is therefore **evidence**, not a rewrite. This ADR records
the paths, the duplicated responsibilities, one proposed core, the repo
placement question, and a strangler order that never stops the Rakazo UI.

## 2. Current data flows

### 2.1 Beeper/Matrix ↔ Rakazo agent

```text
Beeper/Matrix
  appservice transaction  or  fi.mau.as_sync websocket
        │
        ▼
src/transport.rs  (router / websocket_session)
        │  TenantRuntime::ingest(txn, payload)
        ▼
src/store.rs  Store::ingest
  transactions(tenant,txn)          ← replay guard
  inbox UNIQUE(tenant,event_id)     ← durable inbound identity
  event_nonces(tenant,nonce)        ← echo guard
        │
        ▼
src/service.rs  deliver_inbound()
        │  Rakazo threads.send(bot, text, clientNonce = event_id)
        ▼
Rakazo agent thread
        │
        ▼
src/service.rs  poll_binding()  →  src/rakazo.rs threads.messages(after seq)
        │  dedupe by seq; message_links(tenant,bot,rakazo_id,part)
        ▼
src/matrix.rs  send_event(room, ghost, txn, ...)
        │  send_receipts(tenant,bot,event_id) → seq
        ▼
Beeper/Matrix
```

Writers of a message row on this path: `src/transport.rs`,
`src/rich.rs::inbound_content`, `src/service.rs`, `src/matrix.rs`, `src/store.rs`.

### 2.2 Rakazo UI ↔ ohmOS conversation (the visible Muse chat)

There are now **two consumers of the same ohmOS conversation with disjoint
ownership**. Neither one is allowed to do the other's job.

```text
WhatsApp / Telegram / SMS
        │  existing native conversation on the CAT S22 ohmphone
        ▼
ohmphone local store + native outbox
        │  Iroh ALPN "ohm-bridge/1"
        ▼
ohmos-provisioning-1  services/provisioning/src/agent_conversations.rs
  GET  /messages/agent-conversations
  GET  /messages/agent-conversations/:id
  POST /messages/agent-conversations/:id/messages
        │  HTTP(S), bearer; attachment id = <channel>:<remote_id>
        ├───────────────────────────────────────────────┐
        │ display owner                                  │ agent owner
        ▼                                               ▼
Rakazo API adapter                            rakazo-beeper [[agent]]
  packages/adapters/src/                      transport = "ohmos"
  selected-conversation-attachments.ts          src/ohm/agent_conversations.rs
        │ endpoint + "/agent-conversations"         │ src/ohm/ohmos.rs
        ▼                                           │ inbound from_me=false
Rakazo API                                      │   → pi-notify --kind steer --urgent
  packages/adapters/src/                        │     --ref rakazo:<conversation>:<native_id>
  conversation-bots.ts                          │ outbound reply
  ensureBot/synchronize/send                    │   → POST …/messages with stable request_id
        │                                           ▼
        ▼                                     roster `worker` pane (default: agent key)
Rakazo UI  (display only; current Muse binding id = muse-whatsapp)
```

Display path: `conversation-bots.ts::synchronize` imports native rows
(`native:<sha256([botId, native_id])>`) and the Rakazo API send path uses
`conversationBotTransportNonce(botId, clientNonce)` → ohmOS
`queue_outbound_once`. The provider echo is then imported by `synchronize`.

Agent path: `src/ohm/agent_conversations.rs` speaks exactly the three routes
above (`src/ohm/agent_conversations.rs:13-21`), offline unless an `[[agent]]`
`base_url` is configured (`src/ohm/mod.rs:13-17`), with a typed 401 and a typed
502/503/504 (`degraded`, never a success: `src/ohm/agent_conversations.rs:127-145`).
`src/ohm/ohmos.rs::poll_once` (`:352-404`) delivers each `from_me=false` row at
most once, keyed by the phone `native_id`, and `reply` (`:415-444`) posts with a
stable `request_id` so a retry replays at the phone instead of duplicating.
The ownership split is stated in `src/ohm/ohmos.rs:9-15`: this code owns agent
delivery + outbound; the hosted `conversationAttachments` binding owns display.

Writers of a message row on the display path: `conversation-bots.ts::synchronize`
(imports/upserts native messages) and the Rakazo API send path
(`conversation-bots.ts::send` → ohmOS `queue_outbound_once`).
Writers on the agent path: the ohmOS native store (phone), plus the local
dedupe ledger `src/ohm/ohmos.rs` writes to `<key>-ohmos.json`.

### 2.3 Rakazo conversation ↔ local fleet pane

```text
Rakazo UI conversation (allowlisted bot/space/thread)
        │
        ▼
src/orch/watch.rs  poll_once()
  state: bot_id, thread_id, last_seq, delivered_ids, own_nonces
        │  one user message, new seq/id
        ▼
pi-notify send --kind steer --worker <name>
  --ref rakazo:<threadId>:<messageId>
        │
        ▼
notify/outbox.jsonl
        │  extensions/inbox-delivery.ts fs.watch  (30 s poll is only fallback)
        ▼
pi.sendUserMessage(...)  → the target pane is woken

reply direction (committed base):
src/orch/reply.rs
  only conversationAttachments.directSend on exactly one linked attachment
  no assistant-append RPC; normal Agent bot = unsupported
```

Writers: `src/orch/watch.rs` writes only the notify row and its state file;
`src/orch/reply.rs` writes through `directSend`; `bin/pi-notify` writes the
outbox; `extensions/inbox-delivery.ts` marks per-consumer delivery.

**Roster source (current, not a hardcoded PM list).** The local fleet roster is
derived at runtime by `src/orch/roster.rs`: `verticals.toml` (resolved through
`PI_PM_VERTICALS`, then `PI_PM_ROOT`/`PI_AGENT_HOME`, then `~/.pi/agent`) is the
canonical vertical list, and `pi-pm list --tsv` supplies each live pane, phase
and worker count (`src/orch/roster.rs:1-21`, `:197-218`, `:249-291`). Stable keys
are derived (`fleet-orch-v1`, `fleet-pm-<vertical>-v1`), so a new PM appears from
a data edit. A paused vertical stays visible but is never routed
(`src/orch/mod.rs:181-187`).

**Which Rakazo bots the Beeper bridge polls.** PR #19 added `auto_bots`: when
`auto_bots = true` (`src/config.rs:41-44`), `select_bot_ids` adds the live space's
orchestrator root (matched by `spawnKey == "fleet-orch-v1"` or name
`Fleet Orchestrator`), **every active direct child of that root** (the PMs), and
any active bot named `Muse` (`src/service.rs:102-146`; selection is refreshed on
every discover tick, `:224-230`). Explicit `bot_ids` are still honored and
archived bots stop being polled. Selection is relationship/live-space based,
while `orch watch` remains a single allowlisted conversation
(`RAKAZO_ORCH_BOT_ID`/`THREAD_ID`) — the two are not the same list.

### 2.4 Fleet IRC / pi bus

```text
bin/pi-bus  (append-only JSONL, scopes, escalations)
        │
        ├── bin/pi-bus-ircd.lua ──► IRC clients
        │      channels from scopes; DMs persisted as dm:<a>|<b>
        │      transcript-YYYYMMDD.jsonl
        │      DM to orch → pi-notify send --kind note (kind= steer for others)
        │
        ├── bin/pi-irc-tls  (mTLS CN → principal)
        │
        ├── bin/pi-bus-unibus  (chat.v1 envelopes, transcript pipe)
        │
        ├── bin/pi-note  (durable DM/note; notes/*.jsonl; transcript; pi-notify)
        │
        └── bin/pi-notify ──► notify/outbox.jsonl ──► inbox-delivery fs.watch

bin/pi-note / pi-bus / unibus ──► transcript-YYYYMMDD.jsonl
```

Writers: `bin/pi-bus` (bus rows), `bin/pi-bus-ircd.lua` (transcript/DM rows),
`bin/pi-bus-unibus` (transcript rows), `bin/pi-note` (notes/bus/transcript),
`bin/pi-notify` (outbox).

### 2.5 Every writer of a message row (consolidated)

| Ledger | Writer(s) | Primary identity |
| --- | --- | --- |
| `rakazo-beeper` `bridge.db` `inbox` | `src/transport.rs`, `src/service.rs` | `(tenant,event_id)` |
| `bridge.db` `message_links`, `send_receipts`, `event_nonces` | `src/service.rs`, `src/store.rs` | `(tenant,bot,rakazo_id,part)`, `(tenant,bot,event_id)`, `(tenant,nonce)` |
| Rakazo `messages` | Rakazo API (`threads.send`), `conversation-bots.ts::synchronize` | `clientNonce`; `native:<sha256([botId,native_id])>` |
| `irc-inbox.sqlite` `irc_messages` | `src/irc/run.rs`, `src/irc/client.rs`, `src/irc/inbox.rs` | `msgid:` / `src:` / `chunk:` / `out:` |
| `fleet-bus/irc/transcript-*.jsonl` | `bin/pi-bus-ircd.lua`, `bin/pi-bus-unibus`, `bin/pi-note` | FNV-1a row id; note `msg_id` |
| `fleet-bus/notes/*.jsonl` | `bin/pi-note` | `msg_id` |
| `notify/outbox.jsonl` | `bin/pi-notify`, `bin/pi-bus-ircd.lua`, `bin/pi-note` | row `id` + `--ref` |
| ohmOS phone store / outbox | ohmOS messaging pump, `agent_conversation.rs` | `(owner,device,request_id)`, native id |
| Beeper/Matrix timeline | `src/matrix.rs` | Matrix event id; `event_nonces` |

## 3. The Muse answer (do not conflate the two paths)

**The Muse chat currently visible in the Rakazo UI is an ohmOS WhatsApp
conversation attachment.** It is not the fleet IRC `#muse` and it is not a
Beeper contact.

- Live binding evidence (`~/.pi/agent/MUSE-BRIDGE-2026-10-21.md`):
  `CONVERSATION_ATTACHMENTS_JSON` endpoint `https://ohm.fpl.dev/messages`,
  binding `id=muse-whatsapp`, `label=Muse`, `channel=whatsapp`,
  `remoteId=1807055946647697@bot`.
- The provider appends `/agent-conversations`; the live route is
  `ohmOS agent_conversations → Iroh → CAT S22 ohmphone → WhatsApp`.
- The Rakazo row writer is `conversation-bots.ts::synchronize`. The row identity
  is `native:<sha256([botId, native_id])>`, where `botId` is
  `conversationBotId(attachment.id)` and `native_id` is the native WhatsApp
  message id.
- Outbound from the Rakazo UI uses
  `conversationBotTransportNonce(botId, clientNonce)` → ohmOS `request_id` →
  the phone's `queue_outbound_once` key `(owner, device, request_id)`.
- The fleet IRC room `#muse` is a different identity: principal `muse` in
  `fleet-bus/irc/grants.json`, rooms `#muse`/`#buddies`, rows in
  `fleet-bus/irc/transcript-YYYYMMDD.jsonl`. It is still a distinct peer from
  the WhatsApp conversation and must not be merged with it.
- Since PR #20, `rakazo-beeper` **does** reference the WhatsApp Muse path: it is
  a second consumer of the same ohmOS `agent-conversations` API, owning agent
  delivery and outbound only. The relevant code is
  `src/ohm/agent_conversations.rs` (HTTP provider, typed 401 and typed
  502/503/504 `degraded`), `src/ohm/ohmos.rs` (poll/reply/durable ledger) and
  `src/main.rs` (`ohm agent-probe|agent-reply|agent-serve`). The Rakazo UI
  still gets its rows from `fpl-agents`, and this provider never posts into the
  Rakazo UI, so a message is not doubled (`src/ohm/ohmos.rs:9-15`).

**Ownership split, stated explicitly:** Rakazo/`fpl-agents` owns **display** of
the attachment (`conversation-bots.ts::synchronize` is the row writer); the
`rakazo-beeper` ohmOS provider owns **agent delivery and outbound** for the same
native conversation. The old blanket claim that "no source or config file under
`rakazo-beeper` references Muse" was true at `e99f122` and is now stale; it was
replaced by this split when PR #20 merged.

One consequence matters for the wake contract: the Rakazo **UI attachment** is
still a human display relay and does not call `pi-notify`/`pi-note`, so display
alone does not wake a fleet pane. The ohmOS **agent provider** does comply: it
delivers each inbound row via `pi-notify send --kind steer --urgent --ref
rakazo:<conversation>:<native_id>` (`src/ohm/ohmos.rs:121-183`, `:352-404`).
These are two different consumers; do not represent the display binding as a
fleet wake, and do not add a poller to make it one.

## 4. Duplicated responsibilities

| Responsibility | Current implementations | Problem |
| --- | --- | --- |
| **Identity** | `stable_id([tenant,...])`; Matrix ghost; `msgid`/`src:` IRC key; `native:<hash>`; `conversationBotId`; `identityKey`; ohmOS `<platform>:<remote_id>`; fleet principal + delegation; note `msg_id` | The same human/peer has several unrelated ids. There is no single place to answer “is this the same message/peer?” |
| **Dedupe** | Beeper `(tenant,txn)`, `(tenant,event_id)`, `event_nonces`, `message_links`, `send_receipts`; IRC `source_key`; orch `last_seq` + `delivered_ids` + `own_nonces`; A2A `(from,to,content_hash)`; tapback `(escalation_id,message_id)`; ohmOS `(owner,device,request_id)`; fpl-agents `(attachmentId,clientNonce)` + `native:<hash>`; `pi-note` `msg_id` | Every adapter is replay-safe only in its own ledger. A cross-path retry is a new message. |
| **Transcript** | Rakazo `messages`; `irc_messages`; `fleet-bus/irc/transcript-*.jsonl`; `notes/*.jsonl`; ohmOS phone store; Beeper room timeline | Several “sources of truth”. Replay and audit require knowing which one is authoritative. |
| **Retry** | Beeper `inbox.attempts/next_at`; IRC `pending`→`sent`/`unknown` (never resend); fpl-agents `SyncHealth` backoff + immutable send request; orch watermark not advanced on notify failure; ohmOS `queue_outbound_once` | Retry semantics differ across paths; a timeout can mean “unknown” on one path and “safe to retry” on another. |
| **Wake** | `pi-notify` outbox + `inbox-delivery` `fs.watch`; IRC DM to orch → `pi-notify`; `orch watch` → `pi-notify steer`; ohmOS agent provider → `pi-notify steer --urgent`; Rakazo UI attachment → display only | Chat delivery must be event-driven. Writing a transcript row and waiting for a poll is a bug. |
| **Routing** | `src/fleet/route.rs` (offline M0); `src/orch/roster.rs` + `src/orch/mod.rs` (derived roster); `fleet-bus/irc/grants.json` (IRC/unibus); `pi-note` target resolution | Two routing models (bot/thread and principal/room) with no shared table. |

## 5. Proposed one-core layout

```text
irc-bridge/
  core/            identity + durable inbox/outbox + routing + idempotency
  adapters/rakazo/ Rakazo/Beeper/Matrix RPC
  adapters/muse/   org:muse / #muse and the ohmOS conversation attachment
  adapters/pi/     pi-bus/pi-note/pi-notify
  irc/             ZNC/TLS client or edge
```

### 5.1 Core owns

1. **One durable ledger.** A single SQLite database (WAL, `synchronous=FULL`) is
   authoritative for inbound intent, outbound intent, receipts, echo
   suppression and delivery state. Transcripts are projections, never the
   source of truth.
2. **One message identity.** Every row gets a canonical `msg_id` derived from
   immutable fields and a namespaced native identity:

   ```text
   in:<adapter>:<account>:<native_id>      inbound from a transport
   out:<adapter>:<account>:<local_id>      outbound intent (written before send)
   echo:<adapter>:<native_id>              observed echo/ack
   ```

   The adapter must never invent a second local identity for the same native
   message. Reusing an id with different content is a contract violation.
3. **One state machine.**

   ```text
   received → queued → sent → delivered
        │         │        └→ unknown (timeout; never auto-resent)
        │         └→ failed (permanent)
        └→ duplicate (no-op)
   ```

   `pending` is written before an irreversible step. A crash is reconciled to
   `unknown`, not silently replayed. Replay is always by ledger id.
4. **Routing table.** One derived table: adapter + account + conversation →
   adapter + account + target (pane/principal/thread). The table is data, not a
   hardcoded PM array. Missing/unknown targets queue with a visible reason; no
   fallback to the orchestrator.
5. **Wake dispatcher.** A `Notifier` trait with one production implementation:
   append to `notify/outbox.jsonl` (or the equivalent `pi-note` path) carrying a
   stable `--ref`. No polling loop is added to the core. The core exposes
   “needs wake” as a durable fact; `inbox-delivery` owns the actual wake.
6. **Echo suppression.** Compare native ids, local outbound ids and adapter
   nonces; an inbound row that matches an outbound intent is marked seen and is
   not routed back.
7. **Policy injection, not policy ownership.** Identity/grants are supplied by
   the owning policy source (today `fleet-bus/irc/grants.json` for the fleet;
   tenant config for Rakazo). The core validates, it does not mint certs or
   grants.

### 5.2 Adapters own

| adapter | owns | does not own |
| --- | --- | --- |
| `adapters/rakazo` | Matrix appservice/websocket transport, tenant registration, ghost/room lifecycle, Rakazo RPC (`threads.*`, `conversationAttachments.*`, `messages.react`), per-tenant config | appservice registration (Beeper/`bbctl`), tenant secrets, Rakazo UI |
| `adapters/muse` | ohmOS `agent-conversations` HTTP + Iroh phone hop, WhatsApp/Telegram native ids, owner/device scope; and the `org:muse`/`#muse` IRC profile as a *distinct account*. **Current home of the ohmOS provider: `rakazo-beeper` `src/ohm/{agent_conversations,ohmos}.rs`; extraction means moving it behind this boundary, not writing a second one.** | phone credentials/history, ohmOS store, WhatsApp client; never merges the two Muse identities; never writes Rakazo UI rows |
| `adapters/pi` | `pi-bus`/`pi-note`/`pi-notify` calls, fleet principals, pane targets, transcript projection | grant issuance/CA, pane lifecycle, pi runtime |
| `irc/` | ZNC/TLS client or edge, CAP/msgid handling, chunking, keepalive | routing policy, transcript ownership |

The two Muse paths stay separate at the adapter boundary even though both are
“Muse”. `muse-whatsapp` is a selected native conversation; `#muse` is a fleet
IRC room. Collapsing them would be a correctness bug.

## 6. Repo placement (human decision)

| option | advantages | costs / risks |
| --- | --- | --- |
| **A. agentsmith (`~/.pi/agent`)** | Already owns `fleet-bus/irc/grants.json`, certs, `pi-bus`, `pi-note`, `pi-notify`, the transcript and the deploy. One policy owner. | It is a local-first script/home repo with no Cargo workspace or Rust release. Adding a Rust core/CI there makes agentsmith own a second toolchain and a library release it does not otherwise need. |
| **B. ohmOS** | Owns the phone/attachment protocol and has a Rust workspace. | Couples fleet routing and Beeper/Matrix to ohmOS releases. ohmOS explicitly owns the *native messaging protocol*; the fleet bridge is not its product. Rakazo and pi would depend on an ohmOS release cadence. |
| **C. new repo `FuturePresentLabs/irc-bridge`** | Clean boundary: Rust workspace for `core` + `adapters` + `irc`; both Rakazo and agentsmith consume one versioned library/CLI. No new daemon required: the core can be a library at first. Does not make either product the owner of the fleet contract. | A new repo needs an owner, CI, release policy and a decision about where grants/deploy live. Risk of a third place if agentsmith is not the policy owner. |

**Recommendation: option C, with agentsmith retaining policy ownership.**
Create `FuturePresentLabs/irc-bridge` as the single Rust workspace and make the
fleet/agentsmith owner (the same human/merge authority that owns
`~/.pi/agent`) the repo owner. `agentsmith` keeps `fleet-bus/irc/grants.json`,
certificates and deploy wiring; the new repo holds only the core, adapters and
the IRC edge/client. The first slice is a **library**, consumed by
`rakazo-beeper`; no new long-running service is introduced.

This is a human decision. Do not merge or publish the new repo until the owner
records it. If the owner prefers “no new repo”, option A is the fallback, with
the core as a Cargo workspace under `~/.pi/agent/irc-bridge` and agentsmith
explicitly accepting the Rust release burden. Option B is not recommended.

## 7. Strangler migration order (Rakazo UI stays up)

The invariant: at every phase the current Rakazo UI path can still read and
write the same conversation. New code is exercised behind a flag; old code is
removed only after parity evidence.

### Phase 0 — this PR: evidence and ADR

- Inventory and this ADR. No production change, no new daemon, no second
  transcript.
- Reconciled against `origin/main` `41a2144`, which includes PR #19 (auto-add
  orchestrator, PMs and Muse) and PR #20 (the ohmOS agent-conversation
  provider). Rebase target and component facts in this revision are those of
  that commit.
- Human decision on repo placement. If the decision is “new repo”, create it
  empty; do not move code yet.

### Phase 1 — extract the durable IRC client contract (recommended first slice)

- Extract `rakazo-beeper` `src/irc` plus the inbox/outbox state machine into a
  library crate (`irc-bridge-core` or `irc-bridge/core`) with **no behavior
  change**.
- `rakazo-beeper` consumes the library. The binary keeps the same commands,
  config and `irc-inbox.sqlite` schema; the crate is not a second daemon.
- Tests: restart with the same inbox inserts nothing; duplicate inbound inserts
  nothing; an unknown/ambiguous outbound is never automatically resent; our own
  echo does not create an inbound row; chunk reassembly is stable.
- This slice does **not** touch `src/orch/**` or `src/fleet/route.rs`.
- Base: current `origin/main` (`41a2144`, which already includes the derived
  roster, the auto-fleet selection and the ohmOS agent provider). If another
  roster/provider branch is still open in another checkout, rebase on it rather
  than duplicating its files.

### Phase 2 — `adapters/pi`

- Wire `pi-bus`/`pi-note`/`pi-notify` through the core ledger and the single
  `Notifier` contract.
- Keep `bin/pi-bus`, `bin/pi-note`, `bin/pi-notify` as compatibility CLIs; they
  become thin callers or emit the same rows. Do not create a second transcript.
- Prove: a `done`/`steer` row still wakes an idle pane via `inbox-delivery`
  `fs.watch`; replay is a no-op; a notify failure retains the row.

### Phase 3 — `adapters/muse`

- The ohmOS `agent-conversations` provider already exists in `rakazo-beeper`
  (`src/ohm/agent_conversations.rs`, `src/ohm/ohmos.rs`, CLI in `src/main.rs`),
  offline by default and owning agent delivery + outbound. Phase 3 is an
  **extraction behind the adapter boundary**, not a new implementation: the
  `org:muse`/`#muse` IRC profile stays a separate account.
- Preserve `muse-whatsapp` binding, `conversationBotId`, `native:<hash>` and
  `conversationBotTransportNonce` semantics exactly. The first version can be a
  mirror/read-through: the core records identity and intent, the existing
  `conversation-bots.ts` sync remains the writer for the Rakazo UI.
- Do not merge the WhatsApp attachment with `#muse`; do not change the Rakazo UI
  bot name or thread.
- Add the missing wake handoff only as an explicit target: a message that must
  wake a pane goes through `pi-notify`/`pi-note`; a UI-only attachment does not
  silently gain a new poller.

### Phase 4 — `adapters/rakazo` (Beeper/Matrix)

- Route Beeper inbound/outbound through the core with the same
  `(tenant,event_id)` / `event_nonces` / `message_links` / `send_receipts`
  guarantees.
- Keep the old appservice path until parity: the new adapter can shadow-write
  the ledger, compare decisions read-only, then take over behind a feature flag.
- No change to Beeper registration, tenant secrets, appservice namespace or
  room IDs during the migration.

### Phase 5 — decommission

- Remove the old path only after: one real Beeper round trip, one real Rakazo UI
  Muse round trip, one fleet pane wake, restart/replay with no duplicates, and
  a rollback rehearsal (flag off = old path).
- The old `irc-inbox.sqlite` / `bridge.db` remain readable for audit until a
  recorded retention decision. Do not delete a ledger to “clean up”.

## 8. Explicitly not in the core

- Beeper/Matrix appservice registration, `bbctl`, homeserver credentials,
  tenant onboarding, Beeper room lifecycle.
- Rakazo UI, model/agent runtime, prompt assembly, run scheduling, artifacts or
  media rendering.
- ohmOS phone pairing, WhatsApp/Telegram credentials, native history or the
  native outbox/dispatch pump.
- Fleet grant issuance, CA/cert lifecycle, delegation signing and identity
  policy ownership (the core consumes a policy interface; `grants.json` remains
  the fleet's source).
- Pane lifecycle, tmux injection, `inbox-delivery` itself. The core hands a
  durable wake fact to `pi-notify`/`pi-note`; the extension owns the wake.
- A transcript as the source of truth. The transcript is a projection.
- A second IRC daemon, a second bridge, a second transcript, or a new polling
  loop.

## 9. Wake contract (binding on every adapter)

- Chat delivery is event-driven. `inbox-delivery` watches
  `notify/outbox.jsonl` with `fs.watch` and wakes an idle target with
  `pi.sendUserMessage` (streaming: `followUp`). The 30 s interval is a fallback,
  not the primary path.
- Every adapter that must wake a pane hands the message to `pi-notify`/
  `pi-note` for the target pane, with a stable `--ref`. Writing only an
  IRC/transcript row and waiting for a poll is a bug.
- A missed message must be recoverable from the durable ledger, not “wait for
  the next tick”.
- Current status: `orch watch`, the IRC DM-to-orch path and the ohmOS agent
  provider (`ohm agent-serve` → `pi-notify --kind steer --urgent`) comply. The
  Rakazo **UI attachment** does not: it is display-only today and must not be
  represented as a fleet wake until an explicit handoff is implemented.

## 10. Open decisions for the human

1. **Repo placement:** new repo (recommended), agentsmith, or ohmOS.
2. **Muse scope:** the ohmOS conversation now has two consumers (Rakazo UI
   display, `rakazo-beeper` agent delivery/outbound) plus the separate
   `org:muse`/`#muse` IRC peer. Confirm the intended product surface: keep the
   current split, or make the IRC peer a third account? This ADR keeps all
   three separate and says do not merge them.
3. **Beeper exposure:** is Muse expected to appear as a Beeper contact, or only
   as the Rakazo UI attachment bot? Beeper only sees the bots listed in
   `bot_ids`; that is a separate configuration decision.
4. **First slice owner/base:** confirm Phase 1 on current `origin/main`
   (`41a2144`, derived roster + auto-fleet + ohmOS agent provider) and that no
   one is editing `src/orch/**` or `src/fleet/route.rs` concurrently.

## 11. Consequences

- Positive: one message identity and one ledger; replay-safe retries; a single
  place to audit “who wrote this row”; no second transcript; the Rakazo UI
  keeps working through a strangler sequence.
- Negative / cost: the adapter boundary must be explicit about native ids;
  the new repo needs CI/release ownership; the Muse paths must stay distinct;
  Phases 1–4 require temporary dual-write/read-through and a feature flag.
- Non-goal: a “universal chat” model that erases native semantics. The core
  routes and dedupes; adapters preserve their transport’s truth.

## 12. Evidence basis for this revision

- Branch `pi/irc-bridge-extraction` rebased onto `origin/main` `41a2144`.
  `git diff --stat origin/main...HEAD` shows only `docs/*.md`.
- Every file:line citation added or corrected in this revision was checked with
  `git grep -n` against that commit; the commands are listed in
  `irc-bridge-inventory.md` §9.
- No code, test, config, daemon, deploy, transcript or secret was added by this
  PR. No live network call, Rakazo session, ohmOS request or phone contact was
  made while writing it.

**Not verified here (explicitly out of scope for a docs-only PR):**

- The live ohmOS leg. `docs/ohm-client.md` records the phone/Iroh hop as
  currently returning 502 (typed `degraded`); this ADR does not claim a
  successful live snapshot, delivery or reply.
- Any deployed behavior on `agents.fpl.dev`, pris, the CAT S22 phone, Beeper or
  the fleet panes. The documents describe code and configuration, not uptime.
- Whether a real Muse message currently reaches `pi-notify` end to end; the
  code path and its offline tests exist, the production invocation does not.
- Cross-repo commits other than the pinned `rakazo-beeper` `41a2144`
  (`fpl-agents`, `ohmos`, `agentsmith` pins are unchanged from the original
  inventory).
