# Quick Start

Services running: Celestia, Anvil, Hyperlane
Warp token: `0x4ed7c70F96B99c776995fB64377f0d4aB3B0e1C1`

## Steps

**1. Export warp token:**
```bash
export WARP_TOKEN=0x4ed7c70F96B99c776995fB64377f0d4aB3B0e1C1
```

**2. Query initial balance:**
```bash
cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545
```

**3. Build:**
```bash
cargo build --release
```

**4. Fund relayer:**
```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

**5. Start backend (Terminal 1):**
```bash
cargo run --release -- backend --port 8080
```

**6. Derive address (Terminal 2):**
```bash
cargo run --release -- derive-address \
  --domain 31337 \
  --recipient 0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266
```

**7. Create request (replace `celestia1...` with output from step 6):**
```bash
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "celestia1...",
    "dest_domain": 31337,
    "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
  }'
```

**8. Start relayer (Terminal 3):**
```bash
RUST_LOG=info cargo run --release -- relayer \
  --celestia-rpc http://localhost:1317 \
  --backend-url http://localhost:8080
```

**9. Fund forwarding address (replace `celestia1...`):**
```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1... 1000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

**10. Wait ~30 seconds, then query final balance:**
```bash
cast call $WARP_TOKEN "balanceOf(address)(uint256)" 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --rpc-url http://localhost:8545
```

Done.
