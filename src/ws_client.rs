/// Connects to the HFT generator WebSocket and keeps a live price cache.
/// The DashMap<symbol, price> is shared with the order engine.
use crate::models::MarketDataMessage;
use dashmap::DashMap;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::connect_async;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub type PriceCache = Arc<DashMap<String, f64>>;

pub fn new_price_cache() -> PriceCache {
    Arc::new(DashMap::new())
}

pub async fn run_ws_client(ws_url: String, prices: PriceCache, cancel: CancellationToken) {
    loop {
        if cancel.is_cancelled() {
            break;
        }

        info!("Connecting to HFT feed: {}", ws_url);
        match connect_async(&ws_url).await {
            Err(e) => {
                warn!("WS connect failed: {}. Retrying in 3s…", e);
                tokio::select! {
                    _ = sleep(Duration::from_secs(3)) => {}
                    _ = cancel.cancelled() => break,
                }
            }
            Ok((stream, _)) => {
                info!("HFT feed connected");
                let (_, mut read) = stream.split();

                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            info!("WS client shutting down");
                            return;
                        }
                        msg = read.next() => {
                            match msg {
                                None => {
                                    warn!("HFT feed closed. Reconnecting…");
                                    break;
                                }
                                Some(Err(e)) => {
                                    error!("WS error: {}. Reconnecting…", e);
                                    break;
                                }
                                Some(Ok(m)) if m.is_text() => {
                                    match m.to_text() {
                                        Ok(text) => match serde_json::from_str::<MarketDataMessage>(text) {
                                            Ok(tick) => { prices.insert(tick.symbol.clone(), tick.price); }
                                            Err(e) => warn!("Failed to parse market data message: {}", e),
                                        },
                                        Err(e) => warn!("Received non-UTF8 WebSocket frame: {}", e),
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Brief back-off before reconnect
                tokio::select! {
                    _ = sleep(Duration::from_secs(1)) => {}
                    _ = cancel.cancelled() => break,
                }
            }
        }
    }
}
