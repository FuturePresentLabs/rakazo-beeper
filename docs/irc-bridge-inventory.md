# IRC bridge inventory (read-only)

Status: **evidence only.** This document records the paths that exist today and
the identity/dedupe keys each one uses. It makes no production change, opens no
Rakazo session, and contains no secrets.

## Evidence grades

Every row of every table carries exactly one grade, and the evidence cell holds
the command that produced it. A row is graded at its weakest claim.

- `[C]` **confirmed** — the cited file:line was opened *and* the claim was
exercised by a command run for this revision (`cargo test`, `git grep`, a probe).
- `[I]` **inferred** — the code was read and the citation is exact, but nothing
was executed. The cell states what would settle it.
- `[?]` **unknown** — not checkable from this checkout. The cell carries a
`settle with:` command or a named owner.

A `git grep` hit confirms *existence and location*; a `cargo test` name confirms
the *behavior* in the row. The test names below all come from one run of
`cargo test --offline` (161 + 4 + 10 + 40 + 14 + 4 + 3 + 5 + 2 passed, 0 failed).

Pinned checkouts used for this inventory (reconciled against `origin/main`
`ef050a3` for this pass; the original revision was written against `41a2144`):

| repo | commit |
| --- | --- |
| `rakazo-beeper` | `ef050a3` — `[C]` `git rev-parse --short HEAD` |
| `fpl-agents` | `826bc019` — `[C]` `git -C ~/src/fpl/fpl-agents rev-parse --short HEAD` |
| `ohmos` | `849e49c` (local HEAD is `49cdc0b`) — `[C]` `git -C ~/src/fpl/ohmos cat-file -t 849e49c`; citations below re-read at that commit with `git show 849e49c:<path>` |
| `shared-infra` | `8916e0a` (local HEAD is `88fa1e0`) — `[C]` `git -C ~/src/fpl/shared-infra ls-tree --name-only 8916e0a projects/rakazo-beeper/` |
| `agentsmith` (`~/.pi/agent`) | `cef1e41` — `[C]` `git -C ~/.pi/agent rev-parse --short HEAD` |

## 1. Rakazo / Beeper bridge (Rust, `rakazo-beeper`)

The bridge exposes selected Rakazo agents as private, unencrypted Beeper/Matrix
rooms.

| concern | path | evidence |
| --- | --- | --- |
| Beeper/Matrix appservice HTTP | `src/transport.rs` | `[C]` `git grep -n "transactions/{txn}" -- src/transport.rs` → `:40`, inside `router()` (`:31`) |
| Beeper/Matrix client | `src/matrix.rs` | `[C]` `git grep -n "pub fn ghost\|pub async fn send_event\|pub async fn room\|upload_media\|sync_profile" -- src/matrix.rs` → ghost `:60`, `send_event` `:404`, room `:143`, media `:215`, profile `:263` |
| durable Beeper inbox/outbox | `src/store.rs` | `[C]` `git grep -n "CREATE TABLE IF NOT EXISTS" -- src/store.rs` → `inbox :115`, `transactions :114`, `event_nonces :109`, `message_links :106`, `send_receipts :108`, `run_routes :110`, `bindings :99`, `sync_cursors :113` |
| tenant runtime / poll | `src/service.rs` | `[C]` `git grep -n "pub fn ingest\|pub async fn deliver_inbound\|pub async fn poll_binding" -- src/service.rs` → `:148`, `:385`, `:443` |
| Rakazo RPC client | `src/rakazo.rs` | `[C]` `git grep -n '"threads", "send"\|"conversationAttachments", "directSend"\|"threads", "react"\|"threads", "appendAssistant"' -- src/rakazo.rs` → `threads.send :424`, `directSend :335`, `messages.react :429`, **`threads.appendAssistant :409`** (corrected; the original row claimed there is no assistant-append RPC) |
| configuration | `config.example.toml` | `[C]` `git grep -n "auto_bots" -- config.example.toml src/config.rs` → `config.example.toml:23`, `src/config.rs:44` (inside `:41-44`); `[[tenants]]`/`registration`/`bot_ids` in the same file |
| selected bot set | `src/service.rs` | `[C]` `git grep -n "fn select_bot_ids\|fn selected_bot_ids" -- src/service.rs` → `:102-146`, refreshed per tick via `discover()` call at `:229` (the original `:224-230` range was the doc comment; the call is `:229`) |
| deployment owner | `shared-infra/projects/rakazo-beeper/` | `[C]` `git -C ~/src/fpl/shared-infra ls-tree --name-only 8916e0a projects/rakazo-beeper/` → `live-deployment.json`, `main.tf`, `release-request.json` (+ `tests`, `volume-catalog.json`) |
| personal registration path | `secrets/registration-personal.json` (runtime, not git) | `[?]` the file is deliberately untracked and outside this checkout. `settle with:` `ls -l secrets/registration-personal.json` on the deployment host, and `git grep -n "bbctl register" README.md` for the documented acquisition path (owner: fleet/agentsmith deploy owner) |

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
| `(tenant, txn)` | `[C]` `transactions` table, `src/store.rs:114` (`PRIMARY KEY(tenant,txn)`) |
| `(tenant, event_id)` | `[C]` `inbox UNIQUE(tenant,event_id)`, `src/store.rs:115-120` |
| `(tenant, nonce)` → `event_id` | `[C]` `event_nonces`, `src/store.rs:109` |
| `(tenant, bot, rakazo_id, part)` → `matrix_id` | `[C]` `message_links`, `src/store.rs:106` |
| `(tenant, bot, event_id)` → `seq` | `[C]` `send_receipts`, `src/store.rs:108` |
| `(tenant, bot, run_id)` → `event_id` | `[C]` `run_routes`, `src/store.rs:110` |
| `stable_id([tenant, ...])` | `[C]` `git grep -n "fn stable_id" -- src/lib.rs` → `:19` |

