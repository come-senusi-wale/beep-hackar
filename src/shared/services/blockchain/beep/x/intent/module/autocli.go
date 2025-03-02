package intent

import (
	autocliv1 "cosmossdk.io/api/cosmos/autocli/v1"

	modulev1 "beep/api/beep/intent"
)

// AutoCLIOptions implements the autocli.HasAutoCLIConfig interface.
func (am AppModule) AutoCLIOptions() *autocliv1.ModuleOptions {
	return &autocliv1.ModuleOptions{
		Query: &autocliv1.ServiceCommandDescriptor{
			Service: modulev1.Query_ServiceDesc.ServiceName,
			RpcCommandOptions: []*autocliv1.RpcCommandOptions{
				{
					RpcMethod: "Params",
					Use:       "params",
					Short:     "Shows the parameters of the module",
				},
				{
					RpcMethod: "IntentsAll",
					Use:       "list-intents",
					Short:     "List all Intents",
				},
				{
					RpcMethod:      "Intents",
					Use:            "show-intents [id]",
					Short:          "Shows a Intents by id",
					PositionalArgs: []*autocliv1.PositionalArgDescriptor{{ProtoField: "id"}},
				},
				// this line is used by ignite scaffolding # autocli/query
			},
		},
		Tx: &autocliv1.ServiceCommandDescriptor{
			Service:              modulev1.Msg_ServiceDesc.ServiceName,
			EnhanceCustomCommand: true, // only required if you want to use the custom command
			RpcCommandOptions: []*autocliv1.RpcCommandOptions{
				{
					RpcMethod: "UpdateParams",
					Skip:      true, // skipped because authority gated
				},
				{
					RpcMethod:      "CreateIntent",
					Use:            "create-intent [inputToken] [amount] [outputToken] [min-output] [intent-type] [target-chain] [memo]",
					Short:          "Send a create-intent tx",
					PositionalArgs: []*autocliv1.PositionalArgDescriptor{{ProtoField: "inputToken"}, {ProtoField: "amount"}, {ProtoField: "outputToken"}, {ProtoField: "minOutput"}, {ProtoField: "intentType"}, {ProtoField: "targetChain"}, {ProtoField: "memo"}},
				},
				{
					RpcMethod:      "AcceptIntent",
					Use:            "accept-intent [id]",
					Short:          "Send a accept-intent tx",
					PositionalArgs: []*autocliv1.PositionalArgDescriptor{{ProtoField: "id"}},
				},
				// this line is used by ignite scaffolding # autocli/tx
			},
		},
	}
}
