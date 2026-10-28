# AgentSmith bridge (multi-agent, host-scoped relay)

Status: **Slice A landed and running on Studio (`averys-mac-studio`)** as the
`rakazo-bridge` tmux session. Central Beachhead still owns the Beeper/Matrix
rooms; this supervisor only owns local (`host == averys-mac-studio`) PM panes.

Two hard rules:

* **No SSH transport and no pane paste.** The only local delivery primitive is
  `pi-notify` (durable outbox + the inbox-delivery extension). There is no
  `pi-tmux send`/`send-keys` path anywhere in this slice.
* **No guessed fleet shape.** The roster command is the source of truth; a row
  without a `host` is an error and a foreign-host row is reported but never
  delivered by this box. A roster failure is fatal for one-shot subcommands (no
  "just orch" fallback); in resident mode (`serve`, the default) it is a
  `cycle-error roster:` line plus a capped backoff, not a process exit.

## The roster source (verified)

`agentsmith bridge roster --json` **does not exist**. The verified command is:

```bash
cd ~/.pi/agent && ./bin/pi-agentsmith once --json
# exit 0, ~25-27 rows, ~234 KB
```

The reference binary exposes rows in `.workers[]` and leaves `.rows` **null**;
newer builds expose `.rows[]`. The parser reads `.rows[]` when it is a non-empty
array and otherwise `.workers[]`, and records which key was used. There is no
fabrication when both are absent.

Per-row fields consumed: `name session host role pane state status cwd branch`
(unknown fields ignored). Role comes from an explicit `role` field when present,
otherwise from `session`:

| row | role |
| --- | --- |
| `session = pm-<vertical>` | PM |
| `session = fleet` | worker |
| anything else (control pane, another host's session) | not a relay target |

This is deliberately not name-prefix parsing: the worker
`ivr-instructor-materials-courseware-mcp-tool` shares a prefix with the
`ivr-instructor-materials` vertical.

The orchestrator is not a worker row (the control pane is excluded); the bridge
supplies `fleet-orch-v1` itself. Muse is transport-backed by Slice B
(`transport = "ohmos"`): listed, but no Rakazo bot is created and no token is
issued for it.

`state == "gone"` (or `"dead"`) means offline and drops the reported pane even
when the roster still carries a stale id.

## Configuration

Committed example: `config/agentsmith.example.toml`; the deployed Studio copy is
`~/.config/rakazo/agentsmith.toml`.

```toml
[agentsmith]
roster_command = ["/Users/ajmwagar/.pi/agent/bin/pi-agentsmith", "once", "--json"]

[bridge]
host = "averys-mac-studio"
state_dir = "/Users/ajmwagar/.local/share/rakazo/bridge"
service_token_dir = "/Users/ajmwagar/.rakazo/tokens"
notify_kind = "steer"
notify_urgent = true
hide_workers = true
seen_reaction = "👍"   # "" disables; 👍 means "received, working"
```

`host` is mandatory: `[bridge] host` first, then `PI_AGENT_HOST`. There is no
hostname guess and no default. `hide_workers` defaults to `true`; an upstream
`bridge.hide_workers` in the roster payload wins when present.

## Commands

```bash
BIN=~/.local/bin/rakazo-beeper-bridge
CFG=~/.config/rakazo/agentsmith.toml
export RAKAZO_ORCH_TOKEN_FILE=~/.rakazo/orch-session.json
export RAKAZO_ORCH_SPACE_ID=b66f30cc72e5beac43f6b9ad9c0d5c9b

$BIN bridge --bridge-config $CFG roster [--json]
$BIN bridge --bridge-config $CFG status [--json]     # per-agent state, no secrets
$BIN bridge --bridge-config $CFG seed [--agent ...]  # watermark to head, no delivery
$BIN bridge --bridge-config $CFG run --interval-s 5  # alias: serve
$BIN bridge --bridge-config $CFG provision --apply --agent pm:fpl
$BIN bridge --bridge-config $CFG reply --agent pm:fpl --text '…'
$BIN bridge --bridge-config $CFG reply --agent pm:fpl --reply-to <msgid> --text '…'
$BIN bridge --bridge-config $CFG reply --agent pm:fpl --from-pane pm-fpl
```

### Migration: seed before serving

A fresh state file starts at watermark 0 and would replay every historical user
message into the pane. **Run `seed` once before the first `run` on an existing
fleet.** It binds each owned agent to its bot/thread and sets the watermark to
the conversation head without delivering. A pre-host state file
(`state_dir/<key>.json`) is migrated into `state_dir/<host>/<key>.json`, so the
old `orch watch` watermark is preserved rather than replayed.

## Supervisor

Each cycle re-loads the roster, then:

1. keeps only rows whose `host == [bridge] host`; foreign rows are reported in
   `foreign=` and never polled or delivered;
2. reconciles one `WatchState` per agent at
   `state_dir/<host>/<safe-key>.json` (added/removed; the file survives removal
   so the same identity does not replay);
3. resolves bots: the root through the idempotent `bots.create`
   (`spawnKey=fleet-orch-v1`), PMs as exact-name children of the live
   orchestrator (missing PMs are **reported**, never fabricated), relay-only
   workers through `spawnKey=fleet-worker-<name>-v1` (archived when they leave
   the roster — and only for keys this host owns or previously tracked);
4. polls each agent thread and delivers new `role == "user"` rows once. Bot/
   system rows, duplicates, and our own service-token echoes are skipped.

Inbound delivery is the existing primitive, now targeted at exactly one pane:

```sh
pi-notify send --kind steer --worker pm-fpl --target %40 --urgent \
  --ref rakazo:<thread>:<message> --text '[rakazo:<thread>:<message>] …'
```

The orchestrator keeps the untargeted (`target:null`) orchestrator lane; a PM or
worker row is addressed at its live roster pane (falling back to its session
name). Without `--target` the inbox-delivery extension treats a row as
orchestrator-lane, so a PM row would be dropped — the target is what makes
per-PM delivery exact.

Dedupe is per agent: message id plus a `seq` watermark (512 ids). A delivery
failure or a `Deferred` result does not advance the watermark. If a bot is
re-created with a new thread, the ledger is reset for that conversation.

### Delivery seam (for a future `swarmd`)

```rust
pub trait Deliverer {
    fn deliver(&self, agent_key: &str, host: &str, worker: &str,
               target: Option<&str>, text: &str, reference: &str)
        -> Result<Delivery>; // Accepted | Deferred
}
```

The only implementation is `PiNotifyDeliverer`. A `swarmd` adapter plugs in here
once it publishes a documented deliver/ack/query surface; it must not invent SSH
or pane injection. `Deferred` means "retry next cycle" and retains the
watermark.

### Migration alias

`orch watch` still works and shares the same state shape. During migration,
point it at the orchestrator's ledger and run **either** it or `bridge run` for
the orchestrator, never both. On Studio the old `orch watch` paths were stopped and its watermark (78)
migrated into `bridge/averys-mac-studio/fleet-orch-v1.json` before the
supervisor started. That meant **both** the tmux runner session
`rakazo-inbound` **and** the `dev.fpl.rakazo-orch-inbound` LaunchAgent
(`KeepAlive=1`), which silently restarted the old watcher; the LaunchAgent is
now booted out and `launchctl disable`d so only one supervisor owns the thread.

## Per-agent service tokens and replies

```bash
# Read-only plan (no side effect).
$BIN bridge provision
# Owner session issues one bot-bound token per visible bot, 0600, printing only
# the bot id and token path. Raw tokens are never printed/logged/put in argv.
$BIN bridge provision --apply [--agent pm:fpl]
```

Scopes: `threads:read` + `threads:write:assistant`, bound to one `{space, bot}`.
Existing token files are never overwritten and the file name is derived from the
agent key, so a new worker name cannot inherit an old identity's credential.

```bash
$BIN bridge reply --agent pm:fpl --text '…'
$BIN bridge reply --agent pm:fpl --reply-to <msgid> --from-pane pm-fpl
```

`--reply-to` sets `replyToMessageId` on `threads.appendAssistant`, so the answer
is threaded under the human's message (absent by default).

`--from-pane <worker>` reads that worker's **last assistant block** from
`tmux capture-pane -p -t <pane>`, strips pi's TUI chrome/footer/tool-call echo,
and posts it. It refuses (`pane-missing`, `pane-empty`, `pane-no-answer`,
`pane-not-an-answer`) rather than guessing, and its nonce is derived from
(agent, reply-to, text), so a retry is a replay rather than a second reply.
`--text` and `--from-pane` are mutually exclusive.

On every first delivery the supervisor also posts the configured
`[bridge] seen_reaction` tapback on the inbound message (one add per message
lifetime, deterministic `seen:<msgid>` nonce, never on duplicates/bot rows). A
reaction failure is logged (`report.seen_failed`) and never fails the delivery
or rewinds the watermark. The tapback is not removed when the reply lands.

The reply resolves the agent through the roster, refuses if the requested bot is
not that agent's, reads exactly that agent's token, and calls
`threads.appendAssistant`. `threads.send` is never used for a bot reply, and no
PM can send as another bot. Every bot this slice creates is `relayOnly = true`;
existing PM bots keep their server-side flag (status reports `relay_only: null`
when the deployment does not expose it yet).

Typing indicators are **not** implemented: the RPC surface this bridge uses
exposes no typing method, so that is an upstream/fpl request rather than
something the bridge can fake.

## Deployed on Studio

Current deployment (2026-10-27, branch `pi/agent-bridge-slice-a` at `27904c1`):

```text
tmux session: rakazo-bridge
runner:       <repo>/scripts/rakazo-bridge-supervisor.sh  (restart + watchdog)
binary:       <repo>/target/debug/rakazo-beeper
config:       ~/.config/rakazo/agentsmith.toml
log:          ~/.local/share/rakazo/bridge/supervisor.log
state:        ~/.local/share/rakazo/bridge/averys-mac-studio/<key>.json
```

The older `~/.local/bin/rakazo-beeper-bridge` and
`~/.local/bin/rakazo-bridge-supervisor.sh` still exist but are **stale** (their
hashes differ from this branch). Until they are reinstalled, the tmux session
tracks the worktree build; a reboot does not restore it (no LaunchAgent). That
is the remaining deployment gap.

Live proof (2026-10-26 UTC, first slice):

* `10:09:57Z` one user message sent to `PM · FPL` (thread
  `cmvov58zu003fkuff061fxjka`).
* `10:10:09Z` supervisor logged `delivered=1`; the `pm-fpl` pane (`%40`)
  received `[steer] pm-fpl: … BRIDGE PROOF …`; `pm-rakazo` (`%32`) and
  `pm-marbles` (`%31`) changed only their elapsed-time/spinner lines.
* `10:10:29Z` `bridge reply --agent pm:fpl` appended exactly one `role=bot` row
  (`#3`, marker `BRIDGE-REPLY-PROOF-20261026T101029Z`); a filtered count of that
  marker in the thread is `1`. `bridge status --json` shows
  `watermark=3`, `last_poll_ok=true`, `token_present=true`.
* The hosted PM still produced its own `#2 bot "No response. Try again."` for
  the inbound row: the server-side `relayOnly` flag is not exposed on this
  deployment yet (`relay_only: null`), so that ack is not from the bridge.

Live proof after the resident/watchdog fix (2026-10-27 UTC):

* `23:36:40Z` one `orch say` user message (seq 7) to `PM · FPL`; this ran on the
  new build, after the supervisor restart.
* `23:37:18Z` `fleet-pm-fpl-v1` watermark moved 6 -> 8, `last_poll_ok=true`,
  `last_delivered_id=cmvrbrgg8000gkpffk4bwe5xg`. The `pm-fpl` tty mtime and
  pane hash changed; `pm-rakazo` and `pm-marbles` both kept the **identical**
  mtime and pane hash.
* `23:37:35Z` `bridge reply --agent pm:fpl` appended exactly one row (`#9`,
  marker count `1`, bot `cmvov58zt003ekuff4mux8qna`, thread
  `cmvov58zu003fkuff061fxjka`); rows before/after were 9 -> 10.
* The hosted echo was row `#8 bot "No response. Try again."`; the bridge
  reported `ignored=1` for it and did not append it. `relay_only: null` is still
  the server-side reason (human escalation E-958 item 1).

Rollback:

```bash
tmux kill-session -t rakazo-bridge
# restore the previous single-orch watcher (its state file is untouched):
launchctl enable gui/$(id -u)/dev.fpl.rakazo-orch-inbound
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.fpl.rakazo-orch-inbound.plist
```

Remove `~/.config/rakazo/agentsmith.toml` and the host-scoped state dir to reset
local watermarks. No production server was modified; the Beeper bridge is
unchanged.


## Resident mode and the supervisor (2026-10-27)

`serve` (alias `run`) is **resident by default**: it loops until signalled and
can never exit 0 after a single iteration. `--once` is the only way to ask for
one cycle, and it says so on stderr (`one-shot: --once ran 1 cycle, exiting`).
That closes both halves of the old failure: a wrapper restarting only on a
non-zero exit cannot mistake a completed cycle for a dead daemon, and an
operator cannot start a daemon that silently dies after one pass.

`--interval-s` defaults to 10; `0` is rejected and values below 2 are accepted
with a warning (the roster command spawns `pi-agentsmith`, so less than 2s is
not a sane cadence). In resident mode a roster or cycle failure logs
`cycle-error roster: <err>` / `cycle-error: <err>` and retries after
`min(interval * 3, 300s)`, never shorter than the interval. The process exits
only on an unrecoverable condition (bad config, missing token file). `os error
35` (EAGAIN from a fork under load) is exactly the transient case this covers.

The wrapper is the other half: it restarts on **any** exit including 0, logs
`rc` and `reason=exit|watchdog`, and kills a child that has not printed a
`cycle` line for `3 * interval + grace` seconds. Liveness is a completed cycle,
not a live PID.

### Runbook

Binary and config live on Studio; the launcher is committed at
`scripts/rakazo-bridge-supervisor.sh`. It resolves the binary from
`RAKAZO_BRIDGE_BIN`, then `target/release/rakazo-beeper`, then
`target/debug/rakazo-beeper`; it never reads or writes a token.

```bash
CFG=$HOME/.config/rakazo/agentsmith.toml
export RAKAZO_ORCH_TOKEN_FILE=$HOME/.rakazo/orch-session.json
export RAKAZO_ORCH_SPACE_ID=b66f30cc72e5beac43f6b9ad9c0d5c9b

# 1. Foreground (resident by default, so this stays up; use --once for one pass).
./target/debug/rakazo-beeper bridge --bridge-config "$CFG" serve --interval-s 10

# 2. Background under the restart/watchdog loop, in its own tmux session.
tmux kill-session -t rakazo-bridge 2>/dev/null   # remove the dead pane first
tmux new-session -d -s rakazo-bridge \
  "export RAKAZO_ORCH_TOKEN_FILE=$HOME/.rakazo/orch-session.json \
   RAKAZO_ORCH_SPACE_ID=b66f30cc72e5beac43f6b9ad9c0d5c9b; \
   RAKAZO_BRIDGE_CONFIG=$CFG RAKAZO_BRIDGE_INTERVAL=10 \
   ./scripts/rakazo-bridge-supervisor.sh >> $HOME/.local/share/rakazo/bridge/supervisor.log 2>&1"

# 3. Check liveness (state on disk is not liveness).
pgrep -fl 'rakazo-beeper bridge'
tmux list-panes -t rakazo-bridge -F '#{pane_id} #{pane_dead} #{pane_current_command}'
./target/debug/rakazo-beeper bridge --bridge-config "$CFG" status --json

# 4. Stop.
tmux kill-session -t rakazo-bridge        # rollback: the whole resident bridge
pkill -f 'rakazo-beeper bridge'           # the binary alone, if no tmux session
```

Log: `$HOME/.local/share/rakazo/bridge/supervisor.log` (launcher and bridge
stdout/stderr; one timestamped `supervisor:` line per start and per restart,
including `reason=exit` or `reason=watchdog` and the exit code).
State: `$HOME/.local/share/rakazo/bridge/averys-mac-studio/<key>.json`.
Never run two supervisors at once: they race on the same watermark files.
