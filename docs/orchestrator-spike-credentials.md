# Orchestrator spike credential spec (blocked)

No secrets are stored in this file. Values below are placeholders and shapes only.

Status: **blocked** — the E-174 guarded spike (`Orch Spike` test agent only) has
not run because no dedicated Rakazo session/space was available. This is the
credential packet for the PM's blocked backlog item.

## Auth flow (how the session token is minted)

1. `rakazo-beeper login` asks Rakazo for a normal OIDC sign-in URL
   (`POST /api/auth/sign-in/social`, provider `oidc`) and validates its
   `client_id`, callback, state, nonce, S256 PKCE and `openid email profile`
   scope (`src/login.rs:43-72`, `:145-164`).
2. It starts a **device authorization** at the issuer for the fixed `--client-id`
   with `scope=openid email profile oidc:authorize:<rakazo-oidc-client-id>`
   (`src/login.rs:205-210`). A human approves the device code at the issuer.
3. The approved device access token is presented to the issuer's authorization
   endpoint; the issuer redirects to Rakazo's exact callback
   `/api/auth/callback/oidc`. Rakazo sets a Better Auth session cookie
   (`better-auth.session_token` or `__Secure-better-auth.session_token`)
   (`src/login.rs:92-113`, `:270-315`).
4. The client verifies `/api/auth/get-session` returns a user id, then saves the
   **cookie value** (not the `name=value` pair) to `--output` with mode 0600 and
   an atomic rename (`src/login.rs:115-140`, `:315-330`).
5. Every orchestrator RPC sends `Authorization: Bearer <that session value>` and
   `x-rakazo-space-id: <space id>` (`src/rakazo.rs:130-135`). There is no model
   API key and no Beeper credential in this path.

## Credential spec table

