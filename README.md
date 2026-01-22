# E2E Test Guide

Quick guide to run the forwarding relayer end-to-end test.

## Prerequisites

- Docker and Docker Compose installed
- Rust toolchain installed

## Steps

### 1. Build the relayer

```bash
cargo build --release
```

### 2. Start the Docker environment

```bash
make start
```

Wait ~30 seconds for all containers to be healthy.

### 3. Fund the relayer account

The relayer uses a test mnemonic that derives to address `celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3`.

```bash
docker exec celestia-validator celestia-appd tx bank send \
  default celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3 10000000utia \
  --fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
```

### 4. Start the relayer

```bash
RUST_LOG=info ./target/release/forwarding-relayer \
  --celestia-rpc http://localhost:1317 \
  --dest-domain 1234 \
  --dest-recipient 0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d \
  --relayer-mnemonic "veteran capital explain keep focus nuclear police casino exercise pitch hover job sleep slam wasp honey tenant breeze hold hat quality upper multiply gossip"
```

The relayer will log the forwarding address it's monitoring:
```
Forwarding Address: celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e
```

### 5. Send tokens to the forwarding address

In another terminal:

```bash
make send-to-address ADDR=celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e AMOUNT=1000000
```

### 6. Verify

Watch the relayer logs. You should see:

```
New deposit detected! Balance changed:
  1000000 utia
Submitting forward: addr=celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e...
Transaction broadcast successfully: <TX_HASH>
```

Query the transaction to confirm success:

```bash
docker exec celestia-validator celestia-appd query tx <TX_HASH> --node http://localhost:26657
```

Look for `code: 0` and `EventTokenForwarded` with `success: true`.

## Cleanup

```bash
make stop
```

## Test Parameters

- **Dest Domain**: 1234 (test chain ID)
- **Dest Recipient**: 0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d
- **Forwarding Address**: celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e (derived from domain + recipient)
- **Relayer Address**: celestia1ehy4f4a0y6zue7xvdr0zuvsawplh7tkh0xlws3
