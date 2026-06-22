use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;

use crate::types::{BookLevel, BookUpdate};

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

/// Orderbook feed task — uses REST polling as fallback when WS token list is dynamic.
/// POLYMARKET_INTEGRATION: replace poll loop with polymarket_client_sdk_v2 WS subscribe_orderbook.
pub fn spawn_orderbook_feed(
    token_ids: Vec<String>,
    cache: BookCache,
    tx: tokio::sync::mpsc::Sender<BookUpdate>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        loop {
            for token_id in &token_ids {
                let url = format!("https://clob.polymarket.com/book?token_id={token_id}");
                match client.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            if let Some(update) = parse_book(token_id, &body) {
                                cache.update(update.clone());
                                let _ = tx.try_send(update);
                            }
                        }
                    }
                    Ok(resp) => {
                        tracing::debug!(status = %resp.status(), token = %token_id, "book poll failed");
                    }
                    Err(e) => tracing::warn!(error = %e, "book poll error"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
}

fn parse_book(token_id: &str, body: &serde_json::Value) -> Option<BookUpdate> {
    let bids = parse_levels(body.get("bids")?);
    let asks = parse_levels(body.get("asks")?);
    Some(BookUpdate {
        asset_id: token_id.to_string(),
        bids,
        asks,
        received_at: std::time::Instant::now(),
    })
}

fn parse_levels(val: &serde_json::Value) -> Vec<BookLevel> {
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