## 2. Rakazo agent API and fleet orchestrator code (Rust, `rakazo-beeper`)

| concern | path | evidence |
| --- | --- | --- |
| Rakazo bot/thread API | `src/rakazo.rs` | `[C]` `git grep -n '"threads", "messages"\|"threads", "send"\|directSend\|"threads", "react"\|"threads", "appendAssistant"' -- src/rakazo.rs` → `bots :297`, `threads.messages :488`, `threads.send :424`, `directSend :335`, `messages.react :429`, `threads.appendAssistant :409` |
| allowlisted inbound watcher | `src/orch/watch.rs` | `[C]` `git grep -n "pi-notify\|--ref" -- src/orch/watch.rs` → `:131`, `:164`, `:198`; behavior confirmed by `cargo test --offline orch::watch::tests::one_user_message_notifies_once_with_the_exact_ref` (pass) |
| interim reply surface | `src/orch/reply.rs` | `[C]` `git grep -n "directSend\|assistant" -- src/orch/reply.rs` → `directSend` `:5`, fail-closed strings `:69`, `:83`; confirmed by `cargo test --offline orch::reply::tests::normal_agent_has_no_reply_surface_and_sends_nothing` (pass). **Correction:** `threads.appendAssistant` *does* exist (`src/rakazo.rs:398-409`) and is used by `bridge reply` (`src/orch/bridge.rs:378`, `src/orch/mod.rs:296`) — see the corrected row below |
| derived roster | `src/orch/roster.rs`, `src/orch/mod.rs` | `[C]` `git grep -n "verticals.toml\|pi-pm" -- src/orch/roster.rs` → `:5`, `:9`, `:127`, `:137`, `:205`; no hardcoded PM array (module docs `:1-21`, live state `:195-218`, `load()` `:288`); confirmed by `cargo test --offline orch::roster::tests::added_vertical_needs_no_source_edit` (pass). The original `:249-291` range drifted; `load()` is `:288` |
| Muse agent provider (Slice B) | `src/ohm/agent_conversations.rs`, `src/ohm/ohmos.rs` | `[C]` `git grep -n "fn poll_once\|fn reply\|Ownership split" -- src/ohm/ohmos.rs` → `:9`, `:370`, `:433`; `cargo test --offline ohm::ohmos::tests::` (14 pass) and `ohm::agent_conversations::tests::catalog_snapshot_and_send_use_the_documented_paths` (pass) |
| Muse CLI surface | `src/main.rs` | `[C]` `git grep -n "AgentProbe\|AgentReply\|AgentServe" -- src/main.rs` → enum variants `:118`, `:126`, `:140` (the original `:108-140` started at the preceding `Serve` doc; corrected range `:108-150`) |
| offline fleet contract | `src/fleet/{route,a2a,tapback}.rs` | `[C]` `git grep -n "fleet::route\|fleet::a2a\|fleet::tapback\|use crate::fleet" -- src/main.rs` → no output (exit 1): `main.rs` is not wired to these modules. Modules exist (`ls src/fleet/` → `a2a.rs mod.rs route.rs tapback.rs`) |
| assistant-append reply path | `src/rakazo.rs`, `src/orch/mod.rs`, `src/orch/bridge.rs` | `[C]` `git grep -n "appendAssistant\|append_assistant" -- src/rakazo.rs src/orch/mod.rs src/orch/bridge.rs` → `rakazo.rs:409`, `orch/mod.rs:296`, `orch/bridge.rs:378`. **Corrected claim:** the old row said "no assistant append" citing `docs/orchestrator-in-rakazo.md:77`; that document line is stale. `docs/agent-bridge.md:181` and `docs/ohm-client.md:177` describe the live path: `bridge reply` → `threads.appendAssistant` with a bot-bound service token (gated on `RAKAZO_ORCH_SERVICE_TOKEN`). `orch reply` (the legacy attachment surface) still fails closed |

**Inbound Rakazo human message → local pane.** `orch watch` keeps a durable JSON
state file with `bot_id`, `thread_id`, `last_seq`, bounded `delivered_ids`,
`own_nonces`, counters and last error. It only accepts an allowlisted bot/space/
thread. A new user message becomes one `pi-notify` `steer` with
`--ref rakazo:<threadId>:<messageId>`. A notify failure does not advance the
watermark.

**Outbound pane → Rakazo.** There are two surfaces, and the old "no
assistant-append RPC" statement is **stale**. `bridge reply` appends an
assistant message in-thread via `threads.appendAssistant`
(`src/rakazo.rs:398-409`) with a bot-bound service token (`src/orch/mod.rs:296`,
`src/orch/bridge.rs:378`); it is the path documented in `docs/agent-bridge.md`
and `docs/ohm-client.md`. The older `orch reply` surface is separate: it uses
`conversationAttachments.directSend` only when the bot has exactly one linked
attachment whose label matches the bot name, with a UUID `clientNonce`, and a
normal Agent bot is `unsupported: no assistant-append RPC` *for that command*.

**Rakazo identity/dedupe keys.**

