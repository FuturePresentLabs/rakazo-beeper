# Fleet orchestrator and PMs in Rakazo

Status: **phase 1 CLI/roster plus the authorised inbound relay.** The relay
code, unit/config diff and rollback are ready; nothing has been deployed to
`agents.fpl.dev`. A reply still needs the upstream assistant-append change
(`pm/rakazo/assistant-append-proposal.md`).

This document describes how the local fleet (the pi orchestrator plus one PM per
live vertical) is represented as first-class Rakazo agents, and how human text in
Rakazo is routed to the right local pane without echoing pane output back to
itself. The implementation lives in `src/orch.rs` (`Orch`, `Entry`, `Kind`) and
the `orch` subcommands in `src/main.rs`. This doc is the reference for the
schema and gaps; it is not a runbook.

## Scope and credentials

`rakazo-beeper orch` reads only these values and never the bridge tenant config:

| variable | meaning |
| --- | --- |
| `RAKAZO_ORCH_TOKEN` or `RAKAZO_ORCH_TOKEN_FILE` | Rakazo session bearer token |
| `RAKAZO_ORCH_SPACE_ID` | space that owns the fleet entries |
| `RAKAZO_ORCH_URL` | optional base URL, default `https://agents.fpl.dev` |

The token is a normal Rakazo session. Secret files must remain owner-readable
only. `orch whoami` is the first command to run in a new environment: it prints
the session owner and the space the server actually resolved, and fails if that
space differs from `RAKAZO_ORCH_SPACE_ID`.

## Allowlisted conversation (inbound relay)

`orch watch`, `orch reply` and `orch status` read exactly one conversation from
the environment. There is no wildcard and no fallback to "the first bot this
session can see":

| variable | meaning |
| --- | --- |
| `RAKAZO_ORCH_BOT_ID` | the one bot id (`cmvic21mh001gkuffgxcmz7lb` for the live Fleet Orchestrator) |
| `RAKAZO_ORCH_SPACE_ID` | the one space id (`b66f30cc72e5beac43f6b9ad9c0d5c9b` for live) |
| `RAKAZO_ORCH_THREAD_ID` | the one thread id; `botId` is not accepted as an implicit fallback |
| `RAKAZO_ORCH_STATE_FILE` | optional default for `--state-file` |

`Allowlist::check` rejects a different bot id, an archived bot, a different
space, or a different thread before any read or write. `bind` also refuses a
state file bound to another bot, so a copied state file cannot re-point the
relay. A mismatch is one log line and an error, never a cross-tenant read.

The bearer token is still the existing Rakazo **session** token behind
`RAKAZO_ORCH_TOKEN_FILE` (mode 0600) or `RAKAZO_ORCH_TOKEN`. No service token is
issued upstream yet; the PM is routing that separately as
`pm/rakazo/service-token-proposal.md`. Do not mint credentials or create a
service account for this increment. The token is never printed or logged.

## Inbound delivery

`orch watch` polls only user-role messages in the allowlisted thread. Each new
message produces exactly one:

```sh
pi-notify send --kind note --worker <name> --ref rakazo:<threadId>:<messageId> \
  --text '[rakazo:<threadId>:<messageId>] <summary>'
```

Dedupe is by message id plus a `seq` watermark in a bounded JSON state file
(512 ids). Restarting with the same state file delivers nothing twice. A
notify failure returns an error and does **not** advance the watermark, so the
next poll retries the same message. There is no `pi-tmux`, `send-keys`, or
`--kind steer` path.

## Interim reply surface (no lie)

There is no assistant-append RPC. `threads.send` always writes a **user**
message and queues a run, so `reply` never calls it. The only outbound path is
`conversationAttachments.directSend` on a linked attachment whose label matches
the allowlisted bot name. The output is explicitly `surface=attachment`.

A normal Agent conversation (including the live Fleet Orchestrator today) has
no linked attachment. `orch reply --dry-run` prints the would-be surface without
sending; without an attachment both dry-run and real reply fail closed with:

```
unsupported: no assistant-append RPC
```

That limitation is intentional: an interim notification is honest, a fake user
message (which would also start a model run) is not.

## Health / status

`orch status` is one JSON line and exits non-zero if any upstream check fails.
It never includes a token, a request body, or message text.

