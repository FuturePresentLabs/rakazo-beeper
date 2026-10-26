# IRC bridge inventory (read-only)

Status: **evidence only.** This document records the paths that exist today and
the identity/dedupe keys each one uses. It makes no production change, opens no
Rakazo session, and contains no secrets.

Pinned checkouts used for this inventory:

| repo | commit |
| --- | --- |
| `rakazo-beeper` | `e99f122` (`origin/main`; the derived roster change) |
| `fpl-agents` | `826bc019` |
| `ohmos` | `849e49c` |
| `shared-infra` | `8916e0a` |
| `agentsmith` (`~/.pi/agent`) | `cef1e41` |

## 1. Rakazo / Beeper bridge (Rust, `rakazo-beeper`)

The bridge exposes selected Rakazo agents as private, unencrypted Beeper/Matrix
rooms.

| concern | path | evidence |
| --- | --- | --- |
| Beeper/Matrix appservice HTTP | `src/transport.rs` | `router()` registers `/tenants/{tenant}/_matrix/app/v1/transactions/{txn}` |
| Beeper/Matrix client | `src/matrix.rs` | ghost registration, room creation, `send_event`, media, profiles |
| durable Beeper inbox/outbox | `src/store.rs` | tables `inbox`, `transactions`, `event_nonces`, `message_links`, `send_receipts`, `run_routes`, `bindings`, `sync_cursors` |
| tenant runtime / poll | `src/service.rs` | `TenantRuntime::ingest`, `deliver_inbound`, `poll_binding` |
| Rakazo RPC client | `src/rakazo.rs` | `threads.send`, `conversationAttachments.directSend`, `messages.react`; no assistant-append RPC on this base |
| configuration | `config.example.toml` | `[[tenants]]`, `registration`, `bot_ids`, `rakazo_space_id` |
| deployment owner | `shared-infra/projects/rakazo-beeper/` | `live-deployment.json`, `main.tf`, `release-request.json` |
| personal registration path | `secrets/registration-personal.json` (runtime, not git) | `bbctl register --json sh-rakazo`; `README.md` |

**Inbound Beeper → Rakazo.** Appservice transaction or `as_sync` websocket →
`TenantRuntime::ingest(txn, payload)` → `Store::ingest(tenant, txn, events)` →
pending `inbox` row → `deliver_inbound()` → Rakazo `threads.send` with the Beeper
event id as the local nonce. Writers: `src/transport.rs`, `src/service.rs`,
`src/rich.rs::inbound_content`.

**Outbound Rakazo → Beeper.** `poll_binding()` reads Rakazo `threads.messages`
after a cursor, dedupes by `seq`, and `relay_page` writes Matrix events.
Writers: `src/service.rs`, `src/matrix.rs`, `src/store.rs`.

**Beeper identity/dedupe keys.**

| key | where |
| --- | --- |
| `(tenant, txn)` | `transactions` table; a replayed appservice transaction is ignored |
| `(tenant, event_id)` | `inbox UNIQUE(tenant,event_id)`; event id is the durable inbound identity |
| `(tenant, nonce)` → `event_id` | `event_nonces`; echo suppression for our own sends |
| `(tenant, bot, rakazo_id, part)` → `matrix_id` | `message_links`; prevents duplicate Matrix event creation per Rakazo message/chunk |
| `(tenant, bot, event_id)` → `seq` | `send_receipts`; reuses a received Rakazo seq |
| `(tenant, bot, run_id)` → `event_id` | `run_routes`; thread routing for run replies |
| `stable_id([tenant, ...])` | `src/lib.rs`; length-delimited SHA-256 used for ghost/fingerprint/transaction ids |

## 2. Rakazo agent API and fleet orchestrator code (Rust, `rakazo-beeper`)

