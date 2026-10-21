# Postmortem: 2026-10-21 agents.fpl.dev unreachable through the edge

- **Status:** draft — incident ongoing at time of writing; root cause is
  edge/Beachhead-side and owned outside this repo
- **Surface(s):** A (app, `agents.fpl.dev`); B (bridge delivery) presumed
  affected, not directly observed
- **UTC window:** start `2026-10-21T17:34:00Z` (first failed check) → end open
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
| 17:40 | orchestrator diagnosis: pris origin is up (`100.74.130.83:18443` listening, TLS rejects anonymous by design) and the reverse-tunnel ssh is ESTABLISHED to Beachhead `134.209.208.195` |
| 17:42 | still timing out from this workstation; incident handed to the edge owner |

## Root cause

Not in this repository. The origin on pris is listening and the reverse tunnel
is established, so the break is on the Beachhead/edge side of the path. Evidence
from this workstation:

```text
curl -4 https://agents.fpl.dev/healthz        -> 000, 20 s timeout (3×)
curl    https://agents.fpl.dev/cdn-cgi/trace  -> 200
curl    https://sso.fpl.dev/                  -> 200, 0.14 s
curl    https://www.google.com/              -> 200, 0.08 s
```

**UNVERIFIED:** which edge component drops the request (Cloudflare origin
routing, Beachhead proxy, or tunnel termination). Settle on Beachhead:
`ss -tnp | grep 18443`, proxy access/error logs for `agents.fpl.dev`, and a
loopback request to the origin from pris.

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

| action | owner | follow-up slug | due |
| --- | --- | --- | --- |
| Schedule `scripts/healthcheck.sh --base` and alert on failure | rakazo PM | `uptime-monitor` (to be filed; not dispatched while the host is blocked) | open |
| Add a bridge-side external reachability probe so queueing is visible as "API unreachable" | rakazo PM | `log-visibility` scope note | open |
| Record the edge-side cause once the Beachhead owner reports it | edge owner → rakazo PM | this postmortem | open |

## Verification

Once the edge is restored, run from a normal workstation:

```sh
scripts/healthcheck.sh --base https://agents.fpl.dev
# expect: OK base /healthz 200, OK base /readyz 200, OK base /rpc/me 401, exit 0
```

Then, from the operator host, verify the bridge recovered: `/readyz`, queue depth
returning to zero, and one round-trip message with no duplicate after restart
(see `docs/runbook.md`).

## Open questions

- Did any delivery fail permanently, or did everything queue? — inspect the
  bridge queue and Rakazo thread state after recovery.
- Why did the edge stop routing while the tunnel stayed up? — edge owner.
- Did this coincide with the host reaching `kern.num_tasks 8192/8000`? Both were
  true at the same time; correlation is not causation and the origin is reported
  healthy, so likely coincidental. Marked **UNVERIFIED**.
