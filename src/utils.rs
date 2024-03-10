use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use phoenix_sdk::sdk_client::SDKClient;

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
