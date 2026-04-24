use lean_core::{Market, Symbol};
use lean_execution::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, ImmediateExecutionModel,
    NullExecutionModel, SecurityData, SpreadExecutionModel, StandardDeviationExecutionModel,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_symbol(ticker: &str) -> Symbol {
    Symbol::create_equity(ticker, &Market::usa())
}

fn make_security(ticker: &str, price: f64, current_qty: f64) -> SecurityData {
    SecurityData {
        symbol: make_symbol(ticker),
        price: Decimal::try_from(price).unwrap(),
        bid: None,
        ask: None,
        volume: None,
        average_volume: None,
        daily_std_dev: None,
        current_quantity: Decimal::try_from(current_qty).unwrap(),
    }
}

fn make_target(ticker: &str, qty: f64) -> ExecutionTarget {
    ExecutionTarget {
        symbol: make_symbol(ticker),
        quantity: Decimal::try_from(qty).unwrap(),
    }
}

fn securities_map(data: Vec<SecurityData>) -> HashMap<String, SecurityData> {
    data.into_iter()
        .map(|s| (s.symbol.value.clone(), s))
        .collect()
}

// ---------------------------------------------------------------------------
// ImmediateExecutionModel tests
// ---------------------------------------------------------------------------
// Mirrors C# ImmediateExecutionModelTests

mod immediate_execution_tests {
    use super::*;

