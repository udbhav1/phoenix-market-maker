use anyhow::anyhow;
use base64::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::mem::size_of;

use phoenix::program::{dispatch_market::load_with_dispatch, MarketHeader};
use phoenix::state::markets::FIFOOrderId;
use phoenix_sdk::orderbook::Orderbook;
use phoenix_sdk::sdk_client::{MarketMetadata, PhoenixOrder, SDKClient};
use solana_sdk::signature::{read_keypair_file, Keypair};

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

pub fn get_market_metadata_from_header_bytes(
    header_bytes: &[u8],
) -> anyhow::Result<MarketMetadata> {
    bytemuck::try_from_bytes(header_bytes)
        .map_err(|_| anyhow!("Failed to deserialize market header"))
        .and_then(MarketMetadata::from_header)
}

pub fn get_book_from_data(
    data: Vec<String>,
) -> anyhow::Result<Orderbook<FIFOOrderId, PhoenixOrder>> {
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

    Ok(Orderbook::from_market(
        market,
        meta.raw_base_units_per_base_lot(),
        meta.quote_units_per_raw_base_unit_per_tick(),
    ))
}
