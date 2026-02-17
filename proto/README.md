## Protobuf Generation

Protobuf is used by Celestia as the canonical encoding format and thus we leverage this for RPC messaging.

In order to interact with the `x/forwarding` module we include the Protobuf definition in this crate under the `proto` directory.

The `buf` toolchain is employed to handle Rust code generation. 
Please refer to the [official installation documentation](https://buf.build/docs/cli/installation/) to get setup with the `buf` CLI.

Rust code-gen is produced from the Protobuf defintions via `buf.gen.yaml` plugins and included in this project under `src/proto`.

### Usage

1. Update module dependencies:

```bash
buf dep update
```

2. Generate the `celestia-forwarding-relayer` Protobuf code by running the following command:

```bash
cd proto
buf generate --template buf.gen.yaml
```

3. Generate the cosmos-sdk dependencies by running the following command:

```bash
cd proto
buf generate --template buf.gen.yaml \
  buf.build/cosmos/cosmos-sdk:aa25660f4ff746388669ce36b3778442 \
  --path cosmos/base/v1beta1/coin.proto
```