| concern | path | evidence |
| --- | --- | --- |
| Rakazo bot/thread API | `src/rakazo.rs` | `bots`, `threads.messages`, `threads.send`, `conversationAttachments.directSend`, `messages.react` |
| allowlisted inbound watcher | `src/orch/watch.rs` | polls one bot/thread, notifies `pi-notify send --kind steer --ref rakazo:<thread>:<message>` |
| interim reply surface | `src/orch/reply.rs` | only `conversationAttachments.directSend`; no assistant append on the committed base |
| derived roster | `src/orch/roster.rs`, `src/orch/mod.rs` | `verticals.toml` + `pi-pm list --tsv`; no hardcoded PM array |
| offline fleet contract | `src/fleet/{route,a2a,tapback}.rs` | M0 only; `main.rs` is not wired to these modules |
| no assistant append | `docs/orchestrator-in-rakazo.md` | "there is no assistant-append RPC"; `orch reply` fails closed without an attachment |

**Inbound Rakazo human message → local pane.** `orch watch` keeps a durable JSON
state file with `bot_id`, `thread_id`, `last_seq`, bounded `delivered_ids`,
`own_nonces`, counters and last error. It only accepts an allowlisted bot/space/
thread. A new user message becomes one `pi-notify` `steer` with
`--ref rakazo:<threadId>:<messageId>`. A notify failure does not advance the
watermark.

**Outbound pane → Rakazo.** On the committed base there is no assistant-append
RPC. `orch reply` uses `conversationAttachments.directSend` only when the bot has
exactly one linked attachment whose label matches the bot name, with a UUID
`clientNonce`. A normal Agent bot is `unsupported: no assistant-append RPC`.

**Rakazo identity/dedupe keys.**

| key | where |
| --- | --- |
| `bot_id` + `thread_id` | watcher binding; a copied state file cannot re-point |
| `last_seq` watermark + `delivered_ids` (512) | `orch/watch.rs`; restart/replay-safe |
| `own_nonces` | `orch/watch.rs`; future echo suppression (currently empty for inbound-only) |
| `rakazo:<threadId>:<messageId>` | `pi-notify --ref`; stable wake identity |
| `clientNonce` | `threads.send`, `messages.react`, `directSend`; caller-supplied; retries reuse it |
| roster `spawnKey` (`fleet-orch-v1`, `fleet-pm-<vertical>-v1`) | `bots.create` idempotency; roots only |
| `parentBotId` + exact child name | child resolution; `spawnKey` is orchestrator-owned |
| A2A `(from, to, content_hash)` | `src/fleet/a2a.rs`; offline dedupe window |
| tapback `(escalation_id, message_id)` | `src/fleet/tapback.rs`; terminal per escalation |

## 3. Rakazo IRC client (Rust, `rakazo-beeper`)

This is a real IRC client for the fleet bus, separate from the Beeper/Matrix
bridge and from the Rakazo UI.

| concern | path | evidence |
| --- | --- | --- |
| ZNC/TLS client | `src/irc/client.rs`, `src/irc/tls.rs` | TLS, CAP, JOIN, send/reply/react, echo-message |
| durable inbox/outbox | `src/irc/inbox.rs`, `src/irc/db` via `irc-inbox.sqlite` | `irc_messages` table |
| replay/dedupe identity | `src/irc/proto.rs` | `msgid:<id>`, else `src:<sha256(target|nick|ts|text|ordinal)>` |
| run loop | `src/irc/run.rs` | reconnect, keepalive, status lines not stored |
| docs | `docs/irc-client.md` | config, wire limits, dedupe, live proof |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| `msgid:<id>` | `source_key` when the server advertises an IRCv3 `msgid` |
| `src:<sha256(target|nick|ts|text|ordinal)>` | fallback when there are no tags; ordinal is the per-connection receive index |
| `chunk:<msgid>:<i>` | tagged numbered chunk parts; assembled into one logical row |
| `out:<msgid>` | outbound intent, written `pending` before the first socket byte |
| status `pending` → `sent` / `unknown` | echo-message acknowledges; ambiguous sends are never auto-retried |