| key | where |
| --- | --- |
| `bot_id` + `thread_id` | `[C]` `git grep -n "bot_id\|thread_id" -- src/orch/watch.rs` → `WatchState` `:258-292` (fields `:262`, `:266`); a copied state file cannot re-point (same fields are the binding) |
| `last_seq` watermark + `delivered_ids` (512) | `[C]` `git grep -n "MAX_DELIVERED_IDS\|delivered_ids" -- src/orch/watch.rs` → `MAX_DELIVERED_IDS :253`, field `:271`, trim `:318-319`; `cargo test --offline orch::watch::tests::restart_from_the_same_state_file_notifies_nothing` (pass) |
| `own_nonces` | `[C]` `src/orch/watch.rs:275`, checked at `:346`; empty for the inbound-only watcher |
| `rakazo:<threadId>:<messageId>` | `[C]` `git grep -n "rakazo:" -- src/orch/watch.rs` → construction `:527`, `--ref` doc `:164`, test assertion `:715`; test `orch::watch::tests::one_user_message_notifies_once_with_the_exact_ref` (pass) |
| `clientNonce` | `[C]` `git grep -n "clientNonce" -- src/rakazo.rs` → `threads.send :420`, `messages.react :430`, `appendAssistant :405`, `directSend :336`, artifacts `:442`; caller-supplied |
| roster `spawnKey` (`fleet-orch-v1`, `fleet-pm-<vertical>-v1`) | `[C]` `git grep -n "fleet-orch-v1\|spawn_key" -- src/orch src/service.rs` → `src/service.rs:120`, root match `src/orch/mod.rs:122-126`, create `:247`; root idempotency |
| `parentBotId` + exact child name | `[C]` `git grep -n "parent_bot_id" -- src/orch/mod.rs src/service.rs` → child match `src/orch/mod.rs:130`, `src/service.rs:128`; `cargo test --offline orch::supervisor::tests::a_missing_pm_is_reported_and_never_faked` (pass) |
| A2A `(from, to, content_hash)` | `[C]` `git grep -n "content_hash" -- src/fleet/a2a.rs` → `:63`, used `:120`; offline dedupe window (`tests/fleet.rs` passes) |
| tapback `(escalation_id, message_id)` | `[C]` `git grep -n "message_id" -- src/fleet/tapback.rs` → key `:202`; `cargo test --offline tapback_duplicate_delivery_records_once_and_second_answer_is_refused` (pass) |

## 3. Muse agent provider (Rust, `rakazo-beeper`, `transport = "ohmos"`)

This is a **second consumer of the same ohmOS conversation**, not a second
bridge. It owns agent delivery and outbound; the Rakazo UI attachment still owns
display (`src/ohm/ohmos.rs:9-15`). It does not use `conversationAttachments`
and never posts into the Rakazo UI.

| concern | path | evidence |
| --- | --- | --- |
| ohmOS HTTP provider | `src/ohm/agent_conversations.rs` | `[C]` `git grep -n "agent-conversations\|CATALOG_PATH" -- src/ohm/agent_conversations.rs` → documented routes `:13-16`, `CATALOG_PATH :47`, snapshot `:706`, send `:713`; `cargo test --offline ohm::agent_conversations::tests::catalog_snapshot_and_send_use_the_documented_paths` (pass). Token from an owner-only file (`token_file_must_be_owner_only` test, pass) |
| offline default | `src/ohm/mod.rs` | `[C]` `git grep -n "Offline by default" -- src/ohm/mod.rs` → `:16-20` (corrected from `:13-17`; the range moved when the `beeper` transport was added); `is_offline` `agent_conversations.rs:513`; `cargo test --offline ohm::agent_conversations::tests::offline_default_makes_zero_network_calls` (pass) |
| typed failures | `src/ohm/agent_conversations.rs` | `[C]` `git grep -n "401 =>\|502..=504\|pub enum AgentError" -- src/ohm/agent_conversations.rs` → enum `:163`, 401 `:676`, 502/503/504 `:679` (corrected from `:127-145`, `:607-615`); `cargo test --offline ohm::agent_conversations::tests::unauthorized_and_degraded_are_typed_results` (pass) |
| inbound delivery | `src/ohm/ohmos.rs` | `[C]` `git grep -n "pub fn poll_once\|fn reference\|fn command_args\|fn deliver" -- src/ohm/ohmos.rs` → `poll_once :370-432` (corrected from `:352-404`), ref `:66`, args `:168`, delivery `:185` (corrected from `:121-183`); `cargo test --offline ohm::ohmos::tests::inbound_is_delivered_once_by_phone_id_and_never_a_pane ohm::ohmos::tests::pi_notify_delivery_is_steer_urgent_with_a_ref` (pass) |
| outbound reply | `src/ohm/ohmos.rs` | `[C]` `git grep -n "pub fn reply" -- src/ohm/ohmos.rs` → `:433-460` (corrected from `:415-444`); `cargo test --offline ohm::ohmos::tests::reply_is_idempotent_for_the_same_request_id` (pass) |
| CLI | `src/main.rs` | `[C]` `git grep -n "AgentProbe\|AgentReply\|AgentServe" -- src/main.rs` → `:118`, `:126`, `:140` (corrected range `:108-150`) |
| durable ledger | `src/ohm/ohmos.rs` | `[C]` `git grep -n "pub struct AgentState\|fn save" -- src/ohm/ohmos.rs` → `:278-292` (corrected from `:264-284`), `save :462-484` (corrected from `:465-479`); `cargo test --offline ohm::ohmos::tests::reply_state_survives_a_restart ohm::ohmos::tests::inbound_dedupe_survives_a_restart` (pass) |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| phone `native_id` | `[C]` `git grep -n "native_id" -- src/ohm/ohmos.rs` → dedupe `:352-420`, bounded `seen` `:278-292`; `cargo test --offline ohm::ohmos::tests::inbound_dedupe_survives_a_restart` (pass) |
| `(agent, text)` → `stable_request_id` = `rakazo-<agent>-<24 hex>` | `[C]` `git grep -n "pub fn stable_request_id" -- src/ohm/agent_conversations.rs` → `:738` (corrected from `:665-673`); `cargo test --offline ohm::agent_conversations::tests::stable_request_id_is_deterministic_and_agent_scoped` (pass) |
| `request_id` | `[C]` `git grep -n "own_requests\|replies" -- src/ohm/ohmos.rs` → `AgentState :278-292`, `reply` insert `:448-450`, echo suppression `:392-396` |
| conversation id (`<channel>:<remote_id>`, e.g. `whatsapp:…@bot`) | `[C]` `git grep -n "conversation_id\|platform.*remote_id" -- src/ohm/ohmos.rs src/ohm/agent_conversations.rs` → `ohmos.rs:66`, `:404-408`; catalog item `agent_conversations.rs:337-340` |

