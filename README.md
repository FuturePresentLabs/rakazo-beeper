# Rakazo → Beeper

A Rust application-service bridge that exposes selected Rakazo agents as private
chats in Beeper. One executable supports a personal self-hosted instance and an
operator-configured multi-tenant instance. Beeper Desktop does not have to stay open.

FPL's Beachhead Shroud deployment is owned by
[shared-infra](https://github.com/FuturePresentLabs/shared-infra/tree/deploy/rakazo-beeper/projects/rakazo-beeper).
The source-owned `.fab/pipelines.json` publishes the immutable rootfs consumed by
that deployment. The personal Beachhead tenant is live with six agent chats;
an owner message, assistant reply and restart without duplicate delivery were
verified. The native user-service instructions below are for personal self-hosting.

**Initial scope:** new text messages from the configured Beeper owner go to the
agent's existing Rakazo thread. Completed bot text and questions return to Beeper.
Existing text history is imported automatically, including chats connected before
history support was installed. Messages sent
through Rakazo's web interface are not echoed as the owner's Beeper messages.

History uses Beeper's native batch backfill API, preserving original timestamps
and attributing old prompts to the owner and replies to the agent. It imports all
available text before the chat's saved initial baseline, one page per sync, without
notifications or changing read markers. Durable progress and deterministic event
IDs make retries and restarts safe. Imported prompts cannot trigger the agent.
Attachments, tool output and internal reasoning are excluded, as with live messages.
Homeservers without Beeper's batch API cannot import history; failures remain
visible and retry without advancing the history cursor.

Agent profile pictures and chat icons follow Rakazo, including uploaded PNG,
JPEG, WebP and GIF avatars and static PNGs of its generated mascots. The renderer
uses Rakazo's geometry and palette with source-derived parity vectors; the API
does not expose rendered mascot images. The bridge copies no external image URL.
Changed pictures and names sync automatically. Uploaded media references and
successful profile updates are cached per tenant and agent across restarts.
The FPL logo identifies the bridge bot and protocol in Beeper; individual agent
pictures remain the chat icons. The bundled logo is the existing FPL square mark
from `waitlist/web/public/images/fpl_square.png`.

**Current limitation:** agent rooms are intentionally unencrypted. End-to-bridge
encryption is not implemented. Beeper's servers and the bridge operator can read
their contents. The bridge creates its own private rooms with encryption disabled;
do not start an encrypted DM with a ghost manually. Encrypted or unexpectedly
shared rooms pause delivery. This is an initial self-hosted pilot, not a completed
public SaaS onboarding product.

## Build and verify

```sh
cargo check --locked
cargo test --locked
cargo build --release --locked
```

The implementation and runtime are Rust. SQLite is bundled through its existing
Rust binding; TLS and authenticated payload encryption use established libraries.
`bbctl` is Beeper's external registration utility, used once for enrollment. No Go,
Node, Python, bridge framework, or proxy process is required to run this service.

## Personal setup

1. Install [Beeper's bridge manager](https://github.com/beeper/bridge-manager) and
   authenticate with `bbctl login` if necessary. Create a separate registration:

   ```sh
   umask 077
   mkdir -p secrets data
   bbctl register --json sh-rakazo > secrets/registration-personal.json
   ```

   Keep the **entire JSON response**. It includes `registration`, `homeserver_url`,
   `homeserver_domain`, and `your_user_id`. Do not register with a push address
   when using the default outbound WebSocket transport. Never delete an existing
   registration to fix a connection problem: deletion removes its rooms.

2. Copy `config.example.toml` to `config.toml`. Set the owner and homeserver to
   the exact values from the registration bundle. Set the Rakazo space ID to
   the space that owns the selected agents. Paths are relative to the config file.

3. Generate the database key and supply a valid **Rakazo session bearer token**:

   ```sh
   target/release/rakazo-beeper generate-key > secrets/database-key
   # Place the Rakazo session token in secrets/rakazo-token using your secret manager.
   chmod 600 secrets/database-key secrets/rakazo-token secrets/registration-personal.json
   export RAKAZO_BEEPER_KEY_FILE="$PWD/secrets/database-key"
   export RAKAZO_PERSONAL_TOKEN_FILE="$PWD/secrets/rakazo-token"
   ```

   For an issuer enrolled for device authorization, obtain a dedicated session:

   ```sh
   target/release/rakazo-beeper login --output secrets/rakazo-token
   ```

   Open the displayed FPL verification URL, enter the code, and approve the
   named Rakazo login. The Rust client completes Rakazo's existing OIDC flow,
   preserving state, nonce, and S256 PKCE, then saves its new session atomically
   with mode 0600. It reads no browser cookies. Its temporary issuer credential
   stays at the issuer and is discarded after login. No FPL refresh credential
   is stored. Issuer enrollment is described in the shared-infra deployment.

   The token is a Better Auth session for this Rakazo installation, not a model
   API key, FPL OIDC access token, or Beeper access token. Use a dedicated session;
   expired/revoked sessions fail visibly and delivery remains queued. Run login
   again and restart the service to replace a revoked session. Each configured secret env name
   accepts either its direct value or a corresponding `*_FILE` variable, never
   both. Secret files must be owner-readable only on Unix.

4. Discover agents, explicitly select their IDs, and validate access:

   ```sh
   target/release/rakazo-beeper bots --tenant personal
   # Put selected IDs in bot_ids = ["..."] in config.toml.
   target/release/rakazo-beeper check
   target/release/rakazo-beeper run
   ```

   `check` performs read-only Rakazo checks; it does not create chats or validate
   every Beeper endpoint. `run` creates a ghost and private room for each selected
   agent, then invites the owner. Accept those room invitations in Beeper and
   send a text message. Bot discovery is constrained to the token's selected space.
   `bot_ids = []` creates no rooms. Stop the service before changing its configuration.

## Agora-one: native systemd service

The personal deployment uses an independent user service and native Linux binary:

```text
~/.local/share/rakazo-beeper/
  bin/rakazo-beeper
  config.toml
  secrets/registration-personal.json
  secrets/database-key
  secrets/rakazo-token
  data/bridge.db
~/.config/systemd/user/rakazo-beeper.service
```

Build on the Linux host with `cargo build --release --locked`. Install the binary
under `bin/`, put the personal config and credentials in the layout above, and
copy `deploy/rakazo-beeper.service` into the user's systemd directory. It reads
credentials from files, binds HTTP on loopback, and writes only the data directory.

```sh
mkdir -p ~/.local/share/rakazo-beeper/{bin,secrets,data} ~/.config/systemd/user
chmod 700 ~/.local/share/rakazo-beeper ~/.local/share/rakazo-beeper/secrets
cp target/release/rakazo-beeper ~/.local/share/rakazo-beeper/bin/
cp deploy/rakazo-beeper.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now rakazo-beeper.service
systemctl --user status rakazo-beeper.service
journalctl --user -u rakazo-beeper.service -n 50 --no-pager
curl --fail http://127.0.0.1:8080/readyz
```

If the service must survive logout, enable lingering for the service user through
the host's existing administration process. Do not run two instances using the same
registration. A replaced Beeper connection exits with status 78, which the supplied
systemd unit does not restart automatically.

## Docker

`deploy/compose.yaml` builds the same Rust executable. Set `BRIDGE_HOME` to an
absolute directory containing the config, `secrets/`, and `data/` above. Change
`listen` to `0.0.0.0:8080` **inside the container**. The host port stays on loopback.
The container runs as UID/GID 10001; give that UID ownership of this deployment's
secret files and data directory, with the secret files remaining mode 0600.

```sh
export BRIDGE_HOME=/absolute/path/to/bridge
docker compose -f deploy/compose.yaml config --quiet
docker compose -f deploy/compose.yaml up --build -d
docker compose -f deploy/compose.yaml logs --tail 50
```

The Compose restart policy is suitable for a single operator-managed instance;
stop it before moving its registration to a different host to avoid reconnect
competition. No public listener, tunnel, or inbound firewall rule is needed in
WebSocket mode. For a standard Matrix homeserver, set `websocket = false`, provide
the same JSON bundle format, register its application service with the homeserver,
and route its push URL to `/tenants/<id>/_matrix/app/v1/transactions/<txn>`.

## FPL-hosted mode

Append `[[tenants]]` entries to the same config. Each customer requires a separate
Beeper application-service registration, exact owner, Rakazo session, explicit
Rakazo space, and bot allowlist. The operator enrolls customers and deploys the
configuration; there is no unauthenticated tenant-creation API or public signup UI.
New credentials and configuration require a service restart.

Each tenant has an independent worker and outbound Beeper connection. Storage,
event deduplication, ghost IDs, message nonces, and cursors include the tenant ID.
Changing an existing tenant's owner, upstream endpoints, registration identity, or
space is refused against the existing database; create a new tenant ID instead.
Separate token rotation retains the existing identity and cursors.

This version uses one SQLite database and one process per installation. A process
lock prevents competing workers on the same database. It is appropriate for a
bounded, operator-managed hosted pilot. Horizontal replicas, per-tenant quotas,
self-service enrollment, automatic credential refresh, billing, and end-to-bridge
encryption remain prerequisites for a broad public service. Tenant isolation is
an application boundary; the process and its operator can access all configured
credentials. Deploy separate instances when a process isolation boundary is needed.

## Delivery and operations

- Incoming transactions and messages commit together before Beeper is acknowledged.
  Retries reuse Rakazo `clientNonce`, and outgoing Matrix transaction IDs remain
  stable. These provide retry deduplication under the upstream APIs' retention rules,
  not an unconditional exactly-once guarantee.
- Queued message bodies use XChaCha20-Poly1305 with tenant/event-associated data.
  Completed queue bodies are removed. Tokens remain in secret files/environment,
  never in SQLite. Room IDs, agent IDs, tenant IDs, and delivery metadata are stored
  in plaintext. Back up the database and its encryption key separately.
- Polling waits until a thread has no active run, paginates back to its committed
  message sequence, and forwards completed bot text in sequence. A snapshot bounds
  the read so a concurrently starting run cannot be forwarded partially. There is
  no per-token streaming. Long messages split at UTF-8 boundaries.
- Each delivery checks authoritative room membership and encryption state. Rooms
  are private, non-federated, and grant the owner message-sending permission while
  reserving invites and state changes for the ghost.
- Unsupported attachments, edits, and reactions produce warnings and are not
  forwarded. Tool output and internal reasoning are not forwarded. Questions are
  text only; approve agent actions in Rakazo. Rich cards and editing already-sent
  messages are not synchronized. Failed agent runs are visible in Rakazo; there
  is no dedicated Beeper run-failure notification yet.
- `/healthz` reports process liveness; `/readyz` checks recent successful sync and
  WebSocket connectivity; `/metrics` exposes aggregate readiness and queue depth.
  Keep these endpoints private. Readiness stays down after receiving an encrypted
  event until the room is corrected and the service restarts.
- Network/auth failures retry with backoff for queued sends and WebSocket reconnects.
  Logs omit message bodies, upstream response bodies, and credential values. A
  failing agent does not prevent other agents being polled, but inbound sends are
  ordered per tenant: an unresolvable first message holds subsequent sends.
- Before removing an agent from `bot_ids`, drain its inbound queue. Otherwise its
  queued messages remain pending and require restoring the allowlist. Archived or
  inaccessible agents fail visibly; existing rooms are not silently deleted.
- Transaction/event deduplication metadata is retained indefinitely in this pilot.
  Monitor disk space. Backups should use SQLite's online backup facility or stop
  the service before copying the database; include the WAL if copying a live DB.

## Sources and compatibility

The Rakazo adapter follows upstream
[`elie222/rakazo` at `535de960`](https://github.com/elie222/rakazo/tree/535de9600bdd90530aa7cd954bf2d29b4a083aaf),
particularly `packages/contracts/src/rpc.ts`, `events.ts`, and `apps/api/src/app.ts`.
The deployed installation may differ; `check` validates authenticated bot discovery,
and a live round trip is still required before declaring the deployment verified.

Protocol references:
[oRPC](https://orpc.dev/docs/rpc/protocol),
[Matrix application services](https://spec.matrix.org/v1.16/application-service-api/),
[Beeper bridge manager](https://github.com/beeper/bridge-manager), and
[Beeper as_sync framing](https://github.com/mautrix/go/blob/main/appservice/websocket.go).
Source annotations appear at the corresponding implementations.
