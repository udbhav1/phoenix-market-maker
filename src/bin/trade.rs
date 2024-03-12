extern crate phoenix_market_maker;
use phoenix_market_maker::utils::{
    book_to_aggregated_levels, get_book_from_account_data, get_network,
    get_payer_keypair_from_path, get_time_ms, get_ws_url, symbols_to_market_address, Book,
};

use anyhow::anyhow;
use clap::Parser;
use csv::Writer;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::str::FromStr;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

use phoenix_sdk::sdk_client::SDKClient;
use solana_cli_config::{Config, CONFIG_FILE};
use solana_sdk::pubkey::Pubkey;

async fn connect_and_run(url: Url, subscribe_msg: Message) -> anyhow::Result<()> {
    Ok(())
}

async fn run(ws_url: &str, market_address: &str) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text("".to_owned());
    let sleep_time: u64 = env::var("TRACK_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRACK_DISCONNECTS_BEFORE_EXIT")?.parse()?;
    let mut disconnects = 0;

    loop {
        if let Err(e) = connect_and_run(url.clone(), subscribe_msg.clone()).await {
            info!("Websocket disconnected with error: {:?}", e);
            disconnects += 1;

            if disconnects >= max_disconnects {
                error!("Exceeded max disconnects, exiting...");
                return Err(anyhow!(
                    "Experienced {} disconnections, not trying again",
                    max_disconnects
                ));
            }

            info!("Reconnect attempt #{}...", disconnects + 1);
            sleep(Duration::from_secs(sleep_time)).await;
        }
    }
}

#[derive(Parser)]
struct Args {
    /// RPC endpoint: devnet, mainnet, helius_devnet, helius_mainnet, etc.
    #[clap(short, long)]
    url: Option<String>,

    /// Case insensitive: sol, bonk, jto, jup, etc.
    #[clap(short, long)]
    base_symbol: String,

    /// Case insensitive: usdc, sol, usdt, etc.
    #[clap(short, long)]
    quote_symbol: String,

    /// Optional log file path to mirror stdout.
    #[clap(short, long, default_value = "./logs/trade.log")]
    log: String,

    /// Optional keypair path. Defaults to Solana CLI config file.
    #[clap(short, long)]
    keypair_path: Option<String>,
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv()?;

    let api_key = env::var("HELIUS_API_KEY")?;
    let log_level = env::var("TRADE_LOG_LEVEL")?;

    let args = Args::parse();

    // log to both stdout and file
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(args.log)?;
    let file_appender = tracing_subscriber::fmt::layer()
        .with_writer(log_file)
        .with_ansi(false);
    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_appender)
        .with(EnvFilter::new(log_level))
        .init();

    let config = match CONFIG_FILE.as_ref() {
        Some(config_file) => Config::load(config_file).unwrap_or_else(|_| {
            warn!("Failed to load personal config file: {}", config_file);
            Config::default()
        }),
        None => Config::default(),
    };
    let trader = get_payer_keypair_from_path(&args.keypair_path.unwrap_or(config.keypair_path))?;
    let provided_network = args.url.clone().unwrap_or(config.json_rpc_url);
    let network_url = &get_network(&provided_network, &api_key)?.to_string();

    let mut sdk = SDKClient::new(&trader, network_url).await?;

    let base_symbol = args.base_symbol;
    let quote_symbol = args.quote_symbol;

    let market_address = symbols_to_market_address(&sdk, &base_symbol, &quote_symbol).await?;
    let market_pubkey = Pubkey::from_str(&market_address)?;

    info!(
        "Found market address for {}/{}: {}",
        base_symbol, quote_symbol, market_address
    );

    sdk.add_market(&market_pubkey).await?;

    run(&get_ws_url(&provided_network, &api_key)?, &market_address).await?;

    Ok(())
}
