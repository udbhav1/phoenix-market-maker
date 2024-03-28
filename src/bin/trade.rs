extern crate phoenix_market_maker;

use phoenix_market_maker::exchanges::{exchange_stream, ExchangeUpdate};
#[allow(unused_imports)]
use phoenix_market_maker::exchanges::{
    kraken::KrakenHandler, okx::OkxHandler, phoenix::PhoenixHandler,
    phoenix_fill::PhoenixFillHandler,
};
use phoenix_market_maker::network_utils::{
    get_network, get_payer_keypair_from_path, get_solana_ws_url, ConnectionStatus,
};
#[allow(unused_imports)]
use phoenix_market_maker::network_utils::{get_time_ms, get_time_s};
use phoenix_market_maker::phoenix_utils::{
    book_to_aggregated_levels, generate_trade_csv_columns, generate_trade_csv_row, get_midpoint,
    get_quotes_from_width_and_lean, get_rwap, send_trade_rpc, send_trade_tpu, setup_maker,
    symbols_to_market_address, PhoenixFillRecv,
};

use clap::Parser;
use csv::Writer;
use std::env;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::Duration;
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use phoenix::program::new_order::{
    CondensedOrder, FailedMultipleLimitOrderBehavior, MultipleOrderPacket,
};
use phoenix::program::{
    create_cancel_all_order_with_free_funds_instruction, create_new_multiple_order_instruction,
};
use phoenix::state::Side;
use phoenix_sdk::sdk_client::SDKClient;
use solana_cli_config::{Config, CONFIG_FILE};
use solana_client::rpc_client::RpcClient;
use solana_client::tpu_client::{TpuClient, TpuClientConfig};
use solana_sdk::{
    commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Keypair, signer::Signer,
};

