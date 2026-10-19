# Security

## Reporting a vulnerability

Use GitHub's private vulnerability reporting:
https://github.com/FuturePresentLabs/rakazo-beeper/security/advisories/new

Please keep credentials, registration bundles, conversation contents and
exploitable details out of public issues. Include the affected commit, expected
behavior and a minimal reproducer with synthetic credentials. Maintainers
currently support the latest `main`; there are no versioned releases yet.

## Deployment boundary

This is an operator-managed pilot. Agent rooms are unencrypted: Beeper and the
bridge operator can read messages. Each tenant has a separate registration,
owner, Rakazo session, space and agent allowlist. Tenant isolation is enforced
within one process; use separate instances for process isolation. Do not expose
the health, readiness, metrics or application-service listener publicly.

Keep the registration, Rakazo token and database key in private files, outside
Git. SQLite queue bodies are encrypted; routing and deduplication metadata are
not. Back up the database and its key separately. Never run two instances with
the same registration or database. See README.md for deployment and recovery.

## Development checks

CI runs Rust checks and scans the complete Git history with Gitleaks. Before
publishing artifacts, inspect the packaging context and scan both repository
history and artifact contents. A clean secret scan does not certify runtime
security. Dependencies can be checked with `cargo audit --file Cargo.lock`.