## 4. Rakazo IRC client (Rust, `rakazo-beeper`)

This is a real IRC client for the fleet bus, separate from the Beeper/Matrix
bridge and from the Rakazo UI.

| concern | path | evidence |
| --- | --- | --- |
| ZNC/TLS client | `src/irc/client.rs`, `src/irc/tls.rs` | `[C]` `git grep -n "pub fn open" -- src/irc/client.rs; git grep -n "pub fn connect" -- src/irc/tls.rs` → `client.rs:55`, `tls.rs:191`; TLS/CAP/JOIN/send/reply/react/echo are exercised by `cargo test --offline irc::client::tests::` and `irc::tls::tests::` (all pass) |
| durable inbox/outbox | `src/irc/inbox.rs` via `irc-inbox.sqlite` | `[C]` `git grep -n "CREATE TABLE IF NOT EXISTS irc_messages" -- src/irc/inbox.rs` → `:22`; `git grep -n "irc-inbox.sqlite" -- src/irc/config.rs` → `:63`. **Correction:** there is no `src/irc/db` module (`ls src/irc/` → `client config inbox keepalive mod proto run tls`); the schema and all statements live in `inbox.rs` |
| replay/dedupe identity | `src/irc/proto.rs` | `[C]` `git grep -n "fn source_key\|fn msgid_key\|"chunk:\|out:" -- src/irc/proto.rs src/irc/inbox.rs` → `proto.rs:176`, `:184`, `inbox.rs:152`, `:231`; `cargo test --offline irc::proto::tests::source_key_is_stable_for_the_same_replay_position` (pass) |
| run loop | `src/irc/run.rs` | `[C]` `git grep -n "pub fn connect" -- src/irc/run.rs` → `:285`; `cargo test --offline irc::run::tests::znc_status_messages_are_not_stored_as_bus_traffic irc::run::tests::ingest_skips_own_nick_and_dedupes_on_a_second_pass` (pass) |
| docs | `docs/irc-client.md` | `[C]` `grep -c "config\|dedupe\|wire" docs/irc-client.md` → non-zero; file present in this checkout |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| `msgid:<id>` | `[C]` `src/irc/proto.rs:184-185` (`msgid_key`) |
| `src:<sha256(target\|nick\|ts\|text\|ordinal)>` | `[C]` `src/irc/proto.rs:176-181`; `cargo test --offline irc::proto::tests::source_key_is_stable_for_the_same_replay_position` (pass) |
| `chunk:<msgid>:<i>` | `[C]` `src/irc/inbox.rs:152`; `cargo test --offline irc::inbox::tests::tagged_numbered_chunks_reassemble_into_one_row` (pass) |
| `out:<msgid>` | `[C]` `src/irc/inbox.rs:231`, `:246`, `:256` |
| status `pending` → `sent` / `unknown` | `[C]` `git grep -n "status='unknown'\|pending" -- src/irc/inbox.rs` → startup reconciliation `:103`, outbound write `:217`; `cargo test --offline irc::inbox::tests::outbound_pending_becomes_unknown_across_restart` (pass) |

**Writers.** `src/irc/run.rs` and `src/irc/client.rs` write inbound before
printing; `src/irc/inbox.rs` writes outbound before the socket write. The
inbox is the authority for unread/acked/replied/reactions.

## 5. Fleet IRC and agent bus (agentsmith, `~/.pi/agent`)

| concern | path | evidence |
| --- | --- | --- |
| append-only bus + escalations | `bin/pi-bus` | `[C]` `rg -n "escalations.jsonl" ~/.pi/agent/bin/pi-bus` → `:25` (`ESC_FILE`); file present (`ls ~/.pi/agent/fleet-bus/escalations.jsonl`) |
| bus ↔ IRC daemon | `bin/pi-bus-ircd.lua` | `[C]` `rg -n "dm:\|transcript" ~/.pi/agent/bin/pi-bus-ircd.lua` → `dm:<a>\|<b>` scope `:17-21`, transcript id `:173-182` |
| TLS/mTLS edge | `bin/pi-irc-tls` | `[C]` `rg -n "PASS cn=" ~/.pi/agent/bin/pi-irc-tls` → `:5` (CN → principal) |
| unibus bridge | `bin/pi-bus-unibus` | `[C]` `rg -n "chat.v1" ~/.pi/agent/bin/pi-bus-unibus` → `:13-21`, `SCHEMA :57` |
| durable notes/DMs | `bin/pi-note` | `[C]` `rg -n "notes/\|msg_id" ~/.pi/agent/bin/pi-note` → `:25` (`fleet-bus/notes/<principal>.jsonl`), `:384`, `:405` |
| notification outbox | `bin/pi-notify`, `extensions/inbox-delivery.ts` | `[C]` `rg -n "fs.watch\|sendUserMessage\|outbox.jsonl" ~/.pi/agent/extensions/inbox-delivery.ts` → event-driven `:13`, `:103`, outbox `:60`; `rg -n "OUTBOX=" ~/.pi/agent/bin/pi-notify` → `:54` |
| identity/grants | `fleet-bus/irc/grants.json`, `docs/IDENTITY-MODEL.md` | `[C]` `ls ~/.pi/agent/fleet-bus/irc/grants.json ~/.pi/agent/docs/IDENTITY-MODEL.md` → both present |
| transcript | `fleet-bus/irc/transcript-YYYYMMDD.jsonl` | `[C]` `ls ~/.pi/agent/fleet-bus/irc/transcript-*.jsonl` → `20261024`…`20261028` present (append-only JSONL) |
| Muse peer grant | `fleet-bus/irc/grants.json` | `[C]` `python3 -c "import json;p=json.load(open('$HOME/.pi/agent/fleet-bus/irc/grants.json'));print(p['principals']['muse']['rooms'],p['principals']['muse']['streams'])"` → `['#muse', '#buddies'] ['chat.room.muse*', 'chat.dm.*']`. This is **not** the Rakazo UI chat |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| principal (`orch`, `pm:<v>`, `org:<name>`, `buddy:<name>`) | `[C]` same `python3` grants read → keys include `orch`, `pm:fleet`, `buddy:ajmwagar`, `muse`; one entry governs IRC and unibus |
| worker delegation `(parent_principal, worker_slug, delegation_id)` | `[I]` shape read at `IDENTITY-MODEL.md:28`, `pi-note:6`, `pi-note:56`; no live delegation file exists on this host right now (`ls fleet-bus/delegations/` → `.gitkeep` only), so the runtime rule is unexecuted. `settle with:` `ls ~/.pi/agent/fleet-bus/delegations/` during a live spawn |
| deterministic transcript id = FNV-1a of `(ts,nick,cn,target,kind,text)` | `[C]` `rg -n "fnv1a64" ~/.pi/agent/bin/pi-bus-ircd.lua` → `:173-182` |
| `(bus:file:line)` pointer in replayed text | `[C]` `rg -n "bus:%s" ~/.pi/agent/bin/pi-bus-ircd.lua` → `:755`, line cache `:1017` |
| `dm-delivered.tsv` and `source-delivered.tsv` | `[C]` `ls ~/.pi/agent/fleet-bus/irc/*-delivered.tsv` → both present |
| note `msg_id` + `prev_id` | `[C]` `rg -n "msg_id|prev_id" ~/.pi/agent/bin/pi-note` → `:384`, `:405`, `:618` |
| `chat.v1` `msg_id` / `prev_id` | `[C]` `rg -n "msg_id|prev_id" ~/.pi/agent/bin/pi-bus-unibus` → envelope `:19-21`, `SCHEMA :57` |
| notify outbox row `id` + `--ref` | `[C]` `rg -n "OUTBOX|--ref" ~/.pi/agent/bin/pi-notify` → outbox `:54`, append `:434`, flag parse `:1239` |