| # | Credential | Flow | Exact env var / CLI + shape | Audience, scope, company claim | Base URL | Client registration / account needs | Consumed at (file:line) |
|---|---|---|---|---|---|---|---|
| 1 | Rakazo orchestration session (direct) | Minted by `login` device flow, or supplied by the operator | `RAKAZO_ORCH_TOKEN=<session-value>` — one Better Auth session cookie value; no `Bearer ` prefix, no `name=`, no newline; non-empty; mutually exclusive with the `_FILE` form | Bearer for Rakazo RPC API; audience is the Rakazo origin. No JWT/`aud` or company claim is decoded by this client. Server binds the session to its account/company; the client enforces the space separately. | Base URL `RAKAZO_ORCH_URL`, default `https://agents.fpl.dev` | A dedicated test-company/space Rakazo account; do **not** reuse the bridge tenant token or a personal production session | Read at `src/orch.rs:100`; secret resolution `src/config.rs:158-185`; header built at `src/rakazo.rs:130-135`; passed in at `src/orch.rs:112` |
| 2 | Rakazo orchestration session (file) | Preferred form of #1 | `RAKAZO_ORCH_TOKEN_FILE=<path>` — path to a file holding exactly one session value; Unix mode with no group/other bits (`mode & 0o077 == 0`, e.g. 0600); trailing whitespace trimmed, leading whitespace preserved; non-empty; mutually exclusive with `RAKAZO_ORCH_TOKEN`. `secret()` follows symlinks, so point it at a real owner-only file. | Same as #1 | Same as #1 | Same as #1 | Read at `src/orch.rs:100`; permission/content checks `src/config.rs:158-185` |
| 3 | Rakazo space binding | Read from the test space; must match the session's resolved `me.spaceId` | `RAKAZO_ORCH_SPACE_ID=<opaque-space-id>` — non-empty string, no whitespace padding; **not** a URL or company name | Sent as `x-rakazo-space-id`; scopes all RPC calls. No company claim is trusted from the token: `orch whoami` compares `me.spaceId` to this value and fails on mismatch. Use a dedicated test space, not a production PM space. | Same base URL as #1 | The test account must be a member of this dedicated space | Read at `src/orch.rs:102-104`; header `src/rakazo.rs:135`; `me` fields `src/orch.rs:118-122`; enforced `src/main.rs:231-239` |
| 4 | Rakazo API base URL | Optional override for a non-default test instance | `RAKAZO_ORCH_URL=https://<host>[:port]` — absolute origin; no userinfo, query or fragment; HTTPS required except loopback HTTP | Determines the RPC audience and login callback origin | Default: `https://agents.fpl.dev`; test value must be the exact origin (no trailing path) | Rakazo must expose the RPC surface and callback at this origin | Read at `src/orch.rs:106-110`; URL validation `src/config.rs:196-207` |
| 5 | OIDC issuer (minting only) | Used by `login` for discovery/device grant | `--issuer https://<issuer>` — exact origin, path `/`, no query/fragment | Device token audience is the issuer; discovery must advertise matching issuer/device/token endpoints, both same-origin | Default: `https://sso.fpl.dev` | Issuer must support RFC 8628 device authorization and OIDC discovery | CLI `src/main.rs:43-44`; validated `src/login.rs:166-204`; device grant `src/login.rs:205-265` |
| 6 | OIDC client id (minting only) | Device authorization client | `--client-id <id>` — `[A-Za-z0-9_-]{1,256}`; must equal the `client_id` in Rakazo's authorize URL | Public device client; no client secret in this repo. Scope requested: `openid email profile oidc:authorize:<rakazo-oidc-client-id>`; the `oidc:authorize:` target is Rakazo's own OIDC client identity, and it must not change during pairing | Issuer origin from #5 | Register `rakazo-beeper` (default) at the issuer for the device grant; allow `openid`, `email`, `profile`, `oidc:authorize:<rakazo-oidc-client-id>`; no client secret | CLI default `src/main.rs:45-46`; used `src/login.rs:166`, `:205-210`, `:238-245`; target validation `src/login.rs:43-72`, `:275-289` |
| 7 | Session output file (minting only) | `login --output` writes #1/#2 | `--output <path>` — parent must exist; target absent or a regular non-symlink; created mode 0600; atomic rename; contains the session value plus newline | Holds the bearer from #1 | n/a | The directory must be writable by the operator; store outside the repo, e.g. a private secrets dir | CLI `src/main.rs:47-48`, `:125-132`; writer `src/login.rs:115-140` |
| 8 | Test identity / company membership | Human approval of the device code | No env var — this is an account/space prerequisite | The approving account must be in the **dedicated test company/space**. Company/organization claims are not read by this client; `me.email` and `me.spaceId` are the only identity fields used. | n/a | A test Rakazo user (not the production bridge owner) and membership in the test space; access to the issuer's device approval page | `src/orch.rs:118-122`; `src/main.rs:231-239` |

## Registration checklist for the PM backlog item

- [ ] Issuer registers a public device-grant client for `--client-id rakazo-beeper`
      (or a distinct test client id) with no secret and the four scopes in row 6.
- [ ] Rakazo's OIDC provider is configured to the same issuer/client target and
      exact callback `<RAKAZO_ORCH_URL>/api/auth/callback/oidc`; `openid email
      profile` only, S256 PKCE.
- [ ] A dedicated test company/space exists and has an id for
      `RAKAZO_ORCH_SPACE_ID`; it is not the production PM space.
- [ ] An operator runs `rakazo-beeper login` with the test URLs/client, approves
      the device code with the test account, and places the output where
      `RAKAZO_ORCH_TOKEN_FILE` points with mode 0600.
- [ ] `rakazo-beeper orch whoami` prints `space <id>` equal to
      `RAKAZO_ORCH_SPACE_ID` before any entity is created.
- [ ] Only then create the single `Orch Spike` root (`spawnKey
      fleet-orch-spike-v1`); no PM children or production wiring.

## Explicit non-choices

- No `RAKAZO_ORCH_TOKEN` value is stored in this repository, this document, the
  shell history, or any test fixture.
- No OIDC client secret is needed or accepted by this flow.
- No production `agents.fpl.dev` PM space, bridge tenant token, Beeper token, or
  deploy credential is part of the spike.