    /// No targets provided → no orders submitted.
    /// Mirrors: OrdersAreNotSubmittedWhenNoTargetsToExecute
    #[test]
    fn no_targets_no_orders() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let orders = model.execute(&[], &securities);
        assert!(orders.is_empty(), "Expected no orders for empty targets");
    }

    /// Target qty=100, current=0 → market buy 100.
    /// Mirrors: OrdersAreSubmittedImmediatelyForTargetsToExecute (openOrdersQuantity=0, qty=10)
    #[test]
    fn no_position_buy_order() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(100));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
        assert!(
            orders[0].quantity > Decimal::ZERO,
            "Should be a buy (positive qty)"
        );
    }

    /// Target=100, current=60 → market buy 40 (delta only).
    /// Mirrors: OrdersAreSubmittedImmediatelyForTargetsToExecute (openOrdersQuantity=3, expectedTotalQuantity=7)
    #[test]
    fn partial_position_delta_order() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 60.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(40));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    /// Target=100, current=100 → no order (already at target).
    #[test]
    fn already_at_target_no_order() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 100.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &securities);

        assert!(
            orders.is_empty(),
            "No order should be generated when already at target"
        );
    }

    /// Target=-100, current=0 → market sell 100 (short).
    #[test]
    fn short_position_sell_order() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", -100.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(-100));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
        assert!(
            orders[0].quantity < Decimal::ZERO,
            "Should be a sell (negative qty)"
        );
    }

    /// Target=0, current=50 → market sell 50 (liquidate).
    /// Mirrors: liquidation semantics (PortfolioTarget quantity = 0)
    #[test]
    fn liquidate_target_zero() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 50.0)]);
        let targets = vec![make_target("AAPL", 0.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(-50));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    /// Multiple targets in one call — each should produce its own delta order.
    #[test]
    fn multiple_targets_produce_multiple_orders() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![
            make_security("AAPL", 250.0, 0.0),
            make_security("MSFT", 300.0, 20.0),
        ]);
        let targets = vec![make_target("AAPL", 10.0), make_target("MSFT", 30.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 2);

        let aapl_order = orders.iter().find(|o| o.symbol.value == "AAPL").unwrap();
        assert_eq!(aapl_order.quantity, dec!(10));

        let msft_order = orders.iter().find(|o| o.symbol.value == "MSFT").unwrap();
        assert_eq!(msft_order.quantity, dec!(10)); // 30 - 20 = 10
    }

    /// Security not present in securities map → treats current_qty as 0 and still orders.
    #[test]
    fn unknown_security_defaults_current_qty_to_zero() {
        let mut model = ImmediateExecutionModel::new();
        // Provide an empty securities map (security data missing for AAPL)
        let securities: HashMap<String, SecurityData> = HashMap::new();
        let targets = vec![make_target("AAPL", 50.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(50));
    }

    /// Verifies the order type is always Market for ImmediateExecutionModel.
    /// Mirrors: market order type expectation in C# tests
    #[test]
    fn order_type_is_always_market() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 42.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    /// Partially filled scenario: current=70 (partial fill of a 100 order), target=80.
    /// Delta = 80 - 70 = 10. Remaining open order qty (30) is not tracked here (handled
    /// at the broker layer), but holdings should be accounted for.
    /// Mirrors: PartiallyFilledOrdersAreTakenIntoAccount
    #[test]
    fn partial_fill_remaining_delta_ordered() {
        let mut model = ImmediateExecutionModel::new();
        // current_quantity reflects filled holdings = 70
        let securities = securities_map(vec![make_security("AAPL", 250.0, 70.0)]);
        let targets = vec![make_target("AAPL", 80.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(10)); // 80 - 70 = 10
    }

    /// Second execute call with a higher target: incremental delta ordered.
    /// Mirrors: NonFilledAsyncOrdersAreTakenIntoAccount
    #[test]
    fn incremental_target_increase_ordered_as_delta() {
        let mut model = ImmediateExecutionModel::new();
        // First call: target=80, current=0 → order 80
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let first_targets = vec![make_target("AAPL", 80.0)];
        let first_orders = model.execute(&first_targets, &securities);
        assert_eq!(first_orders.len(), 1);
        assert_eq!(first_orders[0].quantity, dec!(80));

        // Second call: target=100, current still 0 (order not yet filled) → order 100
        let second_targets = vec![make_target("AAPL", 100.0)];
        let second_orders = model.execute(&second_targets, &securities);
        assert_eq!(second_orders.len(), 1);
        assert_eq!(second_orders[0].quantity, dec!(100)); // 100 - 0 (ImmediateModel is stateless)
    }

    /// Tag should identify the model that generated the order.
    #[test]
    fn order_tag_identifies_model() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert!(
            orders[0].tag.contains("ImmediateExecutionModel"),
            "Tag should identify model, got: {}",
            orders[0].tag
        );
    }

    /// Symbol on the order should match the target symbol.
    #[test]
    fn order_symbol_matches_target() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].symbol.value, "AAPL");
    }

    /// on_securities_changed should not panic (it's a no-op on ImmediateExecutionModel).
    #[test]
    fn on_securities_changed_does_not_panic() {
        let mut model = ImmediateExecutionModel::new();
        let added = vec![make_symbol("AAPL")];
        let removed = vec![make_symbol("MSFT")];
        model.on_securities_changed(&added, &removed);
    }

    /// model.name() returns the expected string.
    #[test]
    fn model_name_is_correct() {
        let model = ImmediateExecutionModel::new();
        assert_eq!(model.name(), "ImmediateExecutionModel");
    }
}

// ---------------------------------------------------------------------------
// NullExecutionModel tests
// ---------------------------------------------------------------------------

mod null_execution_tests {
    use super::*;

    /// NullExecutionModel always returns empty orders regardless of targets.
    #[test]
    fn null_returns_empty_for_no_targets() {
        let mut model = NullExecutionModel::new();
        let securities: HashMap<String, SecurityData> = HashMap::new();
        let orders = model.execute(&[], &securities);
        assert!(orders.is_empty());
    }

    /// NullExecutionModel returns empty even when targets are provided.
    #[test]
    fn null_returns_empty_for_valid_targets() {
        let mut model = NullExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &securities);

        assert!(
            orders.is_empty(),
            "NullExecutionModel should never emit orders"
        );
    }

    /// NullExecutionModel: multiple targets → still empty.
    #[test]
    fn null_returns_empty_for_multiple_targets() {
        let mut model = NullExecutionModel::new();
        let securities = securities_map(vec![
            make_security("AAPL", 250.0, 0.0),
            make_security("MSFT", 300.0, 50.0),
        ]);
        let targets = vec![make_target("AAPL", 100.0), make_target("MSFT", 200.0)];

        let orders = model.execute(&targets, &securities);

        assert!(orders.is_empty());
    }

    /// on_securities_changed should not panic.
    #[test]
    fn null_on_securities_changed_does_not_panic() {
        let mut model = NullExecutionModel::new();
        model.on_securities_changed(&[make_symbol("AAPL")], &[]);
    }

    /// model.name() returns expected string.
    #[test]
    fn null_model_name_is_correct() {
        let model = NullExecutionModel::new();
        assert_eq!(model.name(), "NullExecutionModel");
    }
}

