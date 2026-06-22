use super::StatusSnapshot;

pub fn render(status: &StatusSnapshot) -> String {
    let trades_rows: String = status
        .recent_trades
        .iter()
        .map(|t| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.4}</td><td>{:.2}</td><td>{}</td></tr>",
                t.ts.format("%Y-%m-%d %H:%M"),
                t.mode,
                t.market_id,
                t.entry_price,
                t.size_shares,
                t.source,
            )
        })
        .collect();

    let tuning_rows: String = status
        .recent_tuning_events
        .iter()
        .map(|t| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                t.ts.format("%Y-%m-%d %H:%M"),
                t.parameter,
                t.old_value,
                t.new_value,
                t.reason,
            )
        })
        .collect();

    format!(
        r#"<!DOCTYPE html>
<html><head><title>Polymarket NO Bot</title>
<style>
body {{ font-family: system-ui, sans-serif; margin: 2rem; background: #0f1117; color: #e6e6e6; }}
table {{ border-collapse: collapse; width: 100%; margin-top: 1rem; }}
th, td {{ border: 1px solid #333; padding: 0.5rem; text-align: left; }}
th {{ background: #1a1d27; }}
.card {{ background: #1a1d27; padding: 1rem; border-radius: 8px; margin-bottom: 1rem; }}
.pos {{ color: #4ade80; }} .neg {{ color: #f87171; }}
</style></head><body>
<h1>Polymarket Short-NO Bot</h1>
<div class="card">
  <strong>Mode:</strong> {mode}<br>
  <strong>Paper equity:</strong> ${paper_eq:.2} (realized ${paper_r:.2}, unrealized ${paper_u:.2})<br>
  <strong>Live equity:</strong> ${live_eq:.2} (realized ${live_r:.2}, unrealized ${live_u:.2})<br>
  <strong>Drawdown:</strong> {dd:.2}% | <strong>Open positions:</strong> {pos}<br>
  <strong>Latency p50:</strong> decision→order {lat_d}ms, order→fill {lat_f}ms<br>
  <strong>Circuit breaker:</strong> live_disabled={live_off}, block_entries={block}
</div>
<h2>Recent Trades</h2>
<table><tr><th>Time</th><th>Mode</th><th>Market</th><th>Entry</th><th>Size</th><th>Source</th></tr>{trades}</table>
<h2>Auto-Tuning Log</h2>
<table><tr><th>Time</th><th>Parameter</th><th>Old</th><th>New</th><th>Reason</th></tr>{tuning}</table>
<p><a href="/api/status">JSON status</a> | <a href="/metrics">Prometheus metrics</a></p>
</body></html>"#,
        mode = status.mode,
        paper_eq = status.paper_pnl.equity,
        paper_r = status.paper_pnl.realized_pnl,
        paper_u = status.paper_pnl.unrealized_pnl,
        live_eq = status.live_pnl.equity,
        live_r = status.live_pnl.realized_pnl,
        live_u = status.live_pnl.unrealized_pnl,
        dd = status.paper_pnl.drawdown_fraction * 100.0,
        pos = status.open_positions,
        lat_d = status.latency_p50_decision_ms,
        lat_f = status.latency_p50_fill_ms,
        live_off = status.circuit_breaker.live_disabled,
        block = status.circuit_breaker.block_new_entries,
        trades = trades_rows,
        tuning = tuning_rows,
    )
}
