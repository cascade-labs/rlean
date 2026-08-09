//! Live portfolio / insight state for the Verglas run catalog.
//!
//! Backtest and live share the same `BacktestStreamUpdate` → `RunCatalog` path.
//! Live additionally streams restore checkpoints (`rlean.checkpoints`) so a
//! restarted paper deployment can reload cash, holdings, open orders, and
//! framework insights without local sidecar files.

use crate::framework::FrameworkState;
use rlean_algorithm::{portfolio::SecurityPortfolioManager, qc_algorithm::QcAlgorithm};
use rlean_alpha::InsightCollectionSnapshot;
use rlean_core::{DateTime, Resolution, Symbol};
use rlean_live::AccountState;
use rlean_orders::order::Order;
use rlean_orders::{OrderEvent, TransactionManager};
use rlean_statistics::Trade;
use rust_decimal::Decimal;
use std::sync::{Arc, Mutex};
use tracing::warn;

pub const LIVE_INSIGHTS_SCHEMA_VERSION: u32 = 1;
pub const LIVE_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LiveInsightSnapshot {
    pub schema_version: u32,
    pub deployment_id: Option<String>,
    pub written_at: String,
    pub state: InsightCollectionSnapshot,
}

#[derive(Debug, Clone)]
pub struct LiveRestoreState {
    pub account_state: AccountState,
    pub order_events: Vec<OrderEvent>,
    pub trades: Vec<Trade>,
    pub insights: Option<LiveInsightSnapshot>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LiveCheckpoint {
    pub schema_version: u32,
    pub deployment_id: Option<String>,
    pub written_at: String,
    pub portfolio: serde_json::Value,
    pub open_orders: Vec<Order>,
    pub insights: Option<LiveInsightSnapshot>,
}

pub fn build_live_checkpoint(
    time: DateTime,
    portfolio: &SecurityPortfolioManager,
    transactions: &TransactionManager,
    framework: Option<&Arc<Mutex<FrameworkState>>>,
    deploy_id: Option<&str>,
    started_at: chrono::DateTime<chrono::Utc>,
) -> (LiveCheckpoint, Vec<rlean_alpha::InsightEvent>) {
    let updated_at = chrono::Utc::now().to_rfc3339();
    let holdings = portfolio
        .all_holdings()
        .into_iter()
        .filter(|holding| {
            holding.symbol.security_type() != rlean_core::SecurityType::Base
                || holding.is_invested()
        })
        .map(|holding| {
            serde_json::json!({
                "symbol": holding.symbol.value,
                "symbol_id": holding.symbol,
                "sid": holding.symbol.id.sid,
                "market": holding.symbol.id.market.as_str(),
                "security_type": holding.symbol.security_type().to_string(),
                "quantity": holding.quantity.to_string(),
                "average_price": holding.average_price.to_string(),
                "last_price": holding.last_price.to_string(),
                "market_value": holding.market_value().to_string(),
                "portfolio_value_contribution": holding.portfolio_value_contribution().to_string(),
                "unrealized_pnl": holding.unrealized_pnl.to_string(),
                "realized_pnl": holding.realized_pnl.to_string(),
                "total_fees": holding.total_fees.to_string(),
                "total_funding": holding.total_funding.to_string(),
                "contract_multiplier": holding.contract_multiplier.to_string(),
                "invested": holding.is_invested(),
            })
        })
        .collect::<Vec<_>>();

    let portfolio_payload = serde_json::json!({
        "status": "running",
        "time": time.to_string(),
        "updated_at": updated_at,
        "started_at": started_at.to_rfc3339(),
        "cash": portfolio.cash.read().to_string(),
        "starting_cash": portfolio.starting_cash().to_string(),
        "total_portfolio_value": portfolio.total_portfolio_value().to_string(),
        "total_holdings_value": portfolio.total_holdings_value().to_string(),
        "unrealized_pnl": portfolio.unrealized_profit().to_string(),
        "total_return": portfolio.total_return_pct().to_string(),
        "total_fees": portfolio.total_fees.read().to_string(),
        "total_funding": portfolio.total_funding.read().to_string(),
        "holdings": holdings,
    });

    let open_orders = transactions
        .get_all_orders()
        .into_iter()
        .filter(|order| order.is_open())
        .collect::<Vec<_>>();

    let mut insight_events = Vec::new();
    let insights = framework.and_then(|framework| {
        let Ok(mut fw) = framework.lock() else {
            return None;
        };
        insight_events = fw.take_insight_events();
        Some(LiveInsightSnapshot {
            schema_version: LIVE_INSIGHTS_SCHEMA_VERSION,
            deployment_id: deploy_id.map(str::to_owned),
            written_at: chrono::Utc::now().to_rfc3339(),
            state: fw.insight_snapshot(),
        })
    });

    (
        LiveCheckpoint {
            schema_version: LIVE_CHECKPOINT_SCHEMA_VERSION,
            deployment_id: deploy_id.map(str::to_owned),
            written_at: chrono::Utc::now().to_rfc3339(),
            portfolio: portfolio_payload,
            open_orders,
            insights,
        },
        insight_events,
    )
}

pub fn parse_checkpoint_json(payload: &str) -> Option<LiveCheckpoint> {
    match serde_json::from_str::<LiveCheckpoint>(payload) {
        Ok(checkpoint) if checkpoint.schema_version == LIVE_CHECKPOINT_SCHEMA_VERSION => {
            Some(checkpoint)
        }
        Ok(checkpoint) => {
            warn!(
                "live checkpoint restore: unsupported schema version {}",
                checkpoint.schema_version
            );
            None
        }
        Err(err) => {
            warn!("live checkpoint restore: failed to parse payload: {err:#}");
            None
        }
    }
}

pub fn account_state_from_checkpoint(checkpoint: &LiveCheckpoint) -> Option<AccountState> {
    let portfolio = &checkpoint.portfolio;
    let parse_dec = |value: &serde_json::Value, key: &str| -> Decimal {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| Decimal::from_str_exact(s).ok())
            .unwrap_or(Decimal::ZERO)
    };