async fn trading_logic(
    mut phoenix_rx: mpsc::Receiver<ExchangeUpdate>,
    mut fill_rx: mpsc::Receiver<ExchangeUpdate>,
    mut oracle_rx: mpsc::Receiver<ExchangeUpdate>,
    mut phoenix_status_rx: mpsc::Receiver<ConnectionStatus>,
    mut fill_status_rx: mpsc::Receiver<ConnectionStatus>,
    mut oracle_status_rx: mpsc::Receiver<ConnectionStatus>,
    sdk: &SDKClient,
    rpc_client: &RpcClient,
    tpu_client: &TpuClient,
    trader_keypair: &Keypair,
    market_pubkey: &Pubkey,
    mut csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let mut orderbook_connected = false;
    let mut fill_connected = false;
    let mut oracle_connected = false;

    let market_metadata = &sdk.get_market_metadata(&market_pubkey).await?;
    let cancel_ix = create_cancel_all_order_with_free_funds_instruction(
        &market_pubkey,
        &trader_keypair.pubkey(),
    );
    let should_trade = bool::from_str(&env::var("TRADE_QUOTES_ENABLED")?)?;
    let use_tpu = bool::from_str(&env::var("TRADE_USE_TPU")?)?;
    let width_bps: f64 = env::var("TRADE_WIDTH_BPS")?.parse()?;
    let base_size: f64 = env::var("TRADE_BASE_SIZE")?.parse()?;
    let dump_threshold: f64 = env::var("TRADE_DUMP_THRESHOLD")?.parse()?;
    let max_lean: f64 = env::var("TRADE_MAX_LEAN")?.parse()?;
    let time_in_force: u64 = env::var("TRADE_TIME_IN_FORCE")?.parse()?;

    info!("Quotes enabled: {}", should_trade);

    let mut base_inventory: i32 = 0;
    let mut latest_phoenix_book = None;
    let mut latest_oracle_bbo = None;

    loop {
        tokio::select! {
            Some(phoenix_update) = phoenix_rx.recv() => {
                let mut recv_orderbook = Some(phoenix_update);
                let mut skipped = 0;
                while let Ok(newer_orderbook) = phoenix_rx.try_recv() {
                    skipped += 1;
                    recv_orderbook = Some(newer_orderbook);
                }
                if skipped > 0 {
                    info!("Skipped {} orderbooks in queue", skipped);
                }

                latest_phoenix_book = match recv_orderbook.unwrap() {
                    ExchangeUpdate::Phoenix(book) => Some(book),
                    _ => panic!("Received non-Phoenix book update in Phoenix channel")
                };
            },
            Some(fill) = fill_rx.recv() => {
                let fill = match fill {
                    ExchangeUpdate::PhoenixFill(fill) => fill,
                    _ => panic!("Received non-Fill update in Fill channel")
                };
                let mut my_side = fill.side;
                if fill.taker == trader_keypair.pubkey().to_string() {
                    my_side = my_side.opposite();
                }

                let verb = match my_side {
                    Side::Bid => { base_inventory += fill.size as i32;  "Bought" },
                    Side::Ask => { base_inventory -= fill.size as i32; "Sold" },
                };

                warn!("{} {} lots at price {}, Inventory: {}", verb, fill.size, (fill.price as f64) / 10000.0, (base_inventory as f64) / 100.0);

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
            Some(oracle_update) = oracle_rx.recv() => {
                let mut recv_oracle = Some(oracle_update);
                let mut skipped = 0;
                while let Ok(price) = oracle_rx.try_recv() {
                    skipped += 1;
                    recv_oracle = Some(price);
                }
                if skipped > 0 {
                    info!("Skipped {} oracle prices in queue", skipped);
                }

                latest_oracle_bbo = match recv_oracle.unwrap() {
                    ExchangeUpdate::Oracle(bbo) => Some(bbo),
                    _ => panic!("Received non-oracle book update in oracle channel")
                };
            },
            Some(status) = phoenix_status_rx.recv() => {
                orderbook_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(status) = fill_status_rx.recv() => {
                fill_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(status) = oracle_status_rx.recv() => {
                oracle_connected = matches!(status, ConnectionStatus::Connected);
            },
            else => {
                error!("in tokio::select! else branch");
            }
        }

        // TODO think about this
        // only send new quotes if
        // 1) haven't dropped any streams
        // 2) have an oracle price
        // 3) oracle price has changed since our last quote send
        if orderbook_connected && fill_connected && oracle_connected && latest_oracle_bbo.is_some()
        // && latest_oracle_price.unwrap() != last_trade_opportunity_oracle_price
        {
            let oracle_bbo = latest_oracle_bbo.clone().unwrap();
            let oracle_midpoint = oracle_bbo.midpoint();

            if let Some(phoenix_recv) = &latest_phoenix_book {
                let (bids, asks) = book_to_aggregated_levels(&phoenix_recv.book, 2);
                debug!("bids: {:?}", bids);
                debug!("asks: {:?}", asks);

                let midpoint = get_midpoint(&bids, &asks);
                let rwap = get_rwap(&bids, &asks);
                let best_bid = bids.first().unwrap().0;
                let best_ask = asks.first().unwrap().0;
                let width = (best_ask - best_bid) * 10000.0 / midpoint;

                // linearly interpolate up to max_lean
                let inventory_ratio = (base_inventory as f64) / 100.0 / dump_threshold;
                let inventory_ratio = inventory_ratio.clamp(-1.0, 1.0);
                let lean = max_lean * inventory_ratio;
                // let (bid_price, ask_price) =
                //     get_quotes_from_width_and_lean(midpoint, width_bps, lean);
                let (bid_price, ask_price) =
                    get_quotes_from_width_and_lean(midpoint, width - 2.0, lean);

                let mut bid_size = base_size;
                let mut ask_size = base_size;
                if base_inventory > 0 {
                    if base_inventory.abs() as f64 / 100.0 >= dump_threshold {
                        bid_size = 0.0;
                        ask_size = base_inventory.abs() as f64 / 100.0;
                    }
                } else if base_inventory < 0 {
                    if base_inventory.abs() as f64 / 100.0 >= dump_threshold {
                        ask_size = 0.0;
                        bid_size = base_inventory.abs() as f64 / 100.0;
                    }
                }

                info!(
                    "Width: {}bps, Distance from oracle: {}bps, Quoting {:.4} @ {:.4}, {}x{}, Inventory: {}",
                    width.round(),
                    ((rwap - oracle_midpoint) * 10000.0 / oracle_midpoint).round(),
                    bid_price,
                    ask_price,
                    bid_size,
                    ask_size,
                    (base_inventory as f64) / 100.0,
                );

                let mut ixs = vec![cancel_ix.clone()];

                let expiry_ts = get_time_s()? + time_in_force;

                let mut bids = Vec::new();
                let mut asks = Vec::new();

                if bid_size > 0.0 {
                    bids.push(CondensedOrder {
                        price_in_ticks: sdk
                            .float_price_to_ticks_rounded_down(&market_pubkey, bid_price)?,
                        size_in_base_lots: sdk
                            .raw_base_units_to_base_lots_rounded_down(&market_pubkey, bid_size)?,
                        last_valid_slot: None,
                        last_valid_unix_timestamp_in_seconds: Some(expiry_ts),
                    });
                }

                if ask_size > 0.0 {
                    asks.push(CondensedOrder {
                        price_in_ticks: sdk
                            .float_price_to_ticks_rounded_down(&market_pubkey, ask_price)?,
                        size_in_base_lots: sdk
                            .raw_base_units_to_base_lots_rounded_down(&market_pubkey, ask_size)?,
                        last_valid_slot: None,
                        last_valid_unix_timestamp_in_seconds: Some(expiry_ts),
                    });
                }

                let place_multiple_orders = create_new_multiple_order_instruction(
                    &market_pubkey,
                    &trader_keypair.pubkey(),
                    &market_metadata.base_mint,
                    &market_metadata.quote_mint,
                    &MultipleOrderPacket {
                        bids,
                        asks,
                        client_order_id: Some(110110110),
                        failed_multiple_limit_order_behavior:
                            FailedMultipleLimitOrderBehavior::FailOnInsufficientFundsAndAmendOnCross,
                    },
                );
                ixs.push(place_multiple_orders);

                if should_trade {
                    if use_tpu {
                        send_trade_tpu(&tpu_client, &trader_keypair, ixs)?;
                    } else {
                        let signature = send_trade_rpc(&rpc_client, &trader_keypair, ixs)?;
                        debug!("Trade Signature: {}", signature);
                    }
                }
            }
        } else {
            if !orderbook_connected {
                warn!("Orderbook stream not connected");
            }
            if !fill_connected {
                warn!("Fill stream not connected");
            }
            if !oracle_connected {
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

    /// Optional CSV path to dump fills to.
    #[clap(short, long)]
    output: Option<String>,

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

    let rpc_network = env::var("RPC_NETWORK")?;
    let api_key = env::var("HELIUS_API_KEY")?;
    let log_level = env::var("TRADE_LOG_LEVEL")?;
    let sleep_sec_between_ws_connect = env::var("TRADE_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;
    let disconnects_before_exit = env::var("TRADE_DISCONNECTS_BEFORE_EXIT")?.parse()?;

    let args = Args::parse();

    info!("Starting trade program");

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

    let filter_string = format!(
        "phoenix_market_maker={0},trade={0},solana_client=error",
        log_level
    );

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_appender)
        .with(EnvFilter::new(filter_string))
        .init();

    let config = match CONFIG_FILE.as_ref() {
        Some(config_file) => Config::load(config_file).unwrap_or_else(|_| {
            warn!("Failed to load personal config file: {}", config_file);
            Config::default()
        }),
        None => Config::default(),
    };
    let trader = get_payer_keypair_from_path(&args.keypair_path.unwrap_or(config.keypair_path))?;
    let trader_address: String = trader.pubkey().to_string();
    let network_url = get_network(&rpc_network, &api_key)?.to_string();

    info!("Current trader address: {}", trader_address);

    let rpc_client = RpcClient::new_with_timeout_and_commitment(
        network_url.clone(),
        Duration::from_secs(30),
        CommitmentConfig::confirmed(),
    );

    // both trading_logic() and the fill exchange stream need this and its not cloneable
    let mut sdk1 = SDKClient::new(&trader, &network_url).await?;
    let mut sdk2 = SDKClient::new(&trader, &network_url).await?;

    let base_symbol = args.base_symbol;
    let quote_symbol = args.quote_symbol;

    let market_address = symbols_to_market_address(&sdk1, &base_symbol, &quote_symbol).await?;
    let market_pubkey = Pubkey::from_str(&market_address)?;

    info!(
        "Trading on {}/{} at address: {}",
        base_symbol, quote_symbol, market_address
    );

    sdk1.add_market(&market_pubkey).await?;
    sdk2.add_market(&market_pubkey).await?;

    info!(
        "Base units per lot: {}",
        sdk1.raw_base_units_per_base_lot(&market_pubkey)?,
    );

    let start = Instant::now();
    rpc_client.get_latest_blockhash()?;
    let end = Instant::now();
    info!("Time to get blockhash: {:?}", end.duration_since(start));

    match setup_maker(&sdk1, &rpc_client, &trader, &market_pubkey).await? {
        Some(sig) => {
            info!("Setup tx signature: {:?}", sig);
        }
        None => {
            info!("No setup tx required");
        }
    }

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

    let ws_url = get_solana_ws_url(&rpc_network, &api_key)?;

    let rpc_for_tpu = RpcClient::new_with_timeout_and_commitment(
        network_url.clone(),
        Duration::from_secs(5),
        CommitmentConfig::confirmed(),
    );
    let tpu_client = TpuClient::new(
        Arc::new(rpc_for_tpu),
        &ws_url,
        TpuClientConfig { fanout_slots: 12 },
    )?;

    let (phoenix_tx, phoenix_rx) = mpsc::channel(256);
    let (phoenix_status_tx, phoenix_status_rx) = mpsc::channel(16);

    let (fill_tx, fill_rx) = mpsc::channel(32);
    let (fill_status_tx, fill_status_rx) = mpsc::channel(16);

    let (oracle_tx, oracle_rx) = mpsc::channel(256);
    let (oracle_status_tx, oracle_status_rx) = mpsc::channel(16);

    // move blocks take ownership so have to clone before
    let trader_pubkey = trader.pubkey();
    let base_symbol1 = base_symbol.clone();
    let quote_symbol1 = quote_symbol.clone();
    let market_address1 = market_address.clone();
    let base_symbol2 = base_symbol.clone();
    let quote_symbol2 = quote_symbol.clone();
    let market_address2 = market_address.clone();
    let base_symbol3 = base_symbol.clone();
    let quote_symbol3 = quote_symbol.clone();

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

    tokio::spawn(async move {
        exchange_stream::<PhoenixFillHandler>(
            fill_tx,
            fill_status_tx,
            &base_symbol2,
            &quote_symbol2,
            Some(market_address2),
            Some(trader_pubkey),
            Some(&sdk1),
            sleep_sec_between_ws_connect,
            disconnects_before_exit,
        )
        .await
        .unwrap()
    });

    tokio::spawn(async move {
        exchange_stream::<KrakenHandler>(
            oracle_tx,
            oracle_status_tx,
            &base_symbol3,
            &quote_symbol3,
            None,
            None,
            None,
            sleep_sec_between_ws_connect,
            disconnects_before_exit,
        )
        .await
        .unwrap();
    });

    trading_logic(
        phoenix_rx,
        fill_rx,
        oracle_rx,
        phoenix_status_rx,
        fill_status_rx,
        oracle_status_rx,
        &sdk2,
        &rpc_client,
        &tpu_client,
        &trader,
        &market_pubkey,
        csv_writer.as_mut(),
    )
    .await
    .unwrap();

    Ok(())
}
