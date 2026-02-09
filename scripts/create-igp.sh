#!/bin/bash

# Script to create IGP (Interchain Gas Paymaster) for forwarding module

set -euo pipefail

echo "Creating IGP (Interchain Gas Paymaster)..."

# Create IGP
docker exec celestia-validator celestia-appd tx hyperlane hooks igp create utia \
  --from hyp \
  --keyring-backend test \
  --chain-id celestia-zkevm-testnet \
  --fees 800utia \
  --yes \
  --broadcast-mode sync

echo "Waiting for IGP creation transaction to be confirmed..."
sleep 6

# Get IGP ID
IGP_ID=$(docker exec celestia-validator celestia-appd query hyperlane hooks igps --output json | jq -r '.igps[0].id')

if [ -z "$IGP_ID" ] || [ "$IGP_ID" = "null" ]; then
  echo "ERROR: Failed to create IGP or extract IGP ID"
  exit 1
fi

echo "Created IGP with ID: $IGP_ID"

# Set destination gas config for Anvil domain (1234) with zero costs for local testing
echo "Configuring destination gas config for domain 1234..."
docker exec celestia-validator celestia-appd tx hyperlane hooks igp set-destination-gas-config \
  "$IGP_ID" \
  1234 \
  1 \
  0 \
  0 \
  --from hyp \
  --keyring-backend test \
  --chain-id celestia-zkevm-testnet \
  --fees 800utia \
  --yes \
  --broadcast-mode sync

echo "Waiting for gas config transaction to be confirmed..."
sleep 6

# Verify gas config was set
echo "Verifying gas config..."
docker exec celestia-validator celestia-appd query hyperlane hooks destination-gas-configs "$IGP_ID" --output json | jq '.'

echo "IGP setup complete!"
