# Scheduled uptime check

`deploy/healthcheck.timer` runs `deploy/healthcheck.service` every 5 minutes
(`Persistent=true`, 30 s jitter); the service runs `scripts/monitor.sh`, which calls
`scripts/healthcheck.sh --base https://agents.fpl.dev`, appends one line to
`~/.local/state/rakazo-beeper/healthcheck.log` and exits with the check's status.
No secrets, no restarts: it only probes `/healthz`, `/readyz` and unauthenticated
`POST /rpc/me` (401 is the pass).

## Install (user units; this repo does not enable them)
```sh
install -Dm755 scripts/{healthcheck,monitor}.sh ~/.local/share/rakazo-beeper/scripts/
install -Dm644 docs/monitoring.md ~/.local/share/rakazo-beeper/docs/monitoring.md
install -Dm644 deploy/healthcheck.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now healthcheck.timer
```

## Read the results
```sh
systemctl --user list-timers healthcheck.timer   # next and last run
journalctl --user -u healthcheck.service -n 50   # stdout of recent runs
tail -n 20 ~/.local/state/rakazo-beeper/healthcheck.log
```

A run exits nonzero and logs `FAIL <surface> <path> <detail>`: `/healthz` = app down,
`/readyz` = up but not ready (`runbook.md`), `FAIL base /rpc/me` = auth stopped
rejecting unauthenticated RPC. `systemctl --user status healthcheck.service` shows the
last exit. **UNVERIFIED:** the `%h/.local/share/rakazo-beeper/...` paths assume the
`deploy/rakazo-beeper.service` layout (settle: `systemctl --user cat rakazo-beeper.service`);
host reachability (settle: `curl -fsS -o /dev/null -w '%{http_code}\n' https://agents.fpl.dev/healthz`).

## Limits
This runs on **one** machine. If it, its user manager or its network is down it detects
nothing: a convenience signal, not availability monitoring. Put the real signal on an
external hosted check outside FPL infra on `/healthz` and `/readyz`, alerting on-call.
