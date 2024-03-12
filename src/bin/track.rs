extern crate phoenix_market_maker;
use phoenix_market_maker::network_utils::{
    get_network, get_payer_keypair_from_path, get_time_ms, get_ws_url,
    AccountSubscribeConfirmation, AccountSubscribeResponse, ACCOUNT_SUBSCRIBE_JSON,
};
use phoenix_market_maker::phoenix_utils::{
    book_to_aggregated_levels, get_book_from_account_data, get_ladder, symbols_to_market_address,
    Book,
};

use anyhow::anyhow;
use clap::Parser;
use csv::Writer;
use futures::{SinkExt, StreamExt};
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

fn generate_csv_columns(levels: usize) -> Vec<String> {
    let mut columns = vec!["timestamp".to_string(), "slot".to_string()];
    for i in 1..=levels {
        columns.push(format!("BID{}", i));
        columns.push(format!("BID_SIZE{}", i));
        columns.push(format!("ASK{}", i));
        columns.push(format!("ASK_SIZE{}", i));
    }

    columns
}

fn generate_csv_row(
    timestamp: u64,
    slot: u64,
    bids: &[(f64, f64)],
    asks: &[(f64, f64)],
    levels: usize,
    precision: usize,
) -> Vec<String> {
    let mut row = vec![timestamp.to_string(), slot.to_string()];
    for i in 0..levels {
        if i < bids.len() {
            row.push(format!("{:.1$}", bids[i].0, precision));
            row.push(format!("{:.1$}", bids[i].1, precision));
        } else {
            row.push("".to_string());
            row.push("".to_string());
        }
        if i < asks.len() {
            row.push(format!("{:.1$}", asks[i].0, precision));
            row.push(format!("{:.1$}", asks[i].1, precision));
        } else {
            row.push("".to_string());
            row.push("".to_string());
        }
    }

    row
}

fn process_book(
    orderbook: &Book,
    timestamp: u64,
    slot: u64,
    csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let levels: usize = env::var("TRACK_BOOK_LEVELS")?.parse()?;
    let precision: usize = env::var("TRACK_BOOK_PRECISION")?.parse()?;

    let (bids, asks) = book_to_aggregated_levels(&orderbook, levels);

    match csv_writer {
        Some(w) => {
            let row = generate_csv_row(timestamp, slot, &bids, &asks, levels, precision);
            w.write_record(&row)?;
            w.flush()?;
        }
        None => {
            debug!("No CSV writer provided, skipping writing book");
        }
    }

    Ok(())
}

async fn connect_and_run(
    url: Url,
    subscribe_msg: Message,
    mut csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to websocket");

    let (mut write, mut read) = ws_stream.split();
    write.send(subscribe_msg).await?;

    let mut is_first_message = true;
    let mut i = 1;
    while let Some(message) = read.next().await {
        match message {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    if is_first_message {
                        let confirmation: AccountSubscribeConfirmation = serde_json::from_str(&s)?;
                        debug!("Subscription confirmed with ID: {}", confirmation.result);
                        is_first_message = false;
                    } else {
                        let recv_ts = get_time_ms()?;

                        info!("Received message #{}", i);
                        i += 1;

                        let parsed: AccountSubscribeResponse = serde_json::from_str(&s)?;

                        let orderbook =
                            get_book_from_account_data(parsed.params.result.value.data)?;

                        let slot = parsed.params.result.context.slot;

                        let precision: usize = env::var("TRACK_BOOK_PRECISION")?.parse()?;
                        let ladder_str = get_ladder(&orderbook, 5, precision);
                        debug!("Market Ladder:\n{}", ladder_str);

                        let writer_ref = csv_writer.as_mut().map(|w| &mut **w);
                        process_book(&orderbook, recv_ts, slot, writer_ref)?;
                    }
                }
                _ => {}
            },
            Err(e) => return Err(anyhow!(e)),
        }
    }

    Ok(())
}

async fn run(
    ws_url: &str,
    market_address: &str,
    mut csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(ACCOUNT_SUBSCRIBE_JSON.replace("{1}", market_address));
    let sleep_time: u64 = env::var("TRACK_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRACK_DISCONNECTS_BEFORE_EXIT")?.parse()?;
    let mut disconnects = 0;

    loop {
        // i am nowhere near good enough at rust to know if this is the right way to do this
        // but it allows me to pass csv_writer to connect_and_run in the loop
        let writer_ref = csv_writer.as_mut().map(|w| &mut **w);

        if let Err(e) = connect_and_run(url.clone(), subscribe_msg.clone(), writer_ref).await {
            warn!("Websocket disconnected with error: {:?}", e);
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

    /// Optional CSV path to dump book to.
    #[clap(short, long)]
    output: Option<String>,

    /// Optional log file path to mirror stdout.
    #[clap(short, long, default_value = "./logs/track.log")]
    log: String,

    /// Optional keypair path. Defaults to Solana CLI config file.
    #[clap(short, long)]
    keypair_path: Option<String>,
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv()?;

    let api_key = env::var("HELIUS_API_KEY")?;
    let log_level = env::var("TRACK_LOG_LEVEL")?;

    let args = Args::parse();

    info!("Starting track program");

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

    let mut csv_writer = if let Some(path) = args.output {
        let path = Path::new(&path);
        let file_exists = path.exists();

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .append(true)
            .open(&path)?;

        let mut writer = Writer::from_writer(file);

        // If the file was newly created (or is empty), write the headers
        if !file_exists || std::fs::metadata(&path)?.len() == 0 {
            let levels: usize = env::var("TRACK_BOOK_LEVELS")?.parse()?;
            let columns = generate_csv_columns(levels);
            writer.write_record(&columns)?;
        }
        info!("Dumping books to: {}", path.display());
        Some(writer)
    } else {
        info!("No CSV provided, not dumping books");
        None
    };

    run(
        &get_ws_url(&provided_network, &api_key)?,
        &market_address,
        csv_writer.as_mut(),
    )
    .await?;

    Ok(())
}
