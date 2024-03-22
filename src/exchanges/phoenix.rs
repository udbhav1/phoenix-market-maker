use crate::network_utils::{get_solana_ws_url, get_time_ms};
use crate::phoenix_utils::{get_book_from_account_data, ExchangeUpdate, PhoenixRecv};

use super::ExchangeWebsocketHandler;
use std::env;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use phoenix_sdk::sdk_client::SDKClient;
use solana_sdk::pubkey::Pubkey;

const SUBSCRIBE_JSON: &str = r#"
{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "accountSubscribe",
    "params": [
        "{1}",
        {
            "encoding": "base64+zstd",
            "commitment": "confirmed"
        }
    ]
}"#;

#[derive(Serialize, Deserialize, Debug)]
pub struct PhoenixSubscribeConfirmation {
    pub jsonrpc: String,
    pub result: u64,
    pub id: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PhoenixResponse {
    pub jsonrpc: String,
    pub method: String,
    pub params: PhoenixParams,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PhoenixParams {
    pub result: PhoenixResult,
    pub subscription: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PhoenixResult {
    pub context: PhoenixContext,
    pub value: PhoenixValue,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PhoenixContext {
    pub slot: u64,
}

#[derive(Serialize, Deserialize, Debug)]
#[allow(non_snake_case)]
pub struct PhoenixValue {
    pub lamports: u64,
    pub data: Vec<String>,
    pub owner: String,
    pub executable: bool,
    pub rentEpoch: u64,
    pub space: u64,
}

pub struct PhoenixHandler;

impl ExchangeWebsocketHandler for PhoenixHandler {
    type Confirmation = PhoenixSubscribeConfirmation;
    type Response = PhoenixResponse;

    fn get_name() -> String {
        "Phoenix".to_string()
    }

    fn get_ws_url() -> String {
        let network_str = env::var("RPC_NETWORK").unwrap();
        let api_key = env::var("HELIUS_API_KEY").unwrap();
        get_solana_ws_url(&network_str, &api_key).unwrap()
    }

    fn get_subscribe_json(
        _base_symbol: &str,
        _quote_symbol: &str,
        market_address: Option<String>,
    ) -> String {
        SUBSCRIBE_JSON.replace("{1}", &market_address.unwrap())
    }

    async fn parse_confirmation(s: &str) -> anyhow::Result<String> {
        let confirmation: PhoenixSubscribeConfirmation = serde_json::from_str(&s)
            .with_context(|| format!("Failed to deserialize Okx subscribe confirmation: {}", s))?;

        Ok(confirmation.id.to_string())
    }

    async fn parse_response(
        s: &str,
        _trader_pubkey: Option<Pubkey>,
        _sdk: Option<&SDKClient>,
    ) -> anyhow::Result<Vec<ExchangeUpdate>> {
        let timestamp_ms = get_time_ms()?;

        let response: PhoenixResponse = serde_json::from_str(&s)
            .with_context(|| format!("Failed to deserialize Phoenix response: {}", s))?;
        let book = get_book_from_account_data(response.params.result.value.data)?;
        let slot = response.params.result.context.slot;

        Ok(vec![ExchangeUpdate::Phoenix(PhoenixRecv {
            book,
            timestamp_ms,
            slot,
        })])
    }
}
