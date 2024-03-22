pub mod okx;
pub mod phoenix;

use crate::network_utils::ConnectionStatus;
use crate::phoenix_utils::BookUpdate;

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

pub trait ExchangeWsHandler {
    type Confirmation: DeserializeOwned;
    type Response: DeserializeOwned;

    fn get_name() -> String;
    fn get_ws_url() -> String;
    fn get_subscribe_json(
        base_symbol: &str,
        quote_symbol: &str,
        market_address: Option<String>,
    ) -> String;
    fn parse_confirmation(s: &str) -> anyhow::Result<String>;
    fn parse_response(s: &str) -> anyhow::Result<BookUpdate>;
}

async fn handle_exchange_stream<Exchange: ExchangeWsHandler>(
    url: Url,
    subscribe_msg: Message,
    tx: Sender<BookUpdate>,
    status_tx: Sender<ConnectionStatus>,
) -> anyhow::Result<()> {
    let exchange_name = Exchange::get_name();

    let (ws_stream, _) = connect_async(url).await?;
    info!("Connected to {} websocket", exchange_name);
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
                        let id = Exchange::parse_confirmation(&s)?;
                        debug!("{} subscription confirmed with ID: {}", exchange_name, id);
                        is_first_message = false;
                    } else {
                        info!("Received {} update #{}", exchange_name, i);
                        i += 1;

                        let parsed = Exchange::parse_response(&s)?;

                        tx.send(parsed).await?;
                    }
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

pub async fn exchange_stream<Exchange: ExchangeWsHandler>(
    tx: Sender<BookUpdate>,
    status_tx: Sender<ConnectionStatus>,
    base_symbol: &str,
    quote_symbol: &str,
    market_address: Option<String>,
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
