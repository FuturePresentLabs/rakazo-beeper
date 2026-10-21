# Postmortem: 2026-10-21 agents.fpl.dev unreachable through the edge

- **Status:** final (root cause confirmed in the app guest, not the edge)
- **Surface(s):** A (app, `agents.fpl.dev`); B (bridge delivery) presumed
  affected, not directly observed
- **UTC window:** start `2026-10-21T17:34:00Z` (first failed check) →
  end `2026-10-21T19:25:00Z` (proxied restored and verified)
- **Detection:** `scripts/healthcheck.sh --base https://agents.fpl.dev`, run by
  the PM as part of verifying an unrelated docs change; no monitor exists yet
- **Severity:** sev1 for surface A (customer-facing UI unreachable)

## Impact

The Rakazo web app could not be reached through its public hostname: every app
path timed out while Cloudflare's edge still answered. Because Beeper delivery
depends on the same Rakazo API, outbound/inbound delivery is presumed to have
failed or queued for the duration. **UNVERIFIED:** bridge `/readyz` and queue
depth — the bridge is private (`10.118.0.11:18789`) and needs operator
transport. Settle with the runbook's bridge check from the operator host.

No data loss is known. Delivery is designed to queue on API failure and retry,
so the expected outcome is delayed messages, not dropped ones — **UNVERIFIED**
until the queue is inspected after recovery.

## Detection

- First failed check: `2026-10-21T17:34Z` (approx), exit 1 with three
  `curl (28)` timeouts.
- Detection lag: effectively zero for the check that ran, but nothing was
  watching: the check ran incidentally, not on a schedule. That is the gap.

## Timeline (UTC)

| time | event |
| --- | --- |
| ~17:34 | `healthcheck.sh --base` returns `FAIL … curl: (28)` for `/healthz`, `/readyz`, `/rpc/me` |
| 17:35 | reproduced 3× over 60 s; `/cdn-cgi/trace` returns 200 (edge healthy) |
| 17:36 | `sso.fpl.dev` 200 (0.14 s), `google.com` 200 (0.08 s) → local network/DNS fine |
| 17:36 | IPv6 connect fails immediately; DNS resolves to Cloudflare A/AAAA |
| 17:39 | escalated (E-192) as a new blocker needing human/shared-infra action |
| ~17:40 | orchestrator: origin process up, reverse tunnel ESTABLISHED; initial theory = edge/Beachhead |
| 17:40 | orchestrator diagnosis: pris origin is up (`100.74.130.83:18443` listening, TLS rejects anonymous by design) and the reverse-tunnel ssh is ESTABLISHED to Beachhead `134.209.208.195` |
| 17:42 | still timing out from this workstation; incident handed to the edge owner |

## Root cause

The **app guest wedged inside its Firecracker microVM**, not the edge. The guest
(`10.205.0.101:5173`) stopped answering while its firecracker process spun at
~29% CPU for ~40 minutes; the guest log (readable at the time) shows normal
`http.request.completed` lines and then silence. The public 522/timeout was a
symptom of the chain: edge → Beachhead → tunnel → Palisade → wedged guest. The
earlier "edge config" theory was wrong; DNS was never the problem and the
hostname must stay `proxied=true`.

Contributing cause: the workload shares a box with the control plane and has no
per-guest CPU limit or liveness watchdog, so a spinning guest could not be
detected or restarted automatically. Evidence from this workstation during the
incident:

```text
curl -4 https://agents.fpl.dev/healthz        -> 000, 20 s timeout (3×)
curl    https://agents.fpl.dev/cdn-cgi/trace  -> 200
curl    https://sso.fpl.dev/                  -> 200, 0.14 s
curl    https://www.google.com/              -> 200, 0.08 s
```

Confirmed by replay: a shroud `start` request for the workload restored
service, and `proxied=true` was re-established. The guest itself had no
self-recovery path, which is the defect this postmortem changes.

## Contributing factors

- No scheduled uptime check: the outage was found by accident during unrelated
  work. `scripts/healthcheck.sh` exists but nothing runs it periodically.
- The public host health is not visible from the bridge's own readiness: a green
  bridge can coexist with an unreachable Rakazo API.
- The bridge has no public endpoint, so a workstation cannot distinguish
  "delivery queueing" from "delivery broken" without operator transport.

## What changed the outcome

Nothing yet on our side; the PM correctly did not deploy or restart shared-infra.
The diagnosis was fast because the check separates edge health (`/cdn-cgi/trace`)
from app health and because an unrelated service (`sso.fpl.dev`) was tested as a
control.

## What we will change

| action | owner | follow-up | status |
| --- | --- | --- | --- |
| Scheduled `scripts/healthcheck.sh --base` + external hosted check | rakazo PM | PR #7 merged; install pending approval | open |
| Guest liveness watchdog: 30 s probe, 3 failures → replay that workload's start request over `shroud.control.v1`, one event + pi-notify blocker; never restart host/control plane | fleet | `need` posted | open |
| Boot replay unit: oneshot, `After=shroud-rakazo-spike.service`, replays the start request; root-only file, no `guestEnv` logging | fleet | `need` posted | open |
| No compiles in guests (removes the trigger class) | rakazo PM | PR #11 merged | done |
| Keep the hostname `proxied=true` | shared-infra | — | done |

## Verification

Confirmed from a normal workstation after recovery:

```sh
scripts/healthcheck.sh --base https://agents.fpl.dev
# OK base /healthz 200, OK base /readyz 200, OK base /rpc/me 401, exit 0
```

Then, from the operator host, verify the bridge recovered: `/readyz`, queue depth
returning to zero, and one round-trip message with no duplicate after restart
(see `docs/runbook.md`).

## Open questions

- Did any delivery fail permanently, or did everything queue? — inspect the
  bridge queue and Rakazo thread state after recovery.
- Why did the guest wedge (spin) in the first place? — guest logs end at normal
  request completion; no OOM/kill line was recorded. Watchdog + limits are the
  mitigation until the trigger is identified.
- Did the host nearing `kern.num_tasks 8192/8000` contribute? It was concurrent,
  but the guest wedge is explained by the guest itself; treat the host task table
  as an independent risk, not this root cause.