**Writers.** `src/irc/run.rs` and `src/irc/client.rs` write inbound before
printing; `src/irc/inbox.rs` writes outbound before the socket write. The
inbox is the authority for unread/acked/replied/reactions.

## 4. Fleet IRC and agent bus (agentsmith, `~/.pi/agent`)

| concern | path | evidence |
| --- | --- | --- |
| append-only bus + escalations | `bin/pi-bus` | JSONL scopes, `fleet-bus/escalations.jsonl` |
| bus ↔ IRC daemon | `bin/pi-bus-ircd.lua` | channels from scopes, DMs persisted as `dm:<a>|<b>` |
| TLS/mTLS edge | `bin/pi-irc-tls` | CN → principal, grants |
| unibus bridge | `bin/pi-bus-unibus` | `chat.v1` envelopes and transcript pipe |
| durable notes/DMs | `bin/pi-note` | `fleet-bus/notes/*.jsonl`, transcript row, `pi-notify` |
| notification outbox | `bin/pi-notify`, `extensions/inbox-delivery.ts` | `notify/outbox.jsonl`; `fs.watch` wakes an idle target via `pi.sendUserMessage` |
| identity/grants | `fleet-bus/irc/grants.json`, `docs/IDENTITY-MODEL.md` | one grant store for IRC edge and unibus |
| transcript | `fleet-bus/irc/transcript-YYYYMMDD.jsonl` | append-only JSONL; replay source |
| Muse peer grant | `fleet-bus/irc/grants.json` | principal `muse`, rooms `#muse`, `#buddies`; this is **not** the Rakazo UI chat |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| principal (`orch`, `pm:<v>`, `org:<name>`, `buddy:<name>`) | `grants.json`; one entry governs IRC and unibus |
| worker delegation `(parent_principal, worker_slug, delegation_id)` | `fleet-bus/delegations/<worker>.json` |
| deterministic transcript id = FNV-1a of `(ts,nick,cn,target,kind,text)` | `bin/pi-bus-ircd.lua`; reconnect replay maps to the same row |
| `(bus:file:line)` pointer in replayed text | keeps the client `src:` fallback stable when the replay window shifts |
| `dm-delivered.tsv` and `source-delivered.tsv` | orchestrator-pane DM dedupe and source-id dedupe |
| note `msg_id` + `prev_id` | `bin/pi-note`; duplicate `msg_id` is a no-op |
| `chat.v1` `msg_id` / `prev_id` | unibus bridge envelope |
| notify outbox row `id` + `--ref` | `bin/pi-notify`; `inbox-delivery` per-consumer ack |

## 5. ohmOS phone/attachment transport (Rust, `ohmos`)

| concern | path | evidence |
| --- | --- | --- |
| Rakazo/Muse conversation API | `services/provisioning/src/agent_conversations.rs` | `GET /messages/agent-conversations`, `GET /:id`, `POST /:id/messages` |
| Iroh phone hop | same file | ALPN `ohm-bridge/1`; catalog/snapshot/send over the paired phone |
| selected-conversation policy seam | `crates/ohmos-msg-host/src/agent_conversation.rs` | owner/device-scoped `Attachment`, `Snapshot`, `QueueReceipt` |
| native store dedupe | `crates/ohmos-msg-host/src/agent_conversation.rs` + `crates/ohmos-messaging` | `queue_outbound_once`; SQLite schema v9 atomic binding/outbox |
| docs | `docs/ohm-bridge-iroh.md`, `docs/agent-conversation-attachments.md` | topology, qualification, ownership rules |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| `(platform, remote_id)` | native conversation identity; `attachment_id()` = `<channel>:<remote_id>` |
| `(owner_id, device_id)` | `Scope`; checked on every read/write |
| `(owner, device, request_id)` | `queue_outbound_once` request key; immutable send identity |
| native outbox `local:<outbox_id>` | provisional echo identity |
| native message id (`native_id`) | snapshot/reconcile identity; edits/deletes resolved by native id |

