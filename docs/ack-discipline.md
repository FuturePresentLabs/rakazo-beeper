# Acknowledgement discipline

Status: **implemented** (`src/fleet/ack.rs`, `src/store.rs`, `src/service.rs`,
`src/orch/supervisor.rs`).

Two surfaces carry a tapback to the human, and both now obey the same policy:

| surface | the tapback | the ledger | the visible admission |
| --- | --- | --- | --- |
| Beeper bridge (`agents.fpl.dev`) | a 👍 reaction on the human's message | `ack_obligations` in the bridge database | a message in the Beeper room |
| agent bridge (`bridge serve`) | `threads.react` on the human's Rakazo message | `<state_dir>/<host>/<key>.acks.json` | the agent's own message in the conversation (`threads.appendAssistant`) |

## The policy in three sentences

1. **An acknowledgement is a promise**, not a receipt: the tapback says "I saw
   this and I will follow up", and the promise is recorded in the durable
   ledger before the tapback is sent.
2. **An acknowledgement is only allowed mid-thread** — the default policy
   `mid_thread_only` fires only while the agent is already working on a prior
   task, because an idle agent answers and an answer is not an
   acknowledgement.
3. **A broken promise is admitted**: the hook nudges the agent once, and if
   the follow-up still does not arrive it raises exactly one visible notice in
   the thread saying that it acknowledged the message and then did not follow
   up.

Acknowledging every message is noise: it trains the human to ignore
acknowledgements, and a promise nobody keeps is worse than silence because it
says the message landed and then nothing happens.

## The policy is explicit and configurable

`ack_policy` is a setting of the bridge that owns the tapback: a `[tenants]`
entry for the Beeper bridge, a `[bridge]` entry for the agent bridge. The
default is `mid_thread_only`.

| value | behaviour |
| --- | --- |
| `never` | never acknowledge; the reply is the only response. |
| `mid_thread_only` (default) | acknowledge only while the agent is busy on a prior task. |
| `always` | acknowledge every inbound message. |

```toml
[[tenants]]
id = "personal"
# ...
ack_policy = "mid_thread_only"
ack_due_seconds = 900   # when an acknowledged message is due for its follow-up
ack_grace_seconds = 300 # grace period before the broken promise is admitted
```

```toml
[bridge]
# ...
ack_policy = "mid_thread_only"
ack_due_seconds = 900
ack_grace_seconds = 300
```

An unknown policy name is a configuration error, not a guess between `never`
and `always`. Every decision is logged with the reason it was allowed or
blocked (`acknowledgement considered` in the bridge log).

## The obligation ledger

An acknowledgement creates a row in the ledger of its surface: `ack_obligations`
in the Beeper bridge database (the same place as the rest of the bridge state),
or `<state_dir>/<host>/<key>.acks.json` beside the agent bridge's watch state.
Either way a promise survives a restart with everything else.

| column | meaning |
| --- | --- |
| `ack_id` | the identity of the promise: `ack-<hash(agent, thread, message)>` |
| `message_id` | the acknowledged inbound message |
| `thread` | the room the human reads |
| `agent` | the agent that owes the follow-up |
| `created_at`, `due_by` | when the promise was made and when it is due |
| `seq` | the acked message's position in the agent's thread (ordering) |
| `status` | `open`, `fulfilled` or `expired` |
| `fulfilled_by`, `run_id` | the reply that kept the promise and its run |
| `nudged_at`, `noticed_at` | when the single nudge and the single notice happened |
| `notice_attempts` | bounded retries of a failed admission |

The identity is stable: the same message always maps to the same `ack_id`, so a
retry, a duplicate delivery or a second sweep reuses the row instead of
minting a new one.

**The promise is recorded before the tapback is sent.** If the ledger cannot be
written, no acknowledgement is sent at all: an unrecordable promise must not
be made, and the log says why (`ledger write failed, so no promise was made`).
If the tapback itself fails, the row is dropped — the human was never told, so
nothing is owed.

## The hook

The sweep runs on the bridge's existing cadence (`Runtime::tick` for the Beeper
bridge, the supervisor cycle for the agent bridge). It is not a new daemon.

For every `open` obligation past `due_by`:

1. **One nudge.** The agent is reminded once, in its own thread: a `pi-notify`
   steer to the agent's pane, or a message in the agent's conversation. On the
   Beeper bridge the nudge is the bridge's message, never the human's: it is
   recorded in `bridge_outbound` and is never relayed back into the human's
   room.
2. **After the grace period, one visible admission.** The obligation becomes
   `expired` and the human is told, in the thread:

   > I acknowledged your message and then did not follow up (ack-…). The
   > acknowledgement was a promise, and I did not keep it.

   The notice text is a pure function of the obligation: no clock, no counter,
   no per-tick wording, so it is byte-identical on every attempt. The Matrix
   transaction id is derived from the `ack_id`, so a retry is a replay and
   never a second message. A notice that renamed itself every tick would look
   like a new admission each time and would mint a row per minute.

The nudge happens once per obligation; a failed admission is retried a bounded
number of times (three) and then the promise is closed without a notice rather
than retried forever.

## Fulfilment

When the agent replies in the thread, the matching obligations are closed and
the reply's message id and run id are recorded. The agent bridge reads the
agent's own reply in the conversation; the Beeper bridge reads it when the
reply is relayed into the human's room.

**The thread and the ordering are enough.** The reply does not have to quote
the acknowledged message: any reply in the same thread that comes after the
acknowledged message keeps the promise. The agent's answer is matched by the
room (`thread`), the agent (`agent`) and the agent-thread position (`seq`).

## What is never acknowledged

- A message that arrived while the agent was idle (the default policy): the
  answer is the response.
- A room the agent cannot answer in.
- A message that was already acknowledged: one message, one promise, one
  tapback.
- Anything at all when `ack_policy = "never"`.
- Anything at all when the tapback is disabled (`seen_reaction = ""` on the
  agent bridge): no tapback, no promise, nothing owed.
