/// HTML backtest report generator.
///
/// Produces a self-contained single-file HTML report with:
///   - Key statistics summary
///   - Equity curve chart (Chart.js, CDN)
///   - Drawdown chart
///   - Monthly returns heatmap
use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::charting::{ChartCollection, SeriesType};
use crate::runner::BacktestResult;

// ── LEAN-compatible output file writers ───────────────────────────────────────

/// Write `<id>-order-events.json` — serialised list of all OrderEvent structs.
pub fn write_order_events_json(result: &BacktestResult, path: &Path) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(&result.order_events)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json)
}

/// Summary statistics struct written to `<id>-summary.json`.
#[derive(Serialize)]
struct SummaryJson {
    #[serde(rename = "TotalReturn")]
    total_return: f64,
    #[serde(rename = "CAGR")]
    cagr: f64,
    #[serde(rename = "SharpeRatio")]
    sharpe_ratio: f64,
    #[serde(rename = "SortinoRatio")]
    sortino_ratio: f64,
    #[serde(rename = "ProbabilisticSharpeRatio")]
    probabilistic_sharpe_ratio: f64,
    #[serde(rename = "MaxDrawdown")]
    max_drawdown: f64,
    #[serde(rename = "CalmarRatio")]
    calmar_ratio: f64,
    #[serde(rename = "OmegaRatio")]
    omega_ratio: f64,
    #[serde(rename = "RecoveryFactor")]
    recovery_factor: f64,
    #[serde(rename = "AnnualStandardDeviation")]
    annual_standard_deviation: f64,
    #[serde(rename = "Alpha")]
    alpha: f64,
    #[serde(rename = "Beta")]
    beta: f64,
    #[serde(rename = "TreynorRatio")]
    treynor_ratio: f64,
    #[serde(rename = "WinRate")]
    win_rate: f64,
    #[serde(rename = "LossRate")]
    loss_rate: f64,
    #[serde(rename = "ProfitLossRatio")]
    profit_loss_ratio: f64,
    #[serde(rename = "Expectancy")]
    expectancy: f64,
    #[serde(rename = "TotalNetProfit")]
    total_net_profit: f64,
    #[serde(rename = "TotalTrades")]
    total_trades: usize,
    #[serde(rename = "WinningTrades")]
    winning_trades: usize,
    #[serde(rename = "LosingTrades")]
    losing_trades: usize,
    #[serde(rename = "AverageTradeDurationDays")]
    average_trade_duration_days: f64,
    #[serde(rename = "StartingCash")]
    starting_cash: f64,
    #[serde(rename = "FinalValue")]
    final_value: f64,
    #[serde(rename = "TradingDays")]
    trading_days: i64,
}

/// Write `<id>-summary.json` — flat key/value stats matching LEAN's summary format.
pub fn write_summary_json(result: &BacktestResult, path: &Path) -> std::io::Result<()> {
    use rust_decimal::prelude::ToPrimitive;
    let s = &result.statistics;
    let summary = SummaryJson {
        total_return:                result.total_return,
        cagr:                        s.compounding_annual_return.to_f64().unwrap_or(0.0),
        sharpe_ratio:                s.sharpe_ratio.to_f64().unwrap_or(0.0),
        sortino_ratio:               s.sortino_ratio.to_f64().unwrap_or(0.0),
        probabilistic_sharpe_ratio:  s.probabilistic_sharpe_ratio.to_f64().unwrap_or(0.0),
        max_drawdown:                s.drawdown.to_f64().unwrap_or(0.0),
        calmar_ratio:                s.calmar_ratio.to_f64().unwrap_or(0.0),
        omega_ratio:                 s.omega_ratio.to_f64().unwrap_or(0.0),
        recovery_factor:             s.recovery_factor.to_f64().unwrap_or(0.0),
        annual_standard_deviation:   s.annual_standard_deviation.to_f64().unwrap_or(0.0),
        alpha:                       s.alpha.to_f64().unwrap_or(0.0),
        beta:                        s.beta.to_f64().unwrap_or(0.0),
        treynor_ratio:               s.treynor_ratio.to_f64().unwrap_or(0.0),
        win_rate:                    s.win_rate.to_f64().unwrap_or(0.0),
        loss_rate:                   s.loss_rate.to_f64().unwrap_or(0.0),
        profit_loss_ratio:           s.profit_loss_ratio.to_f64().unwrap_or(0.0),
        expectancy:                  s.expectancy.to_f64().unwrap_or(0.0),
        total_net_profit:            s.total_net_profit.to_f64().unwrap_or(0.0),
        total_trades:                s.total_trades,
        winning_trades:              s.winning_trades,
        losing_trades:               s.losing_trades,
        average_trade_duration_days: s.average_trade_duration_days.to_f64().unwrap_or(0.0),
        starting_cash:               result.starting_cash,
        final_value:                 result.final_value,
        trading_days:                result.trading_days,
    };
    let json = serde_json::to_string_pretty(&summary)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json)
}

