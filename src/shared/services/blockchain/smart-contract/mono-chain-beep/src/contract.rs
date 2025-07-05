#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{to_json_binary, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdResult};
use cw2::set_contract_version;
use execute::{
    execute_add_supported_protocols, execute_add_supported_tokens, execute_create_intent,
    execute_fill_intent, execute_remove_supported_protocols, execute_remove_supported_tokens,
    execute_update_admin, execute_update_default_timeout_height, execute_withdraw_intent_fund,
};

use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};
use crate::state::{Config, CONFIG};

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:mono-chain-beep";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    let config = Config {
        admin: info.sender.clone(),
        supported_tokens: msg.supported_tokens,
        default_timeout_height: msg.default_timeout_height,
        supported_protocols: msg.supported_protocols,
    };

    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("admin", info.sender.to_string())
        .add_attribute(
            "default_timeout_height",
            msg.default_timeout_height.to_string(),
        )
        .add_attribute("timestamp", env.block.time.to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(deps: DepsMut, env: Env, info: MessageInfo, msg: ExecuteMsg) -> StdResult<Response> {
    match msg {
        ExecuteMsg::CreateIntent {
            intent_type,
            input_tokens,
            timeout,
            tip,
        } => execute_create_intent(deps, env, info, intent_type, input_tokens, timeout, tip),
        ExecuteMsg::FillIntent {
            intent_id,
            intent_type,
        } => execute_fill_intent(deps, env, info, intent_id, intent_type),
        ExecuteMsg::WithdrawIntentFund { intent_id } => {
            execute_withdraw_intent_fund(deps, env, info, intent_id)
        }
        ExecuteMsg::UpdateAdmin { new_admin } => execute_update_admin(deps, env, info, new_admin),
        ExecuteMsg::AddSupportedTokens { tokens } => {
            execute_add_supported_tokens(deps, env, info, tokens)
        }
        ExecuteMsg::RemoveSupportedTokens { tokens } => {
            execute_remove_supported_tokens(deps, env, info, tokens)
        }
        ExecuteMsg::AddSupportedProtocols { protocols } => {
            execute_add_supported_protocols(deps, env, info, protocols)
        }
        ExecuteMsg::RemoveSupportedProtocols { protocols } => {
            execute_remove_supported_protocols(deps, env, info, protocols)
        }
        ExecuteMsg::UpdateDefaultTimeoutHeight {
            default_timeout_height,
        } => execute_update_default_timeout_height(deps, env, info, default_timeout_height),
    }
}

pub mod execute {
    use cosmwasm_std::{Addr, BankMsg, Coin, CosmosMsg, StdError, Uint128, WasmMsg};
    use sha2::{Digest, Sha256};

    use crate::state::{BeepCoin, Intent, IntentStatus, IntentType, ESCROW, INTENTS, USER_NONCE};

    use super::*;

    pub fn execute_create_intent(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        intent_type: IntentType,
        input_tokens: Vec<BeepCoin>,
        timeout: Option<u64>,
        tip: BeepCoin,
    ) -> StdResult<Response> {
        let config = CONFIG.load(deps.storage)?;
        let nonce = USER_NONCE.load(deps.storage, &info.sender).unwrap_or(0);
        let id = generate_intent_id(info.sender.clone(), nonce);

        validate_intent_type(&intent_type, &config)?;
        let validate_msg = validate_tokens(&deps, &env, &info, &input_tokens).unwrap();
        let tip_msg = validate_tokens(&deps, &env, &info, &[tip.clone()]).unwrap();
        let msgs = [validate_msg.clone(), tip_msg.clone()].concat();

        let intent = Intent {
            id: id.clone(),
            creator: info.sender.clone(),
            input_tokens: input_tokens.clone(),
            intent_type,
            executor: None,
            status: IntentStatus::Active,
            created_at: env.block.height,
            timeout: env.block.height + timeout.unwrap_or(config.default_timeout_height),
            tip,
        };

        INTENTS.save(deps.storage, &id, &intent)?;
        ESCROW.save(deps.storage, (&info.sender, &id), &input_tokens)?;
        USER_NONCE.save(deps.storage, &info.sender, &(nonce + 1))?;

        Ok(Response::new()
            .add_messages(msgs.clone())
            .add_attribute("action", "create_intent")
            .add_attribute("intent_id", id)
            .add_attribute("status", "active")
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_fill_intent(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        intent_id: String,
        intent_type: IntentType,
    ) -> StdResult<Response> {
        let mut intent = INTENTS.load(deps.storage, &intent_id)?;

        if intent.status != IntentStatus::Active {
            return Err(StdError::generic_err("Intent is not active"));
        }

        let msgs = validate_filling(&deps, &env, &info, &intent_id, &intent_type)?;

        // Update intent status
        intent.executor = Some(info.sender.clone());
        intent.status = IntentStatus::Completed;
        INTENTS.save(deps.storage, &intent_id, &intent)?;

        // Transfer input tokens to executor
        let mut transfer_msgs = Vec::new();
        for token in intent.input_tokens {
            if token.is_native {
                let msg = add_native_transfer_msg(&token.token, &info.sender, token.amount)?;
                transfer_msgs.push(msg);
            } else {
                let msg = add_cw20_transfer_msg(&token.token, &info.sender, token.amount)?;
                transfer_msgs.push(msg);
            }
        }

        // Transfer tip to executor
        if intent.tip.is_native {
            let msg = add_native_transfer_msg(&intent.tip.token, &info.sender, intent.tip.amount)?;
            transfer_msgs.push(msg);
        } else {
            let msg = add_cw20_transfer_msg(&intent.tip.token, &info.sender, intent.tip.amount)?;
            transfer_msgs.push(msg);
        }

        // Transfer output tokens to creator
        let mut output_msgs = msgs;
        match intent_type {
            IntentType::Swap { output_tokens } => {
                for token in output_tokens {
                    let recipient = token.target_address.unwrap_or(intent.creator.clone());
                    if token.is_native {
                        let msg = add_native_transfer_msg(&token.token, &recipient, token.amount)?;
                        output_msgs.push(msg);
                    } else {
                        let msg = add_cw20_transfer_msg(&token.token, &recipient, token.amount)?;
                        output_msgs.push(msg);
                    }
                }
            }
        }

        // Combine all messages
        let all_msgs = [transfer_msgs, output_msgs].concat();

        Ok(Response::new()
            .add_messages(all_msgs)
            .add_attribute("action", "fill_intent")
            .add_attribute("intent_id", intent_id)
            .add_attribute("executor", info.sender)
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_withdraw_intent_fund(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        intent_id: String,
    ) -> StdResult<Response> {
        // check if intent
        let mut intent = INTENTS.load(deps.storage, &intent_id)?;

        if env.block.height < intent.timeout {
            return Err(StdError::generic_err("Intent has not expired"));
        }

        if intent.creator != info.sender {
            return Err(StdError::generic_err("Unauthorized"));
        }

        // update intent
        intent.status = IntentStatus::Expired;
        INTENTS.save(deps.storage, &intent_id, &intent)?;

        let tokens = ESCROW.load(deps.storage, (&info.sender, &intent_id))?;
        let mut messages: Vec<CosmosMsg> = vec![];

        for token in tokens {
            if token.is_native {
                messages.push(add_native_transfer_msg(
                    &token.token,
                    &info.sender,
                    token.amount,
                )?);
            } else {
                messages.push(add_cw20_transfer_msg(
                    &token.token,
                    &info.sender,
                    token.amount,
                )?);
            }
        }

        // remove funds from escrow
        ESCROW.remove(deps.storage, (&info.sender, &intent_id));

        Ok(Response::new()
            .add_messages(messages)
            .add_attribute("action", "withdraw_intent_fund")
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_update_admin(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        new_admin: Addr,
    ) -> StdResult<Response> {
        let mut config = CONFIG.load(deps.storage)?;

        if info.sender != config.admin {
            return Err(StdError::generic_err("Unauthorized"));
        }

        config.admin = new_admin.clone();
        CONFIG.save(deps.storage, &config)?;

        Ok(Response::new()
            .add_attribute("action", "execute_update_admin")
            .add_attribute("old_admin", info.sender.to_string())
            .add_attribute("new_admin", new_admin.to_string())
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_add_supported_tokens(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        tokens: Vec<String>,
    ) -> StdResult<Response> {
        let mut config = CONFIG.load(deps.storage)?;

        if info.sender != config.admin {
            return Err(StdError::generic_err("Unauthorized"));
        }

        for token in tokens {
            if !config.supported_tokens.contains(&token) {
                config.supported_tokens.push(token);
            }
        }

        CONFIG.save(deps.storage, &config)?;

        Ok(Response::new()
            .add_attribute("action", "execute_add_supported_tokens")
            .add_attribute("status", "success")
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_remove_supported_tokens(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        tokens: Vec<String>,
    ) -> StdResult<Response> {
        let mut config = CONFIG.load(deps.storage)?;

        if info.sender != config.admin {
            return Err(StdError::generic_err("Unauthorized"));
        }

        config
            .supported_tokens
            .retain(|token| !tokens.contains(token));

        CONFIG.save(deps.storage, &config)?;

        Ok(Response::new()
            .add_attribute("action", "execute_remove_supported_tokens")
            .add_attribute("status", "success")
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_add_supported_protocols(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        protocols: Vec<String>,
    ) -> StdResult<Response> {
        let mut config = CONFIG.load(deps.storage)?;

        if info.sender != config.admin {
            return Err(StdError::generic_err("Unauthorized"));
        }

        for protocol in protocols {
            if !config.supported_protocols.contains(&protocol) {
                config.supported_protocols.push(protocol);
            }
        }

        CONFIG.save(deps.storage, &config)?;

        Ok(Response::new()
            .add_attribute("action", "execute_add_supported_protocols")
            .add_attribute("status", "success")
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_remove_supported_protocols(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        protocols: Vec<String>,
    ) -> StdResult<Response> {
        let mut config = CONFIG.load(deps.storage)?;

        if info.sender != config.admin {
            return Err(StdError::generic_err("Unauthorized"));
        }

        config
            .supported_protocols
            .retain(|protocol| !protocols.contains(protocol));

        CONFIG.save(deps.storage, &config)?;

        Ok(Response::new()
            .add_attribute("action", "execute_remove_supported_protocols")
            .add_attribute("status", "success")
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    pub fn execute_update_default_timeout_height(
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        default_timeout_height: u64,
    ) -> StdResult<Response> {
        let mut config = CONFIG.load(deps.storage)?;

        if info.sender != config.admin {
            return Err(StdError::generic_err("Unauthorized"));
        }

        config.default_timeout_height = default_timeout_height;
        CONFIG.save(deps.storage, &config)?;

        Ok(Response::new()
            .add_attribute("action", "execute_update_default_timeout_height")
            .add_attribute("status", "success")
            .add_attribute("timestamp", env.block.time.to_string()))
    }

    fn validate_intent_type(intent_type: &IntentType, config: &Config) -> StdResult<()> {
        match intent_type {
            IntentType::Swap { output_tokens } => {
                for token in output_tokens {
                    if !config.supported_tokens.contains(&token.token) {
                        return Err(StdError::generic_err(format!(
                            "Unsupported tokens used. Supplied token: {:?}, Supported tokens: {:?}",
                            token.token, config.supported_tokens
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_filling(
        deps: &DepsMut,
        env: &Env,
        info: &MessageInfo,
        intent_id: &str,
        intent_type: &IntentType,
    ) -> StdResult<Vec<CosmosMsg>> {
        let mut tokens = ESCROW
            .load(deps.storage, (&info.sender, intent_id))
            .unwrap();
        let intent = INTENTS.load(deps.storage, intent_id)?;
        let mut msgs = vec![];

        match intent_type {
            IntentType::Swap { output_tokens } => {
                for token in output_tokens {
                    if token.is_native {
                        // Find the coin with matching denom in the sent funds
                        let sent_amount = info
                            .funds
                            .iter()
                            .find(|coin| coin.denom == token.token)
                            .map(|coin| coin.amount)
                            .unwrap_or_default();

                        // Check if sent amount matches required amount
                        if sent_amount < token.amount {
                            return Err(StdError::generic_err(format!(
                                "Insufficient native token sent. Required: {}, Sent: {}",
                                token.amount, sent_amount
                            )));
                        }
                    } else {
                        let transfer_from_msg = validate_cw20_token_payment(
                            &deps.as_ref(),
                            env,
                            info,
                            &token.token,
                            token.amount,
                        )?;

                        msgs.push(transfer_from_msg);
                    }

                    // Add token to escrow
                    tokens.push(BeepCoin {
                        token: token.token.clone(),
                        amount: token.amount,
                        is_native: token.is_native,
                    });
                }
            }
        }

        // cross check tokens
        for token in intent.input_tokens {
            assert!(tokens.contains(&token));
        }

        Ok(msgs)
    }

    fn generate_intent_id(creator: Addr, nonce: u128) -> String {
        let mut hasher = Sha256::new();

        // Combine multiple factors for better uniqueness
        let input = format!("{}:{}", creator, nonce);

        hasher.update(input.as_bytes());
        let result = hasher.finalize();

        format!("intent{}", hex::encode(&result[..20]))
    }

    fn validate_tokens(
        deps: &DepsMut,
        env: &Env,
        info: &MessageInfo,
        input_tokens: &[BeepCoin],
    ) -> Result<Vec<CosmosMsg>, ContractError> {
        let config = CONFIG.load(deps.storage)?;
        let mut msg: Vec<CosmosMsg> = vec![];
        for token in input_tokens {
            if !config.supported_tokens.contains(&token.token) {
                return Err(ContractError::Std(StdError::generic_err(
                    "Unsupported token: {}",
                )));
            }

            if token.is_native {
                validate_native_token_payment(info, &token.token, token.amount)?;
            } else {
                let transfer_from_msg = validate_cw20_token_payment(
                    &deps.as_ref(),
                    env,
                    info,
                    &token.token,
                    token.amount,
                )?;
                msg.push(transfer_from_msg);
            }
        }

        Ok(msg)
    }

    fn validate_native_token_payment(
        info: &MessageInfo,
        denom: &str,
        required_amount: Uint128,
    ) -> StdResult<()> {
        // Find the coin with matching denom in the sent funds
        let sent_amount = info
            .funds
            .iter()
            .find(|coin| coin.denom == denom)
            .map(|coin| coin.amount)
            .unwrap_or_default();

        // Check if sent amount matches required amount
        if sent_amount < required_amount {
            return Err(StdError::generic_err(format!(
                "Insufficient native token sent. Required: {}, Sent: {}",
                required_amount, sent_amount
            )));
        }

        Ok(())
    }

    fn validate_cw20_token_payment(
        deps: &Deps,
        env: &Env,
        info: &MessageInfo,
        token_address: &str,
        required_amount: Uint128,
    ) -> StdResult<CosmosMsg> {
        // Query token balance
        let balance: cw20::BalanceResponse = deps.querier.query_wasm_smart(
            token_address,
            &cw20::Cw20QueryMsg::Balance {
                address: info.sender.to_string(),
            },
        )?;

        // Check if user has sufficient balance
        if balance.balance < required_amount {
            return Err(StdError::generic_err(format!(
                "Insufficient CW20 token balance. Required: {}, Balance: {}",
                required_amount, balance.balance
            )));
        }

        // Query allowance
        let allowance: cw20::AllowanceResponse = deps.querier.query_wasm_smart(
            token_address,
            &cw20::Cw20QueryMsg::Allowance {
                owner: info.sender.to_string(),
                spender: env.contract.address.to_string(),
            },
        )?;

        // Check if contract has sufficient allowance
        if allowance.allowance < required_amount {
            return Err(StdError::generic_err(format!(
                "Insufficient CW20 token allowance. Required: {}, Allowance: {}",
                required_amount, allowance.allowance
            )));
        }

        // move funds to contract
        let messages: CosmosMsg = (WasmMsg::Execute {
            contract_addr: token_address.to_string(),
            msg: to_json_binary(&cw20::Cw20ExecuteMsg::TransferFrom {
                owner: info.sender.to_string(),
                recipient: env.contract.address.to_string(),
                amount: required_amount,
            })?,
            funds: vec![],
        })
        .into();

        Ok(messages)
    }

    fn add_cw20_transfer_msg(
        token_address: &str,
        to: &Addr,
        amount: Uint128,
    ) -> StdResult<CosmosMsg> {
        Ok(WasmMsg::Execute {
            contract_addr: token_address.to_string(),
            msg: to_json_binary(&cw20::Cw20ExecuteMsg::Transfer {
                recipient: to.to_string(),
                amount,
            })?,
            funds: vec![],
        }
        .into())
    }

    // Helper function to handle native token transfers
    fn add_native_transfer_msg(denom: &str, to: &Addr, amount: Uint128) -> StdResult<CosmosMsg> {
        Ok(BankMsg::Send {
            to_address: to.to_string(),
            amount: vec![Coin {
                denom: denom.to_string(),
                amount,
            }],
        }
        .into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::state::ExpectedToken;
        use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env};
        use cosmwasm_std::{
            coins, from_json, Addr, ContractResult, Response, SystemError, SystemResult, WasmQuery,
        };
        use cw20::{AllowanceResponse, BalanceResponse, Cw20ExecuteMsg, Expiration};

        #[test]
        fn test_instantiate() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("creator"), &[]);

            let msg = InstantiateMsg {
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 20,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };

            let res: Response =
                instantiate(deps.as_mut(), env.clone(), info.clone(), msg.clone()).unwrap();

            // Ensure the response contains expected attributes
            assert_eq!(res.attributes.len(), 4);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "instantiate");
            assert_eq!(res.attributes[1].key, "admin");
            assert_eq!(res.attributes[1].value, "creator");
            assert_eq!(res.attributes[2].key, "default_timeout_height");
            assert_eq!(res.attributes[2].value, "20");

            // Verify the config state
            let config: Config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(config.admin, Addr::unchecked("creator"));
            assert_eq!(
                config.supported_tokens,
                vec!["token1".to_string(), "token2".to_string()]
            );
            assert_eq!(config.default_timeout_height, 20);
            assert_eq!(
                config.supported_protocols,
                vec!["protocol1".to_string(), "protocol2".to_string()]
            );
        }

        #[test]
        fn test_validate_intent_type_with_supported_tokens() {
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };

            let intent_type = IntentType::Swap {
                output_tokens: vec![
                    ExpectedToken {
                        token: "token1".to_string(),
                        is_native: true,
                        amount: Uint128::new(100),
                        target_address: None,
                    },
                    ExpectedToken {
                        token: "token2".to_string(),
                        is_native: true,
                        amount: Uint128::new(200),
                        target_address: None,
                    },
                ],
            };

            let result = validate_intent_type(&intent_type, &config);
            assert!(result.is_ok());
        }

        #[test]
        fn test_validate_intent_type_with_unsupported_tokens() {
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };

            let intent_type = IntentType::Swap {
                output_tokens: vec![
                    ExpectedToken {
                        token: "token1".to_string(),
                        is_native: true,
                        amount: Uint128::new(100),
                        target_address: None,
                    },
                    ExpectedToken {
                        token: "token3".to_string(),
                        is_native: true,
                        amount: Uint128::new(200),
                        target_address: None,
                    },
                ],
            };

            let result = validate_intent_type(&intent_type, &config);
            assert!(result.is_err());
            assert_eq!(
                result.unwrap_err(),
                StdError::generic_err("Unsupported tokens used. Supplied token: \"token3\", Supported tokens: [\"token1\", \"token2\"]")
            );
        }

        #[test]
        fn test_execute_create_intent() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("creator"), &coins(1000, "token"));

            // Set up initial config
            let config = Config {
                admin: info.sender.clone(),
                supported_tokens: vec!["token".to_string()],
                default_timeout_height: 20,
                supported_protocols: vec!["protocol".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Set up initial user nonce
            USER_NONCE
                .save(&mut deps.storage, &info.sender, &0)
                .unwrap();

            let intent_type = IntentType::Swap {
                output_tokens: vec![ExpectedToken {
                    token: "token".to_string(),
                    is_native: true,
                    amount: Uint128::new(100),
                    target_address: None,
                }],
            };
            let input_tokens = vec![BeepCoin {
                token: "token".to_string(),
                amount: Uint128::new(1000),
                is_native: true,
            }];
            let timeout = Some(2);
            let tip = BeepCoin {
                token: "token".to_string(),
                amount: Uint128::new(10),
                is_native: true,
            };

            let res = execute_create_intent(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                intent_type.clone(),
                input_tokens.clone(),
                timeout,
                tip.clone(),
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 4);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "create_intent");
            assert_eq!(res.attributes[1].key, "intent_id");
            assert_eq!(res.attributes[2].key, "status");
            assert_eq!(res.attributes[2].value, "active");

            // Verify intent saved in storage
            let intent_id = res.attributes[1].value.clone();
            let saved_intent = INTENTS.load(&deps.storage, &intent_id).unwrap();
            assert_eq!(saved_intent.creator, info.sender);
            assert_eq!(saved_intent.input_tokens, input_tokens);
            assert_eq!(saved_intent.intent_type, intent_type);
            assert_eq!(saved_intent.status, IntentStatus::Active);
            assert_eq!(saved_intent.created_at, env.block.height);
            assert_eq!(saved_intent.timeout, env.block.height + 2);
            assert_eq!(saved_intent.tip, tip);

            // Verify nonce incremented
            let nonce = USER_NONCE.load(&deps.storage, &info.sender).unwrap();
            assert_eq!(nonce, 1);
        }

        #[test]
        fn test_execute_create_intent_with_cw20_tokens() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("creator"), &[]);

            // Set up initial config
            let config = Config {
                admin: info.sender.clone(),
                supported_tokens: vec!["cw20_token".to_string()],
                default_timeout_height: 20,
                supported_protocols: vec!["protocol".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Set up initial user nonce
            USER_NONCE
                .save(&mut deps.storage, &info.sender, &0)
                .unwrap();

            let intent_type = IntentType::Swap {
                output_tokens: vec![ExpectedToken {
                    token: "cw20_token".to_string(),
                    is_native: false,
                    amount: Uint128::new(100),
                    target_address: None,
                }],
            };
            let input_tokens = vec![BeepCoin {
                token: "cw20_token".to_string(),
                amount: Uint128::new(1000),
                is_native: false,
            }];
            let timeout = Some(2);
            let tip = BeepCoin {
                token: "cw20_token".to_string(),
                amount: Uint128::new(10),
                is_native: false,
            };

            // Mock balance and allowance queries for CW20 tokens
            let cw20_token_address = String::from("cw20_token");

            // Clone necessary variables to avoid moving them into the closure
            let cw20_token_address_clone = cw20_token_address.clone();
            let info_sender_clone = info.sender.clone();

            // Mock the balance and allowance queries
            deps.querier.update_wasm(move |query| {
                let cw20_token_address = cw20_token_address_clone.clone();
                let info_sender = info_sender_clone.clone();
                let env = mock_env();

                match query {
                    WasmQuery::Smart { contract_addr, msg } => {
                        if contract_addr == &cw20_token_address {
                            if let Ok(cw20::Cw20QueryMsg::Balance { address }) = from_json(&msg) {
                                if address == info_sender.to_string() {
                                    return SystemResult::Ok(ContractResult::Ok(
                                        to_json_binary(&cw20::BalanceResponse {
                                            balance: Uint128::from(1000u128),
                                        })
                                        .unwrap(),
                                    ));
                                }
                            } else if let Ok(cw20::Cw20QueryMsg::Allowance { owner, spender }) =
                                from_json(&msg)
                            {
                                if owner == info_sender.to_string()
                                    && spender == env.contract.address.to_string()
                                {
                                    return SystemResult::Ok(ContractResult::Ok(
                                        to_json_binary(&cw20::AllowanceResponse {
                                            allowance: Uint128::from(1000u128),
                                            expires: Expiration::Never {},
                                        })
                                        .unwrap(),
                                    ));
                                }
                            }
                        }
                        SystemResult::Err(SystemError::UnsupportedRequest {
                            kind: "".to_string(),
                        })
                    }
                    _ => SystemResult::Err(SystemError::UnsupportedRequest {
                        kind: "".to_string(),
                    }),
                }
            });

            let res = execute_create_intent(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                intent_type.clone(),
                input_tokens.clone(),
                timeout,
                tip.clone(),
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 4);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "create_intent");
            assert_eq!(res.attributes[1].key, "intent_id");
            assert_eq!(res.attributes[2].key, "status");
            assert_eq!(res.attributes[2].value, "active");

            // Verify intent saved in storage
            let intent_id = res.attributes[1].value.clone();
            let saved_intent = INTENTS.load(&deps.storage, &intent_id).unwrap();
            assert_eq!(saved_intent.creator, info.sender);
            assert_eq!(saved_intent.input_tokens, input_tokens);
            assert_eq!(saved_intent.intent_type, intent_type);
            assert_eq!(saved_intent.status, IntentStatus::Active);
            assert_eq!(saved_intent.created_at, env.block.height);
            assert_eq!(saved_intent.timeout, env.block.height + 2);
            assert_eq!(saved_intent.tip, tip);

            // Verify nonce incremented
            let nonce = USER_NONCE.load(&deps.storage, &info.sender).unwrap();
            assert_eq!(nonce, 1);

            // println!("Msgs: {:?}", res.messages);
            // println!("Atr: {:?}", res.attributes);
        }

        #[test]
        fn test_validate_filling_with_native_tokens() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("executor"), &coins(1000, "token"));

            // Set up initial intent
            let intent = Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: env.block.height,
                timeout: env.block.height + 120,
                tip: BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            };
            INTENTS.save(&mut deps.storage, "intent1", &intent).unwrap();

            // Set up initial escrow
            ESCROW
                .save(
                    &mut deps.storage,
                    (&Addr::unchecked("executor"), "intent1"),
                    &intent.input_tokens,
                )
                .unwrap();

            // Validate filling
            let msgs = validate_filling(
                &deps.as_mut(),
                &env,
                &info,
                "intent1",
                &IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
            )
            .unwrap();

            // Verify that no additional messages are required
            assert!(msgs.is_empty());

            // Verify that the tokens were added to the escrow
            let escrow_tokens = ESCROW
                .load(&deps.storage, (&info.sender, "intent1"))
                .unwrap();
            assert_eq!(
                escrow_tokens,
                vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }]
            );
        }

        #[test]
        fn test_validate_filling_with_insufficient_native_tokens() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("executor"), &coins(500, "token"));

            // Set up initial intent
            let intent = Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: env.block.height,
                timeout: env.block.height + 120,
                tip: BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            };
            INTENTS.save(&mut deps.storage, "intent1", &intent).unwrap();
            ESCROW
                .save(
                    &mut deps.storage,
                    (&info.sender, "intent1"),
                    &vec![
                        BeepCoin {
                            token: "token".to_string(),
                            amount: Uint128::new(10),
                            is_native: true,
                        },
                        BeepCoin {
                            token: "token".to_string(),
                            amount: Uint128::new(1000),
                            is_native: true,
                        },
                    ],
                )
                .unwrap();

            // Validate filling
            let result = validate_filling(
                &deps.as_mut(),
                &env,
                &info,
                "intent1",
                &IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(
                result.unwrap_err(),
                StdError::generic_err("Insufficient native token sent. Required: 1000, Sent: 500")
            );
        }

        #[test]
        fn test_add_cw20_transfer_msg() {
            let token_address = "token_address";
            let recipient = Addr::unchecked("recipient");
            let amount = Uint128::new(1000);

            let msg = add_cw20_transfer_msg(token_address, &recipient, amount).unwrap();

            let expected_msg = CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: token_address.to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: recipient.to_string(),
                    amount,
                })
                .unwrap(),
                funds: vec![],
            });

            assert_eq!(msg, expected_msg);
        }

        #[test]
        fn test_add_native_transfer_msg() {
            let denom = "token";
            let recipient = Addr::unchecked("recipient");
            let amount = Uint128::new(1000);

            let msg = add_native_transfer_msg(denom, &recipient, amount).unwrap();

            let expected_msg = CosmosMsg::Bank(BankMsg::Send {
                to_address: recipient.to_string(),
                amount: vec![Coin {
                    denom: denom.to_string(),
                    amount,
                }],
            });

            assert_eq!(msg, expected_msg);
        }

        #[test]
        fn test_execute_fill_intent_with_native_tokens() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("executor"), &coins(1000, "token"));

            // Set up initial intent
            let intent = Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: env.block.height,
                timeout: env.block.height + 120,
                tip: BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            };
            INTENTS.save(&mut deps.storage, "intent1", &intent).unwrap();

            // Set up escrow
            ESCROW
                .save(
                    &mut deps.storage,
                    (&Addr::unchecked("creator"), "intent1"),
                    &intent.input_tokens,
                )
                .unwrap();

            ESCROW
                .save(
                    &mut deps.storage,
                    (&Addr::unchecked("executor"), "intent1"),
                    &vec![BeepCoin {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                    }],
                )
                .unwrap();

            // Execute fill intent
            let res = execute_fill_intent(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                "intent1".to_string(),
                IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 4);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "fill_intent");
            assert_eq!(res.attributes[1].key, "intent_id");
            assert_eq!(res.attributes[1].value, "intent1");
            assert_eq!(res.attributes[2].key, "executor");
            assert_eq!(res.attributes[2].value, "executor");

            // Verify the intent status and executor
            let updated_intent = INTENTS.load(&deps.storage, "intent1").unwrap();
            assert_eq!(updated_intent.status, IntentStatus::Completed);
            assert_eq!(updated_intent.executor, Some(Addr::unchecked("executor")));

            // Verify the messages
            assert_eq!(res.messages.len(), 3);

            // Check for input token transfer to executor
            assert_eq!(
                res.messages[0].msg,
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: "executor".to_string(),
                    amount: vec![Coin {
                        denom: "token".to_string(),
                        amount: Uint128::new(1000),
                    }],
                })
            );

            // Check for tip transfer to executor
            assert_eq!(
                res.messages[1].msg,
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: "executor".to_string(),
                    amount: vec![Coin {
                        denom: "token".to_string(),
                        amount: Uint128::new(10),
                    }],
                })
            );

            // Check for output token transfer to creator
            assert_eq!(
                res.messages[2].msg,
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: "creator".to_string(),
                    amount: vec![Coin {
                        denom: "token".to_string(),
                        amount: Uint128::new(1000),
                    }],
                })
            );
        }

        #[test]
        fn test_execute_fill_intent_with_cw20_tokens() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("executor"), &[]);

            // Set up initial intent
            let intent = Intent {
                id: "intent2".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "cw20_token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: false,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "cw20_token".to_string(),
                        is_native: false,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: env.block.height,
                timeout: env.block.height + 20,
                tip: BeepCoin {
                    token: "cw20_token".to_string(),
                    amount: Uint128::new(10),
                    is_native: false,
                },
            };
            INTENTS.save(&mut deps.storage, "intent2", &intent).unwrap();

            // Set up escrow
            ESCROW
                .save(
                    &mut deps.storage,
                    (&Addr::unchecked("creator"), "intent2"),
                    &intent.input_tokens,
                )
                .unwrap();

            ESCROW
                .save(
                    &mut deps.storage,
                    (&Addr::unchecked("executor"), "intent2"),
                    &vec![BeepCoin {
                        token: "cw20_token".to_string(),
                        is_native: false,
                        amount: Uint128::new(1000),
                    }],
                )
                .unwrap();

            // Mock balance and allowance queries for CW20 tokens
            let cw20_token_address = String::from("cw20_token");

            // Clone necessary variables to avoid moving them into the closure
            let cw20_token_address_clone = cw20_token_address.clone();
            let info_sender_clone = info.sender.clone();

            // Mock the balance and allowance queries
            deps.querier.update_wasm(move |query| {
                let cw20_token_address = cw20_token_address_clone.clone();
                let info_sender = info_sender_clone.clone();
                let env = mock_env();

                match query {
                    WasmQuery::Smart { contract_addr, msg } => {
                        if contract_addr == &cw20_token_address.clone() {
                            if let Ok(cw20::Cw20QueryMsg::Balance { address }) = from_json(&msg) {
                                if address == info_sender.into_string() {
                                    return SystemResult::Ok(ContractResult::Ok(
                                        to_json_binary(&BalanceResponse {
                                            balance: Uint128::from(1000u128),
                                        })
                                        .unwrap(),
                                    ));
                                }
                            } else if let Ok(cw20::Cw20QueryMsg::Allowance { owner, spender }) =
                                from_json(&msg)
                            {
                                if owner == info_sender.into_string()
                                    && spender == env.contract.address.to_string()
                                {
                                    return SystemResult::Ok(ContractResult::Ok(
                                        to_json_binary(&AllowanceResponse {
                                            allowance: Uint128::from(1000u128),
                                            expires: Expiration::Never {},
                                        })
                                        .unwrap(),
                                    ));
                                }
                            }
                        }
                        SystemResult::Err(SystemError::UnsupportedRequest {
                            kind: "".to_string(),
                        })
                    }
                    _ => SystemResult::Err(SystemError::UnsupportedRequest {
                        kind: "".to_string(),
                    }),
                }
            });

            // Execute fill intent
            let res = execute_fill_intent(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                "intent2".to_string(),
                IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "cw20_token".to_string(),
                        is_native: false,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 4);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "fill_intent");
            assert_eq!(res.attributes[1].key, "intent_id");
            assert_eq!(res.attributes[1].value, "intent2");
            assert_eq!(res.attributes[2].key, "executor");
            assert_eq!(res.attributes[2].value, "executor");

            // Verify the intent status and executor
            let updated_intent = INTENTS.load(&deps.storage, "intent2").unwrap();
            assert_eq!(updated_intent.status, IntentStatus::Completed);
            assert_eq!(updated_intent.executor, Some(Addr::unchecked("executor")));

            // Verify the messages
            assert_eq!(res.messages.len(), 4);

            // Check for input token transfer to executor
            assert_eq!(
                res.messages[0].msg,
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: "cw20_token".to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: "executor".to_string(),
                        amount: Uint128::new(1000),
                    })
                    .unwrap(),
                    funds: vec![],
                })
            );

            // Check for tip transfer to executor
            assert_eq!(
                res.messages[1].msg,
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: "cw20_token".to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: "executor".to_string(),
                        amount: Uint128::new(10),
                    })
                    .unwrap(),
                    funds: vec![],
                })
            );

            // Check for output token transfer to contract
            assert_eq!(
                res.messages[2].msg,
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: "cw20_token".to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
                        owner: "executor".to_string(),
                        recipient: env.contract.address.to_string(),
                        amount: Uint128::new(1000)
                    })
                    .unwrap(),
                    funds: vec![],
                })
            );

            // Check for output token transfer to creator
            assert_eq!(
                res.messages[3].msg,
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: "cw20_token".to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: "creator".to_string(),
                        amount: Uint128::new(1000)
                    })
                    .unwrap(),
                    funds: vec![],
                })
            );
        }

        #[test]
        fn test_execute_fill_intent_with_inactive_intent() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("executor"), &coins(1000, "token"));

            // Set up initial intent with Inactive status
            let intent = Intent {
                id: "intent3".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Expired, // Set to Expired status
                created_at: env.block.height,
                timeout: env.block.height + 120,
                tip: BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            };
            INTENTS.save(&mut deps.storage, "intent3", &intent).unwrap();

            // Attempt to execute fill intent
            let result = execute_fill_intent(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                "intent3".to_string(),
                IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(
                result.unwrap_err(),
                StdError::generic_err("Intent is not active")
            );
        }

        #[test]
        fn test_execute_withdraw_intent_fund_before_timeout() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("creator"), &coins(1000, "token"));

            // Set up initial intent
            let intent = Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: env.block.height,
                timeout: env.block.height + 120,
                tip: BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            };
            INTENTS.save(&mut deps.storage, "intent1", &intent).unwrap();

            // Attempt to withdraw funds before timeout
            let result = execute_withdraw_intent_fund(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                "intent1".to_string(),
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(
                result.unwrap_err(),
                StdError::generic_err("Intent has not expired")
            );
        }

        #[test]
        fn test_execute_withdraw_intent_fund_unauthorized() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("not_creator"), &coins(1000, "token"));

            // Set up initial intent
            let intent = Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: env.block.height,
                timeout: env.block.height - 1, // Set to a past height to bypass timeout check
                tip: BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            };
            INTENTS.save(&mut deps.storage, "intent1", &intent).unwrap();

            // Attempt to withdraw funds with unauthorized user
            let result = execute_withdraw_intent_fund(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                "intent1".to_string(),
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), StdError::generic_err("Unauthorized"));
        }

        #[test]
        fn test_execute_withdraw_intent_fund_success() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("creator"), &coins(1000, "token"));

            // Set up initial intent
            let intent = Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator"),
                input_tokens: vec![BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: env.block.height,
                timeout: env.block.height - 1, // Set to a past height to bypass timeout check
                tip: BeepCoin {
                    token: "token".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            };
            INTENTS.save(&mut deps.storage, "intent1", &intent).unwrap();

            // Set up initial escrow
            ESCROW
                .save(
                    &mut deps.storage,
                    (&Addr::unchecked("creator"), "intent1"),
                    &intent.input_tokens,
                )
                .unwrap();

            // Execute withdraw intent fund
            let res = execute_withdraw_intent_fund(
                deps.as_mut(),
                env.clone(),
                info.clone(),
                "intent1".to_string(),
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 2);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "withdraw_intent_fund");

            // Verify the intent status
            let updated_intent = INTENTS.load(&deps.storage, "intent1").unwrap();
            assert_eq!(updated_intent.status, IntentStatus::Expired);

            // Verify the messages
            assert_eq!(res.messages.len(), 1);
            assert_eq!(
                res.messages[0].msg,
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: "creator".to_string(),
                    amount: vec![Coin {
                        denom: "token".to_string(),
                        amount: Uint128::new(1000),
                    }],
                })
            );

            // Verify the escrow entry is removed
            let escrow_result =
                ESCROW.load(&deps.storage, (&Addr::unchecked("creator"), "intent1"));
            assert!(escrow_result.is_err());
        }

        #[test]
        fn test_execute_update_admin_unauthorized() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("not_admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Attempt to update admin with unauthorized user
            let result = execute_update_admin(
                deps.as_mut(),
                env,
                info.clone(),
                Addr::unchecked("new_admin"),
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), StdError::generic_err("Unauthorized"));
        }

        #[test]
        fn test_execute_update_admin_success() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute update admin
            let res = execute_update_admin(
                deps.as_mut(),
                env,
                info.clone(),
                Addr::unchecked("new_admin"),
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 4);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "execute_update_admin");
            assert_eq!(res.attributes[1].key, "old_admin");
            assert_eq!(res.attributes[1].value, "admin");
            assert_eq!(res.attributes[2].key, "new_admin");
            assert_eq!(res.attributes[2].value, "new_admin");

            // Verify the updated config
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(updated_config.admin, Addr::unchecked("new_admin"));
        }

        #[test]
        fn test_execute_add_supported_tokens_unauthorized() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("not_admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Attempt to add supported tokens with unauthorized user
            let result = execute_add_supported_tokens(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["token3".to_string()],
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), StdError::generic_err("Unauthorized"));
        }

        #[test]
        fn test_execute_add_supported_tokens_success() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute add supported tokens
            let res = execute_add_supported_tokens(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["token3".to_string(), "token4".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "execute_add_supported_tokens");
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                updated_config.supported_tokens,
                vec![
                    "token1".to_string(),
                    "token2".to_string(),
                    "token3".to_string(),
                    "token4".to_string()
                ]
            );
        }

        #[test]
        fn test_execute_add_supported_tokens_duplicates() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute add supported tokens with duplicate entries
            let res = execute_add_supported_tokens(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["token2".to_string(), "token3".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "execute_add_supported_tokens");
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config, ensuring no duplicates
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                updated_config.supported_tokens,
                vec![
                    "token1".to_string(),
                    "token2".to_string(),
                    "token3".to_string()
                ]
            );
        }

        #[test]
        fn test_execute_remove_supported_tokens_unauthorized() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("not_admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec![
                    "token1".to_string(),
                    "token2".to_string(),
                    "token3".to_string(),
                ],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Attempt to remove supported tokens with unauthorized user
            let result = execute_remove_supported_tokens(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["token2".to_string()],
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), StdError::generic_err("Unauthorized"));
        }

        #[test]
        fn test_execute_remove_supported_tokens_success() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec![
                    "token1".to_string(),
                    "token2".to_string(),
                    "token3".to_string(),
                ],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute remove supported tokens
            let res = execute_remove_supported_tokens(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["token2".to_string(), "token3".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "execute_remove_supported_tokens");
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(updated_config.supported_tokens, vec!["token1".to_string()]);
        }

        #[test]
        fn test_execute_remove_supported_tokens_not_present() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute remove supported tokens with tokens not in the list
            let res = execute_remove_supported_tokens(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["token3".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "execute_remove_supported_tokens");
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config, ensuring no changes were made
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                updated_config.supported_tokens,
                vec!["token1".to_string(), "token2".to_string()]
            );
        }

        #[test]
        fn test_execute_add_supported_protocols_unauthorized() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("not_admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Attempt to add supported protocols with unauthorized user
            let result = execute_add_supported_protocols(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["protocol3".to_string()],
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), StdError::generic_err("Unauthorized"));
        }

        #[test]
        fn test_execute_add_supported_protocols_success() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute add supported protocols
            let res = execute_add_supported_protocols(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["protocol3".to_string(), "protocol4".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "execute_add_supported_protocols");
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                updated_config.supported_protocols,
                vec![
                    "protocol1".to_string(),
                    "protocol2".to_string(),
                    "protocol3".to_string(),
                    "protocol4".to_string()
                ]
            );
        }

        #[test]
        fn test_execute_add_supported_protocols_duplicates() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute add supported protocols with duplicate entries
            let res = execute_add_supported_protocols(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["protocol2".to_string(), "protocol3".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(res.attributes[0].value, "execute_add_supported_protocols");
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config, ensuring no duplicates
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                updated_config.supported_protocols,
                vec![
                    "protocol1".to_string(),
                    "protocol2".to_string(),
                    "protocol3".to_string()
                ]
            );
        }

        #[test]
        fn test_execute_remove_supported_protocols_unauthorized() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("not_admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec![
                    "protocol1".to_string(),
                    "protocol2".to_string(),
                    "protocol3".to_string(),
                ],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Attempt to remove supported protocols with unauthorized user
            let result = execute_remove_supported_protocols(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["protocol2".to_string()],
            );

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), StdError::generic_err("Unauthorized"));
        }

        #[test]
        fn test_execute_remove_supported_protocols_success() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec![
                    "protocol1".to_string(),
                    "protocol2".to_string(),
                    "protocol3".to_string(),
                ],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute remove supported protocols
            let res = execute_remove_supported_protocols(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["protocol2".to_string(), "protocol3".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(
                res.attributes[0].value,
                "execute_remove_supported_protocols"
            );
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                updated_config.supported_protocols,
                vec!["protocol1".to_string()]
            );
        }

        #[test]
        fn test_execute_remove_supported_protocols_not_present() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute remove supported protocols with protocols not in the list
            let res = execute_remove_supported_protocols(
                deps.as_mut(),
                env,
                info.clone(),
                vec!["protocol3".to_string()],
            )
            .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(
                res.attributes[0].value,
                "execute_remove_supported_protocols"
            );
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config, ensuring no changes were made
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                updated_config.supported_protocols,
                vec!["protocol1".to_string(), "protocol2".to_string()]
            );
        }

        #[test]
        fn test_execute_update_default_timeout_height_unauthorized() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("not_admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Attempt to update default timeout height with unauthorized user
            let result =
                execute_update_default_timeout_height(deps.as_mut(), env, info.clone(), 300);

            // Verify that an error is returned
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), StdError::generic_err("Unauthorized"));
        }

        #[test]
        fn test_execute_update_default_timeout_height_success() {
            let mut deps = mock_dependencies();
            let env = mock_env();
            let info = message_info(&Addr::unchecked("admin"), &[]);

            // Set up initial config
            let config = Config {
                admin: Addr::unchecked("admin"),
                supported_tokens: vec!["token1".to_string(), "token2".to_string()],
                default_timeout_height: 120,
                supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
            };
            CONFIG.save(&mut deps.storage, &config).unwrap();

            // Execute update default timeout height
            let res = execute_update_default_timeout_height(deps.as_mut(), env, info.clone(), 300)
                .unwrap();

            // Verify the response attributes
            assert_eq!(res.attributes.len(), 3);
            assert_eq!(res.attributes[0].key, "action");
            assert_eq!(
                res.attributes[0].value,
                "execute_update_default_timeout_height"
            );
            assert_eq!(res.attributes[1].key, "status");
            assert_eq!(res.attributes[1].value, "success");

            // Verify the updated config
            let updated_config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(updated_config.default_timeout_height, 300);
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::GetConfig {} => to_json_binary(&query::query_config(deps)?),
        QueryMsg::GetIntent { id } => to_json_binary(&query::query_intent(deps, id)?),
        QueryMsg::ListIntents { start_after, limit } => {
            to_json_binary(&query::query_list_intents(deps, start_after, limit)?)
        }
        QueryMsg::GetUserNonce { address } => {
            to_json_binary(&query::query_get_user_nonce(deps, address)?)
        }
    }
}

