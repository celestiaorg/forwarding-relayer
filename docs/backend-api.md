# Forwarding Backend API

This document describes the HTTP APIs exposed by the forwarding backend service in `src/backend.rs`.

The backend stores pending forwarding requests for relayers to poll. By default it listens on the port passed to `forwarding-relayer backend --port`, which defaults to `8080`.

## Base URL

```text
http://localhost:8080
```

## Start the Service

```bash
./target/release/forwarding-relayer backend --port 8080
```

To expose Prometheus metrics on a separate port:

```bash
./target/release/forwarding-relayer backend --port 8080 --metrics-port 9091
```

## Data Model

Forwarding requests use this JSON shape:

```json
{
  "forward_addr": "celestia1...",
  "dest_domain": 1234,
  "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
  "token_id": "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131",
  "created_at": "2026-04-17T12:34:56Z"
}
```

Field notes:

- `forward_addr`: Celestia bech32 forwarding address.
- `dest_domain`: Hyperlane destination domain ID.
- `dest_recipient`: 32-byte hex recipient, typically a left-padded EVM address.
- `token_id`: hex-encoded Hyperlane warp route token identifier.
- `created_at`: optional RFC3339 timestamp. The backend sets this when it creates a request.

## Endpoints

### `GET /forwarding-address`

Derives the deterministic forwarding address for a destination tuple.

Query parameters:

- `dest_domain` required `u32`
- `dest_recipient` required 32-byte hex string
- `token_id` required hex string

Example:

```bash
curl "http://localhost:8080/forwarding-address?dest_domain=1234&dest_recipient=0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266&token_id=0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131"
```

Success response:

```json
{
  "address": "celestia1..."
}
```

Status codes:

- `200 OK`: address derived successfully.
- `400 Bad Request`: invalid `dest_recipient` or `token_id` encoding, or wrong recipient length.

### `GET /forwarding-requests`

Lists all pending forwarding requests ordered by `created_at`.

Example:

```bash
curl http://localhost:8080/forwarding-requests
```

Success response:

```json
[
  {
    "forward_addr": "celestia1...",
    "dest_domain": 1234,
    "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    "token_id": "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131",
    "created_at": "2026-04-17T12:34:56Z"
  }
]
```

Status codes:

- `200 OK`: request list returned.
- `500 Internal Server Error`: backend storage failure.

### `POST /forwarding-requests`

Creates a pending forwarding request. This operation is idempotent by `forward_addr`.

Request body:

```json
{
  "forward_addr": "celestia1...",
  "dest_domain": 1234,
  "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
  "token_id": "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131"
}
```

Example:

```bash
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "celestia1...",
    "dest_domain": 1234,
    "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    "token_id": "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131"
  }'
```

Success response body:

```json
{
  "forward_addr": "celestia1...",
  "dest_domain": 1234,
  "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
  "token_id": "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131",
  "created_at": "2026-04-17T12:34:56Z"
}
```

Status codes:

- `201 Created`: request inserted.
- `200 OK`: request already existed for the same `forward_addr`; existing row returned.
- `429 Too Many Requests`: the client IP exceeded its rate-limit budget (see [Rate Limiting](#rate-limiting)). The response includes a `Retry-After` header (seconds) and a JSON body `{ "error": "rate limit exceeded", "retry_after_secs": <n> }`.
- `500 Internal Server Error`: backend storage failure.

Rate limiting applies only to this submission endpoint. The list (`GET`) and completion (`DELETE`) endpoints are never limited.

### `DELETE /forwarding-requests/:addr`

Removes a pending forwarding request after forwarding completes.

Example:

```bash
curl -X DELETE http://localhost:8080/forwarding-requests/celestia1...
```

Success response body:

```json
{
  "forward_addr": "celestia1...",
  "dest_domain": 1234,
  "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
  "token_id": "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131",
  "created_at": "2026-04-17T12:34:56Z"
}
```

Status codes:

- `200 OK`: request removed and returned.
- `404 Not Found`: no pending request exists for `:addr`.
- `500 Internal Server Error`: backend storage failure.

## Metrics

If the backend is started with `--metrics-port`, Prometheus metrics are exposed at:

```text
http://localhost:<metrics-port>/metrics
```

Current backend metrics include:

- `pending_requests`
- `oldest_pending_request_age_seconds`
- `requests_created_total{result="created|existing|error"}`
- `requests_completed_total{result="removed|not_found|error"}`
- `rate_limited_requests_total` — submissions rejected with `429`.

## Rate Limiting

Submissions (`POST /forwarding-requests`) are rate limited per client IP using a
token bucket. Rate limiting is **disabled** unless a config file is supplied via
`--rate-limit-config` (or the `RATE_LIMIT_CONFIG` env var):

```bash
./target/release/forwarding-relayer backend --port 8080 \
  --rate-limit-config /etc/forwarding-relayer/rate-limit.json
```

The client IP is taken from the TCP peer address, so the backend must be reached
directly (not via a reverse proxy that would mask the source IP).

Config file format (see `deploy/rate-limit.example.json`):

```json
{
  "default_per_minute": 1,
  "whitelist_default_per_minute": 6000,
  "apps": [
    { "name": "my-app", "hosts": ["203.0.113.10", "api.example.com"], "per_minute": 6000 }
  ]
}
```

- `default_per_minute` (default `1`): budget for any IP not on the whitelist.
- `whitelist_default_per_minute` (default `6000`, i.e. 100/s): budget for whitelisted
  Apps that omit their own `per_minute`.
- `apps[].hosts`: IP literals or hostnames. **Hostnames are resolved to IPs once at
  startup**; DNS changes require a restart. An unresolvable host falls back to the
  default budget.
- `apps[].per_minute`: optional per-App override of the whitelist budget.
- A budget of `0` means unlimited.

The budget is also the burst capacity; tokens refill continuously at `budget / 60`
per second.

## Notes

- The backend does not currently provide authentication.
- The backend stores data in SQLite at `storage/backend.db` by default.
- There is no single-request `GET /forwarding-requests/:addr` endpoint at the moment.
