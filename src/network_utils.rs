use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use solana_sdk::signature::{read_keypair_file, Keypair};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ACCOUNT_SUBSCRIBE_JSON: &str = r#"
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

pub const TRANSACTION_SUBSCRIBE_JSON: &str = r#"
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
            "encoding": "jsonParsed",
            "transaction_details": "full",
            "showRewards": false,
            "maxSupportedTransactionVersion": 0
        }
    ]
}"#;

pub enum ConnectionStatus {
    Connected,
    Disconnected,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AccountSubscribeConfirmation {
    pub jsonrpc: String,
    pub result: u64,
    pub id: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AccountSubscribeResponse {
    pub jsonrpc: String,
    pub method: String,
    pub params: AccountSubscribeParams,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AccountSubscribeParams {
    pub result: AccountSubscribeParamsResult,
    pub subscription: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AccountSubscribeParamsResult {
    pub context: AccountSubscribeParamsResultContext,
    pub value: AccountSubscribeParamsResultValue,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AccountSubscribeParamsResultContext {
    pub slot: u64,
}

#[derive(Serialize, Deserialize, Debug)]
#[allow(non_snake_case)]
pub struct AccountSubscribeParamsResultValue {
    pub lamports: u64,
    pub data: Vec<String>,
    pub owner: String,
    pub executable: bool,
    pub rentEpoch: u64,
    pub space: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TransactionSubscribeConfirmation {
    pub jsonrpc: String,
    pub result: i64,
    pub id: i32,
}

pub fn get_payer_keypair_from_path(path: &str) -> anyhow::Result<Keypair> {
    read_keypair_file(&*shellexpand::tilde(path)).map_err(|e| anyhow!(e.to_string()))
}

pub fn get_network(network_str: &str, api_key: &str) -> anyhow::Result<String> {
    match network_str {
        "devnet" | "dev" | "d" => Ok("https://api.devnet.solana.com".to_owned()),
        "mainnet" | "main" | "m" | "mainnet-beta" => {
            Ok("https://api.mainnet-beta.solana.com".to_owned())
        }

        "helius_mainnet" => Ok(format!(
            "https://mainnet.helius-rpc.com/?api-key={}",
            api_key
        )),
        "helius_devnet" => Ok(format!(
            "https://devnet.helius-rpc.com/?api-key={}",
            api_key
        )),
        _ => Err(anyhow!("Invalid network provided: {}", network_str)),
    }
}

pub fn get_ws_url(network_str: &str, api_key: &str) -> anyhow::Result<String> {
    match network_str {
        "helius_mainnet" => Ok(format!("wss://mainnet.helius-rpc.com/?api-key={}", api_key)),
        "helius_devnet" => Ok(format!("wss://devnet.helius-rpc.com/?api-key={}", api_key)),
        _ => Err(anyhow!("Invalid network provided: {}", network_str)),
    }
}

pub fn get_enhanced_ws_url(network_str: &str, api_key: &str) -> anyhow::Result<String> {
    match network_str {
        "helius_mainnet" => Ok(format!(
            "wss://atlas-mainnet.helius-rpc.com?api-key={}",
            api_key
        )),
        "helius_devnet" => Ok(format!(
            "wss://atlas-devnet.helius-rpc.com?api-key={}",
            api_key
        )),
        _ => Err(anyhow!("Invalid network provided: {}", network_str)),
    }
}

pub fn get_time_ms() -> anyhow::Result<u64> {
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH)?;

    Ok(since_epoch.as_secs() * 1_000 + since_epoch.subsec_millis() as u64)
}
