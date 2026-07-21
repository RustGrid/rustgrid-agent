# Agent device authentication contract

`rustgrid-agent login` uses RustGrid's first-party device authorization flow so
an operator never creates an API key, copies a worker UUID, or places an
administrative credential on the worker host. It is inspired by OAuth 2.0
device authorization but is a RustGrid-specific protocol.

## Operator flows

Interactive login:

```sh
rustgrid-agent login
rustgrid-agent status
rustgrid-agent serve
```

Headless and custom-instance login:

```sh
rustgrid-agent login --no-browser --instance https://agentops.example.com
```

Open the printed verification URL on any browser, sign in, select an eligible
tenant, review the hostname, operating system, architecture, installation ID,
and agent version, then explicitly approve or deny it. The CLI stops on denial,
expiry, consumption, an invalid device code, or Ctrl-C. It honors the server's
poll interval and `slow_down` response and retries bounded transient failures.

`rustgrid-agent logout` revokes the bound credential before deleting it
locally. If server revocation cannot be confirmed, the secret is retained for a
safe retry. Revoking the worker or credential in AgentOps immediately blocks
subsequent authenticated worker requests; run `login` again to reconnect.

## HTTP protocol

All endpoints are under `/api/v1/agent-workers` and return
`Cache-Control: no-store, max-age=0`. `device_code`, access tokens, and
authorization headers must never enter URLs, logs, traces, browser storage,
analytics, audit payloads, or error reports.

### Start authorization

`POST /device-authorizations` is unauthenticated and rate limited. The request
contains `client_id: "rustgrid-agent"`, the stable installation UUID, machine
metadata, agent version, and the fixed runtime scope set. A successful `201`
returns:

```json
{
  "device_code": "opaque-high-entropy-secret",
  "user_code": "ABCD-EFGH",
  "verification_uri": "https://agentops.example.com/device",
  "verification_uri_complete": "https://agentops.example.com/device?code=ABCD-EFGH",
  "expires_in": 900,
  "interval": 5
}
```

The database stores only hashes of both codes. The complete URI may contain the
human-readable user code; it never contains the device code.

### Browser review

These endpoints require an authenticated user with
`agents:workers:register`:

- `GET /device-authorizations/{user_code}` returns non-secret machine metadata.
- `POST /device-authorizations/{user_code}/approve` atomically links or safely
  reconnects the installation, rotates its prior device credential, and makes a
  new credential available for one-time delivery.
- `POST /device-authorizations/{user_code}/deny` creates no worker credential.

Approval is tenant-bound. The same installation cannot be silently moved to a
different tenant. Repeated approval returns the prior result without creating a
second worker or credential.

### Token exchange

`POST /device-authorizations/token` is unauthenticated and accepts only:

```json
{"client_id":"rustgrid-agent","device_code":"opaque-high-entropy-secret"}
```

Before approval, HTTP `400` returns a stable machine error such as
`authorization_pending` or `slow_down`, including the next interval. Other
terminal errors are `access_denied`, `expired_token`, `consumed_token`, and
`invalid_device_code`. A successful `200` returns a 30-day `expires_in`, nested
worker and instance metadata, runtime scopes, and `access_token` exactly once.
The encrypted delivery copy is
erased in the same transaction that marks the authorization consumed, so replay
returns `consumed_token`.

The worker credential excludes `agents:workers:register`,
`agents:workers:credentials`, and all API-key administration scopes. It is
bound to the worker, tenant, and installation. Reconnecting the same
installation increments the credential version and revokes the prior device
credential.

## Credential storage and precedence

Interactive secrets use macOS Keychain, Windows Credential Manager, or Linux
Secret Service through the `keyring` crate. A headless system without a working
OS store receives a warning and uses an atomic owner-only file under its user
configuration directory. The fallback rejects symlinks and broad permissions.
The project configuration stores only non-secret identity and storage metadata.
Legacy plaintext `<config>.credentials` files are migrated once and removed.

Endpoint precedence is:

1. `RUSTGRID_API_URL` for the legacy exact API endpoint override.
2. `RUSTGRID_INSTANCE_URL`, then configured `instance_url`, then the production
   default for instance selection.

Worker identity comes only from the configuration written by `login`, and the
corresponding credential comes only from the OS keychain or private-file
fallback. Environment variables cannot replace that pair.

## Compatibility and troubleshooting

The server must be deployed with migration `0047_worker_device_authentication`
and the plural `/device-authorizations` endpoints before interactive login is
enabled. A `404` or `405` start response is reported as an incompatible server;
the agent does not silently fall back to legacy registration. Existing managed
`register` remains a deprecated compatibility command.

- If the browser does not open, rerun with `--no-browser` and use the printed
  URL and code.
- If a code expires or was consumed, run `login` again; codes are not reusable.
- If approval is forbidden, select a tenant where the signed-in user has
  `agents:workers:register`.
- If the OS store is unavailable, verify the warned fallback directory is
  private or configure `RUSTGRID_CREDENTIALS_DIR` for the service account.
- If `status` reports `revoked` or `pending_upgrade`, reauthenticate after the
  operator resolves the worker state in AgentOps.
