#!/bin/bash
# Automated E2E test script for forwarding-relayer with Anvil

set -e

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}  Forwarding Relayer E2E Test (Anvil)  ${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""

# Clean up any existing processes from previous runs
echo -e "${YELLOW}Cleaning up existing processes...${NC}"
lsof -ti:8080 2>/dev/null | xargs -r kill -9 || true
pkill -f "forwarding-relayer backend" || true
pkill -f "forwarding-relayer relayer" || true
sleep 1

# Cleanup on exit
cleanup() {
  echo -e "\n${YELLOW}Cleaning up background processes...${NC}"
  kill $BACKEND_PID 2>/dev/null || true
  kill $RELAYER_PID 2>/dev/null || true
}
trap cleanup EXIT

# Clean up old deployment files and state to ensure fresh deployment
echo -e "${YELLOW}Cleaning up old deployment files and state...${NC}"
rm -f ./hyperlane/hyperlane-cosmosnative.json
rm -f ./hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml
rm -f ./hyperlane/registry/chains/rethlocal/addresses.yaml
rm -f ./storage/balance_cache.db
rm -f ./storage/backend.db

# Step 1: Stop existing containers and start fresh
echo -e "${GREEN}[Step 1/11]${NC} Starting fresh environment..."
make stop 2>/dev/null || true
make start

# Wait for hyperlane-init to complete (deploys contracts)
echo -e "${YELLOW}Waiting for Hyperlane deployment to complete...${NC}"
TIMEOUT=120
ELAPSED=0
while [ $ELAPSED -lt $TIMEOUT ]; do
  STATUS=$(docker inspect hyperlane-init --format='{{.State.Status}}' 2>/dev/null || echo "unknown")
  if [ "$STATUS" = "exited" ]; then
    EXIT_CODE=$(docker inspect hyperlane-init --format='{{.State.ExitCode}}' 2>/dev/null || echo "1")
    if [ "$EXIT_CODE" = "0" ]; then
      echo -e "${GREEN}Hyperlane deployment completed successfully${NC}"
      break
    else
      echo -e "${RED}ERROR: Hyperlane deployment failed (exit code: $EXIT_CODE)${NC}"
      docker logs hyperlane-init 2>&1 | tail -20
      exit 1
    fi
  fi
  sleep 5
  ELAPSED=$((ELAPSED + 5))
  echo -e "${YELLOW}  Waiting... (${ELAPSED}s/${TIMEOUT}s) - status: ${STATUS}${NC}"
done
if [ $ELAPSED -ge $TIMEOUT ]; then
  echo -e "${RED}ERROR: Timed out waiting for Hyperlane deployment${NC}"
  exit 1
fi

# Step 2: Verify services
echo -e "${GREEN}[Step 2/11]${NC} Verifying services..."
docker ps --format "table {{.Names}}\t{{.Status}}" | head -10

# Step 3: Get warp token address
echo -e "${GREEN}[Step 3/11]${NC} Getting warp token address..."
WARP_TOKEN=$(cat ./hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml | grep "addressOrDenom:" | awk '{print $NF}' | tr -d '"')

if [ -z "$WARP_TOKEN" ]; then
  echo -e "${RED}ERROR: Failed to get warp token address${NC}"
  exit 1
fi

echo -e "${BLUE}Warp Token Address: ${WARP_TOKEN}${NC}"
export WARP_TOKEN

# Step 4: Query initial balance
echo -e "${GREEN}[Step 4/11]${NC} Querying initial balance on Anvil..."
INITIAL_BALANCE=$(cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545 | awk '{print $1}')
echo -e "${BLUE}Initial Balance: ${INITIAL_BALANCE}${NC}"

# Step 5: Build relayer
echo -e "${GREEN}[Step 5/11]${NC} Building relayer (if not already built)..."
if [ ! -f "./target/release/forwarding-relayer" ]; then
  cargo build --release
else
  echo -e "${YELLOW}Relayer already built, skipping...${NC}"
fi

# Step 6: Fund relayer account
echo -e "${GREEN}[Step 6/11]${NC} Funding relayer account..."
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657 || echo "Note: May already be funded"

echo -e "${YELLOW}Waiting for transaction to be confirmed...${NC}"
sleep 8

# Step 7: Start backend in background
echo -e "${GREEN}[Step 7/11]${NC} Starting backend server..."
./target/release/forwarding-relayer backend --port 8080 > /tmp/backend.log 2>&1 &
BACKEND_PID=$!
echo -e "${BLUE}Backend PID: ${BACKEND_PID}${NC}"
sleep 2

# Verify backend started successfully
if ! ps -p $BACKEND_PID > /dev/null 2>&1; then
  echo -e "${RED}ERROR: Backend failed to start${NC}"
  echo -e "${YELLOW}Backend logs:${NC}"
  cat /tmp/backend.log
  exit 1
fi

# Verify backend is responding
if ! curl -s http://localhost:8080/forwarding-requests > /dev/null 2>&1; then
  echo -e "${RED}ERROR: Backend is not responding${NC}"
  echo -e "${YELLOW}Backend logs:${NC}"
  cat /tmp/backend.log
  kill $BACKEND_PID 2>/dev/null || true
  exit 1
fi

echo -e "${GREEN}Backend started successfully${NC}"

# Step 8: Derive forwarding address
echo -e "${GREEN}[Step 8/11]${NC} Deriving forwarding address..."
FORWARD_ADDR=$(./target/release/forwarding-relayer derive-address \
  --dest-domain 1234 \
  --dest-recipient 0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266)

if [ -z "$FORWARD_ADDR" ]; then
  echo -e "${RED}ERROR: Failed to derive forwarding address${NC}"
  kill $BACKEND_PID
  exit 1
fi

echo -e "${BLUE}Forwarding Address: ${FORWARD_ADDR}${NC}"

# Step 9: Create forwarding request
echo -e "${GREEN}[Step 9/11]${NC} Creating forwarding request..."
curl -s -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d "{
    \"forward_addr\": \"${FORWARD_ADDR}\",
    \"dest_domain\": 1234,
    \"dest_recipient\": \"0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266\"
  }"
