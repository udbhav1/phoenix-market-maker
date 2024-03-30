use anyhow::anyhow;
use base64::prelude::*;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::mem::size_of;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

use phoenix::program::{dispatch_market::load_with_dispatch, MarketHeader};
use phoenix::state::markets::FIFOOrderId;
use phoenix::state::Side;
use phoenix_sdk::orderbook::{Orderbook, OrderbookKey, OrderbookValue};
use phoenix_sdk::sdk_client::{MarketMetadata, PhoenixOrder, SDKClient};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_client::tpu_client::TpuClient;
use solana_program::hash::Hash;
use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, instruction::Instruction, pubkey::Pubkey,
    signature::Keypair, signature::Signature, signer::Signer, transaction::Transaction,
};

struct BlockhashCache {
    blockhash: Hash,
    timestamp: Instant,
}

lazy_static::lazy_static! {
    static ref CACHE: Mutex<Option<BlockhashCache>> = Mutex::new(None);
}

pub type Book = Orderbook<FIFOOrderId, PhoenixOrder>;

#[derive(Debug, Clone)]
pub struct PhoenixRecv {
    pub book: Book,
    pub timestamp_ms: u64,
    pub slot: u64,
}

#[derive(Debug, Clone)]
pub struct PhoenixFillRecv {
    pub price: u64,
    pub size: u64,
    pub side: Side,
    pub maker: String,
    pub taker: String,
    pub slot: u64,
    pub timestamp: i64,
    pub local_timestamp_ms: u64,
    pub signature: String,
}

// phoenix_sdk::sdk_client::JsonMarketConfig with token array
#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigFormat {
    pub tokens: Vec<TokenInfoConfig>,
    pub markets: Vec<MarketInfoConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct TokenInfoConfig {
    pub name: String,
    pub symbol: String,
    pub mint: String,
    pub logoUri: String,
}

// phoenix_sdk::sdk_client::MarketInfoConfig with clone
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct MarketInfoConfig {
    pub market: String,
    pub baseMint: String,
    pub quoteMint: String,
}

#[derive(Debug, Default, Clone)]
pub struct TokenMap {
    mint_to_symbol: HashMap<String, String>,
    symbol_to_mint: HashMap<String, String>,
}

#[derive(Debug, Default, Clone)]
pub struct MasterDefinitions {
    pub token_lookup: TokenMap,
    pub markets: Vec<MarketInfoConfig>,
}

impl MasterDefinitions {
    pub fn get_symbol(&self, mint: &str) -> Option<&String> {
        self.token_lookup.mint_to_symbol.get(mint)
    }

    pub fn get_mint(&self, symbol: &str) -> Option<&String> {
        self.token_lookup.symbol_to_mint.get(&symbol.to_lowercase())
    }

    pub fn get_market_address(&self, base_mint: &str, quote_mint: &str) -> anyhow::Result<String> {
        let market_info = self
            .markets
            .iter()
            .find(|m| {
                m.baseMint == *base_mint && m.quoteMint == *quote_mint
                    || m.baseMint == *quote_mint && m.quoteMint == *base_mint
            })
            .ok_or_else(|| anyhow!("Failed to find market for provided base/quote"))?;

        Ok(market_info.market.clone())
    }
}

pub async fn symbols_to_market_address(
    sdk: &SDKClient,
    base_symbol: &str,
    quote_symbol: &str,
) -> anyhow::Result<String> {
    let master_defs = parse_market_config(&sdk).await?;

    let base_symbol = base_symbol;
    let quote_symbol = quote_symbol;
    let base_mint = master_defs.get_mint(&base_symbol).ok_or_else(|| {
        anyhow!(
            "Failed to find mint for symbol {} in config file",
            base_symbol
        )
    })?;
    let quote_mint = master_defs.get_mint(&quote_symbol).ok_or_else(|| {
        anyhow!(
            "Failed to find mint for symbol {} in config file",
            quote_symbol
        )
    })?;

    master_defs.get_market_address(base_mint, quote_mint)
}

pub async fn parse_market_config(sdk_client: &SDKClient) -> anyhow::Result<MasterDefinitions> {
    let config_url =
        "https://raw.githubusercontent.com/Ellipsis-Labs/phoenix-sdk/master/master_config.json";

    let genesis = sdk_client.client.get_genesis_hash().await?;

    let cluster = match genesis.to_string().as_str() {
        "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d" => "mainnet-beta",
        "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG" => "devnet",
        _ => "localhost",
    };

    let response = reqwest::get(config_url)
        .await
        .map_err(|e| anyhow!("Failed to get market config file: {}", e))?
        .json::<HashMap<String, ConfigFormat>>()
        .await
        .map_err(|e| anyhow!("Failed to parse market config file: {}", e))?;

    let market_details = response
        .get(cluster)
        .ok_or_else(|| anyhow!("Failed to find cluster {} in market config file", cluster))?;

    let mut mint_to_symbol = HashMap::new();
    let mut symbol_to_mint = HashMap::new();

    for token in market_details.tokens.iter() {
        mint_to_symbol.insert(token.mint.clone(), token.symbol.clone());
        symbol_to_mint.insert(token.symbol.clone().to_lowercase(), token.mint.clone());
    }

    let master_defs = MasterDefinitions {
        token_lookup: TokenMap {
            mint_to_symbol,
            symbol_to_mint,
        },
        markets: market_details.markets.clone(),
    };

    Ok(master_defs)
}

