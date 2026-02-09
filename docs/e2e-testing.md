# E2E Testing Guide

Detailed information about E2E testing for the forwarding-relayer project.

## Architecture

```
Celestia (69420) ─→ Hyperlane Relayer ─→ Anvil (31337)
      │                                        │
      ↓                                        ↓
Forwarding Relayer ───→ Backend API      wTIA token
```

### Components

- **Celestia Validator**: Local testnet, hosts forwarding module (domain 69420)
- **Anvil**: Lightweight EVM chain, independent from Celestia (domain 31337)
- **Hyperlane Init**: Deploys contracts (Mailbox, ISM, IGP, warp routes), runs once
- **Hyperlane Relayer**: Monitors Celestia, relays messages to Anvil, mints tokens
- **Forwarding Relayer**: Monitors forwarding addresses, queries IGP fees, submits MsgForward
- **Backend Server**: REST API for forwarding requests, SQLite storage

## Service Dependencies

Startup order: `celestia-init` → `celestia-validator` → `celestia-bridge` + `anvil` → `hyperlane-init` → `hyperlane-relayer`

Health checks:
- Celestia Validator: Block height > 1
- Celestia Bridge: JSON-RPC `node.Ready` returns true
- Anvil: `cast block-number` succeeds

## Configuration

**Domain IDs:**
- Celestia: 69420
- Anvil: 31337

**Accounts:**
- Anvil account[0]: `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` (deployer, minter, recipient)
- Celestia relayer: `celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3`

**Contract addresses:**

Dynamically generated during deployment. View with:
```bash
docker exec hyperlane-init cat /home/hyperlane/registry/chains/anvil/addresses.yaml
docker exec hyperlane-init cat /home/hyperlane/registry/deployments/warp_routes/TIA/anvil-addresses.yaml
```

**IGP fees:**

Set to minimal values for testing. Forwarding relayer queries via `celestia/forwarding/v1/quote_fee/{domain}` with 1.1x buffer (configurable with `--igp-fee-buffer`).

## Troubleshooting

**Hyperlane init fails:**
- Check logs: `docker logs hyperlane-init`
- Restart: `make stop && make start`

**Cannot find warp token:**
```bash
export WARP_TOKEN=$(docker exec hyperlane-init cat /home/hyperlane/registry/deployments/warp_routes/TIA/anvil-addresses.yaml | grep "synthetic:" | awk '{print $2}' | tr -d '"')
```

**Relayer not detecting deposits:**
- Verify forwarding address: `make derive-address`
- Clear cache: `rm storage/balance_cache.db`
- Restart relayer

**Tokens not arriving on Anvil:**
- Check Hyperlane relayer: `docker logs relayer`
- Restart: `docker restart relayer`
- Verify router enrollment in warp config

**Out of gas:**
```bash
# Fund Celestia relayer
docker exec celestia-validator celestia-appd tx bank send default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 100000000utia --fees 800utia --yes --node http://localhost:26657
```

**Debug commands:**
```bash
docker compose ps  # Check status
docker logs <service_name>  # View logs
cast block-number --rpc-url http://localhost:8545  # Anvil block
curl http://localhost:8080/forwarding-requests | jq  # Backend status
```

## Advanced Usage

**Custom domain/recipient:**
```bash
cargo run --release -- derive-address --domain <DOMAIN> --recipient <RECIPIENT>
curl -X POST http://localhost:8080/forwarding-requests -H "Content-Type: application/json" -d '{"forward_addr": "<addr>", "dest_domain": <DOMAIN>, "dest_recipient": "<RECIPIENT>"}'
```

**Multiple forwarding addresses:**

Relayer monitors all addresses in backend simultaneously.

**Custom IGP fee buffer:**
```bash
cargo run --release -- relayer --backend-url http://localhost:8080 --igp-fee-buffer 1.5
```

**Restart robustness:**

Relayer loads balance cache from disk on restart and automatically detects/forwards pending deposits.

## Performance

**Timing:**
- Anvil startup: ~2-3s
- Celestia startup: ~10-15s
- Hyperlane deployment: ~10-20s
- Total E2E: ~10-30s

**Resource usage:**
- CPU: ~30-50%
- Memory: ~1 GB
- Disk: ~2 GB

**Optimization:**
- Use Anvil for E2E (20x faster, 10x less memory vs Reth)
- Adjust poll interval: `--poll-interval 2`
- Reduce logging: `RUST_LOG=warn`
- Always use release builds
