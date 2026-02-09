#!/bin/bash
set -euo pipefail

export HYP_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
export EVM_CHAIN=rethlocal
export EVM_RPC_URL=http://anvil:8545
export CELESTIA_DOMAIN=69420
export ANVIL_DOMAIN=1234

CONFIG_FILE="hyperlane-cosmosnative.json"

if [[ ! -f "$CONFIG_FILE" ]]; then
  echo "Deploying Hyperlane core EVM contracts to $EVM_CHAIN..."
  hyperlane core deploy --chain $EVM_CHAIN --registry ./registry --yes

  echo "Deploying Hyperlane warp synthetic token EVM contracts to $EVM_CHAIN..."
  hyperlane warp deploy --config ./configs/warp-config.yaml --registry ./registry --yes

  WARP_TOKEN_ADDR=$(grep "addressOrDenom:" ./registry/deployments/warp_routes/TIA/warp-config-config.yaml | awk '{print $NF}' | tr -d '"')
  echo "Warp token: $WARP_TOKEN_ADDR"

  # Read EVM addresses from deployment artifacts
  EVM_MAILBOX=$(grep "^mailbox:" ./registry/chains/rethlocal/addresses.yaml | awk '{print $NF}' | tr -d '"')
  EVM_MERKLE_HOOK=$(grep "^merkleTreeHook:" ./registry/chains/rethlocal/addresses.yaml | awk '{print $NF}' | tr -d '"')
  EVM_VALIDATOR_ANNOUNCE=$(grep "^validatorAnnounce:" ./registry/chains/rethlocal/addresses.yaml | awk '{print $NF}' | tr -d '"')
  echo "EVM Mailbox: $EVM_MAILBOX"
  echo "EVM MerkleTreeHook: $EVM_MERKLE_HOOK"
  echo "EVM ValidatorAnnounce: $EVM_VALIDATOR_ANNOUNCE"

  echo "Deploying Hyperlane NoopISM stack on Celestia..."
  hyp deploy-noopism http://celestia-validator:26657

  # Read Celestia addresses from deployment output
  CEL_MAILBOX=$(node -e "const c=JSON.parse(require('fs').readFileSync('$CONFIG_FILE','utf8')); console.log(c.mailbox_id)")
  CEL_MERKLE_HOOK=$(node -e "const c=JSON.parse(require('fs').readFileSync('$CONFIG_FILE','utf8')); console.log(c.required_hook_id)")
  CEL_ISM=$(node -e "const c=JSON.parse(require('fs').readFileSync('$CONFIG_FILE','utf8')); console.log(c.ism_id)")
  CEL_DEFAULT_HOOK=$(node -e "const c=JSON.parse(require('fs').readFileSync('$CONFIG_FILE','utf8')); console.log(c.default_hook_id)")
  CEL_TOKEN=$(node -e "const c=JSON.parse(require('fs').readFileSync('$CONFIG_FILE','utf8')); console.log(c.collateral_token_id)")
  echo "Celestia Mailbox: $CEL_MAILBOX"
  echo "Celestia MerkleTreeHook: $CEL_MERKLE_HOOK"
  echo "Celestia Token: $CEL_TOKEN"

  echo "Enrolling remote router on EVM..."
  cast send $WARP_TOKEN_ADDR \
    "enrollRemoteRouter(uint32,bytes32)" \
    $CELESTIA_DOMAIN $CEL_TOKEN \
    --private-key $HYP_KEY \
    --rpc-url $EVM_RPC_URL

  echo "Enrolling remote router on Celestia..."
  WARP_TOKEN_LOWERCASE=$(echo $WARP_TOKEN_ADDR | tr '[:upper:]' '[:lower:]' | cut -c 3-)
  hyp enroll-remote-router http://celestia-validator:26657 $CEL_TOKEN $ANVIL_DOMAIN 0x000000000000000000000000$WARP_TOKEN_LOWERCASE

  # Update relayer-config.json with actual deployed addresses
  echo "Updating relayer-config.json with deployed addresses..."
  node -e "
const fs = require('fs');
const config = JSON.parse(fs.readFileSync('relayer-config.json', 'utf8'));

// Update rethlocal (EVM) addresses
config.chains.rethlocal.mailbox = '$EVM_MAILBOX';
config.chains.rethlocal.merkleTreeHook = '$EVM_MERKLE_HOOK';
config.chains.rethlocal.validatorAnnounce = '$EVM_VALIDATOR_ANNOUNCE';
config.chains.rethlocal.interchainGasPaymaster = '$EVM_MERKLE_HOOK';

// Update celestiadev addresses from actual deployment
config.chains.celestiadev.mailbox = '$CEL_MAILBOX';
config.chains.celestiadev.merkleTreeHook = '$CEL_MERKLE_HOOK';
config.chains.celestiadev.interchainSecurityModule = '$CEL_ISM';
config.chains.celestiadev.interchainGasPaymaster = '$CEL_DEFAULT_HOOK';
config.chains.celestiadev.validatorAnnounce = '$CEL_MAILBOX';

fs.writeFileSync('relayer-config.json', JSON.stringify(config, null, 4) + '\n');
console.log('relayer-config.json updated successfully');
"

  echo "Deployment complete!"
else
  echo "Skipping deployment: $CONFIG_FILE already exists."
fi
