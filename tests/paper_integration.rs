use polymarket_no_bot::config::BotConfig;
use polymarket_no_bot::execution::{ExecutionBackend, PaperBackend};
use polymarket_no_bot::storage::Storage;
use polymarket_no_bot::types::{
    BookLevel, BookUpdate, ExecutionMode, OrderRequest, OrderStatus, Side,
};
use polymarket_no_bot::exchange::BookCache;
use std::time::Instant;

#[tokio::test]
async fn paper_backend_integration_with_mock_book() {
    let cache = BookCache::new();
    cache.update(BookUpdate {
        asset_id: "no-token".into(),
        bids: vec![BookLevel { price: 0.84, size: 200.0 }],
        asks: vec![
            BookLevel { price: 0.85, size: 30.0 },
            BookLevel { price: 0.86, size: 70.0 },
        ],
        received_at: Instant::now(),
    });

    let storage = Storage::in_memory().expect("storage");
    let config = BotConfig::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/sample.toml"))
        .expect("config");

    let backend = PaperBackend::new(
        config.execution,
        10_000.0,
        cache,
        storage,
    );

    let req = OrderRequest {
        client_order_id: "test-1".into(),
        token_id: "no-token".into(),
        condition_id: "cond-1".into(),
        side: Side::No,
        limit_price: 0.86,
        size_shares: 50.0,
        mode: ExecutionMode::Paper,
        source: "strategy".into(),
        category: "crypto".into(),
        underlying: "BTC".into(),
    };

    let result = backend.place_order(req).await.expect("order");
    assert!(result.filled_shares > 0.0);
    assert!(matches!(
        result.status,
        OrderStatus::Filled | OrderStatus::PartiallyFilled
    ));
    assert!(result.latency_ms >= 1);

    let balances = backend.balances().await.expect("balances");
    assert!(balances.usdc_available < 10_000.0);
}
