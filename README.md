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
cargo run --release -- backend --port 8080
```

The backend server starts with an empty database in `storage/backend.db`.

### 5. Create a forwarding request

In a **new terminal (Terminal 2)**, create a forwarding request via the REST API:

```bash
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e",
    "dest_domain": 1234,
    "dest_recipient": "0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d"
  }'
```

This creates a forwarding request with:
- **Forward address**: `celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e`
- **Destination domain**: 1234
- **Destination recipient**: `0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d`

Verify the request was created:

```bash
curl http://localhost:8080/forwarding-requests | jq
```

### 6. Start the relayer

In a **new terminal (Terminal 3)**:

```bash
RUST_LOG=info ./target/release/forwarding-relayer relayer \
  --celestia-rpc http://localhost:1317 \
  --backend-url http://localhost:8080 \
  --relayer-mnemonic "veteran capital explain keep focus nuclear police casino exercise pitch hover job sleep slam wasp honey tenant breeze hold hat quality upper multiply gossip"
```

**Note**: This uses the default test mnemonic which derives to `celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3` (the address you funded in step 3).

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

### 7. Send tokens to the forwarding address

In a **new terminal (Terminal 4)**:

```bash
# Using the Makefile helper
make send-to-forward-addr

# Or manually specify the address
make send-to-address ADDR=celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e AMOUNT=1000000
```

This sends 1 TIA (1,000,000 utia) to the forwarding address.

### 8. Watch the relayer automatically forward

Switch back to **Terminal 3** (relayer logs). Within ~6 seconds, you should see:

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

### 9. Verify the transaction

Query the transaction to confirm success:

```bash
docker exec celestia-validator celestia-appd query tx <TX_HASH> --node http://localhost:26657
```

Look for:
- `code: 0` - Transaction succeeded
- `EventTokenForwarded` events with `success: true`

### 10. Check the backend status

Verify the backend was updated:

```bash
curl http://localhost:8080/forwarding-requests | jq
```

You should see the request with `"status": "completed"`.

### 11. Verify balance cache

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

Stop the backend server (Ctrl+C in Terminal 1) and relayer (Ctrl+C in Terminal 3).

## Test Parameters

- **Dest Domain**: 1234 (test chain ID)
- **Dest Recipient**: `0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d`
- **Forwarding Address**: `celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e` (derived from domain + recipient)
- **Relayer Address**: `celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3`

## Testing Restart Robustness

To test that the relayer survives restarts:

1. Complete steps 1-7 above (send tokens but don't wait for forwarding)
2. Stop the relayer (Ctrl+C in Terminal 3)
3. Restart the relayer (run step 6 again)
4. The relayer should:
   - Load the balance cache from disk
   - Detect that tokens are still present (different from cached balance)
   - Forward the tokens automatically

This demonstrates the "stateless-capable" property from the specification.
