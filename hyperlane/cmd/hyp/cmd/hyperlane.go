package cmd

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"os"

	"cosmossdk.io/math"
	"github.com/bcp-innovations/hyperlane-cosmos/util"
	hooktypes "github.com/bcp-innovations/hyperlane-cosmos/x/core/02_post_dispatch/types"
	coretypes "github.com/bcp-innovations/hyperlane-cosmos/x/core/types"
	warptypes "github.com/bcp-innovations/hyperlane-cosmos/x/warp/types"
)


// SetupWithIsm deploys the cosmosnative Hyperlane components using the provided ism identifier.
func SetupWithIsm(ctx context.Context, broadcaster *Broadcaster, ismID util.HexAddress) {
	// Create IGP (Interchain Gas Paymaster) for fee quoting
	msgCreateIgp := hooktypes.MsgCreateIgp{
		Owner: broadcaster.address.String(),
		Denom: denom,
	}

	res := broadcaster.BroadcastTx(ctx, &msgCreateIgp)
	igpID := parseIgpIDFromEvents(res.Events)

	// Set destination gas config for Anvil domain (1234) with zero gas cost for local testing
	msgSetDestGasConfig := hooktypes.MsgSetDestinationGasConfig{
		Owner: broadcaster.address.String(),
		IgpId: igpID,
		DestinationGasConfig: &hooktypes.DestinationGasConfig{
			RemoteDomain: 1234,
			GasOracle: &hooktypes.GasOracle{
				TokenExchangeRate: math.NewInt(1),
				GasPrice:          math.NewInt(1),
			},
			GasOverhead: math.NewInt(100000),
		},
	}

	broadcaster.BroadcastTx(ctx, &msgSetDestGasConfig)

	msgCreateNoopHooks := hooktypes.MsgCreateNoopHook{
		Owner: broadcaster.address.String(),
	}

	res = broadcaster.BroadcastTx(ctx, &msgCreateNoopHooks)
	hooksID := parseHooksIDFromEvents(res.Events)

	msgCreateMailBox := coretypes.MsgCreateMailbox{
		Owner:        broadcaster.address.String(),
		DefaultIsm:   ismID,
		LocalDomain:  69420,
		DefaultHook:  &hooksID,
		RequiredHook: &hooksID,
	}

	res = broadcaster.BroadcastTx(ctx, &msgCreateMailBox)
	mailboxID := parseMailboxIDFromEvents(res.Events)

	msgCreateMerkleTreeHook := hooktypes.MsgCreateMerkleTreeHook{
		MailboxId: mailboxID,
		Owner:     broadcaster.address.String(),
	}

	res = broadcaster.BroadcastTx(ctx, &msgCreateMerkleTreeHook)
	merkleTreeHookID := parseMerkleTreeHookIDFromEvents(res.Events)

	msgSetMailbox := coretypes.MsgSetMailbox{
		Owner:             broadcaster.address.String(),
		MailboxId:         mailboxID,
		DefaultIsm:        &ismID,
		DefaultHook:       &hooksID,
		RequiredHook:      &merkleTreeHookID,
		RenounceOwnership: false,
	}

	res = broadcaster.BroadcastTx(ctx, &msgSetMailbox)

	msgCreateCollateralToken := warptypes.MsgCreateCollateralToken{
		Owner:         broadcaster.address.String(),
		OriginMailbox: mailboxID,
		OriginDenom:   denom,
	}

	res = broadcaster.BroadcastTx(ctx, &msgCreateCollateralToken)
	tokenID := parseCollateralTokenIDFromEvents(res.Events)

	// set ism id on new collateral token (for some reason this can't be done on creation)
	msgSetToken := warptypes.MsgSetToken{
		Owner:    broadcaster.address.String(),
		TokenId:  tokenID,
		IsmId:    &ismID,
		NewOwner: broadcaster.address.String(),
	}

	broadcaster.BroadcastTx(ctx, &msgSetToken)

	cfg := &HyperlaneConfig{
		IsmID:          ismID,
		DefaultHookID:  hooksID,
		RequiredHookID: merkleTreeHookID,
		MailboxID:      mailboxID,
		TokenID:        tokenID,
	}

	writeConfig(cfg)
}

func OverwriteIsm(ctx context.Context, broadcaster *Broadcaster, ismID util.HexAddress, mailbox coretypes.Mailbox, token warptypes.WrappedHypToken) {
	msgSetMailbox := coretypes.MsgSetMailbox{
		Owner:             broadcaster.address.String(),
		MailboxId:         mailbox.Id,
		DefaultIsm:        &ismID,
		RenounceOwnership: false,
	}

	tokenID, err := util.DecodeHexAddress(token.Id)
	if err != nil {
		log.Fatal(err)
	}

	// set ism id on new collateral token (for some reason this can't be done on creation)
	msgSetToken := warptypes.MsgSetToken{
		Owner:    broadcaster.address.String(),
		TokenId:  tokenID,
		IsmId:    &ismID,
		NewOwner: broadcaster.address.String(),
	}

	broadcaster.BroadcastTx(ctx, &msgSetMailbox, &msgSetToken)

	cfg := &HyperlaneConfig{
		IsmID:          ismID,
		DefaultHookID:  *mailbox.DefaultHook,
		RequiredHookID: *mailbox.RequiredHook,
		MailboxID:      mailbox.Id,
		TokenID:        tokenID,
	}

	writeConfig(cfg)
}

// SetupRemoteRouter links the provided token identifier on the cosmosnative deployment with the receiver contract on the counterparty.
// For example: if the provided token identifier is a collateral token (e.g. utia), the receiverContract is expected to be the
// contract address for the corresponding synthetic token on the counterparty.
func SetupRemoteRouter(ctx context.Context, broadcaster *Broadcaster, tokenID util.HexAddress, domain uint32, receiverContract string) {
	msgEnrollRemoteRouter := warptypes.MsgEnrollRemoteRouter{
		Owner:   broadcaster.address.String(),
		TokenId: tokenID,
		RemoteRouter: &warptypes.RemoteRouter{
			ReceiverDomain:   domain,
			ReceiverContract: receiverContract,
			Gas:              math.ZeroInt(),
		},
	}

	res := broadcaster.BroadcastTx(ctx, &msgEnrollRemoteRouter)
	recvContract := parseReceiverContractFromEvents(res.Events)

	fmt.Printf("successfully registered remote router on Hyperlane cosmosnative: \n%s", recvContract)
}

func writeConfig(cfg *HyperlaneConfig) {
	out, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		log.Fatalf("failed to marshal config: %v", err)
	}

	outputPath := "hyperlane-cosmosnative.json"
	if err := os.WriteFile(outputPath, out, 0o644); err != nil {
		log.Fatalf("failed to write JSON file: %v", err)
	}

	fmt.Printf("successfully deployed Hyperlane: \n%s\n", string(out))
}

