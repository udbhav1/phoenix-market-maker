use crate::network_utils::{get_solana_enhanced_ws_url, get_time_ms};
use crate::phoenix_utils::{ExchangeUpdate, PhoenixFillRecv};

use super::ExchangeWebsocketHandler;
use std::env;
use std::str::FromStr;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use phoenix_sdk::sdk_client::{MarketEventDetails, SDKClient};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

pub const SUBSCRIBE_JSON: &str = r#"
{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "transactionSubscribe",
    "params": [
        {
            "vote": false,
            "failed": false,
            "accountRequired": ["{1}"]
        },
        {
            "commitment": "confirmed",
            "encoding": "base64",
            "transaction_details": "full",
            "showRewards": false,
            "maxSupportedTransactionVersion": 0
        }
    ]
}"#;

#[derive(Debug, Serialize, Deserialize)]
pub struct PhoenixFillSubscribeConfirmation {
    pub jsonrpc: String,
    pub result: i64,
    pub id: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PhoenixFillResponse {
    pub params: PhoenixFillParams,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PhoenixFillParams {
    pub result: PhoenixFillResult,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PhoenixFillResult {
    pub signature: String,
}

pub struct PhoenixFillHandler;

impl ExchangeWebsocketHandler for PhoenixFillHandler {
    type Confirmation = PhoenixFillSubscribeConfirmation;
    type Response = PhoenixFillResponse;

    fn get_name() -> String {
        "Phoenix-Fill".to_string()
    }

    fn get_ws_url() -> String {
        let network_str = env::var("RPC_NETWORK").unwrap();
        let api_key = env::var("HELIUS_API_KEY").unwrap();
        get_solana_enhanced_ws_url(&network_str, &api_key).unwrap()
    }

    fn get_num_confirmation_messages() -> usize {
        1
    }

    fn get_subscribe_json(
        _base_symbol: &str,
        _quote_symbol: &str,
        market_address: Option<String>,
    ) -> String {
        SUBSCRIBE_JSON.replace("{1}", &market_address.unwrap())
    }

    async fn parse_confirmation(s: &str) -> anyhow::Result<String> {
        let confirmation: PhoenixFillSubscribeConfirmation = serde_json::from_str(&s)
            .with_context(|| format!("Failed to deserialize Okx subscribe confirmation: {}", s))?;

        Ok(confirmation.id.to_string())
    }

    async fn parse_response(
        s: &str,
        trader_pubkey: Option<Pubkey>,
        sdk: Option<&SDKClient>,
    ) -> anyhow::Result<Vec<ExchangeUpdate>> {
        let timestamp_ms = get_time_ms()?;

        let response: PhoenixFillResponse = serde_json::from_str(&s)
            .with_context(|| format!("Failed to deserialize Phoenix response: {}", s))?;

        let signature_str = response.params.result.signature;
        // TODO profile this
        let events = sdk
            .unwrap()
            .parse_fills(&Signature::from_str(&signature_str)?)
            .await;

        let mut fills = vec![];

        let trader_pubkey = trader_pubkey.unwrap();
        for event in events {
            match event.details {
                MarketEventDetails::Fill(f) => {
                    println!("{:?}", f);
                    if f.maker == trader_pubkey || f.taker == trader_pubkey {
                        let fill = PhoenixFillRecv {
                            price: f.price_in_ticks,
                            size: f.base_lots_filled,
                            side: f.side_filled,
                            maker: f.maker.to_string(),
                            taker: f.taker.to_string(),
                            slot: event.slot,
                            timestamp: event.timestamp,
                            local_timestamp_ms: timestamp_ms,
                            signature: signature_str.clone(),
                        };
                        fills.push(ExchangeUpdate::PhoenixFill(fill));
                    }
                }
                _ => {}
            }
        }

        Ok(fills)
    }
}
