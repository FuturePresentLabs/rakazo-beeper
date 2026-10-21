# Postmortem: <YYYY-MM-DD> <short title>

- **Status:** draft | final
- **Author / reviewer:** <name> / <name>
- **Surface(s):** A (app, agents.fpl.dev) | B (bridge/Beeper delivery) | both
- **UTC window:** start `YYYY-MM-DDTHH:MM:SSZ` → end `YYYY-MM-DDTHH:MM:SSZ`
- **Duration:** <duration, note if it includes detection lag>
- **Severity:** sev1 (all delivery down) | sev2 (degraded/partial) | sev3 (internal)

## Impact

- What a user could not do, in one or two sentences.
- Chats affected: `<n>` of `<n>`; agents affected: `<names/ids>`.
- Messages lost / duplicated / delayed: `<n>` / `<n>` / `<n>`; attachment impact.
- Who noticed first: a check (`scripts/healthcheck.sh`, monitor) or a human.

## Detection

- Signal that fired and when (UTC): <check/alert/human>.
- Detection lag: <time between first user impact and detection>.
- If a human noticed before any check: why no check fired, and what check should
  exist now.

## Timeline (UTC)

| time | event |
| --- | --- |
| HH:MM | first user-visible impact |
| HH:MM | detected by <signal> |
| HH:MM | action taken by <who> (command) |
| HH:MM | service restored / verified |

## Root cause

One paragraph, mechanistic: the actual failure, not "the server was down". Link
the evidence (log excerpt, check output, ids). Anything not yet proven is marked
**UNVERIFIED** with the check that would settle it.

## Contributing factors

- What made this possible (config, single point of failure, missing check).
- What made detection or recovery slower than it needed to be.

## What changed the outcome

- What stopped the bleeding, and whether it was the documented path in
  `docs/runbook.md`. If the runbook was wrong or incomplete, say exactly where.

## What we will change

| action | owner | follow-up slug / issue | due |
| --- | --- | --- | --- |
| <specific, verifiable change> | <who> | <slug> | <date> |

## Verification

Exact commands run after recovery, with results:

```sh
scripts/healthcheck.sh --base https://agents.fpl.dev
# paste output
```

For delivery incidents, add the round-trip check from the runbook (one message
each direction, no duplicates after restart).

## Open questions

- <question> — settle by <command/owner>.
