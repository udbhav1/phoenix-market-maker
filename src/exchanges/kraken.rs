use crate::network_utils::get_time_ms;

use super::{ExchangeUpdate, ExchangeWebsocketHandler, OracleRecv};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use phoenix_sdk::sdk_client::SDKClient;
use solana_sdk::pubkey::Pubkey;

const WS_URL: &str = "wss://ws.kraken.com/v2";

const SUBSCRIBE_JSON: &str = r#"
{
  "method": "subscribe",
  "params": {
    "channel": "ticker",
    "event_trigger": "bbo",
    "symbol": ["{1}"]
  },
  "req_id": 110110110
}"#;

#[derive(Debug, Serialize, Deserialize)]
pub struct KrakenWebsocketConfirmation {
    pub channel: String,
    pub data: Vec<KrakenWebsocketData>,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KrakenWebsocketData {
    #[serde(rename = "api_version")]
    pub api_version: String,
    #[serde(rename = "connection_id")]
    pub connection_id: u64,
    pub system: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KrakenSubscribeConfirmation {
    pub method: String,
    #[serde(rename = "req_id")]
    pub req_id: u64,
    pub result: KrakenConfirmationData,
    pub success: bool,
    #[serde(rename = "time_in")]
    pub time_in: String,
    #[serde(rename = "time_out")]
    pub time_out: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KrakenConfirmationData {
    pub channel: String,
    #[serde(rename = "event_trigger")]
    pub event_trigger: String,
    pub snapshot: bool,
    pub symbol: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KrakenHeartbeat {
    pub channel: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KrakenResponse {
    pub channel: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub data: Vec<KrakenData>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KrakenData {
    pub symbol: String,
    pub bid: f64,
    #[serde(rename = "bid_qty")]
    pub bid_qty: f64,
    pub ask: f64,
    #[serde(rename = "ask_qty")]
    pub ask_qty: f64,
    pub last: f64,
    pub volume: f64,
    pub vwap: f64,
    pub low: f64,
    pub high: f64,
    pub change: f64,
    #[serde(rename = "change_pct")]
    pub change_pct: f64,
}

pub struct KrakenHandler;

impl ExchangeWebsocketHandler for KrakenHandler {
    type Confirmation = KrakenSubscribeConfirmation;
    type Response = KrakenResponse;

    fn get_name() -> String {
        "Kraken".to_string()
    }

    fn get_ws_url() -> String {
        WS_URL.to_string()
    }

    fn get_num_confirmation_messages() -> usize {
        2
    }

    fn get_subscribe_json(
        base_symbol: &str,
        quote_symbol: &str,
        _market_address: Option<String>,
    ) -> String {
        // why does phoenix use $WIF instead of WIF
        let base_symbol = if base_symbol.starts_with("$") {
            base_symbol.strip_prefix("$").unwrap()
        } else {
            base_symbol
        };
        let instrument = match quote_symbol.to_uppercase().as_str() {
            "USDC" | "USDT" => format!("{}/USD", base_symbol.to_uppercase()),
            _ => panic!("TODO check out non-usdc/t Kraken pairs"),
        };
        SUBSCRIBE_JSON.replace("{1}", &instrument)
    }

    async fn parse_confirmation(s: &str) -> anyhow::Result<String> {
        // kraken sends a message when you first connect and before you subscribe
        let websocket_confirmation = serde_json::from_str::<KrakenWebsocketConfirmation>(&s);
        if websocket_confirmation.is_ok() {
            return Ok(websocket_confirmation
                .unwrap()
                .data
                .first()
                .unwrap()
                .connection_id
                .to_string());
        }

        let subscribe_confirmation: KrakenSubscribeConfirmation = serde_json::from_str(&s)
            .with_context(|| {
                format!("Failed to deserialize Kraken message (could be ws or subscribe confirmation): {}", s)
            })?;

        Ok(subscribe_confirmation.req_id.to_string())
    }

    async fn parse_response(
        s: &str,
        _market_address: Option<String>,
        _trader_pubkey: Option<Pubkey>,
        _sdk: Option<&SDKClient>,
    ) -> anyhow::Result<Vec<ExchangeUpdate>> {
        let timestamp_ms = get_time_ms()?;
        let response = serde_json::from_str::<KrakenResponse>(&s);

        let updates = match response {
            Ok(response) => {
                let data = response
                    .data
                    .first()
                    .ok_or_else(|| anyhow!("No data in Kraken response"))?;

                Ok(vec![ExchangeUpdate::Oracle(OracleRecv {
                    exchange: "Kraken".to_string(),
                    bid: data.bid,
                    bid_size: data.bid_qty,
                    ask: data.ask,
                    ask_size: data.ask_qty,
                    timestamp_ms,
                })])
            }
            Err(_) => {
                let _ = serde_json::from_str::<KrakenHeartbeat>(&s).with_context(|| {
                    format!(
                        "Failed to deserialize Kraken message as either heartbeat or ticker: {}",
                        s
                    )
                })?;
                Ok(vec![])
            }
        };

        updates
    }
}
