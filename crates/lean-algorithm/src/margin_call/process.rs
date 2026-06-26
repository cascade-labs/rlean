use lean_orders::OrderEvent;
use rust_decimal::Decimal;
use tracing::warn;

use crate::qc_algorithm::QcAlgorithm;
use crate::securities::SecurityManager;

use super::context::{build_margin_call_context as build_context, MarginCallContext};
use super::execute::execute_margin_call_orders;
use super::model::{
    MarginCallExecutionContext, MarginCallModel, MarginCallModelKind, MarginCallOrderRequest,
};

const MARGIN_CALL_FREQUENCY_NS: i64 = 300_000_000_000; // 5 minutes

/// Outcome of a margin-call scan for one time slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginCallScanOutcome {
    Continue,
    StopBankrupt,
}

/// Fill processor used during synchronous margin-call execution.
pub trait MarginCallFillProcessor {
    fn process_fills(&mut self, events: &mut [OrderEvent]);
}

pub fn build_margin_call_context(
    portfolio: &std::sync::Arc<crate::portfolio::SecurityPortfolioManager>,
    algorithm: &QcAlgorithm,
) -> MarginCallContext {
    build_context(portfolio, algorithm)
}

/// Check backtest bankruptcy guard — mirrors `AlgorithmManager.Run` TPV <= 0 stop.
pub fn check_backtest_bankruptcy(is_backtest: bool, total_portfolio_value: Decimal) -> bool {
    is_backtest && total_portfolio_value <= Decimal::ZERO
}

fn all_symbols_exchange_open(
    orders: &[MarginCallOrderRequest],
    securities: &SecurityManager,
    time: lean_core::DateTime,
) -> bool {
    orders.iter().all(|order| {
        securities
            .get(&order.symbol)
            .map(|security| security.exchange_hours.is_open_at(time))
            .unwrap_or(true)
    })
}

/// Run margin call scan for a time slice. Returns `StopBankrupt` when the caller should halt.
pub fn process_margin_call_scan<P: MarginCallFillProcessor>(
    is_backtest: bool,
    is_warming_up: bool,
    portfolio: &std::sync::Arc<crate::portfolio::SecurityPortfolioManager>,
    algorithm: &mut QcAlgorithm,
    order_processor: &lean_orders::order_processor::OrderProcessor,
    model: &MarginCallModelKind,
    time: lean_core::DateTime,
    next_margin_call_time: &mut lean_core::DateTime,
    trade_bars: &std::collections::HashMap<u64, lean_data::TradeBar>,
    quote_bars: &std::collections::HashMap<u64, lean_data::QuoteBar>,
    fill_processor: &mut P,
    on_margin_call: &mut dyn FnMut(&[MarginCallOrderRequest]) -> Vec<MarginCallOrderRequest>,
    on_margin_call_warning: &mut dyn FnMut(),
) -> MarginCallScanOutcome {
    let total_portfolio_value = portfolio.total_portfolio_value();
    if check_backtest_bankruptcy(is_backtest, total_portfolio_value) {
        warn!(
            "AlgorithmManager.Run(): Portfolio value is less than or equal to zero, stopping algorithm."
        );
        return MarginCallScanOutcome::StopBankrupt;
    }

    if !is_backtest || model.is_null() || is_warming_up {
        return MarginCallScanOutcome::Continue;
    }

    if time < *next_margin_call_time {
        return MarginCallScanOutcome::Continue;
    }
    *next_margin_call_time = lean_core::NanosecondTimestamp(time.0 + MARGIN_CALL_FREQUENCY_NS);

    let scan_ctx = build_margin_call_context(portfolio, algorithm);
    let (mut margin_call_orders, issue_warning) = model.get_margin_call_orders(&scan_ctx);
    let total_margin_used = scan_ctx.total_margin_used;

    let mut executed_count = 0usize;
    if !margin_call_orders.is_empty()
        && all_symbols_exchange_open(&margin_call_orders, &algorithm.securities, time)
    {
        margin_call_orders = on_margin_call(&margin_call_orders);
        let mut exec_ctx = MarginCallExecutionContext {
            portfolio,
            algorithm,
            order_processor,
            time,
            trade_bars,
            quote_bars,
            total_margin_used,
        };
        let tickets = execute_margin_call_orders(&mut exec_ctx, margin_call_orders, fill_processor);
        executed_count = tickets.len();
    }

    if executed_count == 0 && issue_warning {
        on_margin_call_warning();
    }

    MarginCallScanOutcome::Continue
}