```json
{
  "bot_id": "...",
  "conversation_id": "...",
  "thread_id": "...",
  "space_id": "...",
  "watermark": 42,
  "delivered": 3,
  "duplicates": 1,
  "last_delivered_id": "...",
  "last_poll_ok": true,
  "last_poll_at": "2026-10-21T18:00:00Z",
  "last_error": null,
  "upstream": {
    "healthz": {"status": "ok", "code": 200, "at": "..."},
    "token": {"status": "valid", "at": "..."}
  }
}
```

`healthz` is an unauthenticated `GET /healthz` (the token is not sent). Token
validity is a real `me` RPC: an explicit HTTP 401 becomes
`token.status = "invalid", code = 401`, with the time, so a silent expiry can
never look healthy. A transport error is `status = "error"`.

## Deploy diff (not applied) and rollback

Files, none of which changes the server:

- `deploy/rakazo-orch-watch.service` — one long-running `orch watch` unit,
  hardened with `NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`, and a
  single writable state directory.
- `deploy/rakazo-orch.env.example` — the four non-secret environment values plus
  the token file path. Copy it to a mode-0600 `orch.env`; never commit values.
- `deploy/rakazo-orch-health.{service,timer}` — a five-minute `orch status`
  check that fails on a non-200 `/healthz` or a 401/invalid token.

Rollback is local and instant: stop and disable the unit, remove it and
`orch.env`, and keep the state file (deleting it would re-deliver recent
messages). Nothing upstream was created or changed.

## Roster

One root agent and five PM children. Names are stable because the roster key is
also the server-side idempotency key, and because child resolution uses the name.

| key | Rakazo conversation name | kind | parent |
| --- | --- | --- | --- |
| `fleet-orch-v1` | Fleet Orchestrator | root | — |
| `fleet-pm-agentsmith-v1` | PM · Agentsmith | child | Fleet Orchestrator |
| `fleet-pm-edgerunner-v1` | PM · Edgerunner | child | Fleet Orchestrator |
| `fleet-pm-fpl-v1` | PM · FPL | child | Fleet Orchestrator |
| `fleet-pm-fpl-cloud-v1` | PM · FPL Cloud | child | Fleet Orchestrator |
| `fleet-pm-marbles-v1` | PM · Marbles | child | Fleet Orchestrator |

**Transmog is excluded while that vertical is paused.** Do not create
`fleet-pm-transmog-v1`; when the vertical resumes, add its entry back to `PMS`
in `src/orch.rs` and create it through the orchestrator, following the same
naming rules. `orch list` and `orch ensure` intentionally do not mention it, so
a paused vertical cannot be started by accident.

The root is created with `notifyOnFinish = false` and `computerMode = team`.
A fleet entry must never claim the deployment's shared computer.

## The hard constraint: children are not client-created

`bots.create` cannot set `parentBotId`. **Only the orchestrator's own
`spawn_bot` tool can nest a new bot under itself.** No client RPC exposes that
field, so this repository cannot create a PM sub-chat directly.

Consequences, reflected in the code:

- `orch ensure` only *reports* missing children. `--apply` creates roots only;
  `Orch::create` rejects a `Kind::Child` entry.
- `orch spawn <key>` sends an instruction into the orchestrator's conversation
  (`Orch::spawn_prompt`) asking the orchestrator to call `spawn_bot` with the
  exact roster name. It starts one model run and is therefore never part of
  `ensure`.
- Child resolution cannot use `spawnKey`, because the orchestrator's tool owns
  the generated key. A child matches only when it is the live orchestrator's
  `parentBotId` and its name equals the roster name. Renaming a PM child in
  Rakazo breaks resolution; rename the roster entry and the child together, or
  re-spawn it.

If a future Rakazo release makes `parentBotId` writable by a client, prefer that
over prompting a model, but keep the roster key and name invariants.

## Idempotency keys

Three different keys protect three different operations. None of them is a
random value chosen per attempt.

| operation | key | stored where | retry rule |
| --- | --- | --- | --- |
| create root | roster `key` as `spawnKey` | Rakazo server | re-run `ensure --apply`; server returns the existing bot |
| inbound Rakazo → pane | `in:<botId>:<messageSeq>` (or the message `clientNonce` when present) | local mapping DB | same key = same pane injection; never inject twice |
| outbound pane → Rakazo | caller-supplied nonce, default durable `out:<botId>:<localEventId>` | local mapping DB, written **before** the send | reuse the same nonce on every retry |

