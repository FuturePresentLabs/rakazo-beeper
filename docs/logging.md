# Where the logs are

Two surfaces, two log paths. Symptom → action detail: [`runbook.md`](runbook.md).

## A. Rakazo app — Docker containers → pris Loki

`agents.fpl.dev` runs as Docker containers on pris. Alloy (`pris/config.alloy`,
`loki.source.docker`) labels each line with `job="docker"`, `container`, `service`
(compose service) and `box` (`box="pris"`).

```logql
{job="docker"} |= "rakazo"
{job="docker", service="rakazo"} |~ "(?i)(error|panic|fatal|timeout)"
```

The first finds the app before you know its service name; the second is the failing-service filter (`label_values(service)` in Grafana, `docker compose ps`).

## B. Rakazo → Beeper bridge — a Shroud VM, not a container

The bridge is a Firecracker microVM on Beachhead-1: **no Docker container**, never
under `{job="docker"}`. Its stdout/stderr are the VM console log on the Shroud host,
`<shroud --runtime-dir>/logs/fc_<vm>.log` (Beachhead installs shroud with
`--runtime-dir /var/lib/shroud`), with `<vm>` = `shrdpl-<revisionId without rev->`:
`/var/lib/shroud/logs/fc_shrdpl-1bb229606bb443d8b1aa257a6e49ec30.log`
After a redeploy take the VM name from `/var/lib/shroud-deploy/rakazo-beeper/deployments.json`
or read the log through the control socket:
`{"action":"logs","vm":"shrdpl-<revision>","tail":200}`.

Receipts: `<receipt_dir>/rakazo-beeper.json` from the OpenTofu apply; controller
journal `journalctl -u shroud-deploy-rakazo-beeper.service` on Beachhead-1.
**UNVERIFIED:** the concrete `receipt_dir` value (private apply variable). Settle
with `grep -r receipt_dir` in that host's shared-infra apply vars.

## Reading the startup line

`rakazo-beeper run` logs one INFO line before any work:
`rakazo-beeper starting version=0.1.0 revision=<RAKAZO_BUILD_REVISION> config=<path> listen=<addr>`
`revision` is `unknown` when `RAKAZO_BUILD_REVISION` is unset; the current deploy
request sets only the key/token/`RUST_LOG` vars, so expect `unknown` until the
deployment stamps it. Check the line against `live-deployment.json`'s image digest.
