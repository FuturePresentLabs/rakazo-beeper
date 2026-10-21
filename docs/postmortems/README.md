# Postmortems

One postmortem per outage. Write it while the timeline is still fresh — within
24 hours of the incident ending, even if the root cause is still open. A
postmortem with "root cause: under investigation" is more useful than a perfect
one written a week later.

## When one is required

- Any user-visible outage of `agents.fpl.dev` or of Beeper↔Rakazo delivery.
- Any duplicate, missed, or delayed delivery (including after a restart).
- Any `/readyz` failure longer than 5 minutes.
- Any rollback, or any deploy that required manual intervention.

Not required for: a single failed request, a scheduled maintenance window
announced in advance, or a locally reproducible development error.

## Filing

File as `docs/postmortems/YYYY-MM-DD-<short-slug>.md` using
[`TEMPLATE.md`](TEMPLATE.md), then add one row to the index below. Keep it
factual and blameless: what happened, what detected it, what changed the
outcome. No speculation stated as fact — mark unknowns **UNVERIFIED**.

The runbook ([`../runbook.md`](../runbook.md)) owns the detection and recovery
commands; a postmortem links to them rather than repeating them.

## Index

| date (UTC) | slug | surface | user impact | duration | status |
| --- | --- | --- | --- | --- | --- |
| 2026-10-21 | [agents-fpl-dev-edge](2026-10-21-agents-fpl-dev-edge.md) | A (app) | Rakazo UI unreachable via Cloudflare | ≥8 min, ongoing | open (edge-side) |
