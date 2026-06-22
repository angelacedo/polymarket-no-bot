use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::types::{BookLevel, BookUpdate};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const PING_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
pub struct BookCache {
    inner: Arc<DashMap<String, BookUpdate>>,
    latest_only: Arc<RwLock<()>>,
}

impl BookCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&self, update: BookUpdate) {
        self.inner.insert(update.asset_id.clone(), update);
    }

    pub fn get(&self, asset_id: &str) -> Option<BookUpdate> {
        self.inner.get(asset_id).map(|r| r.clone())
    }

    pub fn best_no_ask(&self, no_token_id: &str) -> Option<f64> {
        self.get(no_token_id).and_then(|b| b.best_ask())
    }
}

/// WebSocket orderbook feed with automatic reconnection.
pub fn spawn_orderbook_feed(
    token_ids: Vec<String>,
    cache: BookCache,
    tx: mpsc::Sender<BookUpdate>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if token_ids.is_empty() {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            match run_ws_session(&token_ids, &cache, &tx).await {
                Ok(()) => warn!("orderbook websocket session ended, reconnecting"),
                Err(e) => warn!(error = %e, "orderbook websocket error, reconnecting"),
            }

            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    })
}

async fn run_ws_session(
    token_ids: &[String],
    cache: &BookCache,
    tx: &mpsc::Sender<BookUpdate>,
) -> anyhow::Result<()> {
    let (ws_stream, _) = connect_async(WS_URL).await?;
    let (mut write, mut read) = ws_stream.split();

    let subscribe = json!({
        "auth": {},
        "markets": [],
        "assets_ids": token_ids,
    });
    write
        .send(Message::Text(subscribe.to_string().into()))
        .await?;
    info!(tokens = token_ids.len(), "subscribed to orderbook websocket");

    let mut ping_tick = tokio::time::interval(PING_INTERVAL);
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                write.send(Message::Text("PING".into())).await?;
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let trimmed = text.trim();
                        if trimmed.eq_ignore_ascii_case("PONG") || trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(value) = serde_json::from_str::<Value>(&text) {
                            handle_ws_message(&value, cache, tx);
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        write.send(Message::Pong(payload)).await?;
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }
        }
    }

    Ok(())
}

fn handle_ws_message(value: &Value, cache: &BookCache, tx: &mpsc::Sender<BookUpdate>) {
    let event_type = value
        .get("event_type")
        .or_else(|| value.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match event_type {
        "book" => {
            if let Some(asset_id) = value.get("asset_id").and_then(|v| v.as_str()) {
                if let Some(update) = parse_book(asset_id, value) {
                    cache.update(update.clone());
                    let _ = tx.try_send(update);
                }
            }
        }
        "price_change" => {
            if let Some(changes) = value.get("price_changes").and_then(|v| v.as_array()) {
                for change in changes {
                    apply_price_change(change, cache, tx);
                }
            }
        }
        _ => {
            debug!(event_type, "ignored websocket message");
        }
    }
}

fn apply_price_change(change: &Value, cache: &BookCache, tx: &mpsc::Sender<BookUpdate>) {
    let asset_id = match change.get("asset_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return,
    };

    let price = match change.get("price").and_then(parse_level_f64) {
        Some(p) => p,
        None => return,
    };

    let side = change
        .get("side")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();

    let size_opt = change.get("size").and_then(parse_level_f64);

    let mut book = cache.get(&asset_id).unwrap_or(BookUpdate {
        asset_id: asset_id.clone(),
        bids: Vec::new(),
        asks: Vec::new(),
        received_at: Instant::now(),
    });

    let is_bid = side == "BUY";
    let levels = if is_bid {
        &mut book.bids
    } else {
        &mut book.asks
    };

    match size_opt {
        Some(size) if size <= 0.0 => {
            levels.retain(|l| (l.price - price).abs() > 1e-12);
        }
        Some(size) => {
            upsert_level(levels, price, size, is_bid);
        }
        None => {}
    }

    book.received_at = Instant::now();
    cache.update(book.clone());
    let _ = tx.try_send(book);
}

fn upsert_level(levels: &mut Vec<BookLevel>, price: f64, size: f64, is_bid: bool) {
    if let Some(level) = levels
        .iter_mut()
        .find(|l| (l.price - price).abs() < 1e-12)
    {
        level.size = size;
    } else {
        levels.push(BookLevel { price, size });
    }

    if is_bid {
        levels.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        levels.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
    }
}

fn parse_level_f64(v: &Value) -> Option<f64> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_f64())
}

fn parse_book(token_id: &str, body: &Value) -> Option<BookUpdate> {
    let bids = parse_levels(body.get("bids")?);
    let asks = parse_levels(body.get("asks")?);
    Some(BookUpdate {
        asset_id: token_id.to_string(),
        bids,
        asks,
        received_at: Instant::now(),
    })
}

fn parse_levels(val: &Value) -> Vec<BookLevel> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    Some(BookLevel {
                        price: l.get("price")?.as_str()?.parse().ok()?,
                        size: l.get("size")?.as_str()?.parse().ok()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
