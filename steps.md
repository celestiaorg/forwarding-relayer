# E2E Test Steps

## Automated

```bash
make e2e-auto
```

Logs: `/tmp/backend.log` and `/tmp/relayer.log`

## Manual Steps

**1. Start services:**
```bash
make start
```

Wait ~20 seconds.

**2. Get warp token:**
```bash
docker exec hyperlane-init cat /home/hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml
```

Export the `addressOrDenom` value:
```bash
export WARP_TOKEN=0x...
```

**3. Query initial balance:**
```bash
make query-balance
```

**4. Build:**
```bash
cargo build --release
```

**5. Fund relayer:**
```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

**6. Start backend (Terminal 1):**
```bash
cargo run --release -- backend --port 8080
```

**7. Derive forwarding address (Terminal 2):**
```bash
make derive-address
```

Copy the address.

**8. Create forwarding request:**
```bash
curl -X POST http://localhost:8080/forwarding-requests \
  -H "Content-Type: application/json" \
  -d '{
    "forward_addr": "celestia1...",
    "dest_domain": 31337,
    "dest_recipient": "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
  }'
```

**9. Verify request:**
```bash
curl http://localhost:8080/forwarding-requests | jq
```

**10. Start relayer (Terminal 3):**
```bash
RUST_LOG=info ./target/release/forwarding-relayer relayer \
  --celestia-rpc http://localhost:1317 \
  --backend-url http://localhost:8080 \
  --relayer-mnemonic "veteran capital explain keep focus nuclear police casino exercise pitch hover job sleep slam wasp honey tenant breeze hold hat quality upper multiply gossip"
```

**11. Fund forwarding address (Terminal 4):**
```bash
make send-to-address ADDR=celestia1... AMOUNT=1000000
```

**12. Watch relayer logs (Terminal 3):**

Within ~6 seconds:
- New deposit detected
- Submitting forward
- Transaction broadcast

**13. Query final balance (wait ~30 seconds):**
```bash
make query-balance
```

**14. Verify backend status:**
```bash
curl http://localhost:8080/forwarding-requests | jq
```

Status should be "completed".

## Cleanup

```bash
make stop
```

## Troubleshooting

**WARP_TOKEN not set:**
```bash
export WARP_TOKEN=$(docker exec hyperlane-init cat /home/hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml | grep addressOrDenom | awk '{print $2}' | tr -d '"')
```

**No deposits detected:**
- Check forwarding address matches derived address
- Verify backend is running
- Check relayer logs

**Balance still 0:**
- Check Hyperlane relayer: `docker logs relayer`
- Verify remote routers enrolled
- Wait longer (can take up to 60 seconds)

**Services not starting:**
```bash
make stop
make start
```
