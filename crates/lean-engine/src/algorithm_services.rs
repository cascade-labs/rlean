// Engine-owned algorithm service helpers.
//
// Language-neutral mutations of `QcAlgorithm` state that were previously inlined
// in the Python adapter: universe add/remove registration, custom-universe
// leverage metadata, and execution-order-request submission. Language bindings
// only marshal Python objects into Rust types and call these helpers.

use lean_algorithm::algorithm::SecurityChanges;
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{DateTime, Market, Resolution, SecurityType, Symbol};
use lean_data::{CustomDataPoint, Slice};
use lean_execution::{ExecutionOrderType, OrderRequest};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Apply a universe-selection diff to the algorithm: subscribe added symbols at
/// `resolution`, and unsubscribe removed symbols that are neither invested nor
/// serving as an option underlying. Mirrors C# LEAN's UniverseSelection flow.
pub fn register_universe_changes(
    alg: &mut QcAlgorithm,
    added: &[Symbol],
    removed: &[Symbol],
    resolution: Resolution,
) {
    for symbol in added {
        alg.add_security_symbol(symbol.clone(), resolution);
    }
    for symbol in removed {
        if !alg.is_invested(symbol) && !alg.is_option_underlying(symbol) {
            alg.subscription_manager.remove_symbol(symbol);
            alg.securities.remove(symbol);
        }
    }
}

/// Apply one universe-selection result to algorithm state and merge it into the
/// aggregate change set returned for this frontier.
pub fn apply_universe_changes(
    alg: &mut QcAlgorithm,
    merged: &mut SecurityChanges,
    changes: SecurityChanges,
    resolution: Resolution,
) {
    if !changes.has_changes() {
        return;
    }
    register_universe_changes(alg, &changes.added, &changes.removed, resolution);
    merged.added.extend(changes.added);
    merged.removed.extend(changes.removed);
}

/// Apply one custom-universe selection result, including custom data metadata
/// registration, and merge it into the aggregate frontier changes.
pub fn apply_custom_universe_changes(
    alg: &mut QcAlgorithm,
    merged: &mut SecurityChanges,
    changes: SecurityChanges,
    resolution: Resolution,
    custom_data: &HashMap<String, Vec<CustomDataPoint>>,
) {
    if !changes.has_changes() {
        return;
    }
    register_custom_universe_leverage_metadata(alg, &changes.added, custom_data);
    register_universe_changes(alg, &changes.added, &changes.removed, resolution);
    merged.added.extend(changes.added);
    merged.removed.extend(changes.removed);
}

/// Register per-security leverage parsed from custom-universe data points for
/// Hyperliquid crypto futures (other markets/security types are ignored).
pub fn register_custom_universe_leverage_metadata(
    alg: &mut QcAlgorithm,
    symbols: &[Symbol],
    custom_data: &HashMap<String, Vec<CustomDataPoint>>,
) {
    let mut leverage_by_symbol = HashMap::new();
    for point in custom_data.values().flatten() {
        let Some(symbol_value) = point
            .fields
            .get("symbol")
            .and_then(|value| value.as_str())
            .map(|value| value.trim().to_ascii_uppercase())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let Some(leverage) = point.fields.get("max_leverage").and_then(json_to_f64) else {
            continue;
        };
        leverage_by_symbol.insert(symbol_value, leverage);
    }

    for symbol in symbols {
        if symbol.security_type() != SecurityType::CryptoFuture
            || symbol.market().as_str() != Market::HYPERLIQUID
        {
            continue;
        }
        let key = symbol.value.trim().to_ascii_uppercase();
        if let Some(leverage) = leverage_by_symbol.get(&key).copied() {
            alg.register_security_leverage(symbol, leverage);
        }
    }
}

/// Apply the current slice's market prices to algorithm securities before user
/// code and framework models inspect portfolio/security state.
pub fn apply_slice_security_prices(alg: &mut QcAlgorithm, slice: &Slice) {
    for bar in slice.bars.values() {
        alg.securities.update_price(&bar.symbol, bar.close);
        alg.portfolio.update_prices(&bar.symbol, bar.close);
    }
    for quote in slice.quote_bars.values() {
        let bid = quote.bid.as_ref().map(|bar| bar.close).unwrap_or_default();
        let ask = quote.ask.as_ref().map(|bar| bar.close).unwrap_or_default();
        alg.securities.update_quote(&quote.symbol, bid, ask);
        let mid = match (bid, ask) {
            (b, a) if b > rust_decimal::Decimal::ZERO && a > rust_decimal::Decimal::ZERO => {
                (b + a) / rust_decimal::Decimal::from(2u8)
            }
            (b, _) if b > rust_decimal::Decimal::ZERO => b,
            (_, a) if a > rust_decimal::Decimal::ZERO => a,
            _ => rust_decimal::Decimal::ZERO,
        };
        if mid > rust_decimal::Decimal::ZERO {
            alg.portfolio.update_prices(&quote.symbol, mid);
        }
    }
}

/// Advance the canonical algorithm frontier time. This is runner-owned state:
/// SDK/language adapters should expose time as read-only strategy context.
pub fn advance_algorithm_time(alg: &mut QcAlgorithm, time: DateTime) {
    alg.time = time;
    alg.utc_time = time;
}

fn json_to_f64(value: &serde_json::Value) -> Option<f64> {
    if let Some(raw) = value.as_f64() {
        return Some(raw);
    }
    value.as_str()?.trim().parse::<f64>().ok()
}

/// Translate a framework `OrderRequest` into concrete order submissions against
/// the algorithm's transaction manager.
pub fn submit_execution_order_request(alg: &mut QcAlgorithm, request: OrderRequest) {
    if request.order_type == ExecutionOrderType::Update {
        if let Some(order_id) = request.order_id {
            alg.transactions.request_update_order(
                order_id,
                alg.utc_time,
                lean_orders::UpdateOrderFields {
                    quantity: Some(request.quantity),
                    limit_price: request.limit_price,
                    tag: Some(request.tag),
                    ..Default::default()
                },
            );
        }
        return;
    }

    if request.cancel_open_orders {
        for order in alg.transactions.get_open_orders() {
            if order.symbol.id.sid == request.symbol.id.sid {
                alg.transactions.request_cancel_order(
                    order.id,
                    alg.utc_time,
                    format!("{} replace open order", request.tag),
                );
            }
        }
    }

    match request.order_type {
        ExecutionOrderType::Market => {
            alg.market_order_with_options_and_tag(
                &request.symbol,
                request.quantity,
                None,
                false,
                &request.tag,
            );
        }
        ExecutionOrderType::Limit => {
            if let Some(limit_price) = request.limit_price {
                alg.limit_order_with_properties_and_tag(
                    &request.symbol,
                    request.quantity,
                    limit_price,
                    None,
                    false,
                    request.post_only,
                    &request.tag,
                );
            }
        }
        ExecutionOrderType::MarketOnOpen => {
            alg.market_on_open_order(&request.symbol, request.quantity);
        }
        ExecutionOrderType::MarketOnClose => {
            alg.market_on_close_order(&request.symbol, request.quantity);
        }
        ExecutionOrderType::Cancel | ExecutionOrderType::Update => {}
    }
}

/// Submit a batch of framework execution requests against algorithm state.
pub fn submit_execution_order_requests(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    requests: Vec<OrderRequest>,
) {
    if requests.is_empty() {
        return;
    }
    let mut alg = algorithm.lock().unwrap();
    for request in requests {
        submit_execution_order_request(&mut alg, request);
    }
}
