use crate::phoenix_utils::OracleRecv;
use crate::{network_utils::get_time_ms, phoenix_utils::ExchangeUpdate};

use super::ExchangeWebsocketHandler;
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use phoenix_sdk::sdk_client::SDKClient;
use solana_sdk::pubkey::Pubkey;

const WS_URL: &str = "wss://wsaws.okx.com:8443/ws/v5/public";

const SUBSCRIBE_JSON: &str = r#"
{
    "op": "subscribe",
    "args": [
        {
            "channel": "tickers",
            "instId": "{1}"
        }
    ]
}"#;

#[derive(Debug, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct OkxSubscribeConfirmation {
    pub event: String,
    pub arg: OkxArg,
    pub connId: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct OkxArg {
    pub channel: String,
    pub instId: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OkxResponse {
    pub arg: OkxArg,
    pub data: Vec<OkxData>,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct OkxData {
    pub instType: String,
    pub instId: String,
    pub last: String,
    pub lastSz: String,
    pub bidPx: String,
    pub bidSz: String,
    pub askPx: String,
    pub askSz: String,
    pub open24h: String,
    pub high24h: String,
    pub low24h: String,
    pub sodUtc0: String,
    pub sodUtc8: String,
    pub volCcy24h: String,
    pub vol24h: String,
    pub ts: String,
}

pub struct OkxHandler;

impl ExchangeWebsocketHandler for OkxHandler {
    type Confirmation = OkxSubscribeConfirmation;
    type Response = OkxResponse;

    fn get_name() -> String {
        "Okx".to_string()
    }

    fn get_ws_url() -> String {
        WS_URL.to_string()
    }

    fn get_subscribe_json(
        base_symbol: &str,
        quote_symbol: &str,
        _market_address: Option<String>,
    ) -> String {
        let instrument = match quote_symbol.to_uppercase().as_str() {
            "USDC" | "USDT" => format!("{}-USDT-SWAP", base_symbol.to_uppercase()),
            _ => panic!("TODO check out non-usdc/t Okx pairs"),
        };
        SUBSCRIBE_JSON.replace("{1}", &instrument)
    }

    async fn parse_confirmation(s: &str) -> anyhow::Result<String> {
        let confirmation: OkxSubscribeConfirmation = serde_json::from_str(&s)
            .with_context(|| format!("Failed to deserialize Okx subscribe confirmation: {}", s))?;

        Ok(confirmation.connId)
    }

    async fn parse_response(
        s: &str,
        _trader_pubkey: Option<Pubkey>,
        _sdk: Option<&SDKClient>,
    ) -> anyhow::Result<Vec<ExchangeUpdate>> {
        let timestamp_ms = get_time_ms()?;
        let response: OkxResponse = serde_json::from_str(&s)
            .with_context(|| format!("Failed to deserialize Okx response: {}", s))?;
        let data = response
            .data
            .first()
            .ok_or_else(|| anyhow!("No data in Okx response"))?;

        Ok(vec![ExchangeUpdate::Oracle(OracleRecv {
            bid: data.bidPx.parse()?,
            bid_size: data.bidSz.parse()?,
            ask: data.askPx.parse()?,
            ask_size: data.askSz.parse()?,
            timestamp_ms,
        })])
    }
}