The phone owns WhatsApp/Telegram credentials, history and the native outbox.
The provisioning service holds only the bearer and the paired Iroh endpoint.

## 6. Rakazo API conversation attachments (TypeScript, `fpl-agents`)

| concern | path | evidence |
| --- | --- | --- |
| selected-thread client | `packages/adapters/src/selected-conversation-attachments.ts` | endpoint + `/agent-conversations`; `identityKey` |
| import into Rakazo bot/thread | `packages/adapters/src/conversation-bots.ts` | `ensureBot`, `synchronize`, `nativeMessageId`, sync backoff |
| native reply send | `packages/adapters/src/native-conversation-bot-sender.ts` | `conversationBotTransportNonce`, `conversationAttachmentSend`, `nativeReplyExchange` |
| DB binding | `packages/db/prisma/schema.prisma` | `ConversationAttachment` (`bindingId`, `bindingIdentity`, `channel`, `linkedAt`) |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| attachment `id` (e.g. `muse-whatsapp`) | selected-conversation config and `ConversationAttachment` row |
| `identityKey = sha256(["selected-conversation/v1", endpoint, userId, spaceId, remoteAttachmentId, channel, remoteId])` | `selected-conversation-attachments.ts` |
| Rakazo bot id = `conversationBotId(attachment.id)` | `conversation-bots.ts::ensureBot` |
| `native:<sha256([botId, native_id])>` | `conversation-bots.ts::nativeMessageId`; imported Rakazo message id |
| `conversationBotTransportNonce(botId, clientNonce)` | UUID-shaped SHA-256; provider `clientNonce` |
| `deliveryKey` (`bot-message:...`) | native sender; immutable intent |
| `(attachmentId, clientNonce)` | `ConversationAttachmentSend` unique row; duplicate intent rejected |

## 7. The Muse chat visible in the Rakazo UI

**Answer: it is an ohmOS WhatsApp conversation attachment, not the fleet IRC
`#muse` and not a Beeper contact.**

Evidence is the dated live topology in
`~/.pi/agent/MUSE-BRIDGE-2026-10-21.md` plus the current adapter/provisioning
code:

```text
Rakazo UI / API
  CONVERSATION_ATTACHMENTS_JSON endpoint = https://ohm.fpl.dev/messages
  binding: id=muse-whatsapp label=Muse channel=whatsapp
           remoteId=1807055946647697@bot
        │  provider appends /agent-conversations
        ▼
ohm.fpl.dev → pris:12002 → container ohmos-provisioning-1
  services/provisioning/src/agent_conversations.rs
        │  Iroh ALPN ohm-bridge/1
        ▼
CAT S22 ohmphone (ohmOS bridge-iroh)
  local WhatsApp store; native outbox
        ▼
WhatsApp conversation with Muse
```

The writer of the Rakazo message rows is
`packages/adapters/src/conversation-bots.ts::synchronize`, not `threads.send`.
It calls `directSnapshot()` on the selected-conversation client, then creates or
updates `message` rows in the bot's thread:

- Rakazo bot: `conversationBotId(attachment.id)` (one bot per attachment row).
- Rakazo message id: `native:<sha256([botId, native_id])>`.
- Role: `user` when `fromMe`, otherwise `bot`.
- Edits/deletes: reconciled by the same native id, not appended as duplicates.
- Outbound from Rakazo UI: `conversation-bots.send` →
  `conversationBotTransportNonce(botId, clientNonce)` →
  `POST /messages/agent-conversations/:id/messages` → ohmOS `request_id` →
  phone `draft_id` / `queue_outbound_once`.

The fleet IRC `#muse` is a different principal and a different transcript:
`muse` is in `fleet-bus/irc/grants.json` with rooms `#muse` and `#buddies`;
its rows live in `fleet-bus/irc/transcript-YYYYMMDD.jsonl`. Nothing in
`rakazo-beeper`'s `src/`, `config*.toml` or `deploy/` references Muse. The only
Muse references in `docs/` are this inventory and its ADR.

