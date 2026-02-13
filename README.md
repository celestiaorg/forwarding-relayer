# Forwarding Relayer

Cross-chain token forwarding from Celestia to an EVM chain via Hyperlane.

When tokens (utia) are sent to a forwarding address on Celestia, the relayer detects the deposit and submits a `MsgForward` transaction. The Celestia forwarding module locks the tokens and dispatches a Hyperlane message. The Hyperlane relayer relays it to the destination EVM chain, where wrapped tokens (wTIA) are minted.

## Architecture

```
 Celestia (69420)                              Anvil EVM (1234)
┌─────────────────────────┐                   ┌─────────────────────┐
│                         │                   │                     │
│  Forwarding Address     │                   │  wTIA (HypSynthetic)│
│         │               │                   │         ▲           │
│    1. deposit utia      │                   │         │           │
│         │               │                   │    4. mint wTIA     │
│         ▼               │   3. Hyperlane    │         │           │
│  Forwarding Module ─────┼──── message ─────►│  Hyperlane Mailbox  │
│  (collateral + mailbox) │  (hyp relayer)    │                     │
│         ▲               │                   └─────────────────────┘
│         │               │
│    2. MsgForward        │
│    (fwd relayer)        │
└─────────────────────────┘
```

## Guides

- **[Running the E2E Test](docs/running-e2e-tests.md)**: Start all services, forward tokens, and verify the result in one command
- **[Deploying a New Warp Route](docs/deploying-new-warp-route.md)**: Deploy Hyperlane contracts and set up a new token bridge between Celestia and an EVM chain
- **[Forwarding Tokens for Existing Warp Routes](docs/forwarding-existing-warp-routes.md)**: Use the relayer to bridge tokens through an already-deployed warp route

## Quick Start

```bash
# Build the Hyperlane init image (first time only)
make docker-build-hyperlane

# Run the E2E test (starts containers, deploys contracts, forwards tokens, verifies)
make e2e
```

This will:
1. Start Celestia, Anvil, and Hyperlane containers
2. Wait for Hyperlane contract deployment
3. Start the backend and forwarding relayer in-process
4. Derive a forwarding address, create a forwarding request
5. Fund the forwarding address with 1,000,000 utia
6. Wait for the relayer to detect the deposit and submit `MsgForward`
7. Wait for the Hyperlane relayer to relay the message to Anvil
8. Verify the wTIA balance increased on Anvil

Expected output:
```
SUCCESS! 1000000 utia forwarded from Celestia to Anvil as wTIA
```

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) and Docker Compose
- [Rust](https://rustup.rs/) toolchain
- `celestia-app-standalone:local` Docker image (built from [celestia-app](https://github.com/celestiaorg/celestia-app))

## Manual Step-by-Step

### 1. Start the environment

```bash
make docker-build-hyperlane  # first time only
make start
```

Wait for `hyperlane-init` to exit with code 0 (`docker ps` to check).

### 2. Fund the relayer account

The relayer needs gas for `MsgForward` transactions:

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1y3kf30y9zprqzr2g2gjjkw3wls0a35pfs3a58q 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

### 3. Start the backend

```bash
./target/release/forwarding-relayer backend --port 8080
```

### 4. Register a forwarding request

```bash
# Derive the forwarding address
make derive-address

# Create the request
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "<address from derive-address>",
    "dest_domain": 1234,
    "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
  }'
```

### 5. Start the forwarding relayer

```bash
RUST_LOG=info ./target/release/forwarding-relayer relayer \
  --celestia-rpc http://localhost:26657 \
  --backend-url http://localhost:8080 \
  --private-key-hex "6e30efb1d3ebd30d1ba08c8d5fc9b190e08394009dc1dd787a69e60c33288a8c"
```

### 6. Send tokens and verify

```bash
# Fund the forwarding address
make send-to-address ADDR=<forwarding address> AMOUNT=1000000

# After ~30s, check wTIA balance on Anvil
WARP_TOKEN=$(grep addressOrDenom ./hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml | awk '{print $NF}' | tr -d '"')
cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545
```

## Services

| Service | Port | Description |
|---------|------|-------------|
| celestia-validator | 26657 (RPC), 9090 (gRPC) | Celestia node with Hyperlane + forwarding modules |
| anvil | 8545 | Standalone EVM chain (chain-id 1234) |
| relayer | - | Hyperlane relayer (v1.7.0) |
| celestia-bridge | 26658, 2121 | Celestia bridge node |
| hyperlane-init | - | Deploys Hyperlane contracts on both chains (runs once) |

## Configuration

| Parameter | Value |
|-----------|-------|
| Celestia domain | 69420 |
| Anvil domain | 1234 |
| Anvil test account | `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` |
| Relayer address | `celestia1y3kf30y9zprqzr2g2gjjkw3wls0a35pfs3a58q` |
| ISM | NoopISM (testing only) |

## Makefile Targets

```
make start              Start all Docker containers
make stop               Stop containers and remove volumes
make e2e                Full E2E test (start, deploy, forward, verify)
make docker-build-hyperlane  Rebuild Hyperlane init image
make derive-address     Derive forwarding address for Anvil
make transfer           Direct Hyperlane warp transfer (no forwarding)
make send-to-address    Send utia to a Celestia address
make query-balance      Query wTIA balance on Anvil
```
