# Guest rules: run the artifact, never compile in it

The runtime guest is a delivery vehicle, not a build environment.

- Builds happen on a builder outside the runtime guest: CI or an earlier image
  stage. Only the resulting binary and its runtime dependencies enter the guest.
- The guest runs the artifact only. A guest that needs `cargo`, `rustc`, `cc`,
  `make`, `node`, `npm` or an apt `-dev`/build package is a build defect. Fix the
  Dockerfile or the release pipeline, not the guest.
- Keep the root filesystem read-only where the platform allows it; write only to
  explicitly provisioned data and state mounts.
- `scripts/check-no-toolchain.sh` enforces the rule statically in CI. It reads
  the final `FROM` stage of `Dockerfile`, rejects toolchain installs or copies,
  and requires a known slim/runtime base or a `COPY --from=` prebuilt artifact.
  Run it locally before opening a PR: `bash scripts/check-no-toolchain.sh`.
- Deployment, restart and rollback remain operator actions; see
  [`runbook.md`](runbook.md). Build commands are in
  [`../README.md`](../README.md#build-and-verify).