pub mod query {
    use cosmwasm_std::Addr;
    use cw_storage_plus::Bound;

    use crate::{
        msg::{ConfigResponse, IntentResponse, IntentsResponse, UserNonceResponse},
        state::{Intent, CONFIG, INTENTS, USER_NONCE},
    };

    use super::*;

    pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
        let config = CONFIG.load(deps.storage)?;
        Ok(ConfigResponse {
            admin: config.admin,
            supported_tokens: config.supported_tokens,
            default_timeout_height: config.default_timeout_height,
            supported_protocols: config.supported_protocols,
        })
    }

    pub fn query_intent(deps: Deps, id: String) -> StdResult<IntentResponse> {
        let intent = INTENTS.load(deps.storage, &id)?;
        Ok(IntentResponse { intent })
    }

    pub fn query_list_intents(
        deps: Deps,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<IntentsResponse> {
        let start = start_after.unwrap_or_default();
        let limit = limit.unwrap_or(10) as usize;

        let intents: StdResult<Vec<Intent>> = INTENTS
            .range(
                deps.storage,
                Some(Bound::exclusive(&*start)),
                None,
                cosmwasm_std::Order::Ascending,
            )
            .take(limit)
            .map(|item| item.map(|(_, intent)| intent))
            .collect();

        Ok(IntentsResponse { intents: intents? })
    }

    pub fn query_get_user_nonce(deps: Deps, address: Addr) -> StdResult<UserNonceResponse> {
        let nonce = USER_NONCE.load(deps.storage, &address)?;
        Ok(UserNonceResponse { nonce })
    }
}

