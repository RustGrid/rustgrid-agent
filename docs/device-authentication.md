# Agent device authentication contract

`rustgrid-agent login` uses a device flow so an operator never has to create a
worker, copy its UUID, or handle an API key. The Agent Ops service must expose
the two unauthenticated endpoints below. Device codes must be high entropy,
single-use, short-lived, stored hashed, and bound to the final authenticated
user and tenant. Responses must not be cacheable.

## Start authorization

`POST /api/v1/agent-workers/device-authorization`

Request:

```json
{"client_name":"rustgrid-agent"}
```

Response (`200`):

```json
{
  "device_code": "opaque-secret",
  "user_code": "ABCD-EFGH",
  "verification_uri": "https://app.rustgrid.com/device",
  "verification_uri_complete": "https://app.rustgrid.com/device?user_code=ABCD-EFGH",
  "expires_in": 900,
  "interval": 5
}
```

The browser page authenticates the operator, asks for the user code when it is
not already in the URL, shows the worker being authorized, and requires explicit
confirmation. The user code is only a lookup code and must not authenticate API
requests.

## Exchange device code

`POST /api/v1/agent-workers/device-authorization/token`

Request:

```json
{"device_code":"opaque-secret"}
```

Responses:

- `202 Accepted` while the operator has not completed authorization.
- `429 Too Many Requests` when the client polls too quickly; the agent adds five
  seconds to its polling interval.
- `410 Gone` when the code expired or was consumed.
- `200 OK` exactly once after approval:

```json
{"worker_id":"00000000-0000-4000-8000-000000000000","api_key":"rgk_..."}
```

The returned key must be bound to that worker and have only the worker runtime
permissions documented in [the worker API contract](worker-api-contract.md).
Denial may return `403 Forbidden`. Other errors use the API's normal bounded
JSON error response. The agent never logs or prints `device_code` or `api_key`.
