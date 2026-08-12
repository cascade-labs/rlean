//! Live portfolio and insight state for the Verglas run catalog.
//!
//! Backtest and live share the same `BacktestStreamUpdate` → `RunCatalog` path.
//! Live streams account checkpoints to `rlean.checkpoints` and framework insight
//! snapshots to `rlean.insight_state`. A restart loads these independent sources
//! without local sidecar files.

use crate::framework::FrameworkState;
use rlean_algorithm::{portfolio::SecurityPortfolioManager, qc_algorithm::QcAlgorithm};
use rlean_alpha::{Insight, InsightCollectionSnapshot, InsightDirection, InsightType};
use rlean_core::{DateTime, Resolution, Symbol};
use rlean_live::AccountState;
use rlean_orders::order::Order;
use rlean_orders::{OrderEvent, TransactionManager};
use rlean_statistics::Trade;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
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
    pub account_state: Option<AccountState>,
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
}

pub fn build_live_checkpoint(
    time: DateTime,
    portfolio: &SecurityPortfolioManager,
    transactions: &TransactionManager,
    framework: Option<&Arc<Mutex<FrameworkState>>>,
    deploy_id: Option<&str>,
    started_at: chrono::DateTime<chrono::Utc>,
) -> (
    LiveCheckpoint,
    Option<LiveInsightSnapshot>,
    Vec<rlean_alpha::InsightEvent>,
) {
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

    let checkpoint = LiveCheckpoint {
        schema_version: LIVE_CHECKPOINT_SCHEMA_VERSION,
        deployment_id: deploy_id.map(str::to_owned),
        written_at: chrono::Utc::now().to_rfc3339(),
        portfolio: portfolio_payload,
        open_orders,
    };

    (checkpoint, insights, insight_events)
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

/// Reconcile insights emitted while rebuilding model state during warm-up with
/// the durable pre-restart collection.
///
/// <DIV> C# LEAN does not persist framework insights across engine processes.
/// rlean persists them in Verglas, then replays warm-up to rebuild Python and
/// model-local state. The replay can reproduce the same logical insights with
/// new IDs. Keep the restored instance for matching signals so scores and IDs
/// survive, retain genuinely new replay signals, and retain restored event-only
/// signals that historical warm-up cannot reproduce.
pub fn reconcile_live_insights_after_warmup(
    framework: &Arc<Mutex<FrameworkState>>,
    restored: &LiveInsightSnapshot,
    now: DateTime,
) -> Option<(usize, usize)> {
    if restored.schema_version != LIVE_INSIGHTS_SCHEMA_VERSION {
        warn!(
            "live insight warm-up reconciliation: unsupported schema version {}",
            restored.schema_version
        );
        return None;
    }
    let mut framework = framework.lock().ok()?;
    let replayed = framework.insight_snapshot();
    let merged = merge_insight_snapshots(&restored.state, &replayed, now);

    // Warm-up events describe the replayed copies. Replace them with one
    // Restored event set for the reconciled authoritative collection.
    let _ = framework.take_insight_events();
    let active = merged.active.len();
    let closed = merged.closed.len();
    framework.restore_insights(merged, now);
    Some((active, closed))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InsightSemanticKey {
    sid: u64,
    insight_type: u8,
    direction: i8,
    generated_time: i64,
    close_time: i64,
    magnitude: Option<Decimal>,
    confidence: Option<Decimal>,
    weight: Option<Decimal>,
    source_model: String,
}

impl InsightSemanticKey {
    fn from_insight(insight: &Insight) -> Self {
        Self {
            sid: insight.symbol.id.sid,
            insight_type: match insight.insight_type {
                InsightType::Price => 0,
                InsightType::Volatility => 1,
            },
            direction: match insight.direction {
                InsightDirection::Down => -1,
                InsightDirection::Flat => 0,
                InsightDirection::Up => 1,
            },
            generated_time: insight.generated_time_utc.0,
            close_time: insight.close_time_utc.0,
            magnitude: insight.magnitude,
            confidence: insight.confidence,
            weight: insight.weight,
            source_model: insight.source_model.to_string(),
        }
    }
}

fn merge_insight_snapshots(
    restored: &InsightCollectionSnapshot,
    replayed: &InsightCollectionSnapshot,
    now: DateTime,
) -> InsightCollectionSnapshot {
    let restored_by_key: HashMap<InsightSemanticKey, Insight> = restored
        .active
        .iter()
        .chain(restored.closed.iter())
        .cloned()
        .map(|insight| (InsightSemanticKey::from_insight(&insight), insight))
        .collect();
    let mut seen = HashSet::new();
    let mut replayed_duplicates = 0usize;
    let mut active = Vec::with_capacity(replayed.active.len() + restored.active.len());
    let mut closed = Vec::with_capacity(replayed.closed.len() + restored.closed.len());

    for replayed_insight in &replayed.active {
        let key = InsightSemanticKey::from_insight(replayed_insight);
        if !seen.insert(key.clone()) {
            replayed_duplicates += 1;
            continue;
        }
        active.push(
            restored_by_key
                .get(&key)
                .cloned()
                .unwrap_or_else(|| replayed_insight.clone()),
        );
    }
    for replayed_insight in &replayed.closed {
        let key = InsightSemanticKey::from_insight(replayed_insight);
        if !seen.insert(key.clone()) {
            replayed_duplicates += 1;
            continue;
        }
        closed.push(
            restored_by_key
                .get(&key)
                .cloned()
                .unwrap_or_else(|| replayed_insight.clone()),
        );
    }

    let mut restored_only = 0usize;
    for restored_insight in restored.active.iter().chain(restored.closed.iter()) {
        let key = InsightSemanticKey::from_insight(restored_insight);
        if !seen.insert(key) {
            continue;
        }
        restored_only += 1;
        if restored_insight.is_active(now) {
            active.push(restored_insight.clone());
        } else {
            closed.push(restored_insight.clone());
        }
    }

    InsightCollectionSnapshot {
        active,
        closed,
        total_count: replayed
            .total_count
            .saturating_sub(replayed_duplicates)
            .saturating_add(restored_only),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{Market, TimeSpan};
    use rust_decimal_macros::dec;

    #[test]
    fn account_checkpoint_serialization_excludes_insight_state() {
        let checkpoint = LiveCheckpoint {
            schema_version: LIVE_CHECKPOINT_SCHEMA_VERSION,
            deployment_id: Some("live-test".to_string()),
            written_at: "2026-08-12T00:00:00Z".to_string(),
            portfolio: serde_json::json!({"cash": "100000"}),
            open_orders: Vec::new(),
        };

        let json = serde_json::to_value(checkpoint).unwrap();

        assert!(json.get("portfolio").is_some());
        assert!(json.get("open_orders").is_some());
        assert!(json.get("insights").is_none());
        assert!(json.get("state").is_none());
    }

    #[test]
    fn insight_state_round_trips_without_account_state() {
        let now = DateTime::from_secs(2_000_000);
        let insight = Insight::up(
            Symbol::create_equity("SPY", &Market::usa()),
            TimeSpan::from_days(14),
        )
        .with_generated_time_utc(now);
        let snapshot = LiveInsightSnapshot {
            schema_version: LIVE_INSIGHTS_SCHEMA_VERSION,
            deployment_id: Some("live-test".to_string()),
            written_at: "2026-08-12T00:00:00Z".to_string(),
            state: InsightCollectionSnapshot {
                active: vec![insight.clone()],
                closed: Vec::new(),
                total_count: 1,
            },
        };

        let restored: LiveInsightSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();

        assert_eq!(restored.state.active.len(), 1);
        assert_eq!(restored.state.active[0].id, insight.id);
        assert_eq!(restored.state.total_count, 1);
    }

    #[test]
    fn warmup_merge_keeps_restored_identity_without_losing_unique_signals() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let now = DateTime::from_secs(2_000_000);
        let duplicate_time = DateTime::from_secs(1_900_000);
        let mut restored_duplicate = Insight::up(symbol.clone(), TimeSpan::from_days(14))
            .with_generated_time_utc(duplicate_time);
        restored_duplicate.score = Some(dec!(0.75));
        let replayed_duplicate = Insight::up(symbol.clone(), TimeSpan::from_days(14))
            .with_generated_time_utc(duplicate_time);
        assert_ne!(restored_duplicate.id, replayed_duplicate.id);

        let replayed_new = Insight::down(symbol.clone(), TimeSpan::from_days(14))
            .with_generated_time_utc(DateTime::from_secs(1_950_000));
        let mut restored_event = Insight::up(symbol, TimeSpan::from_days(14))
            .with_generated_time_utc(DateTime::from_secs(1_975_000));
        restored_event.source_model = Arc::from("event-only");

        let restored = InsightCollectionSnapshot {
            active: vec![restored_duplicate.clone(), restored_event.clone()],
            closed: Vec::new(),
            total_count: 2,
        };
        let replayed = InsightCollectionSnapshot {
            active: vec![
                replayed_duplicate.clone(),
                replayed_duplicate,
                replayed_new.clone(),
            ],
            closed: Vec::new(),
            total_count: 3,
        };

        let merged = merge_insight_snapshots(&restored, &replayed, now);

        assert_eq!(merged.active.len(), 3);
        assert_eq!(merged.total_count, 3);
        assert!(merged.active.iter().any(
            |insight| insight.id == restored_duplicate.id && insight.score == Some(dec!(0.75))
        ));
        assert!(merged
            .active
            .iter()
            .any(|insight| insight.id == replayed_new.id));
        assert!(merged
            .active
            .iter()
            .any(|insight| insight.id == restored_event.id));
    }
}