/// Write `succeeded-data-requests-<timestamp>.txt` and `failed-data-requests-<timestamp>.txt`.
pub fn write_data_request_files(
    result: &BacktestResult,
    succeeded_path: &Path,
    failed_path: &Path,
) -> std::io::Result<()> {
    std::fs::write(succeeded_path, result.succeeded_data_requests.join("\n"))?;
    std::fs::write(failed_path, result.failed_data_requests.join("\n"))
}

/// Write `<id>-log.txt` (and `log.txt`) — captured log lines for the backtest.
///
/// Currently mirrors the same content: a header line plus a summary of key stats.
/// Full per-line log capture would require a custom tracing subscriber; for now
/// we emit a structured stats summary that matches LEAN's convention of having a
/// log file alongside each result.
pub fn write_log_txt(result: &BacktestResult, path: &Path) -> std::io::Result<()> {
    use rust_decimal::prelude::ToPrimitive;
    let s = &result.statistics;
    let mut lines = Vec::new();
    lines.push(format!("Backtest ID:       {}", result.backtest_id));
    lines.push(format!("Trading Days:      {}", result.trading_days));
    lines.push(format!("Starting Cash:     {:.2}", result.starting_cash));
    lines.push(format!("Final Value:       {:.2}", result.final_value));
    lines.push(format!("Total Return:      {:.4}", result.total_return));
    lines.push(format!("CAGR:              {:.4}", s.compounding_annual_return.to_f64().unwrap_or(0.0)));
    lines.push(format!("Sharpe Ratio:      {:.4}", s.sharpe_ratio.to_f64().unwrap_or(0.0)));
    lines.push(format!("Max Drawdown:      {:.4}", s.drawdown.to_f64().unwrap_or(0.0)));
    lines.push(format!("Total Trades:      {}", s.total_trades));
    lines.push(format!("Win Rate:          {:.4}", s.win_rate.to_f64().unwrap_or(0.0)));
    std::fs::write(path, lines.join("\n"))
}

pub fn write_report(result: &BacktestResult, path: &Path) -> std::io::Result<()> {
    let html = generate_html(result);
    std::fs::write(path, html)
}

/// LEAN-compatible results JSON written to <backtest_dir>/results.json.
///
/// Top-level keys mirror the C# `BacktestResultPacket` format so tooling
/// that understands LEAN output can consume this file directly.
#[derive(Serialize)]
struct BacktestResultJson<'a> {
    #[serde(rename = "Statistics")]
    statistics: &'a lean_statistics::PortfolioStatistics,
    #[serde(rename = "Charts")]
    charts: &'a ChartCollection,
    /// Equity curve as {date → portfolio value} pairs (LEAN "Strategy Equity" series).
    #[serde(rename = "Equity")]
    equity: std::collections::HashMap<&'a str, f64>,
    #[serde(rename = "TradingDays")]
    trading_days: i64,
    #[serde(rename = "StartingCash")]
    starting_cash: f64,
    #[serde(rename = "FinalValue")]
    final_value: f64,
    #[serde(rename = "TotalReturn")]
    total_return: f64,
}

pub fn write_results_json(result: &BacktestResult, path: &Path) -> std::io::Result<()> {
    let equity: std::collections::HashMap<&str, f64> = result
        .daily_dates
        .iter()
        .zip(result.equity_curve.iter())
        .map(|(d, &v)| (d.as_str(), v))
        .collect();

    let json_obj = BacktestResultJson {
        statistics:   &result.statistics,
        charts:       &result.charts,
        equity,
        trading_days: result.trading_days,
        starting_cash: result.starting_cash,
        final_value:  result.final_value,
        total_return: result.total_return,
    };

    let json = serde_json::to_string_pretty(&json_obj)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json)
}

