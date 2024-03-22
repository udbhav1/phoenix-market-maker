use anyhow::anyhow;
use solana_sdk::signature::{read_keypair_file, Keypair};
use std::time::{SystemTime, UNIX_EPOCH};

pub enum ConnectionStatus {
    Connected,
    Disconnected,
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

pub fn get_solana_ws_url(network_str: &str, api_key: &str) -> anyhow::Result<String> {
    match network_str {
        "helius_mainnet" => Ok(format!("wss://mainnet.helius-rpc.com/?api-key={}", api_key)),
        "helius_devnet" => Ok(format!("wss://devnet.helius-rpc.com/?api-key={}", api_key)),
        _ => Err(anyhow!("Invalid network provided: {}", network_str)),
    }
}

pub fn get_solana_enhanced_ws_url(network_str: &str, api_key: &str) -> anyhow::Result<String> {
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

pub fn get_time_s() -> anyhow::Result<u64> {
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH)?;

    Ok(since_epoch.as_secs())
}

pub fn get_time_ms() -> anyhow::Result<u64> {
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH)?;

    Ok(since_epoch.as_secs() * 1_000 + since_epoch.subsec_millis() as u64)
}