The Muse attachment is currently a **human UI relay**. It does not hand a
message to `pi-notify`/`pi-note`, so it does not satisfy the fleet wake contract.
That is a gap to close in an adapter, not evidence that the fleet IRC room is
already wired into Rakazo.

## 8. Every writer of a message row (summary)

| ledger/table | writer(s) | key |
| --- | --- | --- |
| `rakazo-beeper` `bridge.db` `inbox` | `src/transport.rs`, `src/service.rs` | `(tenant,event_id)` |
| `bridge.db` `message_links` / `send_receipts` / `event_nonces` | `src/service.rs`, `src/store.rs` | `(tenant,bot,rakazo_id,part)`, `(tenant,bot,event_id)`, `(tenant,nonce)` |
| Rakazo `messages` table | Rakazo API (`threads.send`), fpl-agents `conversation-bots.ts::synchronize` | `clientNonce`; `native:<sha256([botId,native_id])>` |
| `irc-inbox.sqlite` `irc_messages` | `src/irc/run.rs`, `src/irc/client.rs`, `src/irc/inbox.rs` | `msgid:` / `src:` / `chunk:` / `out:` |
| `fleet-bus/irc/transcript-*.jsonl` | `bin/pi-bus-ircd.lua`, `bin/pi-bus-unibus`, `bin/pi-note` | FNV-1a transcript id; note `msg_id` |
| `fleet-bus/notes/*.jsonl` | `bin/pi-note` | `msg_id` |
| `notify/outbox.jsonl` | `bin/pi-notify`, `bin/pi-bus-ircd.lua`, `bin/pi-note` | row `id` + `--ref` |
| ohmOS phone `bridge.sqlite` / outbox | `agent_conversation.rs` / native messaging pump | `(owner,device,request_id)`, native id |
| Beeper/Matrix room timeline | `src/matrix.rs` | Matrix event id; `event_nonces` for our sends |

## 9. Commands used (read-only)

These are the exact commands used to build this inventory. They contain no
secrets and made no writes.

```sh
cd /Users/ajmwagar/src/fpl/rakazo-beeper
git rev-parse --short HEAD                         # e99f122
git log --oneline -3                               # e99f122, bc044c4, 1a3b220
rg -n "muse" src config.example.toml deploy    # no live Muse path in source/config
rg -n "CREATE TABLE|event_id|nonce|source_key|stable_id" src/store.rs src/irc src/orch src/fleet
sed -n '1,140p' src/store.rs
sed -n '1,120p' src/irc/inbox.rs

cd /Users/ajmwagar/src/fpl/ohmos
git rev-parse --short HEAD                         # 849e49c
rg -n "agent-conversations|queue_outbound_once|attachment_id|request_id" \
  services/provisioning/src/agent_conversations.rs \
  crates/ohmos-msg-host/src/agent_conversation.rs

cd /Users/ajmwagar/src/fpl/fpl-agents
git rev-parse --short HEAD                         # 826bc019
rg -n "nativeMessageId|conversationBotTransportNonce|agent-conversations|identityKey" \
  packages/adapters/src

cd ~/.pi/agent
git rev-parse --short HEAD                         # cef1e41
python3 - <<'PY'
import json
p=json.load(open('fleet-bus/irc/grants.json'))
for k,v in p.get('principals',{}).items():
    print(k, v.get('rooms'), v.get('streams'), v.get('can_dm'))
PY
# principal muse -> rooms ["#muse","#buddies"]; streams ["chat.room.muse*","chat.dm.*"]
head -c 600 fleet-bus/irc/transcript-20261026.jsonl
rg -n "fs.watch|sendUserMessage|outbox.jsonl" extensions/inbox-delivery.ts
```

Read-only verification result: the only live Muse chat path found is the
`muse-whatsapp` ohmOS conversation attachment described above. No command in
this inventory contacted Rakazo, pris, or the phone.