## 6. ohmOS phone/attachment transport (Rust, `ohmos`)

| concern | path | evidence |
| --- | --- | --- |
| Rakazo/Muse conversation API | `services/provisioning/src/agent_conversations.rs` | `[C]` `git -C ~/src/fpl/ohmos show 849e49c:services/provisioning/src/agent_conversations.rs \| grep -n "agent-conversations"` → `:250`, `:314`, `:365` (`GET` catalog, `GET /:id`, `POST /:id/messages`) |
| Iroh phone hop | same file | `[C]` same `git show … \| grep -n "ohm-bridge/1"` → ALPN const `:21`; the live hop itself was **not** exercised here (see §8) |
| selected-conversation policy seam | `crates/ohmos-msg-host/src/agent_conversation.rs` | `[C]` `git -C ~/src/fpl/ohmos show 849e49c:crates/ohmos-msg-host/src/agent_conversation.rs \| grep -n "struct Scope\|pub struct Attachment\|Snapshot\|QueueReceipt"` → `Scope :14`, `Attachment :37`, snapshot schema `:94`, outbound request `:152-200` |
| native store dedupe | `crates/ohmos-msg-host/src/agent_conversation.rs` + `crates/ohmos-messaging` | `[C]` `git -C ~/src/fpl/ohmos grep -n "fn queue_outbound_once" 849e49c -- crates` → `ohmos-messaging/src/lib.rs:258`, `sqlite_store.rs:739`, `store_mem.rs:409`; `git grep -n "SCHEMA_VERSION: i64" 849e49c` → `sqlite_store.rs:15` = 9. Atomic binding/outbox semantics are unexecuted here; `settle with:` `cargo test -p ohmos-messaging queue_outbound_once` in `~/src/fpl/ohmos` |
| docs | `docs/ohm-bridge-iroh.md`, `docs/agent-conversation-attachments.md` | `[C]` `git -C ~/src/fpl/ohmos ls-tree --name-only 849e49c docs/` → both files present |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| `(platform, remote_id)` | `[C]` `git -C ~/src/fpl/ohmos show 849e49c:crates/ohmos-msg-host/src/agent_conversation.rs \| grep -n "platform\|remote_id"` → fields `:37-38`, resolve `:213-223`; `attachment_id()` = `<channel>:<remote_id>` in `services/provisioning/src/agent_conversations.rs:236` |
| `(owner_id, device_id)` | `[I]` `Scope` fields read at `:14-26`; the "checked on every read/write" part is unexecuted here. `settle with:` `cargo test -p ohmos-msg-host scope` in `~/src/fpl/ohmos` |
| `(owner, device, request_id)` | `[C]` `queue_outbound_once` call `:179-190` passes `scope.owner_id`, `scope.device_id`, `request_id`; immutability was only read from the store contract; `settle with:` same `cargo test -p ohmos-msg-host` command |
| native outbox `local:<outbox_id>` | `[C]` same file `:200` (`local_echo_id: format!("local:{outbox_id}")`) |
| native message id (`native_id`) | `[C]` `Attachment.native_id :60`, snapshot row `:76`; edits/deletes resolved by native id was only read from the snapshot schema (`settle with:` `cargo test -p ohmos-msg-host`) |

The phone owns WhatsApp/Telegram credentials, history and the native outbox.
The provisioning service holds only the bearer and the paired Iroh endpoint.

Current in-repo consumer: `rakazo-beeper` `src/ohm/agent_conversations.rs`
(HTTP client) + `src/ohm/ohmos.rs` (poll/reply/ledger), configured by an
`[[agent]] transport = "ohmos"` block. It is a second consumer beside the
`fpl-agents` display binding, not a replacement for it (§3, §8).

## 7. Rakazo API conversation attachments (TypeScript, `fpl-agents`)

