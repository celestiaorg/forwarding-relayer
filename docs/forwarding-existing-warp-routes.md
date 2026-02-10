# Forwarding Tokens for Existing Warp Routes

This guide covers how to use the forwarding relayer to bridge tokens from Celestia to an EVM chain when Hyperlane warp route contracts are already deployed.

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) and Docker Compose
- [Rust](https://rustup.rs/) toolchain
- [Foundry](https://book.getfoundry.sh/getting-started/installation) (`cast` CLI) -- for querying EVM balances
- Running environment: Celestia validator, Anvil (or your EVM chain), and Hyperlane relayer
- Hyperlane contracts deployed on both chains (see [Deploying a New Warp Route](deploying-new-warp-route.md) if needed)

## Overview

The forwarding relayer watches for deposits to special forwarding addresses on Celestia. When tokens arrive, it submits a `MsgForward` transaction which locks the tokens and dispatches a Hyperlane message. The Hyperlane relayer then relays that message to the EVM chain, where wrapped tokens (wTIA) are minted.

```
1. You send utia to a forwarding address on Celestia
2. Forwarding relayer detects the deposit
3. Relayer submits MsgForward (locks tokens, dispatches Hyperlane message)
4. Hyperlane relayer relays the message to the EVM chain
5. wTIA is minted to the recipient on the EVM chain
```

## Step 1: Build the Relayer

```bash
cargo build --release
```

## Step 2: Start the Environment

If containers aren't already running:

```bash
make start
```

Verify Hyperlane deployment is complete:

```bash
docker inspect hyperlane-init --format='{{.State.ExitCode}}'
# Should output: 0
```

## Step 3: Derive a Forwarding Address

Each (destination domain, recipient) pair maps to a unique forwarding address on Celestia. Derive it with:

```bash
# Using the Makefile (defaults to domain 1234, Anvil account[0])
make derive-address
```

Or with the relayer binary for a custom recipient:

```bash
./target/release/forwarding-relayer derive-address \
  --dest-domain 1234 \
  --dest-recipient 0x000000000000000000000000YOUR_EVM_ADDRESS_HERE
```

The `dest-recipient` must be a 32-byte hex-encoded address (pad a 20-byte EVM address with 12 leading zero bytes).

Save the output address -- you'll need it in the next steps.

## Step 4: Start the Backend

The backend tracks forwarding requests in a SQLite database:

```bash
./target/release/forwarding-relayer backend --port 8080
```

Leave this running in a separate terminal.

## Step 5: Register a Forwarding Request

Tell the backend about the forwarding address so the relayer knows to watch it:

```bash
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "celestia1...",
    "dest_domain": 1234,
    "dest_recipient": "0x000000000000000000000000YOUR_EVM_ADDRESS_HERE"
  }'
```

Replace `forward_addr` with the address from Step 3.

You can list existing requests:

```bash
curl http://localhost:8080/forwarding-requests
```

## Step 6: Fund the Relayer Account

The relayer needs gas to submit `MsgForward` transactions on Celestia:

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

The relayer address `celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3` is derived from the default test mnemonic. If you use a custom mnemonic, derive its address and fund that instead.

## Step 7: Start the Forwarding Relayer

```bash
RUST_LOG=info ./target/release/forwarding-relayer relayer \
  --celestia-rpc http://localhost:26657 \
  --celestia-grpc http://localhost:9090 \
  --backend-url http://localhost:8080 \
  --relayer-mnemonic "veteran capital explain keep focus nuclear police casino exercise pitch hover job sleep slam wasp honey tenant breeze hold hat quality upper multiply gossip"
```

Leave this running. It polls the backend for forwarding requests and watches for balance changes every 6 seconds.

### Relayer Configuration

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--celestia-rpc` | `CELESTIA_RPC` | `http://localhost:26657` | Tendermint RPC URL |
| `--celestia-grpc` | `CELESTIA_GRPC` | `http://localhost:9090` | Cosmos SDK gRPC URL |
| `--backend-url` | `BACKEND_URL` | `http://localhost:8080` | Backend API URL |
| `--relayer-mnemonic` | `RELAYER_MNEMONIC` | (required) | BIP39 mnemonic for signing |
| `--chain-id` | `CHAIN_ID` | `celestia-zkevm-testnet` | Celestia chain ID |
| `--poll-interval` | `POLL_INTERVAL` | `6` | Seconds between poll cycles |
| `--igp-fee-buffer` | `IGP_FEE_BUFFER` | `1.1` | Multiplier on quoted IGP fee |
| `--balance-cache-path` | `BALANCE_CACHE_PATH` | `storage/balance_cache.db` | SQLite cache path |

## Step 8: Send Tokens to the Forwarding Address

```bash
make send-to-address ADDR=celestia1... AMOUNT=1000000
```

Or directly:

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1... 1000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

The relayer will detect the deposit on its next poll cycle (~6s), submit `MsgForward`, and the Hyperlane relayer will relay the message to the EVM chain.

## Step 9: Verify on the EVM Side

Check the wTIA balance on Anvil:

```bash
# Find the warp token address
WARP_TOKEN=$(grep addressOrDenom ./hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml | awk '{print $NF}' | tr -d '"')

# Query balance
cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xYOUR_EVM_ADDRESS --rpc-url http://localhost:8545
```

Or using the Makefile:

```bash
WARP_TOKEN=0x... make query-balance
```

The balance should reflect the forwarded amount (minus any IGP fees).

## Forwarding Multiple Transfers

You can send tokens to the same forwarding address multiple times. Each deposit triggers a new `MsgForward` transaction. The relayer uses a balance cache to detect changes -- when the balance at a forwarding address increases, it forwards the new amount.

## Forwarding to Different Recipients

Each recipient needs their own forwarding address. Repeat steps 3-5 for each new `(dest_domain, dest_recipient)` pair:

1. Derive a new forwarding address
2. Register it with the backend
3. Send tokens to the new address

## Troubleshooting

### Relayer says "all tokens failed to forward"

Check that:
- The warp route has enrolled routers on both chains (see [Deploying a New Warp Route](deploying-new-warp-route.md), Step 6)
- The gRPC port (9090) is serving Cosmos SDK services, not CometBFT services (`grpcurl -plaintext localhost:9090 list` should show many services)

### Balance doesn't change on Anvil

Check the Hyperlane relayer logs:

```bash
docker logs relayer
```

If it shows "unknown service", the gRPC port conflict may be the issue. See the note above about port 9090.

### "account not found" errors

The relayer account needs to be funded first (Step 6). It needs utia for gas fees.

### Relayer submits MsgForward but no Hyperlane message appears

The forwarding module may not have found a matching warp token with an enrolled router for the destination domain. Verify the enrollment:

```bash
docker exec celestia-validator celestia-appd query warp list-tokens --node http://localhost:26657
```