// ---------------------------------------------------------------------------
// ExecutionOrderType tests
// ---------------------------------------------------------------------------

mod order_type_tests {
    use super::*;

    /// ImmediateExecutionModel always produces Market orders.
    #[test]
    fn immediate_produces_market_orders() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 50.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    /// ExecutionOrderType variants are distinct.
    #[test]
    fn order_type_variants_are_distinct() {
        assert_ne!(ExecutionOrderType::Market, ExecutionOrderType::Limit);
        assert_ne!(ExecutionOrderType::Market, ExecutionOrderType::MarketOnOpen);
        assert_ne!(
            ExecutionOrderType::Market,
            ExecutionOrderType::MarketOnClose
        );
        assert_ne!(ExecutionOrderType::Limit, ExecutionOrderType::MarketOnOpen);
        assert_ne!(ExecutionOrderType::Limit, ExecutionOrderType::MarketOnClose);
        assert_ne!(
            ExecutionOrderType::MarketOnOpen,
            ExecutionOrderType::MarketOnClose
        );
    }

    /// ExecutionOrderType copies correctly.
    #[test]
    fn order_type_copy() {
        let ot = ExecutionOrderType::Market;
        let ot2 = ot; // Copy trait
        assert_eq!(ot, ot2);
    }
}

// ---------------------------------------------------------------------------
// ExecutionTarget / SecurityData struct tests
// ---------------------------------------------------------------------------

mod struct_tests {
    use super::*;

    /// ExecutionTarget holds symbol and quantity correctly.
    #[test]
    fn execution_target_fields() {
        let t = make_target("AAPL", 42.5);
        assert_eq!(t.symbol.value, "AAPL");
        assert_eq!(t.quantity, Decimal::try_from(42.5).unwrap());
    }

    /// SecurityData holds all fields correctly.
    #[test]
    fn security_data_fields() {
        let s = SecurityData {
            symbol: make_symbol("MSFT"),
            price: dec!(300),
            bid: Some(dec!(299.5)),
            ask: Some(dec!(300.5)),
            volume: Some(dec!(1000000)),
            average_volume: Some(dec!(900000)),
            daily_std_dev: Some(dec!(5)),
            current_quantity: dec!(50),
        };
        assert_eq!(s.symbol.value, "MSFT");
        assert_eq!(s.price, dec!(300));
        assert_eq!(s.bid, Some(dec!(299.5)));
        assert_eq!(s.ask, Some(dec!(300.5)));
        assert_eq!(s.current_quantity, dec!(50));
    }

    /// OrderRequest limit_price is None for market orders from ImmediateExecutionModel.
    #[test]
    fn immediate_order_has_no_limit_price() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];
        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert!(orders[0].limit_price.is_none());
    }
}

// ---------------------------------------------------------------------------
// SpreadExecutionModel tests
// ---------------------------------------------------------------------------
// Based on C# SpreadExecutionModelTests:
// - Orders are not submitted when no targets provided.
// - Orders are submitted when spread <= acceptingSpreadPercent (bid/ask tight).
// - Orders are deferred when spread > acceptingSpreadPercent (bid/ask wide).
// - Pending targets are retried on subsequent execute calls.
// - on_securities_changed removes pending targets for removed symbols.

mod spread_execution_tests {
    use super::*;

    fn make_security_with_quotes(
        ticker: &str,
        price: f64,
        bid: f64,
        ask: f64,
        current_qty: f64,
    ) -> SecurityData {
        SecurityData {
            symbol: make_symbol(ticker),
            price: Decimal::try_from(price).unwrap(),
            bid: Some(Decimal::try_from(bid).unwrap()),
            ask: Some(Decimal::try_from(ask).unwrap()),
            volume: None,
            average_volume: None,
            daily_std_dev: None,
            current_quantity: Decimal::try_from(current_qty).unwrap(),
        }
    }

