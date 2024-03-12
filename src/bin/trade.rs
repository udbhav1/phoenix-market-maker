extern crate phoenix_market_maker;
#[allow(unused_imports)]
use phoenix_market_maker::network_utils::get_time_ms;
use phoenix_market_maker::network_utils::{
    get_enhanced_ws_url, get_network, get_payer_keypair_from_path, get_ws_url,
    AccountSubscribeConfirmation, AccountSubscribeResponse, ConnectionStatus,
    TransactionSubscribeConfirmation, TransactionSubscribeResponse, ACCOUNT_SUBSCRIBE_JSON,
    TRANSACTION_SUBSCRIBE_JSON,
};
use phoenix_market_maker::phoenix_utils::{
    book_to_aggregated_levels, get_book_from_account_data, symbols_to_market_address, Book, Fill,
};

use anyhow::{anyhow, Context};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use solana_sdk::signature::Signature;
use std::env;
use std::fs::OpenOptions;
use std::str::FromStr;
use std::time::Instant;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

use phoenix::state::enums::Side;
use phoenix_sdk::sdk_client::MarketEventDetails;
use phoenix_sdk::sdk_client::SDKClient;
use solana_cli_config::{Config, CONFIG_FILE};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;

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
            Ok(msg) => {
                match msg {
                    Message::Text(s) => {
                        if is_first_message {
                            let confirmation: TransactionSubscribeConfirmation =
                        serde_json::from_str(&s).with_context(|| format!("Failed to deserialize transaction subscribe confirmation: {}", s))?;
                            debug!("Subscription confirmed with ID: {}", confirmation.result);
                            is_first_message = false;
                        } else {
                            let parsed: TransactionSubscribeResponse = serde_json::from_str(&s)?;
                            let signature_str = parsed.params.result.signature;

                            let start = Instant::now();
                            let events =
                                sdk.parse_fills(&Signature::from_str(&signature_str)?).await;
                            let elapsed = start.elapsed();
                            info!(
                                "phoenix_sdk::sdk_client::parse_fills took {}ms",
                                elapsed.as_millis()
                            );

                            for event in events {
                                match event.details {
                                    MarketEventDetails::Fill(f) => {
                                        let fill = Fill {
                                            price: f.price_in_ticks,
                                            size: f.base_lots_filled,
                                            side: f.side_filled,
                                            slot: event.slot,
                                            timestamp: event.timestamp,
                                            signature: signature_str.clone(),
                                        };
                                        tx.send(fill).await?;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
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
    trader_address: &str,
    sdk: &SDKClient,
) -> anyhow::Result<()> {
    let url = Url::from_str(ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(
        TRANSACTION_SUBSCRIBE_JSON
            .replace("{1}", market_address)
            .replace("{2}", trader_address),
    );
    let sleep_time: u64 = env::var("TRADE_SLEEP_SEC_BETWEEN_WS_CONNECT")?.parse()?;

    let max_disconnects: usize = env::var("TRADE_DISCONNECTS_BEFORE_EXIT")?.parse()?;
    let mut disconnects = 0;

    loop {
        if let Err(e) = handle_fill_stream(
            url.clone(),
            subscribe_msg.clone(),
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

async fn trading_logic(
    mut orderbook_rx: mpsc::Receiver<Book>,
    mut fill_rx: mpsc::Receiver<Fill>,
    mut orderbook_status_rx: mpsc::Receiver<ConnectionStatus>,
    mut fill_status_rx: mpsc::Receiver<ConnectionStatus>,
    sdk: &SDKClient,
) {
    let mut orderbook_connected = false;
    let mut fill_connected = false;
    let mut i = 0;

    let mut latest_orderbook: Option<Book> = None;

    loop {
        tokio::select! {
            Some(orderbook) = orderbook_rx.recv() => {
                i += 1;
                info!("Received orderbook #{}", i);
                // orderbook.print_ladder(5, 4);

                // clear queue to get latest book every time we hit this branch
                // this might be necessary since the trading logic will run on every iteration of the loop
                // and each loop iteration processes one element from a random queue
                // then again maybe i wont ever get books fast enough to have more than one in the queue
                latest_orderbook = Some(orderbook);
                while let Ok(newer_orderbook) = orderbook_rx.try_recv() {
                    info!("Newer orderbook in queue, discarding old one");
                    latest_orderbook = Some(newer_orderbook);
                }
            },
            Some(fill) = fill_rx.recv() => {
                info!("Received fill: {:?}", fill);
            },
            Some(status) = orderbook_status_rx.recv() => {
                orderbook_connected = matches!(status, ConnectionStatus::Connected);
            },
            Some(status) = fill_status_rx.recv() => {
                fill_connected = matches!(status, ConnectionStatus::Connected);
            },
        }

        if orderbook_connected && fill_connected {
        } else {
            // One or both streams are disconnected; pause trading logic
            if !orderbook_connected {
                warn!("Orderbook stream not connected");
            }
            if !fill_connected {
                warn!("Fill stream not connected");
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
    let provided_network = args.url.clone().unwrap_or(config.json_rpc_url);
    let network_url = &get_network(&provided_network, &api_key)?.to_string();

    // both trading_logic() and fill_stream() need this and its not cloneable
    let mut sdk1 = SDKClient::new(&trader, network_url).await?;
    let mut sdk2 = SDKClient::new(&trader, network_url).await?;

    let base_symbol = args.base_symbol;
    let quote_symbol = args.quote_symbol;

    let market_address = symbols_to_market_address(&sdk1, &base_symbol, &quote_symbol).await?;
    let market_pubkey = Pubkey::from_str(&market_address)?;

    info!(
        "Found market address for {}/{}: {}",
        base_symbol, quote_symbol, market_address
    );

    sdk1.add_market(&market_pubkey).await?;
    sdk2.add_market(&market_pubkey).await?;

    let ws_url = get_ws_url(&provided_network, &api_key)?;
    let enhanced_ws_url = get_enhanced_ws_url(&provided_network, &api_key)?;

    let (orderbook_tx, orderbook_rx) = mpsc::channel(32);
    let (orderbook_status_tx, orderbook_status_rx) = mpsc::channel(32);

    let (fill_tx, fill_rx) = mpsc::channel(32);
    let (fill_status_tx, fill_status_rx) = mpsc::channel(32);

    // move blocks take ownership so have to clone before
    let market_address1 = market_address.clone();
    let market_address2 = market_address.clone();

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
            &trader_address,
            &sdk2,
        )
        .await
        .unwrap();
    });

    trading_logic(
        orderbook_rx,
        fill_rx,
        orderbook_status_rx,
        fill_status_rx,
        &sdk1,
    )
    .await;

    Ok(())
}