pub fn get_market_metadata_from_header_bytes(
    header_bytes: &[u8],
) -> anyhow::Result<MarketMetadata> {
    bytemuck::try_from_bytes(header_bytes)
        .map_err(|_| anyhow!("Failed to deserialize market header"))
        .and_then(MarketMetadata::from_header)
}

pub fn get_book_from_account_data(data: Vec<String>) -> anyhow::Result<Book> {
    // from solana-account-decoder
    // UiAccountData::Binary(blob, encoding) => match encoding {
    //     UiAccountEncoding::Base58 => bs58::decode(blob).into_vec().ok(),
    //     UiAccountEncoding::Base64 => base64::decode(blob).ok(),
    //     UiAccountEncoding::Base64Zstd => base64::decode(blob).ok().and_then(|zstd_data| {
    //         let mut data = vec![];
    //         zstd::stream::read::Decoder::new(zstd_data.as_slice())
    //             .and_then(|mut reader| reader.read_to_end(&mut data))
    //             .map(|_| data)
    //             .ok()
    //     }),
    let start = Instant::now();

    let blob = &data[0];
    let encoding = &data[1];
    let data = match &encoding[..] {
        "base58" => unimplemented!(),
        "base64" => unimplemented!(),
        "base64+zstd" => Ok(BASE64_STANDARD.decode(blob).ok().and_then(|zstd_data| {
            let mut data: Vec<u8> = vec![];
            zstd::stream::read::Decoder::new(zstd_data.as_slice())
                .and_then(|mut reader| reader.read_to_end(&mut data))
                .map(|_| data)
                .ok()
        })),
        _ => Err(anyhow!("Received unknown data encoding: {}", encoding)),
    }?
    .ok_or(anyhow!("Failed to decode data"))?;

    // from phoenix_sdk::sdk_client::SDKClient.get_market_state()
    let (header_bytes, bytes) = data.split_at(size_of::<MarketHeader>());
    let meta = get_market_metadata_from_header_bytes(header_bytes)?;
    let market = load_with_dispatch(&meta.market_size_params, bytes)
        .map_err(|_| anyhow!("Market configuration not found"))?
        .inner;

    let elapsed = start.elapsed();
    debug!("get_book_from_account_data took {:?}", elapsed);

    Ok(Orderbook::from_market(
        market,
        meta.raw_base_units_per_base_lot(),
        meta.quote_units_per_raw_base_unit_per_tick(),
    ))
}

pub fn book_to_aggregated_levels(
    orderbook: &Book,
    levels: usize,
) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
    let bids = orderbook
        .get_bids()
        .iter()
        .rev()
        .group_by(|(price, _)| price.price() * orderbook.quote_units_per_raw_base_unit_per_tick)
        .into_iter()
        .map(|(price, group)| {
            let size = group.map(|(_, size)| size.size()).sum::<f64>()
                * orderbook.raw_base_units_per_base_lot;
            (price, size)
        })
        .take(levels)
        .collect::<Vec<_>>();

    let asks = orderbook
        .get_asks()
        .iter()
        .group_by(|(price, _)| price.price() * orderbook.quote_units_per_raw_base_unit_per_tick)
        .into_iter()
        .map(|(price, group)| {
            let size = group.map(|(_, size)| size.size()).sum::<f64>()
                * orderbook.raw_base_units_per_base_lot;
            (price, size)
        })
        .take(levels)
        .collect::<Vec<_>>();

    (bids, asks)
}

// this is the second half of phoenix_sdk_core::Orderbook.print_ladder()
// but returns a string instead of using println!() since i want to use tracing
pub fn get_ladder(orderbook: &Book, levels: usize, precision: usize) -> String {
    let mut out = Vec::new();
    let width = 15;

    let (bids, asks) = book_to_aggregated_levels(orderbook, levels as usize);

    for (ask_price, ask_size) in asks.into_iter().rev() {
        let p = format!("{:.1$}", ask_price, precision);
        let s = format!("{:.1$}", ask_size, precision);
        let str = format!("{:width$} {:^width$} {:<width$}", "", p, s);
        out.push(str);
    }
    for (bid_price, bid_size) in bids {
        let p = format!("{:.1$}", bid_price, precision);
        let s = format!("{:.1$}", bid_size, precision);
        let str = format!("{:>width$} {:^width$} {:width$}", s, p, "");
        out.push(str);
    }

    out.join("\n")
}