    /// No targets → no orders.
    /// Mirrors: OrdersAreNotSubmittedWhenNoTargetsToExecute
    #[test]
    fn no_targets_no_orders() {
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 249.0, 251.0, 0.0,
        )]);
        let orders = model.execute(&[], &securities);
        assert!(orders.is_empty(), "Expected no orders for empty targets");
    }

    /// Tight spread (ask == bid == price) → order submitted.
    /// Mirrors: OrdersAreSubmittedWhenRequiredForTargetsToExecute (currentPrice=240, expectedOrders=1)
    #[test]
    fn tight_spread_submits_order() {
        // price=250, bid=250, ask=250 → spread = 0/250 = 0% <= 0.5%
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 250.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1, "Tight spread should submit order");
        assert_eq!(orders[0].quantity, dec!(10));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    /// Wide spread → order deferred (not submitted).
    /// Mirrors: OrdersAreSubmittedWhenRequiredForTargetsToExecute (currentPrice=250, ask=250*1.1, expectedOrders=0)
    #[test]
    fn wide_spread_defers_order() {
        // price=250, bid=250, ask=275 (10% above) → spread = 25/250 = 10% > 0.5%
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 275.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert!(
            orders.is_empty(),
            "Wide spread should defer order, got: {:?}",
            orders
        );
    }

    /// Deferred order retried when spread tightens.
    #[test]
    fn wide_spread_then_tight_spread_submits() {
        let mut model = SpreadExecutionModel::default();

        // First call: wide spread → deferred
        let wide_sec = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 275.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];
        let first_orders = model.execute(&targets, &wide_sec);
        assert!(first_orders.is_empty(), "Should defer on wide spread");

        // Second call: tight spread → order submitted for the pending target
        let tight_sec = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 249.75, 250.25, 0.0,
        )]);
        let second_orders = model.execute(&[], &tight_sec); // no new targets, retry pending
        assert_eq!(second_orders.len(), 1, "Should submit on tight spread");
        assert_eq!(second_orders[0].quantity, dec!(10));
    }

    /// Spread exactly at threshold should be accepted (<=).
    #[test]
    fn spread_exactly_at_threshold_accepted() {
        // acceptingSpreadPercent=0.005, price=200, bid=199, ask=200
        // spread = (200 - 199) / 200 = 0.005 — exactly at threshold
        let mut model = SpreadExecutionModel::new(dec!(0.005));
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 200.0, 199.0, 200.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 5.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(
            orders.len(),
            1,
            "Spread exactly at threshold should be accepted"
        );
    }

    /// Spread just above threshold should be deferred.
    #[test]
    fn spread_just_above_threshold_deferred() {
        // acceptingSpreadPercent=0.005, price=200, bid=198.9, ask=200
        // spread = 1.1 / 200 = 0.0055 > 0.005
        let mut model = SpreadExecutionModel::new(dec!(0.005));
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 200.0, 198.9, 200.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 5.0)];

        let orders = model.execute(&targets, &securities);

        assert!(
            orders.is_empty(),
            "Spread just above threshold should be deferred"
        );
    }

    /// No bid/ask data → execution is allowed (fallback).
    #[test]
    fn no_bid_ask_allows_execution() {
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(
            orders.len(),
            1,
            "No bid/ask should fall back to allowing execution"
        );
    }

    /// Delta ordering: target=100, current=60 → order 40 shares.
    #[test]
    fn delta_only_ordered() {
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 250.0, 60.0,
        )]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(40));
    }

    /// on_securities_changed: removed symbol discards pending order.
    #[test]
    fn removed_security_clears_pending() {
        let mut model = SpreadExecutionModel::default();

        // Queue a pending order by executing with a wide spread
        let wide_sec = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 275.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];
        model.execute(&targets, &wide_sec);

        // Remove the security
        let removed = vec![make_symbol("AAPL")];
        model.on_securities_changed(&[], &removed);

        // Now tighten the spread — should produce no order (pending cleared)
        let tight_sec = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 250.0, 0.0,
        )]);
        let orders = model.execute(&[], &tight_sec);
        assert!(
            orders.is_empty(),
            "Pending should be cleared after security removed"
        );
    }

    /// model.name() returns expected string.
    #[test]
    fn model_name_is_correct() {
        let model = SpreadExecutionModel::default();
        assert_eq!(model.name(), "SpreadExecutionModel");
    }

    /// Order tag identifies the model.
    #[test]
    fn order_tag_identifies_model() {
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 250.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 5.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert!(
            orders[0].tag.contains("SpreadExecutionModel"),
            "Tag should identify model, got: {}",
            orders[0].tag
        );
    }
}