    let cash = parse_dec(portfolio, "cash");
    let mut holdings = Vec::new();
    if let Some(items) = portfolio.get("holdings").and_then(|v| v.as_array()) {
        for item in items {
            let Some(symbol_value) = item.get("symbol_id") else {
                warn!("paper restore: holding missing symbol_id, skipping");
                continue;
            };
            let symbol: Symbol = match serde_json::from_value(symbol_value.clone()) {
                Ok(symbol) => symbol,
                Err(err) => {
                    warn!("paper restore: unparseable symbol_id, skipping: {err}");
                    continue;
                }
            };
            let quantity = parse_dec(item, "quantity");
            if quantity.is_zero() {
                continue;
            }
            holdings.push(rlean_brokerages::BrokerageHolding {
                symbol,
                quantity,
                average_price: parse_dec(item, "average_price"),
                market_price: parse_dec(item, "last_price"),
            });
        }
    }

    let positions = holdings
        .iter()
        .map(|holding| (holding.symbol.id.ticker.clone(), holding.quantity))
        .collect();

    Some(AccountState {
        cash,
        cash_balances: vec![("USD".to_string(), cash)],
        positions,
        holdings,
        open_orders: checkpoint.open_orders.clone(),
        last_sync_time: chrono::Utc::now(),
    })
}

pub fn restore_live_insights(
    framework: &Arc<Mutex<FrameworkState>>,
    snapshot: &LiveInsightSnapshot,
) -> Option<(usize, usize)> {
    if snapshot.schema_version != LIVE_INSIGHTS_SCHEMA_VERSION {
        warn!(
            "live insight restore: unsupported schema version {}",
            snapshot.schema_version
        );
        return None;
    }
    let active_count = snapshot.state.active.len();
    let closed_count = snapshot.state.closed.len();
    framework
        .lock()
        .ok()?
        .restore_insights(snapshot.state.clone(), DateTime::now());
    Some((active_count, closed_count))
}

pub fn apply_initial_brokerage_account_state(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    account_state: &AccountState,
) {
    let mut algorithm = algorithm.lock().unwrap();
    if !account_state.cash_balances.is_empty() {
        *algorithm.portfolio.cash.write() = account_state.cash;
    }

    for holding in &account_state.holdings {
        if !algorithm.securities.contains(&holding.symbol) {
            algorithm.add_security_symbol(holding.symbol.clone(), Resolution::Minute);
        }
        let multiplier = algorithm.contract_multiplier_for_symbol(&holding.symbol);
        algorithm.portfolio.set_holdings(
            &holding.symbol,
            holding.average_price,
            holding.quantity,
            multiplier,
        );
        if holding.market_price > Decimal::ZERO {
            algorithm
                .securities
                .update_price(&holding.symbol, holding.market_price);
            algorithm
                .portfolio
                .update_prices(&holding.symbol, holding.market_price);
        }
    }

    for order in &account_state.open_orders {
        algorithm.transactions.add_or_update_order(order.clone());
    }
}

pub fn set_live_starting_portfolio_value_from_synced_account(
    portfolio: &SecurityPortfolioManager,
) -> Decimal {
    let starting_value = portfolio.total_portfolio_value();
    portfolio.set_starting_cash(starting_value);
    starting_value
}