#[cfg(test)]
mod tests {
    use cosmwasm_std::{testing::mock_dependencies, Addr, Uint128};

    use crate::{
        contract::query::{query_config, query_get_user_nonce, query_intent, query_list_intents},
        state::{
            BeepCoin, Config, ExpectedToken, Intent, IntentStatus, IntentType, CONFIG, INTENTS,
            USER_NONCE,
        },
    };

    #[test]
    fn test_query_config() {
        let mut deps = mock_dependencies();

        // Set up initial config
        let config = Config {
            admin: Addr::unchecked("admin"),
            supported_tokens: vec!["token1".to_string(), "token2".to_string()],
            default_timeout_height: 120,
            supported_protocols: vec!["protocol1".to_string(), "protocol2".to_string()],
        };
        CONFIG.save(&mut deps.storage, &config).unwrap();

        // Query the config
        let res = query_config(deps.as_ref()).unwrap();

        // Verify the response
        assert_eq!(res.admin, Addr::unchecked("admin"));
        assert_eq!(
            res.supported_tokens,
            vec!["token1".to_string(), "token2".to_string()]
        );
        assert_eq!(res.default_timeout_height, 120);
    }

    #[test]
    fn test_query_config_not_set() {
        let deps = mock_dependencies();

        // Attempt to query config that has not been set
        let result = query_config(deps.as_ref());

        // Verify that an error is returned
        assert!(result.is_err());
    }

