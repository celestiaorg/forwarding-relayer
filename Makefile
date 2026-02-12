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

## start: Start all Docker containers (Celestia + Anvil + Hyperlane).
start:
	@echo "--> Starting all Docker containers"
	@docker compose up --detach
.PHONY: start

## stop: Stop all Docker containers and remove volumes.
stop:
	@echo "--> Stopping all Docker containers"
	@docker compose down -v
.PHONY: stop

## transfer: Transfer tokens from Celestia to Anvil via Hyperlane.
transfer:
	@echo "--> Transferring tokens from Celestia to Anvil (domain 1234)"
	@docker exec celestia-validator celestia-appd tx warp transfer \
		0x726f757465725f61707000000000000000000000000000010000000000000000 \
		1234 \
		0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
		"1000000" \
		--from default \
		--fees 800utia \
		--max-hyperlane-fee 100utia \
		--node http://localhost:26657 \
		--yes
.PHONY: transfer-anvil

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

## query-balance: Query wTIA token balance on Anvil (requires WARP_TOKEN env var).
## Usage: WARP_TOKEN=0x... make query-balance
## Or: WARP_TOKEN=0x... RECIPIENT=0x... make query-balance
query-balance:
	@if [ -z "$(WARP_TOKEN)" ]; then \
		echo "Error: WARP_TOKEN environment variable is required."; \
		echo "Usage: WARP_TOKEN=0xYourTokenAddress make query-anvil-balance"; \
		echo ""; \
		echo "To find the token address, check:"; \
		echo "  docker exec hyperlane-init cat /home/hyperlane/registry/deployments/warp_routes/TIA/anvil-addresses.yaml"; \
		exit 1; \
	fi; \
	RECIPIENT=$${RECIPIENT:-0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266}; \
	echo "--> Querying wTIA balance on Anvil"; \
	echo "Token: $(WARP_TOKEN)"; \
	echo "Recipient: $$RECIPIENT"; \
	cast call $(WARP_TOKEN) "balanceOf(address)(uint256)" $$RECIPIENT --rpc-url http://localhost:8545
.PHONY: query-anvil-balance

## spamoor: Run spamoor transaction flooding against the EVM roll-up.
spamoor:
	@echo "--> Running spamoor transaction flooding daemon"
	@echo "Spamoor will be available on localhost:8080"
	@chmod +x scripts/run-spamoor.sh
	@scripts/run-spamoor.sh $(ARGS)
.PHONY: spamoor

## derive-address: Derive the forwarding address for Anvil (domain 1234).
derive-address:
	@echo "--> Deriving forwarding address for Anvil (domain 1234)"
	@echo "Domain: 1234"
	@echo "Recipient: 0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
	@echo ""
	@docker exec celestia-validator celestia-appd query forwarding derive-address 1234 0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266
.PHONY: derive-address-anvil

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

## e2e: Run end-to-end test (starts all containers including forwarding services, waits for deployment, then runs test).
e2e:
	@echo "--> Building forwarding-relayer Docker image"
	@docker build -t forwarding-relayer:local -f Dockerfile .
	@echo "--> Starting Docker containers"
	@docker compose up --detach
	@echo "--> Waiting for Hyperlane deployment to complete..."
	@timeout=120; elapsed=0; while [ $$elapsed -lt $$timeout ]; do \
		status=$$(docker inspect hyperlane-init --format='{{.State.Status}}' 2>/dev/null || echo "unknown"); \
		if [ "$$status" = "exited" ]; then \
			exit_code=$$(docker inspect hyperlane-init --format='{{.State.ExitCode}}' 2>/dev/null || echo "1"); \
			if [ "$$exit_code" = "0" ]; then \
				echo "Hyperlane deployment completed"; \
				break; \
			else \
				echo "ERROR: Hyperlane deployment failed (exit code: $$exit_code)"; \
				docker logs hyperlane-init 2>&1 | tail -10; \
				exit 1; \
			fi; \
		fi; \
		sleep 5; elapsed=$$((elapsed + 5)); \
		echo "  Waiting... ($${elapsed}s/$${timeout}s) - status: $$status"; \
	done; \
	if [ $$elapsed -ge $$timeout ]; then echo "ERROR: Timed out waiting for Hyperlane deployment"; exit 1; fi
	@echo "--> Waiting for forwarding services to be healthy..."
	@timeout=30; elapsed=0; while [ $$elapsed -lt $$timeout ]; do \
		backend_health=$$(docker inspect forwarding-backend --format='{{.State.Health.Status}}' 2>/dev/null || echo "unknown"); \
		if [ "$$backend_health" = "healthy" ]; then \
			echo "Forwarding backend is healthy"; \
			break; \
		fi; \
		sleep 2; elapsed=$$((elapsed + 2)); \
		echo "  Waiting for backend... ($${elapsed}s/$${timeout}s) - health: $$backend_health"; \
	done; \
	if [ $$elapsed -ge $$timeout ]; then echo "ERROR: Timed out waiting for forwarding backend"; exit 1; fi
	@echo "--> Forwarding relayer starting (logs: make logs-relayer)"
	@echo "--> Running E2E test"
	@cargo run --bin e2e -p e2e --release
.PHONY: e2e


## docker-build-hyperlane: Build Hyperlane init Docker image.
docker-build-hyperlane:
	@echo "--> Building hyperlane-init image"
	@docker build -t ghcr.io/celestiaorg/hyperlane-init:local -f hyperlane/Dockerfile .
.PHONY: docker-build-hyperlane

## docker-build-relayer: Build forwarding-relayer Docker image.
docker-build-relayer:
	@echo "--> Building forwarding-relayer image"
	@docker build -t forwarding-relayer:local -f Dockerfile .
.PHONY: docker-build-relayer

## docker-build: Build all Docker images.
docker-build: docker-build-hyperlane docker-build-relayer
.PHONY: docker-build

## logs-backend: Show logs for the forwarding backend service.
logs-backend:
	@docker logs -f forwarding-backend
.PHONY: logs-backend

## logs-relayer: Show logs for the forwarding relayer service.
logs-relayer:
	@docker logs -f forwarding-relayer
.PHONY: logs-relayer
