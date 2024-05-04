use anyhow::anyhow;
use std::env;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

use solana_client::{
    rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig, tpu_client::TpuClient,
};
use solana_program::hash::Hash;
use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, instruction::Instruction, signature::Signature,
    signer::Signer, transaction::Transaction,
};

pub enum ConnectionStatus {
    Connected,
    Disconnected,
}

struct BlockhashCache {
    blockhash: Hash,
    timestamp: Instant,
}

lazy_static::lazy_static! {
    static ref CACHE: Mutex<Option<BlockhashCache>> = Mutex::new(None);
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

pub fn get_latest_valid_blockhash(rpc_client: &RpcClient) -> anyhow::Result<Hash> {
    let should_cache = bool::from_str(&env::var("BLOCKHASH_CACHE_ENABLED")?)?;
    let cache_lifetime =
        Duration::from_secs(u64::from_str(&env::var("BLOCKHASH_CACHE_LIFETIME_SEC")?)?);

    if should_cache {
        if let Ok(cache) = CACHE.try_lock() {
            if let Some(cached) = cache.as_ref() {
                if cached.timestamp.elapsed() < cache_lifetime {
                    debug!("Using cached blockhash");
                    return Ok(cached.blockhash);
                }
            }
        }
    }

    debug!("Fetching new blockhash");
    let mut blockhash;
    loop {
        blockhash = rpc_client.get_latest_blockhash()?;
        if rpc_client.is_blockhash_valid(&blockhash, CommitmentConfig::confirmed())? {
            break;
        }
    }

    if should_cache {
        if let Ok(mut cache) = CACHE.try_lock() {
            debug!("Caching blockhash");
            *cache = Some(BlockhashCache {
                blockhash,
                timestamp: Instant::now(),
            });
        }
    }

    Ok(blockhash)
}

pub fn add_compute_budget(
    mut ixs: Vec<Instruction>,
    compute_unit_limit: u32,
    compute_unit_price: u64,
) -> anyhow::Result<Vec<Instruction>> {
    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit);
    let compute_price_ix = ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price);

    ixs.insert(0, compute_price_ix);
    ixs.insert(0, compute_budget_ix);

    Ok(ixs)
}

pub fn send_trade_rpc(
    rpc_client: &RpcClient,
    trader_keypair: &Keypair,
    mut ixs: Vec<Instruction>,
    compute_unit_limit: u32,
    compute_unit_price: u64,
    duplicate_txs: usize,
) -> anyhow::Result<Signature> {
    ixs = add_compute_budget(ixs, compute_unit_limit, compute_unit_price)?;
    let blockhash = get_latest_valid_blockhash(rpc_client)?;

    let mut signature = None;
    let start = Instant::now();
    for _ in 1..=duplicate_txs {
        signature = Some(rpc_client.send_transaction_with_config(
            &Transaction::new_signed_with_payer(
                &ixs,
                Some(&trader_keypair.pubkey()),
                &[trader_keypair],
                blockhash,
            ),
            RpcSendTransactionConfig {
                skip_preflight: true,
                preflight_commitment: Some(CommitmentLevel::Confirmed),
                encoding: None,
                max_retries: Some(0),
                min_context_slot: None,
            },
        )?);
    }
    let elapsed = start.elapsed();
    debug!("RPC trade sent in {}ms", elapsed.as_millis());

    Ok(signature.ok_or(anyhow!("Trade signature not received"))?)
}

pub fn send_trade_tpu(
    tpu_client: &TpuClient,
    trader_keypair: &Keypair,
    mut ixs: Vec<Instruction>,
    compute_unit_limit: u32,
    compute_unit_price: u64,
    duplicate_txs: usize,
) -> anyhow::Result<()> {
    ixs = add_compute_budget(ixs, compute_unit_limit, compute_unit_price)?;
    let blockhash = get_latest_valid_blockhash(tpu_client.rpc_client())?;

    let start = Instant::now();
    for _ in 1..=duplicate_txs {
        tpu_client.try_send_transaction(&Transaction::new_signed_with_payer(
            &ixs,
            Some(&trader_keypair.pubkey()),
            &[trader_keypair],
            blockhash,
        ))?
    }
    let elapsed = start.elapsed();
    debug!("TPU trade sent in {}ms", elapsed.as_millis());

    Ok(())
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
