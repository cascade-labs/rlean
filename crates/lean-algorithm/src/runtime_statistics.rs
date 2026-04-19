use lean_core::Price;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Key performance metrics computed at end-of-backtest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeStatistics {
    pub equity: Price,
    pub return_pct: Price,
    pub unrealized_pnl: Price,
    pub fees: Price,
    pub net_profit: Price,
    pub holdings: Price,
    pub volume: Price,
    pub drawdown: Price,
    pub portfolio_turnover: Price,
    pub sharpe_ratio: Option<Price>,
    pub sortino_ratio: Option<Price>,
    pub information_ratio: Option<Price>,
    pub win_rate: Option<Price>,
    pub loss_rate: Option<Price>,
    pub profit_loss_ratio: Option<Price>,
    pub alpha: Option<Price>,
    pub beta: Option<Price>,
    pub annual_std: Option<Price>,
    pub tracking_error: Option<Price>,
    pub treynor_ratio: Option<Price>,
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub extra: HashMap<String, String>,
}
