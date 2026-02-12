#!/bin/bash
set -e

# Docker entrypoint for forwarding-relayer
# Supports running either "relayer" or "backend" mode

MODE="${1:-relayer}"

case "$MODE" in
  relayer)
    echo "Starting forwarding relayer..."
    exec forwarding-relayer relayer \
      --celestia-rpc "${CELESTIA_RPC:-http://celestia-validator:26657}" \
      --celestia-grpc "${CELESTIA_GRPC:-http://celestia-validator:9090}" \
      --backend-url "${BACKEND_URL:-http://forwarding-backend:8080}" \
      --relayer-mnemonic "${RELAYER_MNEMONIC:?RELAYER_MNEMONIC is required}" \
      --chain-id "${CHAIN_ID:-celestia-zkevm-testnet}" \
      --poll-interval "${POLL_INTERVAL:-6}" \
      --igp-fee-buffer "${IGP_FEE_BUFFER:-1.1}" \
      --balance-cache-path "${BALANCE_CACHE_PATH:-/app/storage/balance_cache.db}"
    ;;

  backend)
    echo "Starting forwarding backend..."
    exec forwarding-relayer backend \
      --port "${PORT:-8080}" \
      --db-path "${DB_PATH:-/app/storage/backend.db}"
    ;;

  *)
    echo "Unknown mode: $MODE"
    echo "Usage: $0 {relayer|backend}"
    exit 1
    ;;
esac