`Orch::say` refuses empty text and passes the nonce as `clientNonce` to
`threads.send`. `orch say` generates a random nonce only for an interactive
one-off and prints it; scripted/daemon use must persist and reuse its own nonce.
Reusing a nonce with different text is a bug: Rakazo may reconcile to the first
message, and the local table must treat that as a contract violation rather than
silently posting a duplicate.

## Inbound: human text in Rakazo → the correct local pane

Rakazo is the human-facing surface. A human reply in a fleet conversation must
land in the pane that owns that vertical, not in whichever pane happens to be
focused.

1. Resolve the conversation to a roster entry: root by `spawnKey`, child by
   `parentBotId` + exact name (see above). Never route by display name alone,
   because a human can create an unrelated agent with the same name.
2. Reject archived agents and anything outside the configured space.
3. Map the entry to one pane identity using a durable table:
   `entry key → {pane_id, agent_name, worktree, last_seen}`. The mapping is
   written by the orchestrator when it dispatches/knows the pane, not guessed
   from the current tmux window order.
4. If the pane is missing or dead: do not fall back to the orchestrator's pane.
   Record the message as undelivered and surface it (see "Restart" below). A
   dead PM must not receive another vertical's instructions.
5. Inject the human text with the inbound key above. Strip Rakazo-only envelope
   data; keep the human's text verbatim. An injection is a message, not an
   instruction to start a new run or to replay history.

Committed phase 1: `orch show` and `orch say` provide the read/write primitives
against one conversation. The pane-mapping table and injection loop are phase 2.

## Outbound: pane activity → the correct conversation, without echo

Pane output that is meant for the human is posted into exactly the conversation
that owns that pane:

1. Resolve the pane to a roster entry through the same durable table. Unknown
   panes are not published; there is no "guess nearest agent" behaviour.
2. Persist `(localEventId, botId, nonce)` **before** calling `threads.send`.
3. Send with that nonce. On a lost response, retry the same nonce; do not
   generate a second message.
4. Record the resulting `seq`/message id and the nonce in the outbound table.

Echo suppression uses two independent checks:

- **Origin table:** an inbound Rakazo message whose `clientNonce` is a known
  outbound nonce, or whose message id is already linked to a local event, is our
  own write coming back. Mark it seen and do not inject it into any pane.
- **Source attribution:** messages authored by the bridge/orchestrator session
  are not treated as human input. The existing bridge already distinguishes
  source-originated events from agent output; the fleet path reuses that rule
  instead of inventing a role check based on message text.

The pane must also not re-emit its own injected input. Injection therefore marks
the local event as `injected` before typing into the pane, and the pane-output
watcher skips events with that mark. This is the same reservation pattern the
bridge uses for Matrix/Beeper echoes.

Phase 2 must verify one important Rakazo property before production use:
`threads.send` posts a user-style message and may start an agent run. Fleet
outbound text must not accidentally trigger the agent it came from. Until the
live spike confirms a non-triggering path (or a dedicated bot-authored message
surface exists), outbound publishing stays behind the explicit gate and is not
enabled in the daemon.

## Restart behaviour

State is recoverable, and restart must never duplicate a message:

- Re-read `bots.list` on start. Roots are re-matched by `spawnKey`; children by
  live `parentBotId` + name. Archived entries are ignored, never recreated
  automatically.
- The local mapping table is authoritative for panes and for both inbound and
  outbound keys. It is written before the irreversible step (inject or send) and
  committed after, so a crash leaves a retryable record, not a silent gap.
- Undelivered inbound rows (pane absent) remain queued with their original key.
  When the pane returns, deliver once. Do not replay rows already marked
  delivered.
- Never replay history on startup. `orch show` is read-only; `orch ensure`
  without `--apply` makes no writes.
- A revoked/expired session fails visibly. Delivery remains queued; the operator
  runs `login` (or updates the secret file) and restarts. No agent is recreated
  as a recovery shortcut.

## The explicit `--apply` gate

All reads are safe by default. `orch ensure` is a dry run and prints `present`,
`missing` or `dry run: no entries created`; it creates nothing unless the
operator passes `--apply`. `orch spawn` is a separate, explicitly named action
because it starts a model run; it is never implied by `ensure`.

Safe order:

