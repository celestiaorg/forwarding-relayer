# Running the E2E Test

This guide walks you through running the automated end-to-end test, which starts all services, deploys Hyperlane contracts, forwards tokens from Celestia to an EVM chain (Anvil), and verifies the result.

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) and Docker Compose
- [Rust](https://rustup.rs/) toolchain
- `celestia-app-standalone:local` Docker image (built from [celestia-app](https://github.com/celestiaorg/celestia-app))

## Quick Start

```bash
# Build the Hyperlane init image (first time only)
make docker-build-hyperlane

# Run the full E2E test
make e2e
```

That's it. On success you'll see:

```
SUCCESS! 1000000 utia forwarded from Celestia to Anvil as wTIA
```

## What the Test Does

The `make e2e` target runs through these steps automatically:

1. **Starts Docker containers** -- Celestia validator, bridge node, Anvil (EVM), Hyperlane init, and the Hyperlane relayer.
2. **Waits for Hyperlane deployment** -- The `hyperlane-init` container deploys core contracts and warp route tokens on both chains. This takes ~30-60s.
3. **Runs the E2E binary** (`cargo run --bin e2e -p e2e --release`), which:
   - Verifies Anvil is running
   - Queries the initial wTIA balance on Anvil
   - Starts the backend server and creates a forwarding request
   - Funds the relayer account on Celestia (for gas)
   - Starts the forwarding relayer
   - Funds the forwarding address with 1,000,000 utia (triggers the relayer)
   - Polls Anvil for a wTIA balance increase (5s intervals, 120s timeout)
   - Reports success or failure

## Configuration

All defaults work out of the box. If you need to customize:

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--anvil-rpc` | | `http://localhost:8545` | Anvil RPC URL |
| `--celestia-rpc` | | `http://localhost:26657` | Celestia Tendermint RPC URL |
| `--warp-token` | `WARP_TOKEN` | Auto-detected | wTIA token address on Anvil |
| `--recipient` | | `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` | Recipient on Anvil |
| `--backend-port` | | `8080` | Backend API port |
| `--relayer-mnemonic` | `RELAYER_MNEMONIC` | Test mnemonic | Mnemonic for signing Celestia txs |
| `--fund-amount` | | `1000000` | Amount of utia to forward |
| `--dest-domain` | | `1234` | Hyperlane destination domain |
| `--timeout-secs` | | `120` | Max wait time for balance change |

Example with custom options:

```bash
cargo run --bin e2e -p e2e --release -- --fund-amount 5000000 --timeout-secs 180
```

## Teardown

Stop all containers and clean up volumes:

```bash
make stop
```

## Troubleshooting

### Hyperlane deployment times out

The `hyperlane-init` container has a 120s timeout. Check its logs:

```bash
docker logs hyperlane-init
```

Common causes:
- Celestia validator hasn't started producing blocks yet (wait longer)
- The `celestia-app-standalone:local` image is missing or outdated

### E2E test times out waiting for balance change

The relayer detected the deposit but the Hyperlane relay hasn't completed. Check:

```bash
# Forwarding relayer logs (in the cargo output)
# Hyperlane relayer logs
docker logs relayer

# Celestia validator logs
docker logs celestia-validator
```

Common causes:
- Hyperlane relayer hasn't synced yet (wait longer, increase `--timeout-secs`)
- gRPC port conflict -- ensure nothing else is binding to port 9090

### "Failed to connect to Anvil"

Anvil container isn't running or isn't healthy:

```bash
docker ps
docker logs anvil
```

### Re-running the test

If you've already run `make e2e` and want to start fresh:

```bash
make stop
make e2e
```

The `make stop` target removes volumes, which clears all chain state and Hyperlane deployments so the next run starts from scratch.