fn generate_html(r: &BacktestResult) -> String {
    use rust_decimal::prelude::ToPrimitive;
    let s = &r.statistics;

    // ── equity curve JSON arrays ──────────────────────────────────────────
    let dates_json = r.daily_dates.iter()
        .map(|d| format!("\"{}\"", d))
        .collect::<Vec<_>>()
        .join(",");
    let equity_json = r.equity_curve.iter()
        .map(|v| format!("{:.2}", v))
        .collect::<Vec<_>>()
        .join(",");

    // ── drawdown series ───────────────────────────────────────────────────
    let drawdown_json = {
        let mut peak = r.equity_curve.first().copied().unwrap_or(r.starting_cash);
        let mut dd_series: Vec<String> = Vec::new();
        for &v in &r.equity_curve {
            if v > peak { peak = v; }
            let dd = if peak > 0.0 { -((peak - v) / peak * 100.0) } else { 0.0 };
            dd_series.push(format!("{:.2}", dd));
        }
        dd_series.join(",")
    };

    // ── monthly returns heatmap ───────────────────────────────────────────
    // Build (year, month) → return_pct map from daily equity.
    let monthly_returns: BTreeMap<(i32, u32), f64> = {
        let mut map: BTreeMap<(i32, u32), (f64, f64)> = BTreeMap::new(); // (start, end)
        for (date_str, &equity) in r.daily_dates.iter().zip(r.equity_curve.iter()) {
            // date_str is "YYYY-MM-DD"
            let parts: Vec<&str> = date_str.splitn(3, '-').collect();
            if parts.len() < 2 { continue; }
            let year: i32 = parts[0].parse().unwrap_or(0);
            let month: u32 = parts[1].parse().unwrap_or(0);
            let entry = map.entry((year, month)).or_insert((equity, equity));
            if entry.0 > equity { entry.0 = equity; } // track minimum start
            if entry.1 < equity { entry.1 = equity; } // track maximum end — use last
            // Overwrite end with each day so final value = last day's value
            entry.1 = equity;
        }
        // First pass: fix start values — need chronological first value per month
        let mut fixed: BTreeMap<(i32, u32), (f64, f64)> = BTreeMap::new();
        let mut prev_equity = r.starting_cash;
        let mut prev_key: Option<(i32, u32)> = None;
        for (date_str, &equity) in r.daily_dates.iter().zip(r.equity_curve.iter()) {
            let parts: Vec<&str> = date_str.splitn(3, '-').collect();
            if parts.len() < 2 { continue; }
            let year: i32 = parts[0].parse().unwrap_or(0);
            let month: u32 = parts[1].parse().unwrap_or(0);
            let key = (year, month);
            if Some(key) != prev_key {
                fixed.entry(key).or_insert((prev_equity, equity));
                prev_key = Some(key);
            }
            fixed.entry(key).and_modify(|e| e.1 = equity);
            prev_equity = equity;
        }
        fixed.into_iter().map(|(k, (start, end))| {
            let ret = if start > 0.0 { (end - start) / start * 100.0 } else { 0.0 };
            (k, ret)
        }).collect()
    };

    let monthly_html = build_monthly_heatmap(&monthly_returns);

    // ── custom strategy charts ────────────────────────────────────────────
    let custom_charts_html = build_custom_charts(&r.charts);

    // ── stat helpers ──────────────────────────────────────────────────────
    let pct = |v: f64| format!("{:.2}%", v * 100.0);
    let dollar = |v: f64| format!("${:.2}", v);
    let ratio = |v: f64| format!("{:.3}", v);

    let cagr       = s.compounding_annual_return.to_f64().unwrap_or(0.0);
    let sharpe     = s.sharpe_ratio.to_f64().unwrap_or(0.0);
    let sortino    = s.sortino_ratio.to_f64().unwrap_or(0.0);
    let psr        = s.probabilistic_sharpe_ratio.to_f64().unwrap_or(0.0);
    let calmar     = s.calmar_ratio.to_f64().unwrap_or(0.0);
    let omega      = s.omega_ratio.to_f64().unwrap_or(0.0);
    let drawdown   = s.drawdown.to_f64().unwrap_or(0.0);
    let recovery   = s.recovery_factor.to_f64().unwrap_or(0.0);
    let ann_std    = s.annual_standard_deviation.to_f64().unwrap_or(0.0);
    let alpha      = s.alpha.to_f64().unwrap_or(0.0);
    let beta       = s.beta.to_f64().unwrap_or(0.0);
    let treynor    = s.treynor_ratio.to_f64().unwrap_or(0.0);
    let win_rate   = s.win_rate.to_f64().unwrap_or(0.0);
    let pl_ratio   = s.profit_loss_ratio.to_f64().unwrap_or(0.0);
    let expectancy = s.expectancy.to_f64().unwrap_or(0.0);
    let net_profit = s.total_net_profit.to_f64().unwrap_or(0.0);
    let avg_win    = s.average_win_rate.to_f64().unwrap_or(0.0);
    let avg_loss   = s.average_loss_rate.to_f64().unwrap_or(0.0);
    let lg_win     = s.largest_win.to_f64().unwrap_or(0.0);
    let lg_loss    = s.largest_loss.to_f64().unwrap_or(0.0);
    let avg_dur    = s.average_trade_duration_days.to_f64().unwrap_or(0.0);

    let return_color = if r.total_return >= 0.0 { "#4caf50" } else { "#f44336" };

    format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Backtest Report</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>
<style>
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
          background: #0f1117; color: #e0e0e0; padding: 24px; }}
  h1 {{ font-size: 1.6rem; margin-bottom: 4px; color: #fff; }}
  .sub {{ color: #888; font-size: .9rem; margin-bottom: 24px; }}
  .kpi-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(180px,1fr));
               gap: 12px; margin-bottom: 28px; }}
  .kpi {{ background: #1e2130; border-radius: 8px; padding: 14px 16px; }}
  .kpi .label {{ font-size: .75rem; color: #888; text-transform: uppercase; letter-spacing: .05em; }}
  .kpi .value {{ font-size: 1.4rem; font-weight: 600; margin-top: 4px; }}
  .kpi .value.pos {{ color: #4caf50; }}
  .kpi .value.neg {{ color: #f44336; }}
  .kpi .value.neu {{ color: #90caf9; }}
  .charts {{ display: grid; grid-template-columns: 1fr 1fr; gap: 20px; margin-bottom: 28px; }}
  @media (max-width: 900px) {{ .charts {{ grid-template-columns: 1fr; }} }}
  .chart-card {{ background: #1e2130; border-radius: 8px; padding: 16px; }}
  .chart-card h3 {{ font-size: .85rem; color: #aaa; margin-bottom: 12px; text-transform: uppercase; }}
  canvas {{ max-height: 220px; }}
  .stats-grid {{ display: grid; grid-template-columns: 1fr 1fr; gap: 20px; margin-bottom: 28px; }}
  @media (max-width: 700px) {{ .stats-grid {{ grid-template-columns: 1fr; }} }}
  .stats-card {{ background: #1e2130; border-radius: 8px; padding: 16px; }}
  .stats-card h3 {{ font-size: .85rem; color: #aaa; margin-bottom: 10px; text-transform: uppercase; }}
  table {{ width: 100%; border-collapse: collapse; font-size: .88rem; }}
  td {{ padding: 6px 4px; border-bottom: 1px solid #2a2d3e; }}
  td:last-child {{ text-align: right; color: #90caf9; font-variant-numeric: tabular-nums; }}
  .heatmap {{ background: #1e2130; border-radius: 8px; padding: 16px; margin-bottom: 28px; }}
  .heatmap h3 {{ font-size: .85rem; color: #aaa; margin-bottom: 12px; text-transform: uppercase; }}
  .hm-table {{ border-collapse: collapse; font-size: .8rem; }}
  .hm-table th {{ color: #888; font-weight: 500; padding: 4px 8px; text-align: center; }}
  .hm-table td {{ padding: 4px 8px; text-align: center; border-radius: 4px; min-width: 54px; }}
  .footer {{ text-align: center; color: #444; font-size: .75rem; margin-top: 32px; }}
</style>
</head>
<body>
<h1>Backtest Report</h1>
<div class="sub">Generated by Lean-Rust &nbsp;|&nbsp; {trading_days} trading days &nbsp;|&nbsp;
  ${starting_cash:.0} → <span style="color:{return_color}">${final_value:.0}
  ({total_return:+.2}%)</span></div>

<!-- KPI cards -->
<div class="kpi-grid">
  {kpi_cards}
</div>

<!-- Charts -->
<div class="charts">
  <div class="chart-card">
    <h3>Equity Curve</h3>
    <canvas id="equityChart"></canvas>
  </div>
  <div class="chart-card">
    <h3>Drawdown</h3>
    <canvas id="ddChart"></canvas>
  </div>
</div>

<!-- Monthly heatmap -->
<div class="heatmap">
  <h3>Monthly Returns</h3>
  {monthly_html}
</div>

<!-- Custom strategy charts -->
{custom_charts_html}

<!-- Stat tables -->
<div class="stats-grid">
  <div class="stats-card">
    <h3>Risk / Return</h3>
    <table>
      <tr><td>Total Return</td><td>{total_return_pct}</td></tr>
      <tr><td>CAGR</td><td>{cagr}</td></tr>
      <tr><td>Annual Std Dev</td><td>{ann_std}</td></tr>
      <tr><td>Max Drawdown</td><td>{drawdown}</td></tr>
      <tr><td>Sharpe Ratio</td><td>{sharpe}</td></tr>
      <tr><td>Sortino Ratio</td><td>{sortino}</td></tr>
      <tr><td>Calmar Ratio</td><td>{calmar}</td></tr>
      <tr><td>Omega Ratio</td><td>{omega}</td></tr>
      <tr><td>Probabilistic SR</td><td>{psr}</td></tr>
      <tr><td>Recovery Factor</td><td>{recovery}</td></tr>
      <tr><td>Alpha</td><td>{alpha}</td></tr>
      <tr><td>Beta</td><td>{beta}</td></tr>
      <tr><td>Treynor Ratio</td><td>{treynor}</td></tr>
    </table>
  </div>
  <div class="stats-card">
    <h3>Trade Statistics</h3>
    <table>
      <tr><td>Equity Round-Trips</td><td>{total_trades}</td></tr>
      <tr><td>Win Rate</td><td>{win_rate}</td></tr>
      <tr><td>Profit / Loss Ratio</td><td>{pl_ratio}</td></tr>
      <tr><td>Expectancy</td><td>{expectancy}</td></tr>
      <tr><td>Total Net Profit</td><td>{net_profit}</td></tr>
      <tr><td>Average Win</td><td>{avg_win}</td></tr>
      <tr><td>Average Loss</td><td>{avg_loss}</td></tr>
      <tr><td>Largest Win</td><td>{lg_win}</td></tr>
      <tr><td>Largest Loss</td><td>{lg_loss}</td></tr>
      <tr><td>Max Consec. Wins</td><td>{max_cons_wins}</td></tr>
      <tr><td>Max Consec. Losses</td><td>{max_cons_losses}</td></tr>
      <tr><td>Avg Trade Duration</td><td>{avg_dur:.1} days</td></tr>
    </table>
  </div>
</div>

<div class="footer">Lean-Rust Backtest Engine &nbsp;|&nbsp; Cascade Labs</div>

<script>
const DATES  = [{dates_json}];
const EQUITY = [{equity_json}];
const DD     = [{drawdown_json}];

const chartOpts = (label, color, data, fill) => ({{
  type: 'line',
  data: {{ labels: DATES, datasets: [{{ label, data, borderColor: color,
    backgroundColor: fill ? color + '22' : 'transparent',
    borderWidth: 1.5, pointRadius: 0, fill }}] }},
  options: {{
    animation: false, responsive: true, maintainAspectRatio: true,
    interaction: {{ mode: 'index', intersect: false }},
    plugins: {{ legend: {{ display: false }}, tooltip: {{
      callbacks: {{ label: ctx => ' ' + ctx.parsed.y.toFixed(2) }}
    }} }},
    scales: {{
      x: {{ ticks: {{ maxTicksLimit: 6, color: '#666' }}, grid: {{ color: '#1a1d2e' }} }},
      y: {{ ticks: {{ color: '#666' }}, grid: {{ color: '#1a1d2e' }} }}
    }}
  }}
}});

new Chart(document.getElementById('equityChart'), chartOpts('Portfolio Value', '#4caf50', EQUITY, true));
new Chart(document.getElementById('ddChart'),     chartOpts('Drawdown %',      '#f44336', DD,     true));
</script>
</body>
</html>"#,
        trading_days   = r.trading_days,
        starting_cash  = r.starting_cash,
        return_color   = return_color,
        final_value    = r.final_value,
        total_return   = r.total_return * 100.0,
        kpi_cards          = kpi_cards(r),
        monthly_html       = monthly_html,
        custom_charts_html = custom_charts_html,
        total_return_pct = pct(r.total_return),
        cagr           = pct(cagr),
        ann_std        = pct(ann_std),
        drawdown       = pct(drawdown),
        sharpe         = ratio(sharpe),
        sortino        = ratio(sortino),
        calmar         = ratio(calmar),
        omega          = ratio(omega),
        psr            = pct(psr),
        recovery       = ratio(recovery),
        alpha          = pct(alpha),
        beta           = ratio(beta),
        treynor        = ratio(treynor),
        total_trades   = s.total_trades,
        win_rate       = pct(win_rate),
        pl_ratio       = ratio(pl_ratio),
        expectancy     = dollar(expectancy),
        net_profit     = dollar(net_profit),
        avg_win        = dollar(avg_win),
        avg_loss       = dollar(avg_loss),
        lg_win         = dollar(lg_win),
        lg_loss        = dollar(lg_loss),
        max_cons_wins  = s.max_consecutive_wins,
        max_cons_losses= s.max_consecutive_losses,
        avg_dur        = avg_dur,
        dates_json     = dates_json,
        equity_json    = equity_json,
        drawdown_json  = drawdown_json,
    )
}

fn kpi_cards(r: &BacktestResult) -> String {
    use rust_decimal::prelude::ToPrimitive;
    let s = &r.statistics;
    let cagr    = s.compounding_annual_return.to_f64().unwrap_or(0.0);
    let sharpe  = s.sharpe_ratio.to_f64().unwrap_or(0.0);
    let dd      = s.drawdown.to_f64().unwrap_or(0.0);
    let wr      = s.win_rate.to_f64().unwrap_or(0.0);

    let card = |label: &str, value: &str, class: &str| -> String {
        format!(
            r#"<div class="kpi"><div class="label">{}</div><div class="value {}">{}</div></div>"#,
            label, class, value
        )
    };

    let color = |v: f64| if v >= 0.0 { "pos" } else { "neg" };

    vec![
        card("Total Return",  &format!("{:+.2}%", r.total_return * 100.0), color(r.total_return)),
        card("CAGR",          &format!("{:+.2}%", cagr * 100.0),           color(cagr)),
        card("Sharpe",        &format!("{:.3}", sharpe),                    color(sharpe)),
        card("Max Drawdown",  &format!("-{:.2}%", dd * 100.0),             "neg"),
        card("Win Rate",      &format!("{:.1}%", wr * 100.0),              "neu"),
        card("Equity Legs",   &s.total_trades.to_string(),                  "neu"),
    ].join("\n  ")
}

fn build_monthly_heatmap(monthly: &BTreeMap<(i32, u32), f64>) -> String {
    if monthly.is_empty() {
        return "<p style=\"color:#666\">No monthly data.</p>".to_string();
    }

    let years: Vec<i32> = {
        let mut ys: Vec<i32> = monthly.keys().map(|(y, _)| *y).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        ys.sort();
        ys
    };

    let month_names = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];

    let mut html = String::from("<table class=\"hm-table\"><tr><th>Year</th>");
    for m in &month_names { html.push_str(&format!("<th>{}</th>", m)); }
    html.push_str("<th>Total</th></tr>");

    for year in &years {
        html.push_str(&format!("<tr><td style=\"color:#aaa;font-weight:600\">{}</td>", year));
        let mut year_ret = 1.0f64;
        for month in 1u32..=12 {
            if let Some(&ret) = monthly.get(&(*year, month)) {
                let color = ret_color(ret);
                html.push_str(&format!(
                    "<td style=\"background:{};color:#fff\">{:+.1}%</td>",
                    color, ret
                ));
                year_ret *= 1.0 + ret / 100.0;
            } else {
                html.push_str("<td style=\"color:#444\">—</td>");
            }
        }
        let annual_pct = (year_ret - 1.0) * 100.0;
        let ycolor = ret_color(annual_pct);
        html.push_str(&format!(
            "<td style=\"background:{};color:#fff;font-weight:600\">{:+.1}%</td></tr>",
            ycolor, annual_pct
        ));
    }
    html.push_str("</table>");
    html
}

/// Generate HTML + Chart.js blocks for all custom strategy charts.
fn build_custom_charts(charts: &ChartCollection) -> String {
    if charts.charts.is_empty() {
        return String::new();
    }

    // Colour palette — cycled per series within each chart.
    const PALETTE: &[&str] = &[
        "#2196F3", "#F44336", "#4CAF50", "#FF9800",
        "#9C27B0", "#00BCD4", "#FF5722",
    ];

    let mut html = String::new();
    html.push_str("<div class=\"custom-charts\">\n");

    // Sort chart names for deterministic output.
    let mut chart_names: Vec<&String> = charts.charts.keys().collect();
    chart_names.sort();

    for (chart_idx, chart_name) in chart_names.iter().enumerate() {
        let chart = &charts.charts[*chart_name];

        // Sort series names for deterministic output.
        let mut series_names: Vec<&String> = chart.series.keys().collect();
        series_names.sort();

        if series_names.is_empty() { continue; }

        let canvas_id = format!("customChart_{}", chart_idx);

        html.push_str(&format!(
            "<div class=\"chart-card\" style=\"margin-bottom:20px\">\n  <h3>{}</h3>\n  <canvas id=\"{}\"></canvas>\n</div>\n",
            escape_html(chart_name), canvas_id
        ));

        // Build datasets array.
        let mut datasets_js = String::new();
        for (series_idx, series_name) in series_names.iter().enumerate() {
            let series = &chart.series[*series_name];
            let color = series.color.as_deref()
                .unwrap_or(PALETTE[series_idx % PALETTE.len()]);

            // Build labels and data arrays from sorted points.
            let mut points = series.points.clone();
            points.sort_by(|a, b| a.time.cmp(&b.time));

            let labels_js: String = points.iter()
                .map(|p| format!("\"{}\"", p.time))
                .collect::<Vec<_>>()
                .join(",");
            let data_js: String = points.iter()
                .map(|p| format!("{:.6}", p.value))
                .collect::<Vec<_>>()
                .join(",");

            // Determine Chart.js type string.
            let chart_type = match series.series_type {
                SeriesType::Bar => "bar",
                SeriesType::Scatter => "scatter",
                _ => "line",
            };

            if series_idx > 0 { datasets_js.push(','); }
            datasets_js.push_str(&format!(
                r#"{{
  label: "{}",
  type: "{}",
  data: [{}],
  labels_override: [{}],
  borderColor: "{}",
  backgroundColor: "{}22",
  borderWidth: 1.5,
  pointRadius: 0,
  fill: false
}}"#,
                escape_js(series_name), chart_type,
                data_js, labels_js,
                color, color
            ));
        }

        // All series share the same label set (use first series' sorted times).
        let first_series_name = series_names[0];
        let first_series = &chart.series[first_series_name];
        let mut first_points = first_series.points.clone();
        first_points.sort_by(|a, b| a.time.cmp(&b.time));
        let shared_labels: String = first_points.iter()
            .map(|p| format!("\"{}\"", p.time))
            .collect::<Vec<_>>()
            .join(",");

        html.push_str(&format!(
            r#"<script>
(function() {{
  var datasets = [{datasets}];
  // Normalize all series to the shared label timeline.
  var labels = [{labels}];
  datasets.forEach(function(ds) {{
    var map = {{}};
    ds.labels_override.forEach(function(l, i) {{ map[l] = ds.data[i]; }});
    ds.data = labels.map(function(l) {{ return map[l] !== undefined ? map[l] : null; }});
    delete ds.labels_override;
  }});
  new Chart(document.getElementById('{canvas_id}'), {{
    type: 'line',
    data: {{ labels: labels, datasets: datasets }},
    options: {{
      animation: false,
      responsive: true,
      maintainAspectRatio: true,
      interaction: {{ mode: 'index', intersect: false }},
      plugins: {{
        legend: {{ display: true, labels: {{ color: '#aaa' }} }},
        tooltip: {{ callbacks: {{ label: function(ctx) {{ return ' ' + ctx.dataset.label + ': ' + (ctx.parsed.y !== null ? ctx.parsed.y.toFixed(4) : 'N/A'); }} }} }}
      }},
      scales: {{
        x: {{ ticks: {{ maxTicksLimit: 8, color: '#666' }}, grid: {{ color: '#1a1d2e' }} }},
        y: {{ ticks: {{ color: '#666' }}, grid: {{ color: '#1a1d2e' }} }}
      }}
    }}
  }});
}})();
</script>
"#,
            datasets  = datasets_js,
            labels    = shared_labels,
            canvas_id = canvas_id,
        ));
    }

    html.push_str("</div>\n");
    html
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_js(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Map a return percentage to an RGBA background color.
fn ret_color(pct: f64) -> String {
    let clamped = pct.clamp(-10.0, 10.0);
    if clamped >= 0.0 {
        let intensity = (clamped / 10.0 * 180.0) as u8;
        format!("rgba(76,175,80,{:.2})", intensity as f64 / 255.0 + 0.1)
    } else {
        let intensity = ((-clamped) / 10.0 * 180.0) as u8;
        format!("rgba(244,67,54,{:.2})", intensity as f64 / 255.0 + 0.1)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lean_statistics::PortfolioStatistics;
    use rust_decimal_macros::dec;

    /// Construct a minimal `BacktestResult` suitable for testing output writers.
    fn make_result() -> BacktestResult {
        let stats = PortfolioStatistics::compute(
            &[dec!(100_000), dec!(101_000), dec!(102_000)],
            &[],
            &[],
            2,
            dec!(100_000),
            dec!(0),
        );
        BacktestResult {
            trading_days:             2,
            final_value:              102_000.0,
            total_return:             0.02,
            starting_cash:            100_000.0,
            equity_curve:             vec![100_000.0, 101_000.0, 102_000.0],
            daily_dates:              vec![
                "2026-01-02".to_string(),
                "2026-01-03".to_string(),
                "2026-01-04".to_string(),
            ],
            statistics:               stats,
            charts:                   crate::charting::ChartCollection::default(),
            order_events:             vec![],
            succeeded_data_requests:  vec!["SPY/2026-01-02".to_string()],
            failed_data_requests:     vec!["SPY/2026-01-05".to_string()],
            backtest_id:              1_744_000_000,
            benchmark_symbol:         "SPY".to_string(),
        }
    }

    // ── write_results_json ─────────────────────────────────────────────────────

    #[test]
    fn test_results_json_creates_file_with_expected_keys() {
        let dir = tempfile::tempdir().unwrap();
        let result = make_result();
        let id = result.backtest_id;
        let path = dir.path().join(format!("{id}.json"));

        write_results_json(&result, &path).unwrap();

        assert!(path.exists(), "{id}.json should exist");
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v.get("Statistics").is_some(),  "missing 'Statistics' key");
        assert!(v.get("Equity").is_some(),       "missing 'Equity' key");
        assert!(v.get("TradingDays").is_some(),  "missing 'TradingDays' key");
        assert!(v.get("TotalReturn").is_some(),  "missing 'TotalReturn' key");
    }

    // ── write_order_events_json ────────────────────────────────────────────────

    #[test]
    fn test_order_events_json_is_valid_array() {
        let dir = tempfile::tempdir().unwrap();
        let result = make_result();
        let id = result.backtest_id;
        let path = dir.path().join(format!("{id}-order-events.json"));

        write_order_events_json(&result, &path).unwrap();

        assert!(path.exists(), "{id}-order-events.json should exist");
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v.is_array(), "order-events.json must be a JSON array");
    }

    // ── write_summary_json ─────────────────────────────────────────────────────

    #[test]
    fn test_summary_json_contains_required_keys() {
        let dir = tempfile::tempdir().unwrap();
        let result = make_result();
        let id = result.backtest_id;
        let path = dir.path().join(format!("{id}-summary.json"));

        write_summary_json(&result, &path).unwrap();

        assert!(path.exists(), "{id}-summary.json should exist");
        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();

        let required_keys = [
            "TotalReturn", "CAGR", "SharpeRatio", "SortinoRatio",
            "MaxDrawdown", "CalmarRatio", "WinRate", "Expectancy",
            "TotalNetProfit", "TotalTrades", "StartingCash", "FinalValue",
            "TradingDays",
        ];
        for key in &required_keys {
            assert!(v.get(key).is_some(), "summary.json missing key '{key}'");
        }
    }

    // ── write_log_txt ──────────────────────────────────────────────────────────

    #[test]
    fn test_log_txt_contains_backtest_id() {
        let dir = tempfile::tempdir().unwrap();
        let result = make_result();
        let id = result.backtest_id;
        let path = dir.path().join(format!("{id}-log.txt"));

        write_log_txt(&result, &path).unwrap();

        assert!(path.exists(), "{id}-log.txt should exist");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(&id.to_string()),
            "log should contain the backtest ID"
        );
    }

    // ── write_data_request_files ───────────────────────────────────────────────

    #[test]
    fn test_data_request_files_created() {
        let dir = tempfile::tempdir().unwrap();
        let result = make_result();

        let succeeded = dir.path().join("succeeded-data-requests-20260401051450171.txt");
        let failed    = dir.path().join("failed-data-requests-20260401051450171.txt");

        write_data_request_files(&result, &succeeded, &failed).unwrap();

        assert!(succeeded.exists(), "succeeded-data-requests file should exist");
        assert!(failed.exists(),    "failed-data-requests file should exist");

        let succ_content = std::fs::read_to_string(&succeeded).unwrap();
        assert!(succ_content.contains("SPY/2026-01-02"), "succeeded file should list SPY data");

        let fail_content = std::fs::read_to_string(&failed).unwrap();
        assert!(fail_content.contains("SPY/2026-01-05"), "failed file should list missing data");
    }

    // ── full output directory structure ───────────────────────────────────────

    #[test]
    fn test_all_expected_output_files_created() {
        // Verify that the complete set of LEAN-compatible output files can be
        // created in a single output directory, mirroring what main.rs produces.
        let dir = tempfile::tempdir().unwrap();
        let result = make_result();
        let id = result.backtest_id;
        let ts_ms = "20260410120000000";

        write_results_json(&result,      &dir.path().join(format!("{id}.json"))).unwrap();
        write_order_events_json(&result, &dir.path().join(format!("{id}-order-events.json"))).unwrap();
        write_summary_json(&result,      &dir.path().join(format!("{id}-summary.json"))).unwrap();
        write_log_txt(&result,           &dir.path().join(format!("{id}-log.txt"))).unwrap();
        std::fs::copy(
            dir.path().join(format!("{id}-log.txt")),
            dir.path().join("log.txt"),
        ).unwrap();
        write_data_request_files(
            &result,
            &dir.path().join(format!("succeeded-data-requests-{ts_ms}.txt")),
            &dir.path().join(format!("failed-data-requests-{ts_ms}.txt")),
        ).unwrap();
        write_report(&result, &dir.path().join("report.html")).unwrap();

        let expected = [
            format!("{id}.json"),
            format!("{id}-order-events.json"),
            format!("{id}-summary.json"),
            format!("{id}-log.txt"),
            "log.txt".to_string(),
            format!("succeeded-data-requests-{ts_ms}.txt"),
            format!("failed-data-requests-{ts_ms}.txt"),
            "report.html".to_string(),
        ];

        for name in &expected {
            let p = dir.path().join(name);
            assert!(p.exists(), "expected output file '{}' not found", name);
        }
    }
}
