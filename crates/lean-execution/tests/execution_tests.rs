use lean_core::{Market, Symbol};
use lean_execution::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, ImmediateExecutionModel,
    NullExecutionModel, SecurityData,
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
    data.into_iter().map(|s| (s.symbol.value.clone(), s)).collect()
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
        assert!(orders[0].quantity > Decimal::ZERO, "Should be a buy (positive qty)");
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

        assert!(orders.is_empty(), "No order should be generated when already at target");
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
        assert!(orders[0].quantity < Decimal::ZERO, "Should be a sell (negative qty)");
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

        assert!(orders.is_empty(), "NullExecutionModel should never emit orders");
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
        assert_ne!(ExecutionOrderType::Market, ExecutionOrderType::MarketOnClose);
        assert_ne!(ExecutionOrderType::Limit, ExecutionOrderType::MarketOnOpen);
        assert_ne!(ExecutionOrderType::Limit, ExecutionOrderType::MarketOnClose);
        assert_ne!(ExecutionOrderType::MarketOnOpen, ExecutionOrderType::MarketOnClose);
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
