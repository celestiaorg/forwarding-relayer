PROJECT_NAME=$(shell basename "$(PWD)")

## help: Get more info on make commands.
help: Makefile
	@echo " Choose a command run in "$(PROJECT_NAME)":"
	@sed -n 's/^##//p' $< | sort | column -t -s ':' | sed -e 's/^/ /'
.PHONY: help

## check-dependencies: Check if all dependencies are installed.
check-dependencies:
	@echo "--> Checking if all dependencies are installed"
	@if command -v cargo >/dev/null 2>&1; then \
		echo "cargo is installed."; \
	else \
		echo "Error: cargo is not installed. Please install Rust."; \
		exit 1; \
	fi
	@if command -v forge >/dev/null 2>&1; then \
		echo "foundry is installed."; \
	else \
		echo "Error: forge is not installed. Please install Foundry."; \
		exit 1; \
	fi
	@if command -v cargo prove >/dev/null 2>&1; then \
		echo "cargo prove is installed."; \
	else \
		echo "Error: succinct is not installed. Please install SP1."; \
		exit 1; \
	fi
	@echo "All dependencies are installed."
.PHONY: check-dependencies

## start: Start all Docker containers for the demo.
start:
	@echo "--> Starting all Docker containers"
	@docker compose up --detach
.PHONY: start

## stop: Stop all Docker containers and remove volumes.
stop:
	@echo "--> Stopping all Docker containers"
	@docker compose down -v
.PHONY: stop

## transfer: Transfer tokens from celestia-app to the EVM roll-up.
transfer:
	@echo "--> Transferring tokens from celestia-app to the EVM roll-up"
	@docker run --rm \
  		--network $(PROJECT_NAME)_celestia-zkevm-net \
  		--volume $(PROJECT_NAME)_celestia-app:/home/celestia/.celestia-app \
  		ghcr.io/celestiaorg/celestia-app-standalone:feature-zk-execution-ism \
  		tx warp transfer 0x726f757465725f61707000000000000000000000000000010000000000000000 1234 0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d "10000000" \
  		--from default --fees 800utia --max-hyperlane-fee 100utia --node http://celestia-validator:26657 --yes
.PHONY: transfer

## send-to-address: Send tokens to a specified Celestia address for testing.
## Usage: make send-to-address ADDR=celestia1... AMOUNT=1000000
send-to-address:
	@if [ -z "$(ADDR)" ]; then \
		echo "Error: ADDR is required. Usage: make send-to-address ADDR=celestia1... AMOUNT=1000000"; \
		exit 1; \
	fi
	@if [ -z "$(AMOUNT)" ]; then \
		echo "Using default amount: 1000000utia"; \
		AMOUNT=1000000; \
	else \
		AMOUNT=$(AMOUNT); \
	fi; \
	echo "--> Sending $${AMOUNT}utia to $(ADDR)"; \
	docker exec celestia-validator celestia-appd tx bank send \
		default $(ADDR) $${AMOUNT}utia \
		--fees 800utia --yes --chain-id celestia-zkevm-testnet --node http://localhost:26657
.PHONY: send-to-address

## query-balance: Query the balance of the receiver in the EVM roll-up.
query-balance:
	@echo "--> Querying the balance of the receiver on the EVM roll-up"
	@cast call 0x345a583028762De4d733852c9D4f419077093A48 \
  		"balanceOf(address)(uint256)" \
  		0xaF9053bB6c4346381C77C2FeD279B17ABAfCDf4d \
  		--rpc-url http://localhost:8545
.PHONY: query-balance

## spamoor: Run spamoor transaction flooding against the EVM roll-up.
spamoor:
	@echo "--> Running spamoor transaction flooding daemon"
	@echo "Spamoor will be available on localhost:8080"
	@chmod +x scripts/run-spamoor.sh
	@scripts/run-spamoor.sh $(ARGS)
.PHONY: spamoor

## derive-address: Derive the forwarding address for the default test parameters.
derive-address:
	@echo "--> Deriving forwarding address"
	@echo "Domain: 1234"
	@echo "Recipient: 0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d"
	@echo ""
	@cargo test test_derive_forwarding_address_default -- --nocapture 2>&1 | grep "Derived address" || \
		echo "Forwarding Address: celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e"
.PHONY: derive-address

## send-to-forward-addr: Send tokens to the default forwarding address for E2E testing.
send-to-forward-addr:
	@echo "--> Sending tokens to default forwarding address"
	@echo "Make sure the backend server is running with:"
	@echo "  cargo run --example backend_demo  (for E2E testing with pre-populated data)"
	@echo "  OR: cargo run --release -- backend --port 8080"
	@echo "And the relayer is running with:"
	@echo "  cargo run --release -- relayer --backend-url http://localhost:8080"
	@echo ""
	@echo "Database files will be stored in storage/ directory"
	@echo ""
	@$(MAKE) send-to-address ADDR=celestia1tlgp3xflevxl4q9defk8g399qahjcusx7d4r5e AMOUNT=1000000
.PHONY: send-to-forward-addr

e2e:
	cargo run --bin e2e -p e2e --release
.PHONY: e2e

docker-build-hyperlane:
	@echo "--> Building hyperlane-init image"
	@docker build -t ghcr.io/celestiaorg/hyperlane-init:local -f hyperlane/Dockerfile .
.PHONY: docker-build-hyperlane
