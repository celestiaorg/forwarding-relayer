# Simple E2E Setup (No Docker for Anvil)

This guide runs Anvil locally without Docker for maximum simplicity.

## Architecture

```
Celestia (Docker) <----Hyperlane----> Anvil (Local Process)
```

## Prerequisites

```bash
# Install Foundry (includes Anvil)
curl -L https://foundry.paradigm.xyz | bash
foundryup

# Verify
anvil --version
cast --version
```

## Step-by-Step Guide

### 1. Start Celestia (Docker)

```bash
# Start only Celestia services (no Anvil, no Hyperlane)
docker compose up celestia-init celestia-validator celestia-bridge --detach
```

Wait ~15 seconds for Celestia to be healthy.

### 2. Start Anvil (Local)

In a **new terminal**:

```bash
anvil --chain-id 31337 --block-time 1
```

Leave this running. You should see:
```
Available Accounts:
(0) 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 (10000.000000000000000000 ETH)
...
```

### 3. Deploy Hyperlane Contracts

You have two options:

#### Option A: Manual Deployment (Recommended)

```bash
# Install Hyperlane CLI globally
npm install -g @hyperlane-xyz/cli

# Deploy to local Anvil
cd hyperlane

# Deploy core contracts
hyperlane core deploy \
  --chain anvil \
  --registry ./registry \
  --key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --yes

# Deploy warp route (synthetic wTIA)
hyperlane warp deploy \
  --config ./configs/warp-config.yaml \
  --registry ./registry \
  --key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --yes
```

#### Option B: Use Docker Image (if you prefer)

```bash
# Run hyperlane-init as a one-off container
docker run --rm \
  --network host \
  -v ./hyperlane:/home/hyperlane \
  ghcr.io/celestiaorg/hyperlane-init:latest \
  bash -c "cd /home/hyperlane && ./scripts/docker-entrypoint.sh"
```

### 4. Get Warp Token Address

```bash
cat hyperlane/registry/deployments/warp_routes/TIA/anvil-addresses.yaml | grep "synthetic:"
```

Copy the token address and export it:

```bash
export WARP_TOKEN=0xYOUR_TOKEN_ADDRESS
```

### 5. Start Hyperlane Relayer (Local)

In a **new terminal**:

```bash
# Install Hyperlane agent
npm install -g @hyperlane-xyz/agents

# Or use Docker
docker run -d \
  --name hyperlane-relayer \
  --network host \
  -v ./hyperlane:/config \
  gcr.io/abacus-labs-dev/hyperlane-agent:agents-v1.7.0 \
  /app/relayer --config /config/relayer-config.json
```

### 6. Build Forwarding Relayer

```bash
cargo build --release
```

### 7. Fund Relayer Account

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

### 8. Start Backend Server

In **Terminal 1**:

```bash
cargo run --release -- backend --port 8080
```

### 9. Derive Forwarding Address

In **Terminal 2**:

```bash
cargo run --release -- derive-address \
  --domain 31337 \
  --recipient 0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266
```

Copy the output address (e.g., `celestia1abc...`).

### 10. Create Forwarding Request

```bash
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "celestia1YOUR_ADDRESS_HERE",
    "dest_domain": 31337,
    "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
  }'
```

### 11. Start Forwarding Relayer

In **Terminal 3**:

```bash
RUST_LOG=info cargo run --release -- relayer \
  --celestia-rpc http://localhost:1317 \
  --backend-url http://localhost:8080
```

### 12. Query Initial Balance

In **Terminal 4**:

```bash
cast call $WARP_TOKEN \
  "balanceOf(address)(uint256)" \
  0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
  --rpc-url http://localhost:8545
```

Should return: `0`

### 13. Fund Forwarding Address

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1YOUR_ADDRESS_HERE 1000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

### 14. Watch the Relayer

In Terminal 3, you should see within ~6 seconds:
```
New deposit detected!
Submitting forward...
Transaction broadcast successfully
```

Wait ~10-20 seconds for Hyperlane relay.

### 15. Query Final Balance

```bash
cast call $WARP_TOKEN \
  "balanceOf(address)(uint256)" \
  0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
  --rpc-url http://localhost:8545
```

Should return: `1000000` (or close to it)

### 16. Success

Tokens forwarded from Celestia → Anvil.

## Troubleshooting

### Anvil Connection Refused

Make sure Anvil is running and listening on `0.0.0.0:8545`:

```bash
anvil --host 0.0.0.0 --chain-id 31337
```

### Hyperlane Can't Connect to Anvil

If using Docker for Hyperlane, use `--network host` to access local Anvil.

### RPC URL Issues

Local Anvil: `http://localhost:8545`
Docker accessing Anvil: Use `--network host` mode

## Cleanup

```bash
# Stop Anvil (Ctrl+C in Anvil terminal)
# Stop relayer (Ctrl+C in Terminal 3)
# Stop backend (Ctrl+C in Terminal 1)

# Stop Celestia
docker compose down

# Optional: Clean volumes
docker compose down -v
```

## Benefits

- No complex Docker networking
- Anvil starts instantly
- Easy to debug (all logs visible)
- Can use Foundry tools directly
- Simpler configuration
- Works on any platform

## Quick Reference

**Start everything:**
```bash
# Terminal 1: Celestia
docker compose up celestia-init celestia-validator celestia-bridge -d

# Terminal 2: Anvil
anvil --chain-id 31337 --block-time 1

# Terminal 3: Backend
cargo run --release -- backend --port 8080

# Terminal 4: Forwarding Relayer
cargo run --release -- relayer --backend-url http://localhost:8080
```

**Test commands:**
```bash
# Derive address
cargo run --release -- derive-address --domain 31337 --recipient 0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266

# Query balance
cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545

# Fund address
docker exec celestia-validator celestia-appd tx bank send default <address> 1000000utia --fees 800utia --yes --node http://localhost:26657
```
