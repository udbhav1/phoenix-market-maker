extern crate phoenix_market_maker;
use phoenix_market_maker::network_utils::{
    get_network, get_payer_keypair_from_path, get_time_ms, get_ws_url,
    AccountSubscribeConfirmation, AccountSubscribeResponse, ConnectionStatus,
    OkxSubscribeConfirmation, OkxSubscribeResponse, ACCOUNT_SUBSCRIBE_JSON, OKX_SUBSCRIBE_JSON,
    OKX_WS_URL,
};
use phoenix_market_maker::phoenix_utils::{
    book_to_aggregated_levels, get_book_from_account_data, get_ladder, symbols_to_market_address,
    Book, BookRecv, OracleRecv,
};

use anyhow::{anyhow, Context};
use clap::Parser;
use csv::Writer;
use futures::{SinkExt, StreamExt};
use std::env;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::str::FromStr;
use tokio::sync::mpsc::{self, Sender};
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

async fn handle_orderbook_stream(
    url: Url,
    subscribe_msg: Message,
    tx: Sender<BookRecv>,
    status_tx: Sender<ConnectionStatus>,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to orderbook websocket");
    status_tx.send(ConnectionStatus::Connected).await?;

    let (mut write, read) = ws_stream.split();
    let mut fused_read = read.fuse();
    write.send(subscribe_msg.clone()).await?;

    let mut is_first_message = true;
    let mut i = 1;
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
                        let recv_ts = get_time_ms()?;
                        info!("Received book #{}", i);
                        i += 1;

                        let parsed: AccountSubscribeResponse = serde_json::from_str(&s)?;

                        let orderbook =
                            get_book_from_account_data(parsed.params.result.value.data)?;

                        let slot = parsed.params.result.context.slot;

                        let precision: usize = env::var("TRACK_BOOK_PRECISION")?.parse()?;
                        let ladder_str = get_ladder(&orderbook, 5, precision);
                        debug!("Market Ladder:\n{}", ladder_str);

                        tx.send(BookRecv {
                            book: orderbook,
                            timestamp_ms: recv_ts,
                            slot,
                        })
                        .await?;
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
    tx: Sender<BookRecv>,
    status_tx: Sender<ConnectionStatus>,
    ws_url: &str,
    market_address: &str,
) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(ACCOUNT_SUBSCRIBE_JSON.replace("{1}", market_address));
    let sleep_time: u64 = env::var("TRACK_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRACK_DISCONNECTS_BEFORE_EXIT")?.parse()?;
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

async fn handle_okx_stream(
    url: Url,
    subscribe_msg: Message,
    tx: Sender<OracleRecv>,
    status_tx: Sender<ConnectionStatus>,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to okx websocket");
    status_tx.send(ConnectionStatus::Connected).await?;

    let (mut write, read) = ws_stream.split();
    let mut fused_read = read.fuse();
    write.send(subscribe_msg.clone()).await?;

    let mut is_first_message = true;
    let mut i = 1;
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
                        let recv_ts = get_time_ms()?;
                        info!("Received okx price #{}", i);
                        i += 1;

                        let parsed: OkxSubscribeResponse = serde_json::from_str(&s)?;
                        let mark_price: f64 = parsed.data[0].markPx.parse()?;
                        debug!("Okx mark price: {}", mark_price);

                        tx.send(OracleRecv {
                            price: mark_price,
                            timestamp_ms: recv_ts,
                        })
                        .await?;
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
    tx: Sender<OracleRecv>,
    status_tx: Sender<ConnectionStatus>,
    ws_url: &str,
    instrument_id: &str,
) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(OKX_SUBSCRIBE_JSON.replace("{1}", instrument_id));
    let sleep_time: u64 = env::var("TRACK_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRACK_DISCONNECTS_BEFORE_EXIT")?.parse()?;
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

async fn track(
    mut orderbook_rx: mpsc::Receiver<BookRecv>,
    mut oracle_rx: mpsc::Receiver<OracleRecv>,
    mut orderbook_status_rx: mpsc::Receiver<ConnectionStatus>,
    mut oracle_status_rx: mpsc::Receiver<ConnectionStatus>,
    oracle_enabled: bool,
    mut csv_writer: Option<&mut Writer<File>>,
) -> anyhow::Result<()> {
    let mut orderbook_connected = false;
    let mut oracle_connected = false;

    let mut latest_orderbook = None;
    let mut latest_oracle_price = None;

    loop {
        let writer_ref = csv_writer.as_mut().map(|w| &mut **w);

        tokio::select! {
            Some(orderbook) = orderbook_rx.recv() => {
                latest_orderbook = Some(orderbook);
            },
            Some(mark_price) = oracle_rx.recv() => {
                latest_oracle_price = Some(mark_price);
            },
            Some(status) = orderbook_status_rx.recv() => {
                orderbook_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(status) = oracle_status_rx.recv() => {
                oracle_connected = matches!(status, ConnectionStatus::Connected);
            },
        }

        if orderbook_connected {
            if oracle_enabled {
                if let (Some(book_recv), Some(oracle_recv)) =
                    (&latest_orderbook, &latest_oracle_price)
                {
                    process_book_and_oracle_price(
                        &book_recv.book,
                        Some(oracle_recv.price),
                        std::cmp::max(book_recv.timestamp_ms, oracle_recv.timestamp_ms),
                        writer_ref,
                    )?;
                }
            } else {
                if let Some(book_recv) = &latest_orderbook {
                    process_book_and_oracle_price(
                        &book_recv.book,
                        None,
                        book_recv.timestamp_ms,
                        writer_ref,
                    )?;
                }
            }
        } else {
            if !orderbook_connected {
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
    /// RPC endpoint: devnet, mainnet, helius_devnet, helius_mainnet, etc.
    #[clap(short, long)]
    url: Option<String>,

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
            writer.flush()?;
        }
        info!("Dumping books to: {}", path.display());
        Some(writer)
    } else {
        info!("No CSV provided, not dumping books");
        None
    };

    let (orderbook_tx, orderbook_rx) = mpsc::channel(32);
    let (orderbook_status_tx, orderbook_status_rx) = mpsc::channel(32);

    let (okx_tx, okx_rx) = mpsc::channel(256);
    let (okx_status_tx, okx_status_rx) = mpsc::channel(32);

    let ws_url = get_ws_url(&provided_network, &api_key)?;

    tokio::spawn(async move {
        orderbook_stream(
            orderbook_tx,
            orderbook_status_tx,
            &ws_url.clone(),
            &market_address,
        )
        .await
        .unwrap();
    });

    if args.oracle {
        let okx_instrument = format!("{}-USDT-SWAP", base_symbol.to_uppercase());
        tokio::spawn(async move {
            okx_stream(okx_tx, okx_status_tx, &OKX_WS_URL, &okx_instrument)
                .await
                .unwrap();
        });
    }

    track(
        orderbook_rx,
        okx_rx,
        orderbook_status_rx,
        okx_status_rx,
        args.oracle,
        csv_writer.as_mut(),
    )
    .await
    .unwrap();

    Ok(())
}
