# Forwarding Relayer

Cross-chain token forwarding from Celestia to an EVM chain via Hyperlane.

When tokens (utia) are sent to a forwarding address on Celestia, the forwarding relayer detects the deposit and submits a `MsgForward` transaction. The Celestia forwarding module locks the tokens as collateral and dispatches a Hyperlane message. The Hyperlane relayer picks up the dispatch event and relays it to the destination EVM chain, where wrapped tokens (wTIA) are minted.

## Architecture

```
                         Celestia (domain 69420)                          Anvil (domain 1234)
                        ┌──────────────────────┐                        ┌──────────────────┐
                        │                      │                        │                  │
  1. Send utia -------->│  Forwarding Address   │                        │   EVM Warp Token │
                        │        |              │                        │   (HypSynthetic) │
  2. Detect deposit     │        v              │                        │        ^         │
     (forwarding        │  Forwarding Module    │   Hyperlane Message    │        |         │
      relayer)          │        |              │ ---------------------> │   Hyperlane      │
                        │        v              │   (Hyperlane relayer)  │   Mailbox        │
  3. MsgForward ------->│  Collateral Token     │                        │                  │
                        │  + Hyperlane Mailbox  │                        │  4. wTIA minted  │
                        └──────────────────────┘                        └──────────────────┘
```

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) and Docker Compose
- [Rust](https://rustup.rs/) toolchain
- [Foundry](https://book.getfoundry.sh/getting-started/installation) (`cast` is used for balance queries)
- `celestia-app-standalone:local` Docker image (built from [celestia-app](https://github.com/celestiaorg/celestia-app))

## Automated E2E Test

The fastest way to verify everything works:

```bash
# Build the Hyperlane init image (first time only, or after code changes)
make docker-build-hyperlane

# Run the full automated test
make e2e-auto
```

This will:
1. Stop any running containers and start fresh
2. Deploy Hyperlane contracts on both chains (NoopISM on Celestia, test ISM on Anvil)
3. Build the forwarding relayer
4. Derive a forwarding address, create a forwarding request
5. Fund the forwarding address with 1,000,000 utia
6. Wait for the forwarding relayer to detect the deposit and submit `MsgForward`
7. Wait for the Hyperlane relayer to relay the message to Anvil
8. Verify the wTIA balance increased on Anvil

Expected output:
```
Initial Balance: 0
Final Balance:   1000000
SUCCESS! Tokens forwarded successfully!
```

## Manual Step-by-Step

### 1. Start the environment

```bash
make docker-build-hyperlane  # first time only
make start
```

Wait for all services to be healthy. You can check with `docker ps`. The `hyperlane-init` container should have exited with code 0 (it deploys contracts then stops).

### 2. Build the forwarding relayer

```bash
cargo build --release
```

### 3. Fund the relayer account

The forwarding relayer needs gas to submit `MsgForward` transactions:

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

### 4. Start the backend server

The backend stores forwarding requests in a local SQLite database:

```bash
./target/release/forwarding-relayer backend --port 8080
```

### 5. Derive a forwarding address

In a new terminal:

```bash
make derive-address
```

This outputs a Celestia bech32 address like `celestia1u9nwgn95ugajgrsv7hgnr57mhqdrskuze4whfn`. Any tokens sent to this address will be forwarded to the default Anvil recipient (`0xf39Fd6...`) on domain 1234.

### 6. Register a forwarding request

```bash
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "<address from step 5>",
    "dest_domain": 1234,
    "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
  }'
```

### 7. Start the forwarding relayer

In a new terminal:

```bash
RUST_LOG=info ./target/release/forwarding-relayer relayer \
  --celestia-rpc http://localhost:26657 \
  --backend-url http://localhost:8080 \
  --relayer-mnemonic "veteran capital explain keep focus nuclear police casino exercise pitch hover job sleep slam wasp honey tenant breeze hold hat quality upper multiply gossip"
```

The relayer polls the backend for pending forwarding requests and watches their balances on Celestia. When a deposit is detected, it submits `MsgForward`.

### 8. Send tokens to the forwarding address

```bash
make send-to-address ADDR=<address from step 5> AMOUNT=1000000
```

### 9. Verify the transfer

After ~30 seconds, check the wTIA balance on Anvil:

```bash
WARP_TOKEN=$(grep addressOrDenom ./hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml | awk '{print $NF}' | tr -d '"')
cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545
```

Should return `1000000`.

## Services

| Service | Port | Description |
|---------|------|-------------|
| celestia-validator | 26657 (RPC), 9090 (gRPC), 1317 (REST) | Celestia consensus node with Hyperlane + forwarding modules |
| anvil | 8545 | Standalone EVM chain (chain-id 1234) |
| relayer | - | Hyperlane relayer (agents-v1.7.0), relays messages between chains |
| celestia-bridge | 26658, 2121 | Celestia light/bridge node |
| hyperlane-init | - | One-shot container that deploys Hyperlane contracts on both chains |

## Configuration

| Parameter | Value |
|-----------|-------|
| Celestia domain | 69420 |
| Anvil domain | 1234 |
| Celestia chain-id | celestia-zkevm-testnet |
| Anvil chain-id | 1234 |
| Anvil test account | 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 |
| Relayer Celestia address | celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 |
| Hyperlane ISM | NoopISM (testing only) |
| Hyperlane relayer version | agents-v1.7.0 |

## Makefile Targets

```bash
make start                # Start all Docker containers
make stop                 # Stop all containers and remove volumes
make e2e-auto             # Fully automated E2E test
make docker-build-hyperlane  # Rebuild the Hyperlane init Docker image
make derive-address       # Derive forwarding address for Anvil
make transfer             # Transfer tokens via Hyperlane warp (direct, no forwarding)
make send-to-address ADDR=... AMOUNT=...  # Send utia to any Celestia address
make query-balance WARP_TOKEN=0x...       # Query wTIA balance on Anvil
```

## Cleanup

```bash
make stop
```

This stops all containers and removes volumes (chain state). The next `make start` will initialize fresh.