| concern | path | evidence |
| --- | --- | --- |
| selected-thread client | `packages/adapters/src/selected-conversation-attachments.ts` | `[C]` `rg -n "agent-conversations\|selected-conversation/v1" ~/src/fpl/fpl-agents/packages/adapters/src/selected-conversation-attachments.ts` → endpoint `:186`, `identityKey :231-234` |
| import into Rakazo bot/thread | `packages/adapters/src/conversation-bots.ts` | `[C]` `rg -n "ensureBot\|synchronize\|nativeMessageId" …/conversation-bots.ts` → `:100`, `:126`, `:72-73`; `conversationBotId :111`. Backoff policy was only read (`SyncHealth :43-45`, `:303`, `:328`) and not executed; `settle with:` `pnpm -C ~/src/fpl/fpl-agents test conversation-bots` |
| native reply send | `packages/adapters/src/native-conversation-bot-sender.ts` | `[C]` `rg -n "conversationBotTransportNonce\|conversationAttachmentSend\|nativeReplyExchange\|deliveryKey" …/native-conversation-bot-sender.ts` → nonce `:12`, deliveryKey `:33`/`:50`, `nativeReplyExchange :98`, `conversationAttachmentSend :155` |
| DB binding | `packages/db/prisma/schema.prisma` | `[C]` `sed -n '520,539p' ~/src/fpl/fpl-agents/packages/db/prisma/schema.prisma` → `model ConversationAttachment` `:520`, `bindingId :527`, `bindingIdentity :528`, `channel :530`, `linkedAt :531`, unique `(userId,spaceId,bindingId)` `:537` |

**Identity/dedupe keys.**

| key | where |
| --- | --- |
| attachment `id` (e.g. `muse-whatsapp`) | `[C]` `ConversationAttachment.id` (`schema.prisma:522`), used as the selected-conversation config id (`conversation-bots.ts:111`) |
| `identityKey = sha256(["selected-conversation/v1", endpoint, userId, spaceId, remoteAttachmentId, channel, remoteId])` | `[C]` `selected-conversation-attachments.ts:231-234` (the hash inputs are in that block) |
| Rakazo bot id = `conversationBotId(attachment.id)` | `[C]` `conversation-bots.ts:111` (imported `:17`) |
| `native:<sha256([botId, native_id])>` | `[C]` `conversation-bots.ts:72-73` (`nativeMessageId`), used for create/update `:173`, `:246-260` |
| `conversationBotTransportNonce(botId, clientNonce)` | `[C]` `native-conversation-bot-sender.ts:12`; test `packages/adapters/src/native-conversation-bot-sender.test.ts:11-15` asserts the shape and scoping (fixture only; suite not run here) |
| `deliveryKey` (`bot-message:...`) | `[C]` `native-conversation-bot-sender.ts:33`, `:50-51`, used `:78` |
| `(attachmentId, clientNonce)` | `[C]` `schema.prisma:571` (`@@unique([attachmentId, clientNonce], map: "conversation_attachment_sends_request_key")`); client read `native-conversation-bot-sender.ts:155` |

## 8. The Muse chat visible in the Rakazo UI

**Answer: it is an ohmOS WhatsApp conversation attachment, not the fleet IRC
`#muse` and not a Beeper contact.**

Evidence is the dated live topology in
`~/.pi/agent/MUSE-BRIDGE-2026-10-21.md` plus the current adapter/provisioning
code:

```text
              Rakazo UI / API                         rakazo-beeper [[agent]]
  binding id=muse-whatsapp, channel=whatsapp      transport = "ohmos"
  endpoint https://ohm.fpl.dev/messages            src/ohm/agent_conversations.rs
        │ display owner                                  │ agent owner
        │   /agent-conversations                          │   GET /agent-conversations
        │                                                 │   GET /{id}?limit=N
        │                                                 │   POST /{id}/messages
        └───────────────────┬─────────────────────────────┘
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

**The UI display path** (`fpl-agents`) and **the agent path** (`rakazo-beeper`)
read the same native conversation but own disjoint outcomes: display vs agent
delivery/outbound. See §3 for the provider and the ownership table below for
the split.

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
its rows live in `fleet-bus/irc/transcript-YYYYMMDD.jsonl`. It remains a
separate peer from the WhatsApp conversation and must not be merged with it.

### Ownership split after PR #19 and PR #20

The claim that "nothing in `rakazo-beeper` references Muse" was true at
`e99f122` and is **stale** as of `41a2144`: `src/ohm/ohmos.rs` and
`src/ohm/agent_conversations.rs` now consume the same ohmOS conversation, and
`src/service.rs` auto-selects a bot named `Muse`. The ownership is deliberately
split, one owner per WhatsApp message (`src/ohm/ohmos.rs:9-15`):

| concern | owner | evidence |
| --- | --- | --- |
| Rakazo UI display of `muse-whatsapp` | `fpl-agents` `conversation-bots.ts` (unchanged) | `[C]` `rg -n "nativeMessageId\|conversationBotTransportNonce" ~/src/fpl/fpl-agents/packages/adapters/src/conversation-bots.ts …/native-conversation-bot-sender.ts` → `nativeMessageId :72`, nonce `:12` |
| Agent delivery of inbound Muse rows | `rakazo-beeper` `[[agent]] transport = "ohmos"` | `[C]` `git grep -n "fn poll_once" -- src/ohm/ohmos.rs` → `:370-432` (corrected from `:352-404`); delivery args `:168-183`; `cargo test --offline ohm::ohmos::tests::inbound_is_delivered_once_by_phone_id_and_never_a_pane` (pass) |
| Outbound agent reply | `rakazo-beeper`, same provider | `[C]` `git grep -n "pub fn reply" -- src/ohm/ohmos.rs` → `:433-460` (corrected from `:415-444`); `cargo test --offline ohm::ohmos::tests::reply_is_idempotent_for_the_same_request_id` (pass) |
| Provider HTTP/typed errors | `rakazo-beeper` `src/ohm/agent_conversations.rs` | `[C]` routes `:13-16`, `AgentError :163`, 401 `:676`, 502 `:679`, offline `src/ohm/mod.rs:16-20` (line ranges corrected); `cargo test --offline ohm::agent_conversations::tests::unauthorized_and_degraded_are_typed_results` (pass) |
| Beeper bridge auto-selection | `rakazo-beeper` `src/service.rs` | `[C]` `git grep -n "fn select_bot_ids" -- src/service.rs` → `:102-146`, refresh call `:229`; `cargo test --offline service::selection_tests::auto_fleet_selects_orchestrator_children_and_muse_only` (pass) |

Consequences for the wake contract:

- The ohmOS **agent provider** complies: it hands each inbound row to
  `pi-notify` (`src/ohm/ohmos.rs:168-183`), deduped by phone `native_id`, and
the production command is `ohm agent-serve` (`src/main.rs:140`; the original
`src/ohm/ohmos.rs:121-183` and `src/main.rs:132-140` ranges drifted).
- The Rakazo **UI attachment** is still display-only and does not call
  `pi-notify`/`pi-note` (`[I]`: read from `conversation-bots.ts`; no wake call in
  the adapter). That is not a bug in the display binding and must not be
  "fixed" by adding a poller; a wake belongs to the agent provider. `settle
  with:` `rg -n "pi-notify|pi-note" ~/src/fpl/fpl-agents/packages/adapters/src`
  (no hits) plus `rg -n "pi-notify" src/ohm/ohmos.rs` (hits).
- The split is what keeps a message from being doubled: the provider never
  posts into the Rakazo UI, and the UI binding is never asked to deliver to an
  agent (`src/ohm/ohmos.rs:9-15`).

The live leg is **not** confirmed. `docs/ohm-client.md` records the phone/Iroh
hop returning 502 (typed `degraded`), so the provider is offline-complete and
its real snapshot/delivery is `[?]` here: `settle with:` run
`rakazo-beeper ohm agent-probe --agent muse` against the deployed
`https://ohm.fpl.dev/messages` with a valid owner-only token (owner: ohmOS/
phone owner), or `curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer <token>" https://ohm.fpl.dev/messages/agent-conversations` on pris.

