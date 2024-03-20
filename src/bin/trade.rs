extern crate phoenix_market_maker;
use phoenix_market_maker::network_utils::{
    get_enhanced_ws_url, get_network, get_payer_keypair_from_path, get_ws_url,
    AccountSubscribeConfirmation, AccountSubscribeResponse, ConnectionStatus,
    OkxSubscribeConfirmation, OkxSubscribeResponse, TransactionSubscribeConfirmation,
    TransactionSubscribeResponse, ACCOUNT_SUBSCRIBE_JSON, OKX_SUBSCRIBE_JSON, OKX_WS_URL,
    TRANSACTION_SUBSCRIBE_JSON,
};
#[allow(unused_imports)]
use phoenix_market_maker::network_utils::{get_time_ms, get_time_s};
use phoenix_market_maker::phoenix_utils::{
    book_to_aggregated_levels, get_book_from_account_data, get_midpoint,
    get_quotes_from_width_and_lean, get_rwap, send_trade_rpc, send_trade_tpu, setup_maker,
    symbols_to_market_address, Book, Fill,
};

use anyhow::{anyhow, Context};
use clap::Parser;
use csv::Writer;
use futures::{SinkExt, StreamExt};
use std::env;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

use phoenix::program::new_order::{
    CondensedOrder, FailedMultipleLimitOrderBehavior, MultipleOrderPacket,
};
use phoenix::program::{
    create_cancel_all_order_with_free_funds_instruction, create_new_multiple_order_instruction,
};
use phoenix::state::Side;
use phoenix_sdk::sdk_client::{MarketEventDetails, SDKClient};
use solana_cli_config::{Config, CONFIG_FILE};
use solana_client::rpc_client::RpcClient;
use solana_client::tpu_client::{TpuClient, TpuClientConfig};
use solana_sdk::{
    commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Keypair, signature::Signature,
    signer::Signer,
};

fn generate_csv_columns() -> Vec<String> {
    vec![
        "timestamp".to_string(),
        "slot".to_string(),
        "side".to_string(),
        "price".to_string(),
        "size".to_string(),
        "maker".to_string(),
        "taker".to_string(),
        "signature".to_string(),
    ]
}

fn generate_csv_row(fill: &Fill) -> Vec<String> {
    let side = match fill.side {
        Side::Bid => "buy",
        Side::Ask => "sell",
    };
    vec![
        fill.timestamp.to_string(),
        fill.slot.to_string(),
        side.to_owned(),
        fill.price.to_string(),
        fill.size.to_string(),
        fill.maker.to_string(),
        fill.taker.to_string(),
        fill.signature.to_string(),
    ]
}

async fn handle_orderbook_stream(
    url: Url,
    subscribe_msg: Message,
    tx: Sender<Book>,
    status_tx: Sender<ConnectionStatus>,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to orderbook websocket");
    status_tx.send(ConnectionStatus::Connected).await?;

    let (mut write, read) = ws_stream.split();
    let mut fused_read = read.fuse();
    write.send(subscribe_msg.clone()).await?;

    let mut is_first_message = true;
    while let Some(message) = fused_read.next().await {
        match message {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    if is_first_message {
                        let confirmation: AccountSubscribeConfirmation = serde_json::from_str(&s)
                            .with_context(|| {
                            format!(
                                "Failed to deserialize account subscribe confirmation: {}",
                                s
                            )
                        })?;
                        debug!("Subscription confirmed with ID: {}", confirmation.result);
                        is_first_message = false;
                    } else {
                        let parsed: AccountSubscribeResponse = serde_json::from_str(&s)?;

                        let orderbook =
                            get_book_from_account_data(parsed.params.result.value.data)?;
                        tx.send(orderbook).await?;
                    }
                }
                _ => {}
            },
            Err(e) => return Err(anyhow!(e)),
        }
    }

    Ok(())
}

