# Deploying a New Warp Route

This repo now treats the checked-in Hyperlane registry under `hyperlane/` as the source of truth for local Docker deployments.

## Default Local Route

The default route is committed in these files:

- `hyperlane/configs/rethlocal-core.yaml`
- `hyperlane/configs/celestia-core.yaml`
- `hyperlane/registry/chains/rethlocal/addresses.yaml`
- `hyperlane/registry/chains/celestiadev/addresses.yaml`
- `hyperlane/registry/deployments/warp_routes/TIA/celestiadev-rethlocal-deploy.yaml`
- `hyperlane/registry/deployments/warp_routes/TIA/celestiadev-rethlocal-config.yaml`

`hyperlane-init` mounts that directory and runs:

```bash
hyperlane core deploy --chain rethlocal --config ./configs/rethlocal-core.yaml --registry ./registry --yes
hyperlane core read --chain rethlocal --config ./configs/rethlocal-core.yaml --registry ./registry
hyperlane core deploy --chain celestiadev --config ./configs/celestia-core.yaml --registry ./registry --yes
hyperlane core read --chain celestiadev --config ./configs/celestia-core.yaml --registry ./registry
hyperlane warp deploy --warp-route-id TIA --registry ./registry --yes
```

## Customizing a Route

To add or replace a local route:

1. Update the chain metadata/config files under `hyperlane/configs/` and `hyperlane/registry/chains/`.
2. Add a deploy file under `hyperlane/registry/deployments/warp_routes/<SYMBOL>/` that includes both chains, following `celestiadev-rethlocal-deploy.yaml`.
3. Start the stack with `make start` or `docker compose up --detach`.
4. Wait for `hyperlane-init` to finish successfully.
5. Commit the generated `addresses.yaml` and `<route>-config.yaml` files back into the repo.
6. Update `hyperlane/relayer-config.json` if the route changes any relayer-facing addresses.

## Verifying the Generated Artifacts

After deployment, confirm these files exist:

```bash
ls hyperlane/registry/chains/rethlocal/addresses.yaml
ls hyperlane/registry/chains/celestiadev/addresses.yaml
ls hyperlane/registry/deployments/warp_routes/TIA/celestiadev-rethlocal-config.yaml
```

The warp config file contains both the Celestia collateral token ID and the EVM synthetic token address. The E2E harness and the manual forwarding guide both read from that file.