    #[test]
    fn test_query_intent() {
        let mut deps = mock_dependencies();

        // Set up initial intent
        let intent = Intent {
            id: "intent1".to_string(),
            creator: Addr::unchecked("creator"),
            input_tokens: vec![BeepCoin {
                token: "token".to_string(),
                amount: Uint128::new(1000),
                is_native: true,
            }],
            intent_type: IntentType::Swap {
                output_tokens: vec![ExpectedToken {
                    token: "token".to_string(),
                    is_native: true,
                    amount: Uint128::new(1000),
                    target_address: None,
                }],
            },
            executor: None,
            status: IntentStatus::Active,
            created_at: 12345,
            timeout: 67890,
            tip: BeepCoin {
                token: "token".to_string(),
                amount: Uint128::new(10),
                is_native: true,
            },
        };
        INTENTS.save(&mut deps.storage, "intent1", &intent).unwrap();

        // Query the intent
        let res = query_intent(deps.as_ref(), "intent1".to_string()).unwrap();

        // Verify the response
        assert_eq!(res.intent.id, "intent1");
        assert_eq!(res.intent.creator, Addr::unchecked("creator"));
        assert_eq!(res.intent.input_tokens.len(), 1);
        assert_eq!(res.intent.input_tokens[0].token, "token");
        assert_eq!(res.intent.input_tokens[0].amount, Uint128::new(1000));
        assert_eq!(
            res.intent.intent_type,
            IntentType::Swap {
                output_tokens: vec![ExpectedToken {
                    token: "token".to_string(),
                    is_native: true,
                    amount: Uint128::new(1000),
                    target_address: None,
                }],
            }
        );
        assert_eq!(res.intent.executor, None);
        assert_eq!(res.intent.status, IntentStatus::Active);
        assert_eq!(res.intent.created_at, 12345);
        assert_eq!(res.intent.timeout, 67890);
        assert_eq!(res.intent.tip.token, "token");
        assert_eq!(res.intent.tip.amount, Uint128::new(10));
        assert!(res.intent.tip.is_native);
    }