async fn orderbook_stream(
    tx: Sender<Book>,
    status_tx: Sender<ConnectionStatus>,
    ws_url: &str,
    market_address: &str,
) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(ACCOUNT_SUBSCRIBE_JSON.replace("{1}", market_address));
    let sleep_time: u64 = env::var("TRADE_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRADE_DISCONNECTS_BEFORE_EXIT")?.parse()?;
    let mut disconnects = 0;

    loop {
        if let Err(e) = handle_orderbook_stream(
            url.clone(),
            subscribe_msg.clone(),
            tx.clone(),
            status_tx.clone(),
        )
        .await
        {
            status_tx.send(ConnectionStatus::Disconnected).await?;

            warn!("Orderbook websocket disconnected with error: {:?}", e);
            disconnects += 1;
            if disconnects >= max_disconnects {
                error!("Exceeded max disconnects, exiting...");
                return Err(anyhow!(
                    "Orderbook websocket experienced {} disconnections, not trying again",
                    max_disconnects
                ));
            }

            info!("Reconnect attempt #{}...", disconnects + 1);
            sleep(Duration::from_secs(sleep_time)).await;
        }
    }
}

async fn handle_fill_stream(
    url: Url,
    subscribe_msg: Message,
    trader_pubkey: Pubkey,
    tx: Sender<Fill>,
    status_tx: Sender<ConnectionStatus>,
    sdk: &SDKClient,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to fill websocket");
    status_tx.send(ConnectionStatus::Connected).await?;

    let (mut write, read) = ws_stream.split();
    let mut fused_read = read.fuse();
    write.send(subscribe_msg.clone()).await?;

    let mut is_first_message = true;
    while let Some(message) = fused_read.next().await {
        match message {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    if is_first_message {
                        let confirmation: TransactionSubscribeConfirmation =
                            serde_json::from_str(&s).with_context(|| {
                                format!(
                                    "Failed to deserialize transaction subscribe confirmation: {}",
                                    s
                                )
                            })?;
                        debug!("Subscription confirmed with ID: {}", confirmation.result);
                        is_first_message = false;
                    } else {
                        let parsed: TransactionSubscribeResponse = serde_json::from_str(&s)?;
                        let signature_str = parsed.params.result.signature;

                        let start = Instant::now();
                        let events = sdk.parse_fills(&Signature::from_str(&signature_str)?).await;
                        let elapsed = start.elapsed();
                        debug!(
                            "phoenix_sdk::sdk_client::parse_fills took {}ms",
                            elapsed.as_millis()
                        );

                        for event in events {
                            match event.details {
                                MarketEventDetails::Fill(f) => {
                                    if f.maker == trader_pubkey || f.taker == trader_pubkey {
                                        let fill = Fill {
                                            price: f.price_in_ticks,
                                            size: f.base_lots_filled,
                                            side: f.side_filled,
                                            maker: f.maker.to_string(),
                                            taker: f.taker.to_string(),
                                            slot: event.slot,
                                            timestamp: event.timestamp,
                                            signature: signature_str.clone(),
                                        };
                                        tx.send(fill).await?;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            },
            Err(e) => return Err(anyhow!(e)),
        }
    }

    Ok(())
}

async fn fill_stream(
    tx: Sender<Fill>,
    status_tx: Sender<ConnectionStatus>,
    ws_url: &str,
    market_address: &str,
    trader_pubkey: Pubkey,
    sdk: &SDKClient,
) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(TRANSACTION_SUBSCRIBE_JSON.replace("{1}", market_address));
    let sleep_time: u64 = env::var("TRADE_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRADE_DISCONNECTS_BEFORE_EXIT")?.parse()?;
    let mut disconnects = 0;

    loop {
        if let Err(e) = handle_fill_stream(
            url.clone(),
            subscribe_msg.clone(),
            trader_pubkey,
            tx.clone(),
            status_tx.clone(),
            &sdk,
        )
        .await
        {
            status_tx.send(ConnectionStatus::Disconnected).await?;

            warn!("Fill websocket disconnected with error: {:?}", e);
            disconnects += 1;
            if disconnects >= max_disconnects {
                error!("Exceeded max disconnects, exiting...");
                return Err(anyhow!(
                    "Fill websocket experienced {} disconnections, not trying again",
                    max_disconnects
                ));
            }

            info!("Reconnect attempt #{}...", disconnects + 1);
            sleep(Duration::from_secs(sleep_time)).await;
        }
    }
}

async fn handle_okx_stream(
    url: Url,
    subscribe_msg: Message,
    tx: Sender<f64>,
    status_tx: Sender<ConnectionStatus>,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to okx websocket");
    status_tx.send(ConnectionStatus::Connected).await?;

    let (mut write, read) = ws_stream.split();
    let mut fused_read = read.fuse();
    write.send(subscribe_msg.clone()).await?;

    let mut is_first_message = true;
    while let Some(message) = fused_read.next().await {
        match message {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    if is_first_message {
                        let confirmation: OkxSubscribeConfirmation = serde_json::from_str(&s)
                            .with_context(|| {
                                format!("Failed to deserialize okx subscribe confirmation: {}", s)
                            })?;
                        debug!("Subscription confirmed with ID: {}", confirmation.connId);
                        is_first_message = false;
                    } else {
                        let parsed: OkxSubscribeResponse = serde_json::from_str(&s)?;
                        let mark_price: f64 = parsed.data[0].markPx.parse()?;
                        debug!("Okx mark price: {}", mark_price);

                        tx.send(mark_price).await?;
                    }
                }
                _ => {
                    info!("Received non-text message: {:?}", msg);
                }
            },
            Err(e) => return Err(anyhow!(e)),
        }
    }

    Ok(())
}

async fn okx_stream(
    tx: Sender<f64>,
    status_tx: Sender<ConnectionStatus>,
    ws_url: &str,
    instrument_id: &str,
) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(OKX_SUBSCRIBE_JSON.replace("{1}", instrument_id));
    let sleep_time: u64 = env::var("TRADE_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRADE_DISCONNECTS_BEFORE_EXIT")?.parse()?;
    let mut disconnects = 0;

    loop {
        if let Err(e) = handle_okx_stream(
            url.clone(),
            subscribe_msg.clone(),
            tx.clone(),
            status_tx.clone(),
        )
        .await
        {
            status_tx.send(ConnectionStatus::Disconnected).await?;

            warn!("Okx websocket disconnected with error: {:?}", e);
            disconnects += 1;
            if disconnects >= max_disconnects {
                error!("Exceeded max disconnects, exiting...");
                return Err(anyhow!(
                    "Okx websocket experienced {} disconnections, not trying again",
                    max_disconnects
                ));
            }

            info!("Reconnect attempt #{}...", disconnects + 1);
            sleep(Duration::from_secs(sleep_time)).await;
        }
    }
}

