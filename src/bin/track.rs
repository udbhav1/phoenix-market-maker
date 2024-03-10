extern crate phoenix_market_maker;
use phoenix_market_maker::utils::{
    get_market_metadata_from_header_bytes, get_network, get_payer_keypair_from_path, get_ws_url,
    parse_market_config, MasterDefinitions,
};

use anyhow::anyhow;
use base64::prelude::*;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::Read;
use std::mem::size_of;
use std::str::FromStr;
use std::time::Instant;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

use phoenix::program::{dispatch_market::load_with_dispatch, MarketHeader};
use phoenix_sdk::orderbook::Orderbook;
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
      "encoding": "base64+zstd",
      "commitment": "confirmed"
    }
  ]
  }"#;

#[derive(Serialize, Deserialize, Debug)]
struct AccountSubscribeResponse {
    jsonrpc: String,
    method: String,
    params: Params,
}

#[derive(Serialize, Deserialize, Debug)]
struct Params {
    result: Result,
    subscription: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct Result {
    context: Context,
    value: Value,
}

#[derive(Serialize, Deserialize, Debug)]
struct Context {
    slot: u64,
}

#[derive(Serialize, Deserialize, Debug)]
#[allow(non_snake_case)]
struct Value {
    lamports: u64,
    data: Vec<String>,
    owner: String,
    executable: bool,
    rentEpoch: u64,
    space: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct AccountSubscriptionConfirmation {
    jsonrpc: String,
    result: u64,
    id: u64,
}

async fn connect_and_run(url: Url, subscribe_msg: Message) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    println!("Connected to websocket");

    let (mut write, mut read) = ws_stream.split();
    write.send(subscribe_msg).await?;

    let mut is_first_message = true;
    let mut i = 1;
    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    if is_first_message {
                        let confirmation: AccountSubscriptionConfirmation =
                            serde_json::from_str(&s)?;
                        println!("Subscription confirmed with ID: {}", confirmation.result);
                        is_first_message = false;
                    } else {
                        println!("Received message #{}", i);
                        i += 1;
                        let start = Instant::now();
                        let parsed: AccountSubscribeResponse = serde_json::from_str(&s)?;

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

                        let blob = &parsed.params.result.value.data[0];
                        let encoding = &parsed.params.result.value.data[1];
                        let data = match &encoding[..] {
                            "base58" => unimplemented!(),
                            "base64" => unimplemented!(),
                            "base64+zstd" => {
                                Ok(BASE64_STANDARD.decode(blob).ok().and_then(|zstd_data| {
                                    let mut data: Vec<u8> = vec![];
                                    zstd::stream::read::Decoder::new(zstd_data.as_slice())
                                        .and_then(|mut reader| reader.read_to_end(&mut data))
                                        .map(|_| data)
                                        .ok()
                                }))
                            }
                            _ => Err(anyhow!("Received unknown data encoding: {}", encoding)),
                        }?
                        .ok_or(anyhow!("Failed to decode data"))?;

                        let elapsed = start.elapsed();
                        println!("  - Serde + decode took {}ms", elapsed.as_millis());

                        let (header_bytes, bytes) = data.split_at(size_of::<MarketHeader>());
                        let meta = get_market_metadata_from_header_bytes(header_bytes)?;
                        let market = load_with_dispatch(&meta.market_size_params, bytes)
                            .map_err(|_| anyhow!("Market configuration not found"))?
                            .inner;
                        let orderbook = Orderbook::from_market(
                            market,
                            meta.raw_base_units_per_base_lot(),
                            meta.quote_units_per_raw_base_unit_per_tick(),
                        );

                        orderbook.print_ladder(5, 4);
                    }
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

async fn run(ws_url: &str, market_address: &str) {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(ACCOUNT_SUBSCRIBE_JSON.replace("{}", market_address));

    loop {
        if let Err(e) = connect_and_run(url.clone(), subscribe_msg.clone()).await {
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

    run(&get_ws_url(&provided_network, &api_key)?, &market_address).await;

    Ok(())
}
