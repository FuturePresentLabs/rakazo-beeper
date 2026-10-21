# Fleet contract — bot tiers, routing, tapbacks, A2A

Status: **proposed, reviewable.** The offline core is implemented in `src/fleet/`
(PR #9). Creating bots, nesting PM sub-bots and any live `threads.send` are M1,
need a dedicated test space, and are not enabled here.

## 1. Bot tiers

| tier | Rakazo identity | lifetime | addressed by | creates chats |
| --- | --- | --- | --- | --- |
| **T1 orch** | `orch` (`fleet-orch-v1`), stable | permanent | human | yes |
| **T1 PM** | sub-bot of orch: `pm:agentsmith`, `pm:fpl`, `pm:fpl-cloud`, `pm:edgerunner`, `pm:marbles`, `pm:rakazo`, `pm:fleet` | permanent | human directly | yes (own conversation) |
| **T2 worker** | **none** — never a bot, never a chat | until reaped | nobody; emits via its PM | no |
| **T3 node** | stable id, parented under its PM | survives reaps | human directly | opt-in only |

`pm:transmog` exists but is paused: no conversation, not routable. Persistence is
a recorded decision, never a default. A worker's lines appear in its PM's
conversation attributed `worker:<slug>`; raw worker chatter never reaches the
orch DM — only an actionable digest (escalation, decision, tapback-worthy
outcome).

## 2. Conversation → pane mapping

Resolved at runtime; pane ids are **never** hardcoded (they are recycled).

| bot key | conversation | pane resolution | notify target |
| --- | --- | --- | --- |
| `orch` | root human↔orch | window `orch` | `orchestrator` |
| `pm:<v>` | child of orch | window `pm-<v>` | `pm:<v>` |
| `worker:<slug>` | none | none | its PM |
| T3 node | child of its PM | explicit decision | its notify target |

Rules: root by exact `spawnKey`; child by `parentBotId == live orch` **and** exact
name; pane by window name, then verify the π title; cache `{bot → pane,
last_seen}` and update on change; a missing/dead pane queues the message with its
idempotency key and surfaces `unrouted: <bot> pane gone` once. Unknown
conversation ⇒ orch with a visible `unrouted` note — never a silent drop.

## 3. Reaction → answer verb

An ASK is one message:

```text
ASK <escalation-id> — <question>
1) <option>
2) <option>
3) <option>
recommend: <n> — <why>
```

| signal | verb | effect |
| --- | --- | --- |
| 👍 / ✅ | `answer:yes` | accept the recommendation |
| 👎 / ❌ | `answer:no` | reject; returns to the asker as blocked |
| 1️⃣ / "1" (…3) | `answer:<n>` | choose option n |
| ❓ / "?" | `defer:more-info` | asker must post detail; no decision |
| 🕐 / "later" | `defer` | stays open; no nag until the next cadence |

Idempotency: key = `escalation_id + message_id`, terminal per escalation. The
ledger, not the transcript, is authoritative: re-delivery records the answer once;
a reaction on an already-answered ASK is refused visibly
(`already answered: <verb>`), never re-applied. `defer` is not terminal. An
out-of-range option is rejected and records no answer.

## 4. A2A message envelope

```json
{
  "id": "a2a-<stable>",
  "from": "pm:fpl",
  "to": "pm:rakazo",
  "kind": "need|answer|done|steer",
  "ref": "<escalation id or slug>",
  "reply_to": "<id|null>",
  "ts": "2026-10-21T18:17:23Z",
  "content_hash": "sha256:<hex of normalized body>",
  "body": "text",
  "options": ["..."]
}
```

- `from`/`to` must be bot keys from §2; unknown keys are rejected.
- `content_hash` is computed over the normalized body (trimmed, collapsed
  whitespace) so retries compare equal.
- Dedupe key `(from, to, content_hash)` within 24 h: the second copy is a
  duplicate and is not rendered again.
- **No echo:** `to == from`, or a message whose origin is the recipient's own
  outbound nonce/message id, is dropped before rendering.
- Render in the recipient's conversation as `[from pm:fpl] <body>`; the envelope
  is never shown raw.

## 5. Idempotency summary

| direction | key | written |
| --- | --- | --- |
| inbound Rakazo → pane | `in:<botId>:<seq>` (or `clientNonce`) | before injection |
| outbound pane → Rakazo | durable `out:<botId>:<localEventId>` | before `threads.send` |
| ASK answer | `(escalation_id, message_id)` | before applying |
| A2A | `(from, to, content_hash)` + envelope `id` | before render |

A retry always reuses its key. Reusing a key with different content is a contract
violation, not a new message.

## 6. Test-artifact isolation

Selftests (`pi-pm --selftest`, pmgoals) must use isolated `PI_NOTIFY_DIR`,
`PI_BUS_DIR`, `PI_ESCALATE_LEDGER` and `PI_PM_ROOT`, must never register a pane
or bot name, and must assert the real ledgers are unchanged. `pm-selftest` and
`pm-empty` are test artifacts and must never appear as bots, conversations or PM
entries.

## 7. Why M1 is separate

`bots.create` cannot set `parentBotId`; only the orchestrator's own `spawn_bot`
tool can nest a sub-bot. `threads.send` may start an agent run, so outbound
publishing stays gated until the spike proves a non-triggering path. Both facts
are recorded in [`orchestrator-in-rakazo.md`](orchestrator-in-rakazo.md); the
credential packet is [`orchestrator-spike-credentials.md`](orchestrator-spike-credentials.md).
