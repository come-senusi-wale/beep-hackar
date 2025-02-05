package types

import (
	errorsmod "cosmossdk.io/errors"
	sdk "github.com/cosmos/cosmos-sdk/types"
	sdkerrors "github.com/cosmos/cosmos-sdk/types/errors"
)

var _ sdk.Msg = &MsgSendIntentPacket{}

func NewMsgSendIntentPacket(
	creator string,
	port string,
	channelID string,
	timeoutTimestamp uint64,
	actionType string,
	memo string,
	targetChain string,
	minOutput uint64,
	status string,
	executor string,
	expiryHeight uint64,
) *MsgSendIntentPacket {
	return &MsgSendIntentPacket{
		Creator:          creator,
		Port:             port,
		ChannelID:        channelID,
		TimeoutTimestamp: timeoutTimestamp,
		ActionType:       actionType,
		Memo:             memo,
		TargetChain:      targetChain,
		MinOutput:        minOutput,
		Status:           status,
		Executor:         executor,
		ExpiryHeight:     expiryHeight,
	}
}

func (msg *MsgSendIntentPacket) ValidateBasic() error {
	_, err := sdk.AccAddressFromBech32(msg.Creator)
	if err != nil {
		return errorsmod.Wrapf(sdkerrors.ErrInvalidAddress, "invalid creator address (%s)", err)
	}
	if msg.Port == "" {
		return errorsmod.Wrap(sdkerrors.ErrInvalidRequest, "invalid packet port")
	}
	if msg.ChannelID == "" {
		return errorsmod.Wrap(sdkerrors.ErrInvalidRequest, "invalid packet channel")
	}
	if msg.TimeoutTimestamp == 0 {
		return errorsmod.Wrap(sdkerrors.ErrInvalidRequest, "invalid packet timeout")
	}
	return nil
}
