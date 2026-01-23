# Celestia Forwarding Relayer

An off-chain relayer service that monitors forwarding addresses on Celestia and automatically triggers cross-chain token transfers.

## Features

- **Backend Server** - REST API for managing forwarding requests with SQLite storage
- **Persistent Storage** - SQLite-based storage for both balance cache and backend requests
- **Multi-Address Support** - Monitor multiple forwarding addresses simultaneously
- **Robust Status Updates** - Automatically updates backend after successful forwarding

## Quick Start

See [USAGE.md](USAGE.md) for detailed usage instructions.

### Run Backend Server

```bash
cargo run --release -- backend --port 8080
```

The backend server provides a REST API for managing forwarding requests with SQLite persistence. Database files are stored in `storage/` by default.

### Run Relayer

```bash
cargo run --release -- relayer \
  --backend-url http://localhost:8080 \
  --relayer-mnemonic "your mnemonic here"
```

## Documentation

- [USAGE.md](USAGE.md) - Detailed usage guide for new features
- [RELAYER.md](RELAYER.md) - Complete relayer specification
- [CHANGELOG.md](CHANGELOG.md) - Recent changes and migration guide

# E2E Test Guide

Complete end-to-end test guide for the forwarding relayer with backend server.

## Prerequisites

- Docker and Docker Compose installed
- Rust toolchain installed

## Architecture Overview

The E2E test involves:
1. **Celestia Node** (via Docker) - Local testnet for transaction submission
2. **Backend Server** (port 8080) - Provides forwarding requests to the relayer via REST API
3. **Relayer** - Monitors addresses and submits forwarding transactions

## Steps

### 1. Build the relayer

```bash
cargo build --release
```

### 2. Start the Docker environment

```bash
make start
```

Wait ~30 seconds for all containers to be healthy. This starts:
- `celestia-validator` - Celestia testnet node
- Supporting services for cross-chain transfers

### 3. Fund the relayer account

The relayer uses a test mnemonic that derives to address `celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3`.

Fund this account so it can pay gas fees:

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

Verify the balance:

```bash
docker exec celestia-validator celestia-appd query bank balances \
  celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 \
  --node http://localhost:26657
```

### 4. Start the backend server

In a **new terminal (Terminal 1)**:

```bash
# For E2E testing, use the demo example which pre-populates test data
cargo run --example backend_demo
```

You should see output like:

```
Added E2E test forwarding request:
  Forward address: celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e
  Destination domain: 1234
  Destination recipient: 0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d

Backend running on http://localhost:8080
Available endpoints:
  GET  http://localhost:8080/forwarding-requests
  POST http://localhost:8080/forwarding-requests
  PATCH http://localhost:8080/forwarding-requests/{id}/status
```

The backend server is now serving one forwarding request for the test address, stored in SQLite.

**Note:** For production use, run the backend without test data:
```bash
cargo run --release -- backend --port 8080
```
Then create forwarding requests via the REST API. Database files are stored in `storage/` by default.

### 5. Start the relayer

In a **new terminal (Terminal 2)**:

```bash
RUST_LOG=info ./target/release/forwarding-relayer relayer \
  --celestia-rpc http://localhost:1317 \
  --backend-url http://localhost:8080 \
  --relayer-mnemonic "veteran capital explain keep focus nuclear police casino exercise pitch hover job sleep slam wasp honey tenant breeze hold hat quality upper multiply gossip"
```

You should see logs like:

```
Starting forwarding relayer
Celestia RPC: http://localhost:1317
Backend URL: http://localhost:8080
Poll interval: 6s
Processing 1 forwarding requests
Checking balance at celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e
No new deposits detected at celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e
```

The relayer is now polling the backend and monitoring the forwarding address every 6 seconds.

### 6. Send tokens to the forwarding address

In a **new terminal (Terminal 3)**:

```bash
# Using the Makefile helper
make send-to-forward-addr

# Or manually specify the address
make send-to-address ADDR=celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e AMOUNT=1000000
```

This sends 1 TIA (1,000,000 utia) to the forwarding address.

### 7. Watch the relayer automatically forward

Switch back to **Terminal 2** (relayer logs). Within ~6 seconds, you should see:

```
New deposit detected at celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e! Balance changed:
  1000000 utia
IGP fee for domain 1234: quoted=0, max=0utia (1.1x buffer)
Submitting forward: addr=celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e, domain=1234, recipient=0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d, max_fee=0utia
Transaction broadcast successfully: <TX_HASH>
All tokens forwarded successfully from celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e
Updated backend status for celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e to completed
```

The relayer:
1. Detected the new deposit
2. Queried the IGP fee
3. Submitted a `MsgForward` transaction
4. Verified all tokens were forwarded
5. Updated the backend status to "completed"

### 8. Verify the transaction

Query the transaction to confirm success:

```bash
docker exec celestia-validator celestia-appd query tx <TX_HASH> --node http://localhost:26657
```

Look for:
- `code: 0` - Transaction succeeded
- `EventTokenForwarded` events with `success: true`

### 9. Check the backend status

Verify the backend was updated:

```bash
curl http://localhost:8080/forwarding-requests | jq
```

You should see the request with `"status": "completed"`.

### 10. Verify balance cache

Check that the balance cache was saved to disk:

```bash
# View the balance cache database
sqlite3 storage/balance_cache.db "SELECT * FROM balance_cache;"
```

You should see the cached balance for the forwarding address.

## Cleanup

Stop the Docker environment:

```bash
make stop
```

Stop the backend server (Ctrl+C in Terminal 1) and relayer (Ctrl+C in Terminal 2).

## Test Parameters

- **Dest Domain**: 1234 (test chain ID)
- **Dest Recipient**: `0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d`
- **Forwarding Address**: `celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e` (derived from domain + recipient)
- **Relayer Address**: `celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3`

## Testing Restart Robustness

To test that the relayer survives restarts:

1. Complete steps 1-6 above (send tokens but don't wait for forwarding)
2. Stop the relayer (Ctrl+C in Terminal 2)
3. Restart the relayer (run step 5 again)
4. The relayer should:
   - Load the balance cache from disk
   - Detect that tokens are still present (different from cached balance)
   - Forward the tokens automatically

This demonstrates the "stateless-capable" property from the specification.
