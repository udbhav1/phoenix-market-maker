extern crate phoenix_market_maker;

use phoenix_market_maker::exchanges::{exchange_stream, ExchangeUpdate};
#[allow(unused_imports)]
use phoenix_market_maker::exchanges::{
    kraken::KrakenHandler, okx::OkxHandler, phoenix::PhoenixHandler,
    phoenix_fill::PhoenixFillHandler,
};
use phoenix_market_maker::network_utils::{get_network, ConnectionStatus};
#[allow(unused_imports)]
use phoenix_market_maker::network_utils::{get_time_ms, get_time_s};
use phoenix_market_maker::phoenix_utils::{
    generate_trade_csv_columns, generate_trade_csv_row, symbols_to_market_address, PhoenixFillRecv,
};

use clap::Parser;
use csv::Writer;
use std::env;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::str::FromStr;
use tokio::sync::mpsc;
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use phoenix::state::Side;
use phoenix_sdk::sdk_client::SDKClient;
use solana_sdk::{pubkey::Pubkey, signature::Keypair};

async fn stalk(
    mut fill_rx: mpsc::Receiver<ExchangeUpdate>,
    mut fill_status_rx: mpsc::Receiver<ConnectionStatus>,
    stalk_pubkey: &Pubkey,
    price_per_tick: f64,
    units_per_lot: f64,
    mut csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let mut fill_connected = false;

    let mut base_inventory = 0;

    loop {
        tokio::select! {
            Some(fill) = fill_rx.recv() => {
                let fill = match fill {
                    ExchangeUpdate::PhoenixFill(fill) => fill,
                    _ => panic!("Received non-Fill update in Fill channel")
                };
                let mut my_side = fill.side;
                if fill.taker == stalk_pubkey.to_string() {
                    my_side = my_side.opposite();
                }

                let verb = match my_side {
                    Side::Bid => { base_inventory += fill.size as i32;  "Bought" },
                    Side::Ask => { base_inventory -= fill.size as i32; "Sold" },
                };

                warn!("{} {} lots at price {}, Inventory: {}", verb, fill.size, (fill.price as f64) * price_per_tick, (base_inventory as f64) * units_per_lot);

                match csv_writer {
                    Some(ref mut w) => {
                        let row = generate_trade_csv_row(&PhoenixFillRecv {
                            side: my_side,
                            ..fill
                        });
                        w.write_record(&row)?;
                        w.flush()?;
                    }
                    None => {
                        debug!("No CSV writer provided, skipping writing fill");
                    }
                }
            },
            Some(status) = fill_status_rx.recv() => {
                fill_connected = matches!(status, ConnectionStatus::Connected);
            },
            else => {
                error!("in tokio::select! else branch");
            }
        }

        if !fill_connected {
            warn!("Fill stream not connected");
        }
    }
}

#[derive(Parser)]
struct Args {
    /// Address of trader to watch.
    #[clap(short, long)]
    address: String,

    /// Case insensitive: sol, bonk, jto, jup, etc.
    #[clap(short, long)]
    base_symbol: String,

    /// Case insensitive: usdc, sol, usdt, etc.
    #[clap(short, long)]
    quote_symbol: String,

    /// Optional CSV path to dump fills to.
    #[clap(short, long)]
    output: Option<String>,

    /// Optional log file path to mirror stdout.
    #[clap(short, long, default_value = "./logs/stalk.log")]
    log: String,
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv()?;

    let rpc_network = env::var("RPC_NETWORK")?;
    let api_key = env::var("HELIUS_API_KEY")?;
    let log_level = env::var("STALK_LOG_LEVEL")?;
    let sleep_sec_between_ws_connect = env::var("STALK_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;
    let disconnects_before_exit = env::var("STALK_DISCONNECTS_BEFORE_EXIT")?.parse()?;

    let args = Args::parse();

    info!("Starting stalk program");

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

    let filter_string = format!("phoenix_market_maker={0},stalk={0}", log_level);

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_appender)
        .with(EnvFilter::new(filter_string))
        .init();

    let network_url = get_network(&rpc_network, &api_key)?.to_string();

    let stalk_address = args.address;
    let stalk_pubkey = Pubkey::from_str(&stalk_address)?;
    info!("Stalking trader: {}", stalk_address);

    let mut sdk = SDKClient::new(&Keypair::new(), &network_url).await?;

    let base_symbol = args.base_symbol;
    let quote_symbol = args.quote_symbol;

    let market_address = symbols_to_market_address(&sdk, &base_symbol, &quote_symbol).await?;
    let market_pubkey = Pubkey::from_str(&market_address)?;

    info!(
        "Stalking {}/{} at address: {}",
        base_symbol, quote_symbol, market_address
    );

    sdk.add_market(&market_pubkey).await?;

    let units_per_lot = sdk.raw_base_units_per_base_lot(&market_pubkey)?;
    let price_per_tick = sdk.ticks_to_float_price(&market_pubkey, 1)?;

    info!(
        "Units per lot: {}, Price per tick: {}",
        units_per_lot, price_per_tick
    );

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
            let columns = generate_trade_csv_columns();
            writer.write_record(&columns)?;
            writer.flush()?;
        }
        info!("Dumping fills to: {}", path.display());
        Some(writer)
    } else {
        info!("No CSV provided, not dumping fills");
        None
    };

    let (fill_tx, fill_rx) = mpsc::channel(32);
    let (fill_status_tx, fill_status_rx) = mpsc::channel(16);

    tokio::spawn(async move {
        exchange_stream::<PhoenixFillHandler>(
            fill_tx,
            fill_status_tx,
            &base_symbol,
            &quote_symbol,
            Some(market_address),
            Some(stalk_pubkey),
            Some(&sdk),
            sleep_sec_between_ws_connect,
            disconnects_before_exit,
        )
        .await
        .unwrap()
    });

    stalk(
        fill_rx,
        fill_status_rx,
        &stalk_pubkey,
        price_per_tick,
        units_per_lot,
        csv_writer.as_mut(),
    )
    .await
    .unwrap();

    Ok(())
}