echo ""
sleep 1

# Step 10: Start relayer in background
echo -e "${GREEN}[Step 10/11]${NC} Starting forwarding relayer..."
RUST_LOG=info ./target/release/forwarding-relayer relayer \
  --celestia-rpc http://localhost:26657 \
  --backend-url http://localhost:8080 \
  --relayer-mnemonic "veteran capital explain keep focus nuclear police casino exercise pitch hover job sleep slam wasp honey tenant breeze hold hat quality upper multiply gossip" \
  > /tmp/relayer.log 2>&1 &
RELAYER_PID=$!
echo -e "${BLUE}Relayer PID: ${RELAYER_PID}${NC}"
sleep 3

# Step 11: Fund forwarding address
echo -e "${GREEN}[Step 11/11]${NC} Funding forwarding address..."
docker exec celestia-validator celestia-appd tx bank send \
  default ${FORWARD_ADDR} 1000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657

echo ""
echo -e "${YELLOW}Waiting for forwarding and Hyperlane relay...${NC}"

# Poll for balance change instead of fixed sleep
TIMEOUT=90
ELAPSED=0
while [ $ELAPSED -lt $TIMEOUT ]; do
  CURRENT_BALANCE=$(cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545 2>/dev/null | awk '{print $1}')
  if [ "$CURRENT_BALANCE" != "0" ] && [ "$CURRENT_BALANCE" -gt "$INITIAL_BALANCE" ] 2>/dev/null; then
    echo -e "${GREEN}Balance changed! (${ELAPSED}s)${NC}"
    break
  fi
  sleep 5
  ELAPSED=$((ELAPSED + 5))
  echo -e "${YELLOW}  Polling... (${ELAPSED}s/${TIMEOUT}s) balance=${CURRENT_BALANCE:-0}${NC}"
done

# Query final balance
echo ""
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}  Results${NC}"
echo -e "${GREEN}========================================${NC}"

FINAL_BALANCE=$(cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545 | awk '{print $1}')
echo -e "${BLUE}Initial Balance: ${INITIAL_BALANCE}${NC}"
echo -e "${BLUE}Final Balance:   ${FINAL_BALANCE}${NC}"

# Show forwarding relayer logs
echo ""
echo -e "${BLUE}Forwarding Relayer Logs:${NC}"
cat /tmp/relayer.log

if [ "$FINAL_BALANCE" -gt "$INITIAL_BALANCE" ] 2>/dev/null; then
  echo ""
  echo -e "${GREEN}SUCCESS! Tokens forwarded successfully!${NC}"
  EXIT_CODE=0
else
  echo ""
  echo -e "${RED}FAILED: Balance did not increase${NC}"
  echo -e "${YELLOW}Check logs:${NC}"
  echo -e "${YELLOW}  cat /tmp/relayer.log${NC}"
  echo -e "${YELLOW}  docker logs relayer${NC}"
  EXIT_CODE=1
fi

# Backend status
echo ""
echo -e "${BLUE}Backend Status:${NC}"
curl -s http://localhost:8080/forwarding-requests | jq '.[0].status' 2>/dev/null || echo "Unable to query backend"

exit $EXIT_CODE
