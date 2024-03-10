extern crate phoenix_market_maker;
use phoenix_market_maker::utils::{parse_market_config, MasterDefinitions};

use anyhow::anyhow;
use clap::Parser;
use std::str::FromStr;

use phoenix_sdk::sdk_client::SDKClient;
use solana_cli_config::{Config, CONFIG_FILE};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
};

pub fn get_payer_keypair_from_path(path: &str) -> anyhow::Result<Keypair> {
    read_keypair_file(&*shellexpand::tilde(path)).map_err(|e| anyhow!(e.to_string()))
}

pub fn get_network(network_str: &str) -> &str {
    match network_str {
        "devnet" | "dev" | "d" => "https://api.devnet.solana.com",
        "mainnet" | "main" | "m" | "mainnet-beta" => "https://api.mainnet-beta.solana.com",
        "localnet" | "localhost" | "l" | "local" => "http://localhost:8899",
        _ => network_str,
    }
}

#[derive(Parser)]
struct Args {
    /// Optionally, use your own RPC endpoint by passing into the -u flag.
    #[clap(short, long)]
    url: Option<String>,

    // #[clap(short, long)]
    // market: Pubkey,
    //
    #[clap(short, long)]
    base_symbol: String,

    #[clap(short, long)]
    quote_symbol: String,

    /// Optionally include your keypair path. Defaults to your Solana CLI config file.
    #[clap(short, long)]
    keypair_path: Option<String>,
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let config = match CONFIG_FILE.as_ref() {
        Some(config_file) => Config::load(config_file).unwrap_or_else(|_| {
            println!("Failed to load personal config file: {}", config_file);
            Config::default()
        }),
        None => Config::default(),
    };
    let trader = get_payer_keypair_from_path(&args.keypair_path.unwrap_or(config.keypair_path))?;
    let network_url = &get_network(&args.url.unwrap_or(config.json_rpc_url)).to_string();

    let mut sdk = SDKClient::new(&trader, network_url).await?;

    let master_defs: MasterDefinitions = parse_market_config(&sdk).await?;

    let base_symbol = args.base_symbol;
    let quote_symbol = args.quote_symbol;
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

    // find the entry in markets that matches the base
    let market_address = master_defs.get_market_address(base_mint, quote_mint)?;
    let market_pubkey = Pubkey::from_str(&market_address)?;

    println!(
        "Market address for {}/{}: {}",
        base_symbol, quote_symbol, market_address
    );

    sdk.add_market(&Pubkey::from_str(&market_address)?).await?;

    let ladder = sdk.get_market_ladder(&market_pubkey, 10).await?;
    println!("{:?}", ladder);

    let orderbook = sdk.get_market_orderbook(&market_pubkey).await?;

    orderbook.print_ladder(5, 4);

    Ok(())
}
