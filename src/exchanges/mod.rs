pub mod kraken;
pub mod okx;
pub mod phoenix;
pub mod phoenix_fill;

use crate::network_utils::ConnectionStatus;
use crate::phoenix_utils::{PhoenixFillRecv, PhoenixRecv};

use std::str::FromStr;

use anyhow::anyhow;
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc::Sender;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};
use url::Url;

use phoenix_sdk::sdk_client::SDKClient;
use solana_sdk::pubkey::Pubkey;

pub enum ExchangeUpdate {
    Phoenix(PhoenixRecv),
    PhoenixFill(PhoenixFillRecv),
    Oracle(OracleRecv),
}

#[derive(Debug, Clone)]
pub struct OracleRecv {
    pub exchange: String,
    pub bid: f64,
    pub bid_size: f64,
    pub ask: f64,
    pub ask_size: f64,
    pub timestamp_ms: u64,
}

impl OracleRecv {
    pub fn midpoint(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }
}

pub trait ExchangeWebsocketHandler {
    type Confirmation: DeserializeOwned;
    type Response: DeserializeOwned;

    fn get_name() -> String;

    fn get_ws_url() -> String;

    fn get_num_confirmation_messages() -> usize;

    fn get_subscribe_json(
        base_symbol: &str,
        quote_symbol: &str,
        market_address: Option<String>,
    ) -> String;

    #[allow(async_fn_in_trait)]
    async fn parse_confirmation(s: &str) -> anyhow::Result<String>;

    #[allow(async_fn_in_trait)]
    async fn parse_response(
        s: &str,
        trader_pubkey: Option<Pubkey>,
        sdk: Option<&SDKClient>,
    ) -> anyhow::Result<Vec<ExchangeUpdate>>;
}

async fn handle_exchange_stream<Exchange: ExchangeWebsocketHandler>(
    url: Url,
    subscribe_msg: Message,
    tx: Sender<ExchangeUpdate>,
    status_tx: Sender<ConnectionStatus>,
    trader_pubkey: Option<Pubkey>,
    sdk: Option<&SDKClient>,
) -> anyhow::Result<()> {
    let exchange_name = Exchange::get_name();
    let num_confirmation_messages = Exchange::get_num_confirmation_messages();

    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to {} websocket", exchange_name);
    status_tx.send(ConnectionStatus::Connected).await?;

    let (mut write, read) = ws_stream.split();
    let mut fused_read = read.fuse();
    write.send(subscribe_msg.clone()).await?;

    let mut i = 1;
    while let Some(message) = fused_read.next().await {
        match message {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    if i <= num_confirmation_messages {
                        let id = Exchange::parse_confirmation(&s).await?;
                        debug!("{} subscription confirmed with ID: {}", exchange_name, id);
                    } else {
                        debug!("Received {} update #{}", exchange_name, i);

                        let parsed_updates =
                            Exchange::parse_response(&s, trader_pubkey, sdk).await?;
                        for update in parsed_updates {
                            tx.send(update).await?;
                        }
                    }
                    i += 1;
                }
                _ => {
                    info!(
                        "{} websocket received non-text message: {:?}",
                        exchange_name, msg
                    );
                }
            },
            Err(e) => return Err(anyhow!(e)),
        }
    }

    Ok(())
}

/// trader_pubkey and sdk are only necessary for the phoenix fill websocket to parse fills
pub async fn exchange_stream<Exchange: ExchangeWebsocketHandler>(
    tx: Sender<ExchangeUpdate>,
    status_tx: Sender<ConnectionStatus>,
    base_symbol: &str,
    quote_symbol: &str,
    market_address: Option<String>,
    trader_pubkey: Option<Pubkey>,
    sdk: Option<&SDKClient>,
    sleep_sec_between_ws_connect: u64,
    disconnects_before_exit: usize,
) -> anyhow::Result<()> {
    let exchange_name = Exchange::get_name();
    let ws_url = Exchange::get_ws_url();
    let url = Url::from_str(&ws_url).expect("Failed to parse websocket url");
    let subscribe_msg = Message::Text(Exchange::get_subscribe_json(
        base_symbol,
        quote_symbol,
        market_address,
    ));

    let mut disconnects = 0;

    loop {
        if let Err(e) = handle_exchange_stream::<Exchange>(
            url.clone(),
            subscribe_msg.clone(),
            tx.clone(),
            status_tx.clone(),
            trader_pubkey.clone(),
            sdk,
        )
        .await
        {
            status_tx.send(ConnectionStatus::Disconnected).await?;

            warn!(
                "{} websocket disconnected with error: {:?}",
                exchange_name, e
            );
            disconnects += 1;
            if disconnects >= disconnects_before_exit {
                error!("Exceeded max disconnects, exiting...");
                return Err(anyhow!(
                    "{} websocket experienced {} disconnections, not trying again",
                    exchange_name,
                    disconnects_before_exit
                ));
            }

            info!("Reconnect attempt #{}...", disconnects + 1);
            sleep(Duration::from_secs(sleep_sec_between_ws_connect)).await;
        }
    }
}
