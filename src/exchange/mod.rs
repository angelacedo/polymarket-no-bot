mod book;
mod data_api;
mod gamma;
pub(crate) mod retry;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, watch};

pub use book::{BookCache, spawn_orderbook_feed};
pub use data_api::DataApiClient;
pub use gamma::{classify_market, GammaClient};

use crate::config::ExchangeConfig;
use crate::types::{BookUpdate, MarketMeta, WalletTradeEvent};

pub struct ExchangeHub {
    pub gamma: GammaClient,
    pub data_api: DataApiClient,
    pub book_cache: BookCache,
}

impl ExchangeHub {
    pub fn new(config: &ExchangeConfig) -> Self {
        Self {
            gamma: GammaClient::new(config),
            data_api: DataApiClient::new(config),
            book_cache: BookCache::new(),
        }
    }

    pub async fn discover_markets(&self, limit: u32) -> Result<Vec<MarketMeta>> {
        self.gamma.fetch_active_markets(limit).await
    }

    pub fn start_book_feed(
        self: &Arc<Self>,
        token_ids_rx: watch::Receiver<Vec<String>>,
        tx: mpsc::Sender<BookUpdate>,
    ) -> tokio::task::JoinHandle<()> {
        let cache = self.book_cache.clone();
        spawn_orderbook_feed(token_ids_rx, cache, tx)
    }

    pub async fn fetch_wallet_trades(
        &self,
        wallet: &str,
        limit: u32,
    ) -> Result<Vec<WalletTradeEvent>> {
        self.data_api.fetch_trades(wallet, limit).await
    }
}