    #[test]
    fn test_query_intent_not_found() {
        let deps = mock_dependencies();

        // Attempt to query an intent that does not exist
        let result = query_intent(deps.as_ref(), "nonexistent".to_string());

        // Verify that an error is returned
        assert!(result.is_err());
    }

    #[test]
    fn test_query_list_intents() {
        let mut deps = mock_dependencies();

        // Set up multiple intents
        let intents = vec![
            Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator1"),
                input_tokens: vec![BeepCoin {
                    token: "token1".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token1".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: 12345,
                timeout: 67890,
                tip: BeepCoin {
                    token: "token1".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            },
            Intent {
                id: "intent2".to_string(),
                creator: Addr::unchecked("creator2"),
                input_tokens: vec![BeepCoin {
                    token: "token2".to_string(),
                    amount: Uint128::new(2000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token2".to_string(),
                        is_native: true,
                        amount: Uint128::new(2000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: 22345,
                timeout: 77890,
                tip: BeepCoin {
                    token: "token2".to_string(),
                    amount: Uint128::new(20),
                    is_native: true,
                },
            },
        ];

        for intent in &intents {
            INTENTS.save(&mut deps.storage, &intent.id, intent).unwrap();
        }

        // Query the list of intents
        let res = query_list_intents(deps.as_ref(), None, Some(10)).unwrap();

        // Verify the response
        assert_eq!(res.intents.len(), 2);
        assert_eq!(res.intents[0].id, "intent1");
        assert_eq!(res.intents[1].id, "intent2");
    }

    #[test]
    fn test_query_list_intents_with_start_after() {
        let mut deps = mock_dependencies();

        // Set up multiple intents
        let intents = vec![
            Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator1"),
                input_tokens: vec![BeepCoin {
                    token: "token1".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token1".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: 12345,
                timeout: 67890,
                tip: BeepCoin {
                    token: "token1".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            },
            Intent {
                id: "intent2".to_string(),
                creator: Addr::unchecked("creator2"),
                input_tokens: vec![BeepCoin {
                    token: "token2".to_string(),
                    amount: Uint128::new(2000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token2".to_string(),
                        is_native: true,
                        amount: Uint128::new(2000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: 22345,
                timeout: 77890,
                tip: BeepCoin {
                    token: "token2".to_string(),
                    amount: Uint128::new(20),
                    is_native: true,
                },
            },
        ];

        for intent in &intents {
            INTENTS.save(&mut deps.storage, &intent.id, intent).unwrap();
        }

        // Query the list of intents with start_after
        let res = query_list_intents(deps.as_ref(), Some("intent1".to_string()), Some(10)).unwrap();

        // Verify the response
        assert_eq!(res.intents.len(), 1);
        assert_eq!(res.intents[0].id, "intent2");
    }

    #[test]
    fn test_query_list_intents_with_limit() {
        let mut deps = mock_dependencies();

        // Set up multiple intents
        let intents = vec![
            Intent {
                id: "intent1".to_string(),
                creator: Addr::unchecked("creator1"),
                input_tokens: vec![BeepCoin {
                    token: "token1".to_string(),
                    amount: Uint128::new(1000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token1".to_string(),
                        is_native: true,
                        amount: Uint128::new(1000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: 12345,
                timeout: 67890,
                tip: BeepCoin {
                    token: "token1".to_string(),
                    amount: Uint128::new(10),
                    is_native: true,
                },
            },
            Intent {
                id: "intent2".to_string(),
                creator: Addr::unchecked("creator2"),
                input_tokens: vec![BeepCoin {
                    token: "token2".to_string(),
                    amount: Uint128::new(2000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token2".to_string(),
                        is_native: true,
                        amount: Uint128::new(2000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: 22345,
                timeout: 77890,
                tip: BeepCoin {
                    token: "token2".to_string(),
                    amount: Uint128::new(20),
                    is_native: true,
                },
            },
            Intent {
                id: "intent3".to_string(),
                creator: Addr::unchecked("creator3"),
                input_tokens: vec![BeepCoin {
                    token: "token3".to_string(),
                    amount: Uint128::new(3000),
                    is_native: true,
                }],
                intent_type: IntentType::Swap {
                    output_tokens: vec![ExpectedToken {
                        token: "token3".to_string(),
                        is_native: true,
                        amount: Uint128::new(3000),
                        target_address: None,
                    }],
                },
                executor: None,
                status: IntentStatus::Active,
                created_at: 32345,
                timeout: 87890,
                tip: BeepCoin {
                    token: "token3".to_string(),
                    amount: Uint128::new(30),
                    is_native: true,
                },
            },
        ];

        for intent in &intents {
            INTENTS.save(&mut deps.storage, &intent.id, intent).unwrap();
        }

        // Query the list of intents with limit
        let res = query_list_intents(deps.as_ref(), None, Some(2)).unwrap();

        // Verify the response
        assert_eq!(res.intents.len(), 2);
        assert_eq!(res.intents[0].id, "intent1");
        assert_eq!(res.intents[1].id, "intent2");
    }

    #[test]
    fn test_query_get_user_nonce() {
        let mut deps = mock_dependencies();

        // Set up user nonce
        let address = Addr::unchecked("user1");
        USER_NONCE
            .save(&mut deps.storage, &address, &10u128)
            .unwrap();

        // Query the user nonce
        let res = query_get_user_nonce(deps.as_ref(), address.clone()).unwrap();

        // Verify the response
        assert_eq!(res.nonce, 10);
    }

    #[test]
    fn test_query_get_user_nonce_not_found() {
        let deps = mock_dependencies();

        // Attempt to query a nonce for an address that does not exist
        let address = Addr::unchecked("nonexistent_user");
        let result = query_get_user_nonce(deps.as_ref(), address);

        // Verify that an error is returned
        assert!(result.is_err());
    }
}