```sh
rakazo-beeper orch whoami                 # owner + space must match
rakazo-beeper orch list                   # read-only roster reconciliation
rakazo-beeper orch ensure                 # dry run; inspect every line
rakazo-beeper orch ensure --apply         # creates the root only
rakazo-beeper orch spawn fleet-pm-marbles-v1   # one child, one model run
rakazo-beeper orch list                   # confirm parentBotId + name
```

`--only <text>` narrows a dry run or apply to matching keys/names. It does not
override the root/child rule: a child still needs `orch spawn`.

## Phase 2: approved spike (E-174) — not executed in this checkout

The orchestrator approved a guarded live spike. **No entities have been created
and no production conversation has been wired.** Execution is blocked on the
credentials listed below.

### Credentials required before the spike

- `RAKAZO_ORCH_TOKEN_FILE` (preferred, mode 0600) or `RAKAZO_ORCH_TOKEN` — a
  Rakazo session bearer token scoped to a **dedicated test space**. Do not use
  the bridge's tenant token or a personal production session.
- `RAKAZO_ORCH_SPACE_ID` — the ID of that dedicated test space (not the
  production PM space).
- `RAKAZO_ORCH_URL` — only if the test Rakazo is not `https://agents.fpl.dev`.
- If no session exists yet: a device-authorization enrollment for client
  `rakazo-beeper` at the test issuer (default `https://sso.fpl.dev`), run via
  `rakazo-beeper login --rakazo-url <test> --issuer <test> --client-id
  rakazo-beeper --output <private-file>`. That command needs the test account
  to approve the device code; the resulting file becomes
  `RAKAZO_ORCH_TOKEN_FILE`.

### Entities the spike creates (exactly these, nothing else)

1. One root agent: name `Orch Spike`, `spawnKey = fleet-orch-spike-v1`,
   `notifyOnFinish = false`, `computerMode = team`. No PM child agents, no
   production roster entries, no production rooms.
2. One conversation/thread under that agent, created by the first message. Its
   `botId` and thread id are recorded and reused for every later message.
3. No Beeper room, no bridge binding, no PM pane wiring.

### Guardrails for the spike

- **Outbound:** every `threads.send` carries a durable local message id as
  `clientNonce` and an `out:spike:<local-id>` origin tag in the local log. The
  same id is reused on retries; a second send with the same id is suppressed.
- **Echo:** any inbound message whose `clientNonce` is in our outbound table, or
  whose author is the Rakazo agent itself, is dropped before pane injection. It
  must never be posted back.
- **Inbound:** Rakazo human text is delivered to the orchestrator pane through
  the existing `pi-notify` / `pi-orchchat` primitives, not a bespoke injector.
  The lane is recorded; the pane reply must be posted back to the same `botId`
  and thread id.
- **No unintended run:** a received message is not allowed to start a model run
  unless it is explicitly addressed to the spike agent. Verify both an addressed
  and an unaddressed message and record the run ids (or their absence).
- **Rate limit:** at most one message per second in each direction, enforced
  before send/inject.
- **Logging:** log direction, local id, nonce, `botId`, thread id, Rakazo
  `seq`/message id, and outcome for both directions. Never log the token.
- **Restart safety:** persist the send/inject intent before the irreversible
  call; on restart, reconcile by nonce/message id and retry only incomplete
  rows. No history replay and no duplicate delivery.
- **Stop conditions:** if auth fails, a reply lands in a different conversation
  or thread, an echo appears, or an unaddressed message starts a run, stop and
  report the exact ids. Do not fall back to production or the real PM chats.

### Order once credentials exist

1. `orch whoami` and confirm the session resolves to the dedicated test space.
2. Create/reuse only the `Orch Spike` root above (idempotent `spawnKey`).
3. Run the inbound/outbound, no-run, rate-limit and restart checks above.
4. Report the entity ids, thread id, run ids and the two-direction log excerpt.

### Production rollout (still not approved)

1. `orch whoami` / `list` / `ensure` dry run against the target deployment;
   review the exact output.
2. `orch ensure --apply` for the root, then `orch spawn` each live PM child
   (agentsmith, edgerunner, fpl, fpl-cloud, marbles); verify `parentBotId` and
   exact names with `orch list`. Transmog stays absent while paused.
3. Implement the durable pane mapping, inbound injector and outbound publisher
   with the keys and restart rules above, then decide whether the daemon can run
   unattended.