## 9. Every writer of a message row (summary)

| ledger/table | writer(s) | key |
| --- | --- | --- |
| `rakazo-beeper` `bridge.db` `inbox` | `src/transport.rs`, `src/service.rs` | `[C]` `(tenant,event_id)`; `src/store.rs:115-120`, writer `src/service.rs:148` |
| `bridge.db` `message_links` / `send_receipts` / `event_nonces` | `src/service.rs`, `src/store.rs` | `[C]` `(tenant,bot,rakazo_id,part)`, `(tenant,bot,event_id)`, `(tenant,nonce)`; schema `src/store.rs:106-109`, `git grep -n "message_links\|send_receipts\|event_nonces" -- src/service.rs` → hits |
| Rakazo `messages` table | Rakazo API (`threads.send`), fpl-agents `conversation-bots.ts::synchronize` | `[C]` `clientNonce` (`src/rakazo.rs:420`); `native:<sha256([botId,native_id])>` (`conversation-bots.ts:72-73`) |
| `irc-inbox.sqlite` `irc_messages` | `src/irc/run.rs`, `src/irc/client.rs`, `src/irc/inbox.rs` | `[C]` `msgid:` / `src:` / `chunk:` / `out:`; `src/irc/inbox.rs:22`, `:152`, `:231`; `cargo test --offline irc::inbox::tests::` (pass) |
| `fleet-bus/irc/transcript-*.jsonl` | `bin/pi-bus-ircd.lua`, `bin/pi-bus-unibus`, `bin/pi-note` | `[C]` FNV-1a transcript id (`bin/pi-bus-ircd.lua:173-182`); note `msg_id` (`bin/pi-note:384`) |
| `fleet-bus/notes/*.jsonl` | `bin/pi-note` | `[C]` `msg_id` (`bin/pi-note:384`), path `:25` |
| `notify/outbox.jsonl` | `bin/pi-notify`, `bin/pi-bus-ircd.lua`, `bin/pi-note` | `[C]` row `id` + `--ref` (`bin/pi-notify:54`, `:434`) |
| ohmOS phone `bridge.sqlite` / outbox | `agent_conversation.rs` / native messaging pump | `[I]` key `(owner,device,request_id)` read at `849e49c:crates/ohmos-msg-host/src/agent_conversation.rs:179-190`, native id `:60`; the live write path was not executed. `settle with:` `cargo test -p ohmos-msg-host` in `~/src/fpl/ohmos` |
| `<key>-ohmos.json` agent ledger | `src/ohm/ohmos.rs` (`poll_once`/`reply`/`save`) | `[C]` phone `native_id`; `request_id` in `replies`/`own_requests`; bounded `seen` (`MAX_SEEN :32`, `:294`); `save :462-484` |
| Beeper/Matrix room timeline | `src/matrix.rs` | `[C]` Matrix event id (`send_event :404`); `event_nonces` for our sends (`src/store.rs:109`) |

## 10. Commands used (read-only)

These are the exact commands used to build this inventory. They contain no
secrets and made no writes. The first block is the original revision; the
second block is the grading pass that added the tags.

### Grading pass (`ef050a3`, docs-only)

