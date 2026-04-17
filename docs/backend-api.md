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
- `500 Internal Server Error`: backend storage failure.

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

## Notes

- The backend does not currently provide authentication.
- The backend stores data in SQLite at `storage/backend.db` by default.
- There is no single-request `GET /forwarding-requests/:addr` endpoint at the moment.