async fn trading_logic(
    mut orderbook_rx: mpsc::Receiver<Book>,
    mut fill_rx: mpsc::Receiver<Fill>,
    mut oracle_rx: mpsc::Receiver<f64>,
    mut orderbook_status_rx: mpsc::Receiver<ConnectionStatus>,
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

    let mut i = 0;
    let mut latest_orderbook = None;
    let mut base_inventory: i32 = 0;
    let mut latest_oracle_price = None;
    let mut last_trade_opportunity_oracle_price = 0.0;

    loop {
        tokio::select! {
            Some(orderbook) = orderbook_rx.recv() => {
                i += 1;
                debug!("Received orderbook #{}", i);

                // clear queue to get latest book every time we hit this branch
                // this might be necessary since the trading logic will run on every iteration of the loop
                // and each loop iteration processes one element from a random queue
                // then again maybe i wont ever get books fast enough to have more than one in the queue
                latest_orderbook = Some(orderbook);
                let mut skipped = 0;
                while let Ok(newer_orderbook) = orderbook_rx.try_recv() {
                    skipped += 1;
                    latest_orderbook = Some(newer_orderbook);
                }
                if skipped > 0 {
                    info!("Skipped {} orderbooks in queue", skipped);
                }
            },
            Some(fill) = fill_rx.recv() => {
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
                        let row = generate_csv_row(&Fill {
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
            Some(status) = orderbook_status_rx.recv() => {
                orderbook_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(status) = fill_status_rx.recv() => {
                fill_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(status) = oracle_status_rx.recv() => {
                oracle_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(mark_price) = oracle_rx.recv() => {
                latest_oracle_price = Some(mark_price);
                let mut skipped = 0;
                while let Ok(price) = oracle_rx.try_recv() {
                    latest_oracle_price = Some(price);
                    skipped += 1;
                }
                if skipped > 0 {
                    info!("Skipped {} oracle prices in queue", skipped);
                }
            },
            else => {
                info!("IN TOKIO_SELECT ELSE BRANCH");
            }
        }

        // TODO think about this
        // only send new quotes if
        // 1) haven't dropped any streams
        // 2) have an oracle price
        // 3) oracle price has changed since our last quote send
        if orderbook_connected
            && fill_connected
            && oracle_connected
            && latest_oracle_price.is_some()
            && latest_oracle_price.unwrap() != last_trade_opportunity_oracle_price
        {
            let oracle_price = latest_oracle_price.unwrap();
            last_trade_opportunity_oracle_price = oracle_price;

            if let Some(book) = &latest_orderbook {
                let (bids, asks) = book_to_aggregated_levels(&book, 2);
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
                let (bid_price, ask_price) =
                    get_quotes_from_width_and_lean(midpoint, width_bps, lean);

                let mut bid_size = base_size;
                let mut ask_size = base_size;
                if base_inventory > 0 {
                    if base_inventory.abs() as f64 / 100.0 >= dump_threshold {
                        bid_size = 0.0;
                    }
                    ask_size = base_inventory.abs() as f64 / 100.0;
                } else if base_inventory < 0 {
                    if base_inventory.abs() as f64 / 100.0 >= dump_threshold {
                        ask_size = 0.0;
                    }
                    bid_size = base_inventory.abs() as f64 / 100.0;
                }

                info!(
                    "Width: {}bps, Distance from oracle: {}bps, Quoting {:.4} @ {:.4}, {}x{}, Inventory: {}",
                    width.round(),
                    ((rwap - oracle_price) * 10000.0 / oracle_price).round(),
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
                            FailedMultipleLimitOrderBehavior::FailOnInsufficientFundsAndFailOnCross,
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
    /// RPC endpoint: devnet, mainnet, helius_devnet, helius_mainnet, etc.
    #[clap(short, long)]
    url: String,

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

    let api_key = env::var("HELIUS_API_KEY")?;
    let log_level = env::var("TRADE_LOG_LEVEL")?;

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
    let trader_address: String = trader.pubkey().to_string();
    let provided_network = args.url.clone();
    let network_url = get_network(&provided_network, &api_key)?.to_string();

    info!("Current trader address: {}", trader_address);

    let rpc_client = RpcClient::new_with_timeout_and_commitment(
        network_url.clone(),
        Duration::from_secs(60),
        CommitmentConfig::confirmed(),
    );

    // both trading_logic() and fill_stream() need this and its not cloneable
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
            let columns = generate_csv_columns();
            writer.write_record(&columns)?;
            writer.flush()?;
        }
        info!("Dumping fills to: {}", path.display());
        Some(writer)
    } else {
        info!("No CSV provided, not dumping fills");
        None
    };

    let start = Instant::now();
    rpc_client.get_latest_blockhash()?;
    let end = Instant::now();
    info!("Time to get blockhash: {:?}", end.duration_since(start));

    let ws_url = get_ws_url(&provided_network, &api_key)?;
    let enhanced_ws_url = get_enhanced_ws_url(&provided_network, &api_key)?;

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

    let (orderbook_tx, orderbook_rx) = mpsc::channel(32);
    let (orderbook_status_tx, orderbook_status_rx) = mpsc::channel(32);

    let (fill_tx, fill_rx) = mpsc::channel(32);
    let (fill_status_tx, fill_status_rx) = mpsc::channel(32);

    let (okx_tx, okx_rx) = mpsc::channel(256);
    let (okx_status_tx, okx_status_rx) = mpsc::channel(32);

    // move blocks take ownership so have to clone before
    let market_address1 = market_address.clone();
    let market_address2 = market_address.clone();
    let trader_pubkey = trader.pubkey();

    tokio::spawn(async move {
        orderbook_stream(
            orderbook_tx,
            orderbook_status_tx,
            &ws_url.clone(),
            &market_address1,
        )
        .await
        .unwrap();
    });

    tokio::spawn(async move {
        fill_stream(
            fill_tx,
            fill_status_tx,
            &enhanced_ws_url,
            &market_address2,
            trader_pubkey,
            &sdk2,
        )
        .await
        .unwrap();
    });

    let okx_instrument = format!("{}-USDT-SWAP", base_symbol.to_uppercase());
    // let okx_instrument = "JUP-USDT-SWAP".to_owned();
    tokio::spawn(async move {
        okx_stream(okx_tx, okx_status_tx, &OKX_WS_URL, &okx_instrument)
            .await
            .unwrap();
    });

    trading_logic(
        orderbook_rx,
        fill_rx,
        okx_rx,
        orderbook_status_rx,
        fill_status_rx,
        okx_status_rx,
        &sdk1,
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