pub fn get_latest_valid_blockhash(rpc_client: &RpcClient) -> anyhow::Result<Hash> {
    let should_cache = bool::from_str(&env::var("BLOCKHASH_CACHE_ENABLED")?)?;

    if should_cache {
        let cache_lifetime =
            Duration::from_secs(u64::from_str(&env::var("BLOCKHASH_CACHE_LIFETIME_SEC")?)?);

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

pub async fn setup_maker(
    sdk: &SDKClient,
    rpc_client: &RpcClient,
    trader_keypair: &Keypair,
    market_pubkey: &Pubkey,
) -> anyhow::Result<Option<Signature>> {
    let setup_ixs = sdk
        .get_maker_setup_instructions_for_market(market_pubkey)
        .await?;

    // let compute_price = env::var("TRADE_COMPUTE_UNIT_PRICE")?.parse()?;
    // let compute_price_ix = ComputeBudgetInstruction::set_compute_unit_price(compute_price);
    // setup_ixs.insert(0, compute_price_ix);

    debug!("Setup ixs: {:?}", setup_ixs);
    if !setup_ixs.is_empty() {
        info!("Finding valid blockhash");
        let blockhash = get_latest_valid_blockhash(rpc_client)?;
        info!("Sending setup tx");
        return Ok(Some(rpc_client.send_and_confirm_transaction(
            &Transaction::new_signed_with_payer(
                &setup_ixs,
                Some(&trader_keypair.pubkey()),
                &[trader_keypair],
                blockhash,
            ),
        )?));
    } else {
        return Ok(None);
    }
}

pub fn add_compute_budget(mut ixs: Vec<Instruction>) -> anyhow::Result<Vec<Instruction>> {
    let compute_budget = env::var("TRADE_COMPUTE_UNIT_LIMIT")?.parse()?;
    let compute_price = env::var("TRADE_COMPUTE_UNIT_PRICE")?.parse()?;

    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(compute_budget);
    let compute_price_ix = ComputeBudgetInstruction::set_compute_unit_price(compute_price);

    ixs.insert(0, compute_price_ix);
    ixs.insert(0, compute_budget_ix);

    Ok(ixs)
}

pub fn send_trade_rpc(
    rpc_client: &RpcClient,
    trader_keypair: &Keypair,
    mut ixs: Vec<Instruction>,
) -> anyhow::Result<Signature> {
    let dupes: usize = env::var("TRADE_DUPLICATE_TXS")?.parse()?;

    ixs = add_compute_budget(ixs)?;
    let blockhash = get_latest_valid_blockhash(rpc_client)?;

    let mut signature = None;
    let start = Instant::now();
    for _ in 1..=dupes {
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
) -> anyhow::Result<()> {
    let dupes: usize = env::var("TRADE_DUPLICATE_TXS")?.parse()?;

    ixs = add_compute_budget(ixs)?;
    let blockhash = get_latest_valid_blockhash(tpu_client.rpc_client())?;

    let start = Instant::now();
    for _ in 1..=dupes {
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

/// Lean is is between -1 and 1 and represents inventory to offload
/// -1 means we're unacceptably short so bid at fair value
/// 1 means we're unacceptably long so ask at fair value
/// 0 means we're neutral so quote symmetrically
pub fn get_quotes_from_width_and_lean(fair_value: f64, width_bps: f64, lean: f64) -> (f64, f64) {
    let width = fair_value * width_bps / 10_000.0;

    let bid_adjustment = (1.0 + lean) * width / 2.0;
    let ask_adjustment = (1.0 - lean) * width / 2.0;

    let bid = fair_value - bid_adjustment;
    let ask = fair_value + ask_adjustment;

    (bid, ask)
}

pub fn get_midpoint(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> f64 {
    let (bid, _) = bids.first().unwrap();
    let (ask, _) = asks.first().unwrap();

    (bid + ask) / 2.0
}

pub fn get_rwap(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> f64 {
    let (bid, bid_size) = bids.first().unwrap();
    let (ask, ask_size) = asks.first().unwrap();

    (bid * ask_size + ask * bid_size) / (bid_size + ask_size)
}

pub fn generate_trade_csv_columns() -> Vec<String> {
    vec![
        "timestamp".to_string(),
        "local_timestamp".to_string(),
        "slot".to_string(),
        "side".to_string(),
        "price".to_string(),
        "size".to_string(),
        "maker".to_string(),
        "taker".to_string(),
        "signature".to_string(),
    ]
}

pub fn generate_trade_csv_row(fill: &PhoenixFillRecv) -> Vec<String> {
    let side = match fill.side {
        Side::Bid => "buy",
        Side::Ask => "sell",
    };
    vec![
        fill.timestamp.to_string(),
        fill.local_timestamp_ms.to_string(),
        fill.slot.to_string(),
        side.to_owned(),
        fill.price.to_string(),
        fill.size.to_string(),
        fill.maker.to_string(),
        fill.taker.to_string(),
        fill.signature.to_string(),
    ]
}
