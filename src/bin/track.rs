extern crate phoenix_market_maker;
use phoenix_market_maker::utils::{
    get_network, get_payer_keypair_from_path, get_ws_url, parse_market_config, MasterDefinitions,
};

use anyhow::anyhow;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use std::env;
use std::str::FromStr;
use std::time::Instant;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

use phoenix_sdk::sdk_client::SDKClient;
use solana_cli_config::{Config, CONFIG_FILE};
use solana_sdk::pubkey::Pubkey;

const ACCOUNT_SUBSCRIBE_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "accountSubscribe",
  "params": [
    "{}",
    {
      "encoding": "jsonParsed",
      "commitment": "confirmed"
    }
  ]
  }"#;

async fn connect_and_run(
    url: Url,
    subscribe_msg: Message,
    sdk_client: &SDKClient,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    println!("Connected to websocket");

    let (mut write, mut read) = ws_stream.split();
    write.send(subscribe_msg).await?;

    let mut i = 1;
    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    println!("Received text #{}", i);
                    i += 1
                }
                _ => {}
            },
            Err(e) => {
                println!("Websocket received error message: {:?}", e);
            }
        }
    }

    Ok(())
}

async fn run(ws_url: &str, sdk_client: &SDKClient, market_address: &str) {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(ACCOUNT_SUBSCRIBE_JSON.replace("{}", market_address));

    loop {
        if let Err(e) = connect_and_run(url.clone(), subscribe_msg.clone(), &sdk_client).await {
            println!("Websocket disconnected with error: {:?}", e);
            println!("Sleeping...");
            sleep(Duration::from_secs(5)).await;
            println!("Attempting to reconnect...");
        }
    }
}

#[derive(Parser)]
struct Args {
    /// Optionally use your own RPC endpoint by passing into the -u flag.
    #[clap(short, long)]
    url: Option<String>,

    /// Case insensitive: sol, bonk, jto, jup, etc.
    #[clap(short, long)]
    base_symbol: String,

    /// Case insensitive: usdc, sol, usdt, etc.
    #[clap(short, long)]
    quote_symbol: String,

    /// Optionally include your keypair path. Defaults to your Solana CLI config file.
    #[clap(short, long)]
    keypair_path: Option<String>,
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv()?;
    let api_key = env::var("HELIUS_API_KEY")?;

    let args = Args::parse();

    let config = match CONFIG_FILE.as_ref() {
        Some(config_file) => Config::load(config_file).unwrap_or_else(|_| {
            println!("Failed to load personal config file: {}", config_file);
            Config::default()
        }),
        None => Config::default(),
    };
    let trader = get_payer_keypair_from_path(&args.keypair_path.unwrap_or(config.keypair_path))?;
    let provided_network = args.url.clone().unwrap_or(config.json_rpc_url);
    let network_url = &get_network(&provided_network, &api_key)?.to_string();

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

    sdk.add_market(&market_pubkey).await?;

    // let mut time_taken = 0;
    // let n = 10;
    // for _ in 0..n {
    //     print!("\x1B[2J\x1B[1;1H");

    //     let start = Instant::now();
    //     let orderbook = sdk.get_market_orderbook(&market_pubkey).await?;
    //     let elapsed = start.elapsed();
    //     time_taken += elapsed.as_millis();

    //     orderbook.print_ladder(5, 4);
    //     println!("Fetch time: {}ms", elapsed.as_millis());
    // }
    // println!("Average time taken: {}ms", time_taken / n);

    run(
        &get_ws_url(&provided_network, &api_key)?,
        &sdk,
        &market_address,
    )
    .await;

    Ok(())
}
