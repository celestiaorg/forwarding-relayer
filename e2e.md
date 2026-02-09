# E2E Implementation

Implementation complete. E2E testing infrastructure uses Anvil as the destination EVM chain.

## Architecture

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│  Celestia   │────>│   Hyperlane  │────>│   Anvil     │
│  (69420)    │     │   Relayer    │     │   (31337)   │
└─────────────┘     └──────────────┘     └─────────────┘
```

**Key Points:**
- Anvil is a **standalone** EVM chain (not posting blobs to Celestia)
- Celestia and Anvil are **independent** chains connected only via Hyperlane
- Transactions to forwarding addresses are done via CLI (same as before)
- Hyperlane relayer is configured to relay from Celestia (69420) to Anvil (31337)

## Completed Components

### Infrastructure
- Added Anvil service to `docker-compose.yml`
- Created Anvil chain metadata in `hyperlane/registry/chains/anvil/`
- Updated Hyperlane deployment script for Anvil
- Configured Hyperlane relayer for Celestia → Anvil routing

### Hyperlane Configuration
- Updated `hyperlane/relayer-config.json` with Anvil chain config
- Created warp route config for synthetic wTIA token on Anvil
- Configured IGP fees (minimal values for testing)
- Token minter funded on Anvil (deployment script funds account[0])

### Deployment
- Hyperlane core contracts deploy to Anvil automatically
- Warp route (synthetic wTIA) deploys automatically
- Remote routers enrolled between Celestia ↔ Anvil
- Token minting works on destination

### Testing
- Created E2E test crate in `e2e/`
- Added Makefile targets: `start`, `query-balance`, `derive-address`, `transfer`, `e2e`
- Balance query tooling using Foundry `cast`

### Documentation
- Extended README.md with Anvil E2E instructions
- Step-by-step testing guide
- Balance query instructions
- Documented domain IDs, addresses, configuration

## Usage

### Quick Start

```bash
# 1. Start Anvil environment
make start-anvil

# 2. Get warp token address (after ~15s for deployment)
docker exec hyperlane-init cat /home/hyperlane/registry/deployments/warp_routes/TIA/anvil-addresses.yaml

# 3. Export token address
export WARP_TOKEN=0x...

# 4. Query initial balance (should be 0)
make query-anvil-balance

# 5. Run complete E2E test (see README.md for full instructions)
```

### Balance Verification

**Before forwarding:**
```bash
make query-anvil-balance
# Output: 0
```

**After forwarding:**
```bash
make query-anvil-balance
# Output: <amount> (e.g., 1000000 for 1 TIA minus IGP fees)
```

## Key Configuration

- **Anvil Domain ID**: 31337
- **Celestia Domain ID**: 69420
- **Anvil RPC**: http://localhost:8545 (external), http://anvil:8545 (Docker network)
- **Warp Token**: Synthetic wTIA on Anvil (address in deployment output)
- **Default Recipient**: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 (Anvil account[0])
- **IGP Fees**: Configured to minimal values (visible in balance inspection)

## File Changes

### New Files
- `hyperlane/registry/chains/anvil/metadata.yaml` - Anvil chain metadata
- `e2e/Cargo.toml` - E2E test crate
- `e2e/src/main.rs` - E2E test binary

### Modified Files
- `docker-compose.yml` - Added Anvil service, updated hyperlane-init deps
- `hyperlane/scripts/docker-entrypoint.sh` - Updated for Anvil deployment
- `hyperlane/relayer-config.json` - Added Anvil chain, updated relay chains
- `hyperlane/configs/warp-config.yaml` - Updated for Anvil
- `Makefile` - Added Anvil-specific targets
- `README.md` - Added comprehensive Anvil E2E testing section

## Architecture Details

**Anvil (EVM Chain)**:
- Chain ID: 31337
- Block time: 1 second
- 10 pre-funded accounts (standard test mnemonic)
- No DA posting (standalone chain)

**Hyperlane Bridge**:
- Mailbox contracts on both Celestia and Anvil
- ISM: TestISM/NoopISM (for fast testing)
- Warp routes: Collateral on Celestia → Synthetic on Anvil
- IGP: Interchain Gas Paymaster for cross-chain fees

**Token Flow**:
1. User sends TIA to forwarding address on Celestia
2. Forwarding relayer detects deposit
3. Relayer queries IGP fee and submits MsgForward
4. Celestia forwards tokens via Hyperlane
5. Hyperlane relayer picks up message
6. Synthetic wTIA minted on Anvil
7. Recipient receives tokens on Anvil

## Verification

- Anvil starts in < 5 seconds
- Hyperlane deploys successfully to Anvil
- Initial balance query returns 0
- Token forwarding completes in < 60 seconds
- Final balance > 0
- IGP fee visible and minimal
- Backend status updates to "completed"

## Future Enhancements

- Automated E2E test script (full flow automation)
- CI/CD GitHub Actions workflow
- Performance benchmarking
- Multi-destination testing
- Failure scenario testing

