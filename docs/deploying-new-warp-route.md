# Deploying a New Warp Route and Forwarding Tokens

This guide covers how to deploy a new Hyperlane warp route between Celestia and an EVM chain, then use the forwarding relayer to bridge tokens through it.

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) and Docker Compose
- [Rust](https://rustup.rs/) toolchain
- [Foundry](https://book.getfoundry.sh/getting-started/installation) (`cast` CLI)
- `celestia-app-standalone:local` Docker image
- [Hyperlane CLI](https://docs.hyperlane.xyz/docs/reference/cli) (`hyperlane` command)

## Overview

A warp route consists of:
- **Collateral token on Celestia** -- native utia locked by the forwarding module
- **Synthetic token on EVM** -- an ERC20 (e.g., wTIA) minted on the destination chain

The `hyperlane-init` container in docker-compose handles all of this automatically for the default Anvil setup. This guide explains how to customize or extend it for a new route.

## Step 1: Start the Environment

```bash
make docker-build-hyperlane  # first time only
make start
```

Wait for the Celestia validator to start producing blocks:

```bash
# Should return a block height > 0
docker exec celestia-validator celestia-appd query block --node http://localhost:26657 | head -5
```

## Step 2: Configure the Warp Route

Create a warp config file for your token. See [hyperlane/configs/warp-config.yaml](../hyperlane/configs/warp-config.yaml) as a reference:

```yaml
# my-warp-config.yaml
rethlocal:                    # Chain name (must match Hyperlane registry)
  type: synthetic             # Mints wrapped tokens on EVM
  owner: "0xYOUR_OWNER_ADDR"  # Owner of the ERC20 contract
  name: "wTIA"                # Token name
  symbol: "TIA"               # Token symbol
  decimals: 6                 # Must match the native token decimals
```

Key fields:
- `type: synthetic` -- creates a mintable/burnable ERC20 on the EVM side
- `owner` -- the address that can admin the token contract
- `decimals` -- must match utia (6 decimals)

## Step 3: Deploy Hyperlane Core Contracts

If Hyperlane core contracts aren't already deployed on your EVM chain:

```bash
docker exec -it hyperlane-init bash

# Inside the container:
hyperlane core deploy --chain rethlocal --registry ./registry --yes
```

This deploys the Mailbox, MerkleTreeHook, ValidatorAnnounce, and other core contracts.

## Step 4: Deploy the Warp Route

```bash
# Inside the hyperlane-init container:
hyperlane warp deploy --config ./configs/warp-config.yaml --registry ./registry --yes
```

Note the deployed token address from the output. You can also find it in:

```bash
cat ./registry/deployments/warp_routes/TIA/warp-config-config.yaml
# Look for addressOrDenom field
```

## Step 5: Deploy NoopISM on Celestia

The Celestia side needs an ISM (Interchain Security Module) deployment:

```bash
# Inside the hyperlane-init container:
hyp deploy-noopism http://celestia-validator:26657
```

This creates `hyperlane-cosmosnative.json` with the deployed module IDs (mailbox, hooks, ISM, collateral token).

## Step 6: Enroll Remote Routers

Both sides need to know about each other. Get the deployed addresses:

```bash
# Inside the hyperlane-init container:

# EVM warp token address
WARP_TOKEN=$(grep "addressOrDenom:" ./registry/deployments/warp_routes/TIA/warp-config-config.yaml | awk '{print $NF}' | tr -d '"')

# Celestia collateral token ID (from the NoopISM deployment)
CEL_TOKEN=$(node -e "const c=JSON.parse(require('fs').readFileSync('hyperlane-cosmosnative.json','utf8')); console.log(c.collateral_token_id)")
```

Enroll on EVM:

```bash
cast send $WARP_TOKEN \
  "enrollRemoteRouter(uint32,bytes32)" \
  69420 $CEL_TOKEN \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  --rpc-url http://anvil:8545
```

Enroll on Celestia:

```bash
WARP_TOKEN_LOWERCASE=$(echo $WARP_TOKEN | tr '[:upper:]' '[:lower:]' | cut -c 3-)
hyp enroll-remote-router http://celestia-validator:26657 $CEL_TOKEN 1234 0x000000000000000000000000$WARP_TOKEN_LOWERCASE
```

## Step 7: Update Relayer Config

The Hyperlane relayer needs the correct contract addresses. Update [hyperlane/relayer-config.json](../hyperlane/relayer-config.json) with the deployed addresses for both chains. The `docker-entrypoint.sh` script does this automatically for the default setup -- see it as a reference for the fields to update.

After updating, restart the relayer:

```bash
docker compose restart relayer
```

## Step 8: Forward Tokens

Now follow the [Forwarding Tokens for Existing Warp Routes](forwarding-existing-warp-routes.md) guide to:

1. Build the forwarding relayer
2. Derive a forwarding address
3. Start the backend and relayer
4. Fund the forwarding address to trigger a transfer

## Reference: What docker-entrypoint.sh Does

The [hyperlane/scripts/docker-entrypoint.sh](../hyperlane/scripts/docker-entrypoint.sh) automates steps 3-7 for the default Anvil setup. When deploying a new warp route, you can either:

- **Modify the entrypoint script** to include your new route configuration
- **Run the steps manually** inside the `hyperlane-init` container as described above

## Domains

| Chain | Domain ID | Chain ID |
|-------|-----------|----------|
| Celestia (local) | 69420 | celestia-zkevm-testnet |
| Anvil (default) | 1234 | 1234 |

For custom EVM chains, register them in the Hyperlane registry under `hyperlane/registry/chains/<chain-name>/metadata.yaml`.
