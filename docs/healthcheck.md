# Health check

`scripts/healthcheck.sh` is read-only (no credentials, redirects, TLS overrides, writes)
with `--connect-timeout 3 --max-time 10`. Per surface it checks `/healthz` then `/readyz`,
then proves the app rejects an unauthenticated read-only RPC (`POST /rpc/me` → 401 is the
pass). Lines are `OK|FAIL|SKIP <surface> <path> <code|error>`; exit 0 only if all passed, 1 if any failed, 2 on usage errors. `--json` prints one summary line.

```sh
bash scripts/healthcheck.sh --base https://agents.fpl.dev
bash scripts/healthcheck.sh --bridge http://10.118.0.11:18789 --chat URL --json
```

`agents.fpl.dev` is the public Rakazo app; the bridge is private at `http://10.118.0.11:18789`
behind operator transport (no public bridge endpoint). Without `--chat` it reports
`SKIP chat not configured`; a failing bridge check off that transport proves nothing.