```sh
cd /Users/ajmwagar/src/fpl/rakazo-beeper/.worktrees/bridge-inventory-confidence
git rev-parse --short HEAD                          # ef050a3
git log --oneline -1                                 # ef050a3 fix(orch): tolerate relay-only null runId (#24)
cargo test --offline                                 # 243 passed, 0 failed (unit + 7 integration targets)

# every citation touched in the grading pass prints a line at the cited spot
git grep -n "transactions/{txn}" -- src/transport.rs
git grep -n "CREATE TABLE IF NOT EXISTS" -- src/store.rs
git grep -n "fn select_bot_ids\|fn selected_bot_ids" -- src/service.rs
git grep -n "pub fn ingest\|pub async fn deliver_inbound\|pub async fn poll_binding" -- src/service.rs
git grep -n '"threads", "send"\|"threads", "appendAssistant"\|"conversationAttachments", "directSend"' -- src/rakazo.rs
git grep -n "appendAssistant\|append_assistant" -- src/rakazo.rs src/orch/mod.rs src/orch/bridge.rs
git grep -n "fn poll_once\|pub fn reply\|pub struct AgentState\|fn save\|Ownership split" -- src/ohm/ohmos.rs
git grep -n "401 =>\|502..=504\|pub enum AgentError\|fn is_offline\|pub fn stable_request_id" -- src/ohm/agent_conversations.rs
git grep -n "Offline by default" -- src/ohm/mod.rs
git grep -n "AgentProbe\|AgentReply\|AgentServe" -- src/main.rs
git grep -n "verticals.toml\|pi-pm" -- src/orch/roster.rs
git grep -n "fleet::route\|fleet::a2a\|fleet::tapback\|use crate::fleet" -- src/main.rs   # no output

# external pins, read at the pinned commit rather than a moving checkout
git -C ~/src/fpl/ohmos show 849e49c:services/provisioning/src/agent_conversations.rs | grep -n "agent-conversations\|ohm-bridge/1"
git -C ~/src/fpl/ohmos show 849e49c:crates/ohmos-msg-host/src/agent_conversation.rs | grep -n "platform\|remote_id\|queue_outbound_once\|local:"
git -C ~/src/fpl/ohmos grep -n "fn queue_outbound_once" 849e49c -- crates
git -C ~/src/fpl/shared-infra ls-tree --name-only 8916e0a projects/rakazo-beeper/
rg -n "ensureBot|synchronize|nativeMessageId" ~/src/fpl/fpl-agents/packages/adapters/src/conversation-bots.ts
rg -n "conversationBotTransportNonce|conversationAttachmentSend|deliveryKey" ~/src/fpl/fpl-agents/packages/adapters/src/native-conversation-bot-sender.ts
rg -n "identityKey|selected-conversation/v1" ~/src/fpl/fpl-agents/packages/adapters/src/selected-conversation-attachments.ts
rg -n "bindingId|bindingIdentity|conversation_attachment_sends_request_key" ~/src/fpl/fpl-agents/packages/db/prisma/schema.prisma
python3 -c "import json;p=json.load(open('$HOME/.pi/agent/fleet-bus/irc/grants.json'));print(p['principals']['muse'])"
rg -n "fnv1a64|bus:%s|OUTBOX|fs.watch" ~/.pi/agent/bin/pi-bus-ircd.lua ~/.pi/agent/bin/pi-notify ~/.pi/agent/extensions/inbox-delivery.ts
ls ~/.pi/agent/fleet-bus/irc/*-delivered.tsv ~/.pi/agent/fleet-bus/irc/transcript-*.jsonl
```

### Original revision (`41a2144`)

```sh
cd /Users/ajmwagar/src/fpl/rakazo-beeper/.worktrees/irc-bridge-extraction
git rev-parse --short HEAD                         # 41a2144
git log --oneline -3                               # 41a2144, 7fcaa2b, e99f122
git diff --stat origin/main...HEAD                 # docs/*.md only

# citations added/corrected in this revision (all must print a line)
git grep -n "auto_bots" -- src/service.rs src/config.rs config.example.toml
git grep -n "select_bot_ids" -- src/service.rs
git grep -n "fn poll_once\|fn reply\|Ownership split" -- src/ohm/ohmos.rs
git grep -n "pub enum AgentError\|stable_request_id\|is_offline" -- src/ohm/agent_conversations.rs
git grep -n "transport = \"ohmos\"\|Offline by default" -- src/ohm/mod.rs
git grep -n "AgentProbe\|AgentReply\|AgentServe" -- src/main.rs
git grep -n "verticals.toml\|pi-pm" -- src/orch/roster.rs

# original inventory commands (still read-only)
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

**Not verified here.** No live network call, Rakazo session, ohmOS request or
phone contact was made for this revision. In particular, the following stay
`[?]` and each carries its own `settle with:` above (or here):

- the ohmOS live leg: `docs/ohm-client.md` records the phone/Iroh hop as
  502/`degraded`; settle with `rakazo-beeper ohm agent-probe --agent muse`
  against `https://ohm.fpl.dev/messages` with an owner-only token;
- the hosted Beeper bot's server-side `relay_only` flag: this checkout can only
  print what the API returned (tolerated as null since `ef050a3`,
  `src/main.rs:1360`). `settle with:` `rakazo-beeper bridge status --json` on the
  deployment host and compare `relay_only` with the server-side bot record
  (owner: Rakazo API/`relayOnly` server owner, currently pm-fpl per
  `docs/ohm-config.example.toml`);
- the ohmOS **msg-host** runtime (queue/scope enforcement): settle with
  `cargo test -p ohmos-msg-host` in `~/src/fpl/ohmos`;
- deployed behavior on `agents.fpl.dev`/pris/the phone/Beeper, and whether a real
  Muse message currently reaches `pi-notify` end to end. The provider and its
  offline tests exist; production delivery is unproven in this document;
- `secrets/registration-personal.json`: untracked runtime material, not in this
  checkout (owner: deployment host).

The `fpl-agents` and `agentsmith` pins equal their local HEADs and were
re-verified by `git rev-parse`; the `ohmos` (`849e49c` vs local `49cdc0b`) and
`shared-infra` (`8916e0a` vs local `88fa1e0`) pins differ from local HEAD and
their citations were read at the pinned commit with `git show`, so they are
confirmed at the pin, not at today's tip.

Read-only result: the only live Muse path found in this repo is the ohmOS
conversation consumed by two owners — `fpl-agents` for Rakazo UI display and
`rakazo-beeper` (`transport = "ohmos"`) for agent delivery/outbound. The fleet
IRC `#muse` remains a separate principal.