// ---------------------------------------------------------------------------
// StandardDeviationExecutionModel tests
// ---------------------------------------------------------------------------
// Based on C# StandardDeviationExecutionModelTests:
// - Orders are not submitted when no targets provided.
// - Buy orders submitted when bid < SMA - (deviations * std_dev) (price dipped below mean).
// - Sell orders submitted when ask > SMA + (deviations * std_dev) (price spiked above mean).
// - No order when price is within the std dev band.
// - No daily_std_dev → unconditional execution.
// - on_securities_changed removes pending targets for removed symbols.

mod standard_deviation_execution_tests {
    use super::*;

    fn make_security_with_std_dev(
        ticker: &str,
        price: f64,
        bid: f64,
        ask: f64,
        daily_std_dev: f64,
        current_qty: f64,
    ) -> SecurityData {
        SecurityData {
            symbol: make_symbol(ticker),
            price: Decimal::try_from(price).unwrap(),
            bid: Some(Decimal::try_from(bid).unwrap()),
            ask: Some(Decimal::try_from(ask).unwrap()),
            volume: None,
            average_volume: None,
            daily_std_dev: Some(Decimal::try_from(daily_std_dev).unwrap()),
            current_quantity: Decimal::try_from(current_qty).unwrap(),
        }
    }

    /// No targets → no orders.
    /// Mirrors: OrdersAreNotSubmittedWhenNoTargetsToExecute
    #[test]
    fn no_targets_no_orders() {
        let mut model = StandardDeviationExecutionModel::default();
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 245.0, 255.0, 10.0, 0.0,
        )]);
        let orders = model.execute(&[], &securities);
        assert!(orders.is_empty(), "Expected no orders for empty targets");
    }

    /// Buy: bid well below SMA - N*std_dev → order submitted.
    /// Scenario mirrors C#: historicalPrices=[270,260,250], currentPrice=240, deviations=1.5
    /// SMA ≈ 260, STD ≈ 10. Threshold = 260 - 1.5*10 = 245. bid=240 < 245 → buy.
    #[test]
    fn buy_order_when_bid_below_lower_band() {
        // price=260 (proxy SMA), std_dev=10, deviations=1.5 → lower band = 260 - 15 = 245
        // bid=240 < 245 → execute buy
        let mut model = StandardDeviationExecutionModel::new(dec!(1.5));
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 260.0, 240.0, 265.0, 10.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(
            orders.len(),
            1,
            "Should submit buy when bid is below lower band"
        );
        assert!(orders[0].quantity > Decimal::ZERO, "Should be a buy");
    }

    /// Buy: bid within band → order deferred.
    /// Mirrors C#: historicalPrices=[250,250,250], currentPrice=250, expectedOrders=0
    #[test]
    fn no_buy_order_when_bid_within_band() {
        // price=250, std_dev=5, deviations=2 → lower band = 250 - 10 = 240
        // bid=248 > 240 → within band, no order
        let mut model = StandardDeviationExecutionModel::default(); // deviations=2
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 248.0, 252.0, 5.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert!(
            orders.is_empty(),
            "Should not buy when bid is within the band"
        );
    }

    /// Sell: ask well above SMA + N*std_dev → order submitted.
    #[test]
    fn sell_order_when_ask_above_upper_band() {
        // price=250 (SMA proxy), std_dev=10, deviations=2 → upper band = 250 + 20 = 270
        // ask=280 > 270 → execute sell
        let mut model = StandardDeviationExecutionModel::default(); // deviations=2
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 245.0, 280.0, 10.0, 100.0,
        )]);
        let targets = vec![make_target("AAPL", 0.0)]; // sell all (liquidate)

        let orders = model.execute(&targets, &securities);

        assert_eq!(
            orders.len(),
            1,
            "Should submit sell when ask is above upper band"
        );
        assert!(orders[0].quantity < Decimal::ZERO, "Should be a sell");
    }

    /// Sell: ask within band → order deferred.
    #[test]
    fn no_sell_order_when_ask_within_band() {
        // price=250, std_dev=10, deviations=2 → upper band = 270
        // ask=265 < 270 → within band, no order
        let mut model = StandardDeviationExecutionModel::default();
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 245.0, 265.0, 10.0, 100.0,
        )]);
        let targets = vec![make_target("AAPL", 0.0)];

        let orders = model.execute(&targets, &securities);

        assert!(
            orders.is_empty(),
            "Should not sell when ask is within the band"
        );
    }

    /// No daily_std_dev data → unconditional execution.
    #[test]
    fn no_std_dev_allows_unconditional_execution() {
        let mut model = StandardDeviationExecutionModel::default();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(
            orders.len(),
            1,
            "No std_dev data should allow unconditional execution"
        );
    }

    /// Delta ordering: only the unmet quantity is ordered.
    #[test]
    fn delta_only_ordered() {
        // price=250, std_dev=50, deviations=2 → lower band = 250 - 100 = 150
        // bid=140 < 150 → execute buy for delta
        let mut model = StandardDeviationExecutionModel::default();
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 140.0, 260.0, 50.0, 60.0,
        )]);
        let targets = vec![make_target("AAPL", 100.0)]; // need 40 more

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(40));
    }

    /// Deferred order is retried when price moves into favorable range.
    #[test]
    fn deferred_then_favorable_submits() {
        let mut model = StandardDeviationExecutionModel::new(dec!(2.0));

        // First call: bid within band → defer
        // price=250, std_dev=10, band=[230,270], bid=245 (within band)
        let within_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 245.0, 255.0, 10.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];
        let first_orders = model.execute(&targets, &within_sec);
        assert!(first_orders.is_empty(), "Should defer when within band");

        // Second call: bid below band → submit
        // price=250, std_dev=10, lower_band=230, bid=220 → execute
        let favorable_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 220.0, 255.0, 10.0, 0.0,
        )]);
        let second_orders = model.execute(&[], &favorable_sec);
        assert_eq!(
            second_orders.len(),
            1,
            "Should submit when bid falls below lower band"
        );
        assert_eq!(second_orders[0].quantity, dec!(10));
    }

    /// on_securities_changed: removed symbol discards pending order.
    #[test]
    fn removed_security_clears_pending() {
        let mut model = StandardDeviationExecutionModel::default();

        // Queue a pending order (bid within band → deferred)
        let within_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 248.0, 252.0, 5.0, 0.0,
        )]);
        model.execute(&[make_target("AAPL", 10.0)], &within_sec);

        // Remove the security
        model.on_securities_changed(&[], &[make_symbol("AAPL")]);

        // Now provide favorable conditions — should produce no order
        let favorable_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 220.0, 255.0, 10.0, 0.0,
        )]);
        let orders = model.execute(&[], &favorable_sec);
        assert!(
            orders.is_empty(),
            "Pending should be cleared after security removed"
        );
    }

    /// model.name() returns expected string.
    #[test]
    fn model_name_is_correct() {
        let model = StandardDeviationExecutionModel::default();
        assert_eq!(model.name(), "StandardDeviationExecutionModel");
    }

    /// Order tag identifies the model and includes the deviations value.
    #[test]
    fn order_tag_identifies_model() {
        let mut model = StandardDeviationExecutionModel::default();
        // No std_dev → unconditional execution
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 5.0)];

        let orders = model.execute(&targets, &securities);

        assert_eq!(orders.len(), 1);
        assert!(
            orders[0].tag.contains("StandardDeviationExecutionModel"),
            "Tag should identify model, got: {}",
            orders[0].tag
        );
    }
}
