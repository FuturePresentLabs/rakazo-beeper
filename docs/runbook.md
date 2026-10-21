# Rakazo reliability runbook

Two surfaces, two owners, two failure modes. Never conflate them.

| | A. Rakazo app | B. Rakazo → Beeper bridge |
| --- | --- | --- |
| What | `https://agents.fpl.dev` | private Shroud deployment on Beachhead-1 |
| Reachable from | anywhere | operator transport only (`http://10.118.0.11:18789`, controller `:18790`) |
| Code | Rakazo (not this repo) | this repo |
| Logs | container logs → pris Loki | Shroud VM stdout/receipts via operator transport |
| Deploy/restart | shared-infra (human approval) | shared-infra + Shroud (human approval) |

Deployment identifiers, from
`shared-infra/projects/rakazo-beeper/live-deployment.json`:
`svc-deploy-65ad5980c0364834a131aec99bacd12f`, revision
`rev-1bb229606bb443d8b1aa257a6e49ec30`, controller unit
`shroud-deploy-rakazo-beeper.service`.

The bridge has **no public endpoint**. A 200 from `agents.fpl.dev` says nothing
about Beeper delivery.

## First 60 seconds

```sh
scripts/healthcheck.sh --base https://agents.fpl.dev        # surface A
scripts/healthcheck.sh --bridge http://10.118.0.11:18789    # surface B (needs transport)
```

Read the lines, not just the exit code:

- `OK base /healthz` and `/readyz` → A is up.
- `OK base /rpc/me 401` → A still rejects unauthenticated RPC (auth path intact).
- `SKIP chat` → no `--chat` URL configured; not a failure.
- bridge checks failing from a workstation are expected without operator
  transport and prove nothing about the bridge.

## Symptom → action

### A. `agents.fpl.dev` unhealthy or slow
1. Re-run the `--base` checks; note which path fails.
2. Check pris container logs (LogQL over Loki). Alloy labels docker containers
   with `job=docker`, `container`, `service`, `box` (see
   `shared-infra/pris/config.alloy`):
   ```logql
   {job="docker"} |= "rakazo"
   {job="docker", service="<rakazo compose service>"} |= "error"
   ```
   **UNVERIFIED:** the exact `box` value and compose service name. Settle with
   `{job="docker"} | label_values(service)` in Grafana, or
   `docker compose ps` on pris through operator transport.
3. Disk/space: the app is on pris, whose data volume has run near capacity
   before. Check free space before assuming application fault. **UNVERIFIED:**
   exact mount/volume name — `df -h` on pris settles it.
4. If a deploy or restart is required: **stop.** pris/shared-infra changes need
   human approval. Escalate with the failing check and log excerpt.

### B. Bridge `/readyz` unhealthy
1. Confirm whether the process is running and what it logged. Shroud keeps
   operation receipts; **UNVERIFIED:** the exact receipt/journal path on the
   operator host. Settle with `systemctl status shroud-deploy-rakazo-beeper` and
   the Shroud client receipt directory (`receipt_dir` in the OpenTofu apply).
2. Read the reason before restarting. Common, distinguishable causes:
   - **Beeper connection replaced / duplicate instance** — process exits `78`
     (`RestartPreventExitStatus=78` in the supplied systemd unit). Find and stop
     the competing instance; do not recreate the registration, which destroys
     its rooms.
   - **Expired/revoked Rakazo session** — readiness stays unhealthy and work
     stays queued. Run `rakazo-beeper login --output <token-file>`, then restart
     so the new session is read. Never substitute an FPL or model API token.
   - **Archived or missing bot ID** — startup blocks when a configured agent is
     absent from `bots/list`. Check `bots/listArchived`, keep the stored binding,
     and remove confirmed archived IDs from `bot_ids`; do not select all agents
     implicitly.
   - **Encrypted or unexpectedly shared room** — delivery pauses by design.
     Inspect the room; do not weaken authorization checks.
3. Only then consider restart (see below). A restart that drops queued work or
   replays deliveries is worse than a brief outage.

## Restart

Personal/self-hosted unit:
```sh
systemctl --user status rakazo-beeper.service
journalctl --user -u rakazo-beeper.service -n 100 --no-pager
systemctl --user restart rakazo-beeper.service
curl --fail http://127.0.0.1:8080/readyz
```

Hosted Beachhead deployment: restart through its controller (shared-infra
operator transport), not by starting a second process. Rules:

- One writer per registration and per volume. Never run overlapping revisions.
- If the controller cannot replace a volume-backed revision without overlap,
  stop the old revision through its explicit lifecycle first.
- A replaced Beeper connection exits `78`; the supplied unit deliberately does
  not restart it. Find the competing instance.

## Rollback

Rollback is a shared-infra action and needs human approval.

1. Identify the previously qualified immutable rootfs digest (recorded in the
   release request / live deployment). Never substitute a mutable tag.
2. Roll back against the **existing** volume only if the database format is
   compatible. Otherwise restore the matching stopped backup first.
3. Inspect the restored database's cursors before starting: a stale database can
   replay deliveries.
4. Preserve the whole recovery set: controller checkpoint, current image digest,
   stopped disk backup, encryption key, registration. Never delete the
   authoritative disk or the Beeper registration to resolve an attach conflict.
5. After restart verify `/readyz`, empty queue, no duplicate replies, unchanged
   history, and one fresh round-trip message.

## Postmortem

Write one for any user-visible outage, any duplicate/missed/delayed delivery, or
any `/readyz` failure longer than 5 minutes. Keep it short and factual:

- UTC window, surface(s) affected, chats/messages affected.
- Detection signal (which check fired, or who noticed and how).
- UTC timeline; root cause; contributing factors.
- What changed and the verification transcript (exact command + result).
- Follow-up backlog slugs.

A reusable template is queued as the `postmortem-habit` backlog item.

## Escalation

- Product/policy/credential decisions and any pris deploy, restart or rollback:
  human approval, via the PM (`pm:rakazo`) with the failing check + log excerpt.
- Cross-vertical change (e.g. shared-infra pipeline, FPL Auth issuer): post a
  `need` to that vertical's PM; do not edit its tree.
- Never broadcast a blocking question without an escalation id.
