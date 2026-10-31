# Group conversation (transport-neutral core)

Status: core types + routing rules landed; transport adapters follow.

## Stable group ID

Format: `<transport>:<native-group-id>`.

| transport | example | source |
|---|---|---|
| IRC | `irc:#orch` | channel name |
| Beeper | `beeper:<chat_id>` | chat id |
| ohm | `ohm:<conversation_id>` | conversation id |
| Rakazo (1:1) | `rakazo:<thread_id>` | thread id (backward compat) |

For 1:1 threads, `group_id` is `Some(GroupId("rakazo:<thread_id>"))`.
Old messages without `group_id` parse as `None` and are treated as 1:1.

## Sender attribution

Every group message carries `sender_key`: a canonical agent key from
`pi-agentsmith` (`fleet-orch-v1`, `fleet-pm-rakazo-v1`, `agora-one`, `neo`)
or `human:<nick>` for humans. `Message.role` is retained for display.

## Mention / reply routing

- `Message.mentions: Vec<String>` holds canonical agent keys explicitly @mentioned.
- `Message.reply_to_message_id` carries the parent message id (already exists).
- Routing: `should_respond(group, msg, self_key)` decides whether an agent
  responds. See `src/group.rs`.

## Opt-in response policy

`OptInPolicy` on `Group`:
- `OrchestratorOnly` (default): only the orchestrator replies, to human messages
  or explicit mentions. PMs are `relay_only=true` and never reply.
- `AddressedOnly`: only @mentioned agents reply.
- `All`: every non-relay member may reply (bots stay quiet unless addressed).

## Echo suppression

`is_echo(msg, self_key, own_nonces)` returns true when `sender_key == self_key`
or the message's `client_nonce` is in `own_nonces`. Echoes never trigger a
response.

## Membership vs routing

Membership (who is in the group) is separate from routing (who gets notified).
A group has a `members: Vec<GroupMember>` with `agent_key`, `role`, `relay_only`,
`joined_at`. Membership auth is the existing bot token (0600 files). An agent
can be a member but muted (`relay_only=true`).

## Dedupe / correlated receipt

Dedupe key: `"{group_id}:{message_id}:{sender_key}"` (extends `event_nonces`).
A durable receipt is recorded when the notifier acks; on failure, a drop row is
appended to `<state_dir>/<group_stem>.reactions-dropped.jsonl` (reuses the
tapback drop pattern), deduped by ref.

## First transport / canary

IRC `#orch` is already a live multi-agent group chat. First proof: one message
in `#orch` -> `fleet-orch-v1` steer with a group-scoped ref, no duplicate, no
echo loop. Fake events first, then one real message with a durable receipt.
No live group sends until the canary passes.

Beeper and ohm follow as adapters (blocked on their respective tokens/502s).
