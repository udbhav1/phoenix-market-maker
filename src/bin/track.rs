extern crate phoenix_market_maker;

use phoenix_market_maker::exchanges::{exchange_stream, okx::OkxHandler, phoenix::PhoenixHandler};
use phoenix_market_maker::network_utils::{
    get_network, get_payer_keypair_from_path, ConnectionStatus,
};
#[allow(unused_imports)]
use phoenix_market_maker::network_utils::{get_time_ms, get_time_s};
use phoenix_market_maker::phoenix_utils::{
    book_to_aggregated_levels, symbols_to_market_address, Book, ExchangeUpdate,
};

use clap::Parser;
use csv::Writer;
use std::env;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::str::FromStr;
use tokio::sync::mpsc::{self};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use phoenix_sdk::sdk_client::SDKClient;
use solana_cli_config::{Config, CONFIG_FILE};
use solana_sdk::pubkey::Pubkey;

fn generate_csv_columns(levels: usize) -> Vec<String> {
    let mut columns = vec!["timestamp_ms".to_string(), "oracle_price".to_string()];
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
    oracle_price: Option<f64>,
    bids: &[(f64, f64)],
    asks: &[(f64, f64)],
    levels: usize,
    precision: usize,
) -> Vec<String> {
    let mut row = vec![
        timestamp.to_string(),
        oracle_price.map_or(String::new(), |v| v.to_string()),
    ];
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

fn process_book_and_oracle_price(
    orderbook: &Book,
    oracle_price: Option<f64>,
    timestamp: u64,
    csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let levels: usize = env::var("TRACK_BOOK_LEVELS")?.parse()?;
    let precision: usize = env::var("TRACK_BOOK_PRECISION")?.parse()?;

    let (bids, asks) = book_to_aggregated_levels(&orderbook, levels);

    match csv_writer {
        Some(w) => {
            let row = generate_csv_row(timestamp, oracle_price, &bids, &asks, levels, precision);
            w.write_record(&row)?;
            w.flush()?;
        }
        None => {
            debug!("No CSV writer provided, skipping writing book");
        }
    }

    Ok(())
}

async fn track(
    mut phoenix_rx: mpsc::Receiver<ExchangeUpdate>,
    mut oracle_rx: mpsc::Receiver<ExchangeUpdate>,
    mut phoenix_status_rx: mpsc::Receiver<ConnectionStatus>,
    mut oracle_status_rx: mpsc::Receiver<ConnectionStatus>,
    oracle_enabled: bool,
    mut csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let mut phoenix_connected = false;
    let mut oracle_connected = false;

    let mut latest_phoenix_book = None;
    let mut latest_oracle_bbo = None;

    loop {
        let writer_ref = csv_writer.as_mut().map(|w| &mut **w);

        tokio::select! {
            Some(phoenix_update) = phoenix_rx.recv() => {
                latest_phoenix_book = match phoenix_update {
                    ExchangeUpdate::Phoenix(phoenix_recv) => Some(phoenix_recv),
                    _ => panic!("Received non-Phoenix update in Phoenix channel")
                };
            },
            Some(oracle_update) = oracle_rx.recv() => {
                latest_oracle_bbo = match oracle_update {
                    ExchangeUpdate::Oracle(oracle_recv) => Some(oracle_recv),
                    _ => panic!("Received non-oracle update in oracle channel")
                };
            },
            Some(status) = phoenix_status_rx.recv() => {
                phoenix_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(status) = oracle_status_rx.recv() => {
                oracle_connected = matches!(status, ConnectionStatus::Connected);
            },
        }

        if phoenix_connected {
            if oracle_enabled {
                if let (Some(phoenix_recv), Some(oracle_recv)) =
                    (&latest_phoenix_book, &latest_oracle_bbo)
                {
                    process_book_and_oracle_price(
                        &phoenix_recv.book,
                        Some(oracle_recv.midpoint()),
                        std::cmp::max(phoenix_recv.timestamp_ms, oracle_recv.timestamp_ms),
                        writer_ref,
                    )?;
                }
            } else {
                if let Some(book_recv) = &latest_phoenix_book {
                    process_book_and_oracle_price(
                        &book_recv.book,
                        None,
                        book_recv.timestamp_ms,
                        writer_ref,
                    )?;
                }
            }
        } else {
            if !phoenix_connected {
                warn!("Orderbook stream not connected");
            }
            if oracle_enabled && !oracle_connected {
                warn!("Oracle stream not connected");
            }
        }
    }
}

#[derive(Parser)]
struct Args {
    /// Case insensitive: sol, bonk, jto, jup, etc.
    #[clap(short, long)]
    base_symbol: String,

    /// Case insensitive: usdc, sol, usdt, etc.
    #[clap(short, long)]
    quote_symbol: String,

    /// Track oracle price as well.
    #[clap(long)]
    oracle: bool,

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

    let rpc_network = env::var("RPC_NETWORK")?;
    let api_key = env::var("HELIUS_API_KEY")?;
    let log_level = env::var("TRACK_LOG_LEVEL")?;
    let sleep_sec_between_ws_connect = env::var("TRACK_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;
    let disconnects_before_exit = env::var("TRACK_DISCONNECTS_BEFORE_EXIT")?.parse()?;

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
    let network_url = &get_network(&rpc_network, &api_key)?.to_string();

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
            writer.flush()?;
        }
        info!("Dumping books to: {}", path.display());
        Some(writer)
    } else {
        info!("No CSV provided, not dumping books");
        None
    };

    let (phoenix_tx, phoenix_rx) = mpsc::channel(256);
    let (phoenix_status_tx, phoenix_status_rx) = mpsc::channel(16);

    let (oracle_tx, oracle_rx) = mpsc::channel(512);
    let (oracle_status_tx, oracle_status_rx) = mpsc::channel(16);

    let base_symbol1 = base_symbol.clone();
    let quote_symbol1 = quote_symbol.clone();
    let market_address1 = market_address.clone();
    let base_symbol2 = base_symbol.clone();
    let quote_symbol2 = quote_symbol.clone();

    tokio::spawn(async move {
        exchange_stream::<PhoenixHandler>(
            phoenix_tx,
            phoenix_status_tx,
            &base_symbol1,
            &quote_symbol1,
            Some(market_address1),
            None,
            None,
            sleep_sec_between_ws_connect,
            disconnects_before_exit,
        )
        .await
        .unwrap()
    });

    if args.oracle {
        tokio::spawn(async move {
            exchange_stream::<OkxHandler>(
                oracle_tx,
                oracle_status_tx,
                &base_symbol2,
                &quote_symbol2,
                None,
                None,
                None,
                sleep_sec_between_ws_connect,
                disconnects_before_exit,
            )
            .await
            .unwrap();
        });
    }

    track(
        phoenix_rx,
        oracle_rx,
        phoenix_status_rx,
        oracle_status_rx,
        args.oracle,
        csv_writer.as_mut(),
    )
    .await
    .unwrap();

    Ok(())
}
