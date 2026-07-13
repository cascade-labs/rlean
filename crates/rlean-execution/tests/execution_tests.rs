use rlean_core::{DateTime, Market, Symbol, TimeSpan};
use rlean_execution::{
    AdaptiveMakerTakerExecutionModel, AggressivePostOnlyExecutionModel, ExecutionContext,
    ExecutionOpenOrder, ExecutionOrderType, ExecutionTarget, IExecutionModel,
    ImmediateExecutionModel, MakerThenTakerExecutionModel, NullExecutionModel,
    PassiveMakerExecutionModel, SecurityData, SpreadExecutionModel,
    StandardDeviationExecutionModel, VwapExecutionModel,
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
        vwap_price: None,
        average_volume: None,
        daily_std_dev: None,
        end_time: None,
        lot_size: dec!(1),
        minimum_price_variation: dec!(0.01),
        current_quantity: Decimal::try_from(current_qty).unwrap(),
        open_order_quantity: Decimal::ZERO,
    }
}

fn make_security_with_quote(
    ticker: &str,
    price: Decimal,
    bid: Decimal,
    ask: Decimal,
    current_qty: Decimal,
    open_order_qty: Decimal,
) -> SecurityData {
    SecurityData {
        symbol: make_symbol(ticker),
        price,
        bid: Some(bid),
        ask: Some(ask),
        volume: None,
        vwap_price: None,
        average_volume: None,
        daily_std_dev: None,
        end_time: None,
        lot_size: dec!(1),
        minimum_price_variation: dec!(0.01),
        current_quantity: current_qty,
        open_order_quantity: open_order_qty,
    }
}

fn make_target(ticker: &str, qty: f64) -> ExecutionTarget {
    ExecutionTarget {
        symbol: make_symbol(ticker),
        quantity: Decimal::try_from(qty).unwrap(),
        tag: String::new(),
    }
}

fn securities_map(data: Vec<SecurityData>) -> HashMap<u64, SecurityData> {
    data.into_iter().map(|s| (s.symbol.id.sid, s)).collect()
}

fn context_default(securities: &HashMap<u64, SecurityData>) -> ExecutionContext<'_> {
    ExecutionContext::new(DateTime::MIN, securities, &[], dec!(100000))
}

fn make_open_limit_order(
    id: i64,
    ticker: &str,
    quantity: Decimal,
    limit_price: Decimal,
    created_time: DateTime,
    tag: &str,
) -> ExecutionOpenOrder {
    ExecutionOpenOrder {
        id,
        symbol: make_symbol(ticker),
        quantity,
        filled_quantity: Decimal::ZERO,
        remaining_quantity: quantity,
        order_type: ExecutionOrderType::Limit,
        limit_price: Some(limit_price),
        post_only: true,
        tag: tag.to_string(),
        created_time,
        last_update_time: None,
    }
}

fn context_orders<'a>(
    time: DateTime,
    securities: &'a HashMap<u64, SecurityData>,
    open_orders: &'a [ExecutionOpenOrder],
) -> ExecutionContext<'a> {
    ExecutionContext::new(time, securities, open_orders, dec!(100000))
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
        let orders = model.execute(&[], &context_default(&securities));
        assert!(orders.is_empty(), "Expected no orders for empty targets");
    }

    /// Target qty=100, current=0 → market buy 100.
    /// Mirrors: OrdersAreSubmittedImmediatelyForTargetsToExecute (openOrdersQuantity=0, qty=10)
    #[test]
    fn no_position_buy_order() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(40));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    /// Target=100, current=10, open buy=60 → market buy 30.
    /// Mirrors C# OrderSizing.GetUnorderedQuantity / ProjectedHoldings.
    #[test]
    fn open_orders_reduce_unordered_quantity() {
        let mut model = ImmediateExecutionModel::new();
        let mut security = make_security("AAPL", 250.0, 10.0);
        security.open_order_quantity = dec!(60);
        let securities = securities_map(vec![security]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(30));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    #[test]
    fn retained_target_resubmits_until_projected_holdings_match() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 10.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let first = model.execute(&targets, &context_default(&securities));
        let second = model.execute(&[], &context_default(&securities));

        assert_eq!(first.len(), 1);
        assert_eq!(first[0].quantity, dec!(90));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].quantity, dec!(90));
    }

    #[test]
    fn context_open_orders_defer_target_until_actual_holdings_match() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 10.0)]);
        let targets = vec![make_target("AAPL", 100.0)];
        let empty_open_orders = Vec::new();
        let first_context = context_orders(DateTime::from_secs(0), &securities, &empty_open_orders);

        let first = model.execute_with_context(&targets, &first_context);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].quantity, dec!(90));

        let open_orders = vec![ExecutionOpenOrder {
            id: 1,
            symbol: make_symbol("AAPL"),
            quantity: dec!(90),
            filled_quantity: Decimal::ZERO,
            remaining_quantity: dec!(90),
            order_type: ExecutionOrderType::Market,
            limit_price: None,
            post_only: false,
            tag: "ImmediateExecutionModel".to_string(),
            created_time: DateTime::from_secs(0),
            last_update_time: None,
        }];
        let projected_context = context_orders(DateTime::from_secs(1), &securities, &open_orders);
        let second = model.execute_with_context(&[], &projected_context);
        let third = model.execute_with_context(&[], &first_context);

        assert!(second.is_empty());
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].quantity, dec!(90));
    }

    #[test]
    fn context_orders_by_transaction_projected_holdings() {
        let securities = securities_map(vec![
            make_security("REDUCE", 100.0, 100.0),
            make_security("INCREASE", 100.0, 0.0),
        ]);
        let open_orders = vec![ExecutionOpenOrder {
            id: 1,
            symbol: make_symbol("INCREASE"),
            quantity: dec!(90),
            filled_quantity: Decimal::ZERO,
            remaining_quantity: dec!(90),
            order_type: ExecutionOrderType::Market,
            limit_price: None,
            post_only: false,
            tag: "pending".to_string(),
            created_time: DateTime::from_secs(0),
            last_update_time: None,
        }];
        let context = context_orders(DateTime::from_secs(1), &securities, &open_orders);
        let targets = vec![
            (
                make_symbol("INCREASE").id.sid,
                make_symbol("INCREASE"),
                dec!(100),
            ),
            (
                make_symbol("REDUCE").id.sid,
                make_symbol("REDUCE"),
                dec!(50),
            ),
        ];

        let ordered = context.order_targets_by_margin_impact(&targets);

        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].1.value.as_ref(), "REDUCE");
        assert_eq!(ordered[1].1.value.as_ref(), "INCREASE");
        assert_eq!(
            context.unordered_quantity(
                &make_symbol("INCREASE"),
                context.security(&make_symbol("INCREASE")).unwrap(),
                dec!(100),
            ),
            dec!(10)
        );
    }

    /// Target=-100, current=-10, open sell=-60 → market sell 30.
    #[test]
    fn open_short_orders_reduce_unordered_quantity() {
        let mut model = ImmediateExecutionModel::new();
        let mut security = make_security("AAPL", 250.0, -10.0);
        security.open_order_quantity = dec!(-60);
        let securities = securities_map(vec![security]);
        let targets = vec![make_target("AAPL", -100.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(-30));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    /// Target=100, current=100 → no order (already at target).
    #[test]
    fn already_at_target_no_order() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 100.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 2);

        let aapl_order = orders
            .iter()
            .find(|o| o.symbol.value.as_ref() == "AAPL")
            .unwrap();
        assert_eq!(aapl_order.quantity, dec!(10));

        let msft_order = orders
            .iter()
            .find(|o| o.symbol.value.as_ref() == "MSFT")
            .unwrap();
        assert_eq!(msft_order.quantity, dec!(10)); // 30 - 20 = 10
    }

    /// Mirrors C# PortfolioTargetCollection.OrderByMarginImpact:
    /// position-reducing orders first, then larger order value.
    #[test]
    fn orders_by_margin_impact() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![
            make_security("AAPL", 10.0, 100.0),
            make_security("MSFT", 100.0, 0.0),
            make_security("GOOG", 20.0, 0.0),
        ]);
        let targets = vec![
            make_target("MSFT", 1.0),
            make_target("GOOG", 50.0),
            make_target("AAPL", 50.0),
        ];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 3);
        assert_eq!(orders[0].symbol.value.as_ref(), "AAPL");
        assert_eq!(orders[0].quantity, dec!(-50));
        assert_eq!(orders[1].symbol.value.as_ref(), "GOOG");
        assert_eq!(orders[1].quantity, dec!(50));
        assert_eq!(orders[2].symbol.value.as_ref(), "MSFT");
        assert_eq!(orders[2].quantity, dec!(1));
    }

    /// Security not present in securities map -> no execution.
    #[test]
    fn unknown_security_defers_execution() {
        let mut model = ImmediateExecutionModel::new();
        // Provide an empty securities map (security data missing for AAPL)
        let securities: HashMap<u64, SecurityData> = HashMap::new();
        let targets = vec![make_target("AAPL", 50.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert!(orders.is_empty());
    }

    /// Verifies the order type is always Market for ImmediateExecutionModel.
    /// Mirrors: market order type expectation in C# tests
    #[test]
    fn order_type_is_always_market() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 42.0)];

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

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
        let first_orders = model.execute(&first_targets, &context_default(&securities));
        assert_eq!(first_orders.len(), 1);
        assert_eq!(first_orders[0].quantity, dec!(80));

        // Second call: target=100, current still 0 (order not yet filled) → order 100
        let second_targets = vec![make_target("AAPL", 100.0)];
        let second_orders = model.execute(&second_targets, &context_default(&securities));
        assert_eq!(second_orders.len(), 1);
        assert_eq!(second_orders[0].quantity, dec!(100)); // 100 - 0 (ImmediateModel is stateless)
    }

    /// Tag should identify the model that generated the order.
    #[test]
    fn order_tag_identifies_model() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert!(
            orders[0].tag.contains("ImmediateExecutionModel"),
            "Tag should identify model, got: {}",
            orders[0].tag
        );
    }

    /// Non-empty PortfolioTarget tags should flow through to immediate orders.
    #[test]
    fn order_tag_uses_target_tag_when_provided() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let mut target = make_target("AAPL", 10.0);
        target.tag = "orthogonal".to_string();

        let orders = model.execute(&[target], &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].tag, "orthogonal");
    }

    /// Symbol on the order should match the target symbol.
    #[test]
    fn order_symbol_matches_target() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].symbol.value.as_ref(), "AAPL");
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
        let securities: HashMap<u64, SecurityData> = HashMap::new();
        let orders = model.execute(&[], &context_default(&securities));
        assert!(orders.is_empty());
    }

    /// NullExecutionModel returns empty even when targets are provided.
    #[test]
    fn null_returns_empty_for_valid_targets() {
        let mut model = NullExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

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
// VolumeWeightedAveragePriceExecutionModel tests
// ---------------------------------------------------------------------------
// Based on C# VolumeWeightedAveragePriceExecutionModelTests:
// - Orders are not submitted when no targets are provided.
// - Buys execute only when bid < intraday VWAP.
// - Sells execute only when ask > intraday VWAP.
// - Order quantity is capped at 1% of the current bar volume by default.
// - Pending targets are retried on subsequent execute calls.

mod vwap_execution_tests {
    use super::*;

    fn make_security_with_vwap_input(
        ticker: &str,
        price: Decimal,
        bid: Decimal,
        ask: Decimal,
        volume: Decimal,
        current_qty: Decimal,
        seconds: i64,
    ) -> SecurityData {
        SecurityData {
            symbol: make_symbol(ticker),
            price,
            bid: Some(bid),
            ask: Some(ask),
            volume: Some(volume),
            vwap_price: Some(price),
            average_volume: None,
            daily_std_dev: None,
            end_time: Some(DateTime::from_secs(seconds)),
            lot_size: dec!(1),
            minimum_price_variation: dec!(0.01),
            current_quantity: current_qty,
            open_order_quantity: Decimal::ZERO,
        }
    }

    fn seed_vwap(model: &mut VwapExecutionModel, prices: &[Decimal]) {
        for (index, price) in prices.iter().enumerate() {
            let securities = securities_map(vec![make_security_with_vwap_input(
                "AAPL",
                *price,
                *price,
                *price,
                dec!(100),
                Decimal::ZERO,
                60 * index as i64,
            )]);
            let orders = model.execute(&[], &context_default(&securities));
            assert!(orders.is_empty());
        }
    }

    #[test]
    fn no_targets_no_orders() {
        let mut model = VwapExecutionModel::default();
        let securities = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(250),
            dec!(249),
            dec!(251),
            dec!(5000),
            Decimal::ZERO,
            0,
        )]);

        let orders = model.execute(&[], &context_default(&securities));

        assert!(orders.is_empty(), "Expected no orders for empty targets");
    }

    #[test]
    fn default_participation_rate_is_one_percent_current_volume() {
        let mut model = VwapExecutionModel::default();
        seed_vwap(&mut model, &[dec!(270), dec!(260), dec!(250)]);
        let securities = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(250),
            dec!(250),
            dec!(250),
            dec!(500),
            Decimal::ZERO,
            600,
        )]);

        let orders = model.execute(&[make_target("AAPL", 10.0)], &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(5));
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    #[test]
    fn buy_order_caps_at_target_when_current_volume_is_large() {
        let mut model = VwapExecutionModel::default();
        seed_vwap(&mut model, &[dec!(270), dec!(260), dec!(250)]);
        let securities = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(250),
            dec!(250),
            dec!(250),
            dec!(5000),
            Decimal::ZERO,
            600,
        )]);

        let orders = model.execute(&[make_target("AAPL", 10.0)], &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(10));
    }

    #[test]
    fn buy_order_below_lot_size_is_deferred() {
        let mut model = VwapExecutionModel::default();
        seed_vwap(&mut model, &[dec!(270), dec!(260), dec!(250)]);
        let securities = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(250),
            dec!(250),
            dec!(250),
            dec!(50),
            Decimal::ZERO,
            600,
        )]);

        let orders = model.execute(&[make_target("AAPL", 10.0)], &context_default(&securities));

        assert!(orders.is_empty(), "0.5 shares rounds down to zero lots");
    }

    #[test]
    fn buy_order_is_deferred_when_bid_is_not_below_vwap() {
        let mut model = VwapExecutionModel::default();
        seed_vwap(&mut model, &[dec!(230), dec!(240), dec!(250)]);
        let securities = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(250),
            dec!(250),
            dec!(250),
            dec!(50000),
            Decimal::ZERO,
            600,
        )]);

        let orders = model.execute(&[make_target("AAPL", 10.0)], &context_default(&securities));

        assert!(orders.is_empty());
    }

    #[test]
    fn sell_order_executes_when_ask_is_above_vwap() {
        let mut model = VwapExecutionModel::default();
        seed_vwap(&mut model, &[dec!(230), dec!(240), dec!(250)]);
        let securities = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(250),
            dec!(250),
            dec!(250),
            dec!(5000),
            dec!(10),
            600,
        )]);

        let orders = model.execute(&[make_target("AAPL", 0.0)], &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(-10));
    }

    #[test]
    fn deferred_target_retries_when_price_becomes_favorable() {
        let mut model = VwapExecutionModel::default();
        seed_vwap(&mut model, &[dec!(270), dec!(260), dec!(250)]);

        let unfavorable = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(265),
            dec!(265),
            dec!(265),
            dec!(1000),
            Decimal::ZERO,
            600,
        )]);
        let first_orders =
            model.execute(&[make_target("AAPL", 10.0)], &context_default(&unfavorable));
        assert!(first_orders.is_empty());

        let favorable = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(240),
            dec!(240),
            dec!(240),
            dec!(1000),
            Decimal::ZERO,
            660,
        )]);
        let second_orders = model.execute(&[], &context_default(&favorable));

        assert_eq!(second_orders.len(), 1);
        assert_eq!(second_orders[0].quantity, dec!(10));
    }

    #[test]
    fn removed_security_clears_pending_and_vwap_state() {
        let mut model = VwapExecutionModel::default();
        seed_vwap(&mut model, &[dec!(270), dec!(260), dec!(250)]);

        let unfavorable = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(265),
            dec!(265),
            dec!(265),
            dec!(1000),
            Decimal::ZERO,
            600,
        )]);
        model.execute(&[make_target("AAPL", 10.0)], &context_default(&unfavorable));
        model.on_securities_changed(&[], &[make_symbol("AAPL")]);

        let favorable = securities_map(vec![make_security_with_vwap_input(
            "AAPL",
            dec!(240),
            dec!(240),
            dec!(240),
            dec!(1000),
            Decimal::ZERO,
            660,
        )]);
        let orders = model.execute(&[], &context_default(&favorable));

        assert!(orders.is_empty());
    }

    #[test]
    fn model_name_is_correct() {
        let model = VwapExecutionModel::default();
        assert_eq!(model.name(), "VolumeWeightedAveragePriceExecutionModel");
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

        let orders = model.execute(&targets, &context_default(&securities));

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
        assert_ne!(ExecutionOrderType::Limit, ExecutionOrderType::Update);
        assert_ne!(ExecutionOrderType::Cancel, ExecutionOrderType::Update);
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
        assert_eq!(t.symbol.value.as_ref(), "AAPL");
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
            vwap_price: None,
            average_volume: Some(dec!(900000)),
            daily_std_dev: Some(dec!(5)),
            end_time: None,
            lot_size: dec!(1),
            minimum_price_variation: dec!(0.01),
            current_quantity: dec!(50),
            open_order_quantity: dec!(7),
        };
        assert_eq!(s.symbol.value.as_ref(), "MSFT");
        assert_eq!(s.price, dec!(300));
        assert_eq!(s.bid, Some(dec!(299.5)));
        assert_eq!(s.ask, Some(dec!(300.5)));
        assert_eq!(s.minimum_price_variation, dec!(0.01));
        assert_eq!(s.current_quantity, dec!(50));
        assert_eq!(s.open_order_quantity, dec!(7));
    }

    /// OrderRequest limit_price is None for market orders from ImmediateExecutionModel.
    #[test]
    fn immediate_order_has_no_limit_price() {
        let mut model = ImmediateExecutionModel::new();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];
        let orders = model.execute(&targets, &context_default(&securities));

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
            vwap_price: None,
            average_volume: None,
            daily_std_dev: None,
            end_time: None,
            lot_size: dec!(1),
            minimum_price_variation: dec!(0.01),
            current_quantity: Decimal::try_from(current_qty).unwrap(),
            open_order_quantity: Decimal::ZERO,
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
        let orders = model.execute(&[], &context_default(&securities));
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

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

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
        let first_orders = model.execute(&targets, &context_default(&wide_sec));
        assert!(first_orders.is_empty(), "Should defer on wide spread");

        // Second call: tight spread → order submitted for the pending target
        let tight_sec = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 249.75, 250.25, 0.0,
        )]);
        let second_orders = model.execute(&[], &context_default(&tight_sec)); // no new targets, retry pending
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

        let orders = model.execute(&targets, &context_default(&securities));

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

        let orders = model.execute(&targets, &context_default(&securities));

        assert!(
            orders.is_empty(),
            "Spread just above threshold should be deferred"
        );
    }

    /// No bid/ask data → execution is deferred, matching C# SpreadExecutionModel.
    #[test]
    fn no_bid_ask_defers_execution() {
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert!(orders.is_empty(), "Missing bid/ask should defer execution");
    }

    /// Delta ordering: target=100, current=60 → order 40 shares.
    #[test]
    fn delta_only_ordered() {
        let mut model = SpreadExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 250.0, 60.0,
        )]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(40));
    }

    /// Pending targets recalculate unordered quantity against current holdings and open orders.
    #[test]
    fn pending_target_recomputes_unordered_quantity() {
        let mut model = SpreadExecutionModel::default();

        let wide_sec = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 275.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 100.0)];
        let first_orders = model.execute(&targets, &context_default(&wide_sec));
        assert!(first_orders.is_empty(), "Wide spread should defer target");

        let mut security = make_security_with_quotes("AAPL", 250.0, 250.0, 250.0, 40.0);
        security.open_order_quantity = dec!(25);
        let tight_sec = securities_map(vec![security]);
        let second_orders = model.execute(&[], &context_default(&tight_sec));

        assert_eq!(second_orders.len(), 1);
        assert_eq!(second_orders[0].quantity, dec!(35));
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
        model.execute(&targets, &context_default(&wide_sec));

        // Remove the security
        let removed = vec![make_symbol("AAPL")];
        model.on_securities_changed(&[], &removed);

        // Now tighten the spread — should produce no order (pending cleared)
        let tight_sec = securities_map(vec![make_security_with_quotes(
            "AAPL", 250.0, 250.0, 250.0, 0.0,
        )]);
        let orders = model.execute(&[], &context_default(&tight_sec));
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

        let orders = model.execute(&targets, &context_default(&securities));

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
// - Indicators must be ready before orders are submitted.
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
            vwap_price: None,
            average_volume: None,
            daily_std_dev: Some(Decimal::try_from(daily_std_dev).unwrap()),
            end_time: None,
            lot_size: dec!(1),
            minimum_price_variation: dec!(0.01),
            current_quantity: Decimal::try_from(current_qty).unwrap(),
            open_order_quantity: Decimal::ZERO,
        }
    }

    fn seed_prices(model: &mut StandardDeviationExecutionModel, ticker: &str, prices: &[f64]) {
        for price in prices {
            let securities = securities_map(vec![make_security_with_std_dev(
                ticker, *price, *price, *price, 0.0, 0.0,
            )]);
            let orders = model.execute(&[], &context_default(&securities));
            assert!(
                orders.is_empty(),
                "Seeding without targets should not submit orders"
            );
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
        let orders = model.execute(&[], &context_default(&securities));
        assert!(orders.is_empty(), "Expected no orders for empty targets");
    }

    /// Buy: bid well below SMA - N*std_dev → order submitted.
    /// Scenario mirrors C#: historicalPrices=[270,260,250], currentPrice=240, deviations=1.5
    /// SMA ≈ 260, STD ≈ 10. Threshold = 260 - 1.5*10 = 245. bid=240 < 245 → buy.
    #[test]
    fn buy_order_when_bid_below_lower_band() {
        // window=[270,260,230], deviations=1.0 → bid=230 below lower band
        let mut model = StandardDeviationExecutionModel::new(3, dec!(1.0));
        seed_prices(&mut model, "AAPL", &[270.0, 260.0]);
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 230.0, 230.0, 235.0, 0.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

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
        // Constant rolling price creates a zero-width band, so bid=250 is not below it.
        let mut model = StandardDeviationExecutionModel::new(3, dec!(2.0));
        seed_prices(&mut model, "AAPL", &[250.0, 250.0]);
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 250.0, 252.0, 0.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert!(
            orders.is_empty(),
            "Should not buy when bid is within the band"
        );
    }

    /// Sell: ask well above SMA + N*std_dev → order submitted.
    #[test]
    fn sell_order_when_ask_above_upper_band() {
        // window=[230,240,270], deviations=1.0 → ask=280 above upper band
        let mut model = StandardDeviationExecutionModel::new(3, dec!(1.0));
        seed_prices(&mut model, "AAPL", &[230.0, 240.0]);
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 270.0, 265.0, 280.0, 0.0, 100.0,
        )]);
        let targets = vec![make_target("AAPL", 0.0)]; // sell all (liquidate)

        let orders = model.execute(&targets, &context_default(&securities));

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
        let mut model = StandardDeviationExecutionModel::new(3, dec!(2.0));
        seed_prices(&mut model, "AAPL", &[250.0, 250.0]);
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 245.0, 250.0, 0.0, 100.0,
        )]);
        let targets = vec![make_target("AAPL", 0.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert!(
            orders.is_empty(),
            "Should not sell when ask is within the band"
        );
    }

    /// Indicator not ready → execution is deferred.
    #[test]
    fn not_ready_defers_execution() {
        let mut model = StandardDeviationExecutionModel::new(3, dec!(1.0));
        let securities = securities_map(vec![make_security("AAPL", 250.0, 0.0)]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert!(
            orders.is_empty(),
            "Model should wait for the rolling indicator window"
        );
    }

    /// Delta ordering: only the unmet quantity is ordered.
    #[test]
    fn delta_only_ordered() {
        let mut model = StandardDeviationExecutionModel::new(3, dec!(1.0));
        seed_prices(&mut model, "AAPL", &[250.0, 250.0]);
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 140.0, 140.0, 150.0, 0.0, 60.0,
        )]);
        let targets = vec![make_target("AAPL", 100.0)]; // need 40 more

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(40));
    }

    /// MaximumOrderValue caps each submitted slice, matching C# OrderSizing.GetOrderSizeForMaximumValue.
    #[test]
    fn maximum_order_value_caps_order_quantity() {
        let mut model =
            StandardDeviationExecutionModel::with_maximum_order_value(3, dec!(1.0), dec!(2000));
        seed_prices(&mut model, "AAPL", &[200.0, 200.0]);
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 100.0, 100.0, 105.0, 0.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 100.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, dec!(20));
    }

    /// Deferred order is retried when price moves into favorable range.
    #[test]
    fn deferred_then_favorable_submits() {
        let mut model = StandardDeviationExecutionModel::new(3, dec!(1.0));
        seed_prices(&mut model, "AAPL", &[250.0, 250.0]);

        // First call: zero std-dev window, bid is not below the lower band.
        let within_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 250.0, 255.0, 0.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 10.0)];
        let first_orders = model.execute(&targets, &context_default(&within_sec));
        assert!(first_orders.is_empty(), "Should defer when within band");

        // Second call: [250,250,140] makes the bid favorable.
        let favorable_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 140.0, 140.0, 150.0, 0.0, 0.0,
        )]);
        let second_orders = model.execute(&[], &context_default(&favorable_sec));
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
        let mut model = StandardDeviationExecutionModel::new(3, dec!(1.0));
        seed_prices(&mut model, "AAPL", &[250.0, 250.0]);

        // Queue a pending order (bid within band → deferred)
        let within_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 250.0, 250.0, 252.0, 0.0, 0.0,
        )]);
        model.execute(&[make_target("AAPL", 10.0)], &context_default(&within_sec));

        // Remove the security
        model.on_securities_changed(&[], &[make_symbol("AAPL")]);

        // Now provide favorable conditions — should produce no order
        let favorable_sec = securities_map(vec![make_security_with_std_dev(
            "AAPL", 140.0, 140.0, 150.0, 0.0, 0.0,
        )]);
        let orders = model.execute(&[], &context_default(&favorable_sec));
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
        let mut model = StandardDeviationExecutionModel::new(3, dec!(1.0));
        seed_prices(&mut model, "AAPL", &[250.0, 250.0]);
        let securities = securities_map(vec![make_security_with_std_dev(
            "AAPL", 140.0, 140.0, 150.0, 0.0, 0.0,
        )]);
        let targets = vec![make_target("AAPL", 5.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert!(
            orders[0].tag.contains("StandardDeviationExecutionModel"),
            "Tag should identify model, got: {}",
            orders[0].tag
        );
    }
}

// ---------------------------------------------------------------------------
// PassiveMakerExecutionModel tests
// ---------------------------------------------------------------------------

mod passive_maker_execution_tests {
    use super::*;

    #[test]
    fn posts_buy_limit_at_bid_as_post_only() {
        let mut model = PassiveMakerExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(orders[0].quantity, dec!(10));
        assert_eq!(orders[0].limit_price, Some(dec!(99.90)));
        assert!(orders[0].post_only);
        assert!(!orders[0].cancel_open_orders);
    }

    #[test]
    fn posts_sell_limit_at_ask_as_post_only() {
        let mut model = PassiveMakerExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);
        let targets = vec![make_target("AAPL", -10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(orders[0].quantity, dec!(-10));
        assert_eq!(orders[0].limit_price, Some(dec!(100.10)));
        assert!(orders[0].post_only);
        assert!(!orders[0].cancel_open_orders);
    }

    #[test]
    fn falls_back_to_taker_after_passive_attempt_limit() {
        let mut model = PassiveMakerExecutionModel::new(1, dec!(0.01));
        let mut security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let targets = vec![make_target("AAPL", 10.0)];

        let first = model.execute(
            &targets,
            &context_default(&securities_map(vec![security.clone()])),
        );
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);
        assert!(first[0].post_only);

        security.open_order_quantity = dec!(10);
        let second = model.execute(&targets, &context_default(&securities_map(vec![security])));

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Market);
        assert_eq!(second[0].quantity, dec!(10));
        assert!(!second[0].post_only);
        assert!(second[0].cancel_open_orders);
        assert!(second[0].tag.contains("passive-timeout"));
    }

    #[test]
    fn leaves_matching_passive_order_resting_before_timeout() {
        let mut model = PassiveMakerExecutionModel::new(3, dec!(0.01));
        let mut security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let targets = vec![make_target("AAPL", 10.0)];

        let first = model.execute(
            &targets,
            &context_default(&securities_map(vec![security.clone()])),
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        security.open_order_quantity = dec!(10);
        let second = model.execute(
            &targets,
            &context_default(&securities_map(vec![security.clone()])),
        );
        let third = model.execute(&targets, &context_default(&securities_map(vec![security])));

        assert!(second.is_empty());
        assert!(third.is_empty());
    }

    #[test]
    fn uses_execution_context_open_orders_before_snapshot_quantity() {
        let mut model = PassiveMakerExecutionModel::new(3, dec!(0.01));
        let first_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let first_securities = securities_map(vec![first_security]);
        let empty_open_orders = Vec::new();
        let first_context = context_orders(
            DateTime::from_secs(0),
            &first_securities,
            &empty_open_orders,
        );
        let targets = vec![make_target("AAPL", 10.0)];

        let first = model.execute_with_context(&targets, &first_context);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        let stale_snapshot = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);
        let resting = vec![make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.90),
            DateTime::from_secs(0),
            "PassiveMakerExecutionModel post-only",
        )];
        let second_context = context_orders(DateTime::from_secs(60), &stale_snapshot, &resting);

        let second = model.execute_with_context(&[], &second_context);

        assert!(second.is_empty());
    }

    #[test]
    fn replaces_resting_buy_when_bid_improves_before_timeout() {
        let mut model = PassiveMakerExecutionModel::new(3, dec!(0.01));
        let targets = vec![make_target("AAPL", 10.0)];
        let first_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let mut second_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.20),
            dec!(0),
            dec!(10),
        );

        let first = model.execute(
            &targets,
            &context_default(&securities_map(vec![first_security])),
        );
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(first[0].limit_price, Some(dec!(99.90)));

        let second = model.execute(
            &targets,
            &context_default(&securities_map(vec![second_security.clone()])),
        );

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(second[0].quantity, dec!(10));
        assert_eq!(second[0].limit_price, Some(dec!(100.00)));
        assert!(second[0].post_only);
        assert!(second[0].cancel_open_orders);

        second_security.open_order_quantity = dec!(10);
        let third = model.execute(
            &targets,
            &context_default(&securities_map(vec![second_security])),
        );
        assert!(third.is_empty());
    }

    #[test]
    fn updates_context_open_buy_when_bid_improves_before_timeout() {
        let mut model = PassiveMakerExecutionModel::new(3, dec!(0.01));
        let first_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);
        let empty_open_orders = Vec::new();
        let first_context = context_orders(
            DateTime::from_secs(0),
            &first_securities,
            &empty_open_orders,
        );
        model.execute_with_context(&[make_target("AAPL", 10.0)], &first_context);

        let second_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.20),
            dec!(0),
            dec!(10),
        )]);
        let resting = vec![make_open_limit_order(
            7,
            "AAPL",
            dec!(10),
            dec!(99.90),
            DateTime::from_secs(0),
            "PassiveMakerExecutionModel post-only",
        )];
        let second_context = context_orders(DateTime::from_secs(60), &second_securities, &resting);

        let second = model.execute_with_context(&[], &second_context);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Update);
        assert_eq!(second[0].order_id, Some(7));
        assert_eq!(second[0].limit_price, Some(dec!(100.00)));
        assert!(!second[0].cancel_open_orders);
    }

    #[test]
    fn replaces_context_open_buy_when_same_side_target_changes() {
        let mut model = PassiveMakerExecutionModel::new(3, dec!(0.01));
        let first_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);
        let empty_open_orders = Vec::new();
        let first_context = context_orders(
            DateTime::from_secs(0),
            &first_securities,
            &empty_open_orders,
        );
        model.execute_with_context(&[make_target("AAPL", 10.0)], &first_context);

        let second_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.20),
            dec!(4),
            dec!(6),
        )]);
        let mut resting_order = make_open_limit_order(
            7,
            "AAPL",
            dec!(10),
            dec!(99.90),
            DateTime::from_secs(0),
            "PassiveMakerExecutionModel post-only",
        );
        resting_order.filled_quantity = dec!(4);
        resting_order.remaining_quantity = dec!(6);
        let resting = vec![resting_order];
        let second_context = context_orders(DateTime::from_secs(60), &second_securities, &resting);

        let second = model.execute_with_context(&[make_target("AAPL", 15.0)], &second_context);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(second[0].order_id, None);
        assert_eq!(second[0].quantity, dec!(11));
        assert_eq!(second[0].limit_price, Some(dec!(100.00)));
        assert!(second[0].cancel_open_orders);
    }

    #[test]
    fn keeps_resting_buy_when_bid_moves_lower_before_timeout() {
        let mut model = PassiveMakerExecutionModel::new(3, dec!(0.01));
        let targets = vec![make_target("AAPL", 10.0)];
        let first_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let second_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.80),
            dec!(100.00),
            dec!(0),
            dec!(10),
        );

        let first = model.execute(
            &targets,
            &context_default(&securities_map(vec![first_security])),
        );
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(first[0].limit_price, Some(dec!(99.90)));

        let second = model.execute(
            &targets,
            &context_default(&securities_map(vec![second_security])),
        );

        assert!(second.is_empty());
    }

    #[test]
    fn replaces_resting_sell_when_ask_improves_before_timeout() {
        let mut model = PassiveMakerExecutionModel::new(3, dec!(0.01));
        let targets = vec![make_target("AAPL", -10.0)];
        let first_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let second_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.80),
            dec!(100.00),
            dec!(0),
            dec!(-10),
        );

        let first = model.execute(
            &targets,
            &context_default(&securities_map(vec![first_security])),
        );
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(first[0].limit_price, Some(dec!(100.10)));

        let second = model.execute(
            &targets,
            &context_default(&securities_map(vec![second_security])),
        );

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(second[0].quantity, dec!(-10));
        assert_eq!(second[0].limit_price, Some(dec!(100.00)));
        assert!(second[0].post_only);
        assert!(second[0].cancel_open_orders);
    }

    #[test]
    fn falls_back_to_taker_on_adverse_buy_quote_move() {
        let mut model = PassiveMakerExecutionModel::new(10, dec!(0.001));
        let targets = vec![make_target("AAPL", 10.0)];
        let first_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let second_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.10),
            dec!(100.30),
            dec!(0),
            dec!(10),
        );

        let first = model.execute(
            &targets,
            &context_default(&securities_map(vec![first_security])),
        );
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        let second = model.execute(
            &targets,
            &context_default(&securities_map(vec![second_security])),
        );

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Market);
        assert!(second[0].cancel_open_orders);
        assert!(second[0].tag.contains("adverse-selection"));
    }

    #[test]
    fn cancels_open_order_when_target_is_fulfilled() {
        let mut model = PassiveMakerExecutionModel::default();
        let targets = vec![make_target("AAPL", 10.0)];
        let first_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        );
        let fulfilled_security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(10),
            dec!(10),
        );

        let first = model.execute(
            &targets,
            &context_default(&securities_map(vec![first_security])),
        );
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        let second = model.execute(
            &targets,
            &context_default(&securities_map(vec![fulfilled_security])),
        );

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Cancel);
        assert!(second[0].cancel_open_orders);
    }
}

// ---------------------------------------------------------------------------
// AdaptiveMakerTakerExecutionModel tests
// ---------------------------------------------------------------------------

mod aggressive_post_only_execution_tests {
    use super::*;

    #[test]
    fn posts_buy_one_tick_inside_spread() {
        let mut model = AggressivePostOnlyExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);

        let orders = model.execute(&[make_target("AAPL", 10.0)], &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(orders[0].quantity, dec!(10));
        assert_eq!(orders[0].limit_price, Some(dec!(100.09)));
        assert!(orders[0].post_only);
        assert!(!orders[0].cancel_open_orders);
    }

    #[test]
    fn posts_sell_one_tick_inside_spread() {
        let mut model = AggressivePostOnlyExecutionModel::default();
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);

        let orders = model.execute(&[make_target("AAPL", -10.0)], &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(orders[0].quantity, dec!(-10));
        assert_eq!(orders[0].limit_price, Some(dec!(99.91)));
        assert!(orders[0].post_only);
        assert!(!orders[0].cancel_open_orders);
    }

    #[test]
    fn one_tick_spread_uses_touch_prices_without_crossing() {
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.01),
            dec!(0),
            dec!(0),
        )]);

        let buy_orders = AggressivePostOnlyExecutionModel::default()
            .execute(&[make_target("AAPL", 10.0)], &context_default(&securities));
        let sell_orders = AggressivePostOnlyExecutionModel::default()
            .execute(&[make_target("AAPL", -10.0)], &context_default(&securities));

        assert_eq!(buy_orders[0].limit_price, Some(dec!(100.00)));
        assert_eq!(sell_orders[0].limit_price, Some(dec!(100.01)));
        assert!(buy_orders[0].post_only);
        assert!(sell_orders[0].post_only);
    }

    #[test]
    fn later_quote_move_reprices_without_market_fallback() {
        let mut model = AggressivePostOnlyExecutionModel::default();
        let targets = vec![make_target("AAPL", 10.0)];
        let first_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.90),
            dec!(100.10),
            dec!(0),
            dec!(0),
        )]);
        let empty_open_orders = Vec::new();
        let first_context = context_orders(
            DateTime::from_secs(0),
            &first_securities,
            &empty_open_orders,
        );

        let first = model.execute_with_context(&targets, &first_context);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(first[0].limit_price, Some(dec!(100.09)));

        let second_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(101),
            dec!(101.00),
            dec!(102.00),
            dec!(0),
            dec!(10),
        )]);
        let resting = vec![make_open_limit_order(
            7,
            "AAPL",
            dec!(10),
            dec!(100.09),
            DateTime::from_secs(0),
            "AggressivePostOnlyExecutionModel post-only",
        )];
        let second_context =
            context_orders(DateTime::from_secs(3600), &second_securities, &resting);

        let second = model.execute_with_context(&[], &second_context);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Update);
        assert_eq!(second[0].order_id, Some(7));
        assert_eq!(second[0].limit_price, Some(dec!(101.99)));
        assert!(second[0].post_only);
        assert!(!second[0].cancel_open_orders);
    }
}

mod adaptive_maker_taker_execution_tests {
    use super::*;

    #[test]
    fn crosses_immediately_when_spread_is_tight() {
        let mut model = AdaptiveMakerTakerExecutionModel::new(dec!(0.002), 1, dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(0),
            dec!(0),
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
        assert_eq!(orders[0].quantity, dec!(10));
        assert!(!orders[0].post_only);
        assert!(orders[0].tag.contains("tight-spread"));
    }

    #[test]
    fn crosses_immediately_when_spread_is_locked() {
        let mut model = AdaptiveMakerTakerExecutionModel::new(dec!(0.001), 1, dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100),
            dec!(100),
            dec!(0),
            dec!(0),
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
    }

    #[test]
    fn tight_spread_market_order_replaces_existing_passive_order() {
        let mut model = AdaptiveMakerTakerExecutionModel::new(dec!(0.002), 3, dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.00),
            dec!(101.00),
            dec!(0),
            dec!(0),
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let first = model.execute(&targets, &context_default(&securities));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        let tightened = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(4),
            dec!(6),
        )]);
        let second = model.execute(&targets, &context_default(&tightened));

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Market);
        assert_eq!(second[0].quantity, dec!(6));
        assert!(second[0].cancel_open_orders);
        assert!(second[0].tag.contains("tight-spread"));
    }

    #[test]
    fn tight_spread_uses_context_open_orders_for_replacement() {
        let mut model = AdaptiveMakerTakerExecutionModel::new(dec!(0.002), 3, dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(0),
            dec!(0),
        )]);
        let resting = vec![make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.90),
            DateTime::from_secs(0),
            "PassiveMakerExecutionModel post-only",
        )];
        let context = context_orders(DateTime::from_secs(60), &securities, &resting);

        let orders = model.execute_with_context(&[make_target("AAPL", 10.0)], &context);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Market);
        assert_eq!(orders[0].quantity, dec!(10));
        assert!(orders[0].cancel_open_orders);
    }

    #[test]
    fn tight_spread_cancels_passive_order_after_target_is_filled() {
        let mut model = AdaptiveMakerTakerExecutionModel::new(dec!(0.002), 3, dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.00),
            dec!(101.00),
            dec!(0),
            dec!(0),
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let first = model.execute(&targets, &context_default(&securities));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        let filled_with_stale_open_order = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(10),
            dec!(10),
        )]);
        let second = model.execute(&targets, &context_default(&filled_with_stale_open_order));

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Cancel);
        assert_eq!(second[0].quantity, dec!(0));
        assert!(second[0].cancel_open_orders);
        assert!(second[0].tag.contains("cancel stale passive"));
    }

    #[test]
    fn tight_spread_cancels_context_open_order_after_target_is_filled() {
        let mut model = AdaptiveMakerTakerExecutionModel::new(dec!(0.002), 3, dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(10),
            dec!(0),
        )]);
        let resting = vec![make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.90),
            DateTime::from_secs(0),
            "PassiveMakerExecutionModel post-only",
        )];
        let context = context_orders(DateTime::from_secs(60), &securities, &resting);

        let orders = model.execute_with_context(&[make_target("AAPL", 10.0)], &context);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Cancel);
        assert_eq!(orders[0].quantity, dec!(0));
        assert!(orders[0].cancel_open_orders);
    }

    #[test]
    fn posts_passive_limit_when_spread_is_wide() {
        let mut model = AdaptiveMakerTakerExecutionModel::new(dec!(0.001), 1, dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.00),
            dec!(101.00),
            dec!(0),
            dec!(0),
        )]);
        let targets = vec![make_target("AAPL", 10.0)];

        let orders = model.execute(&targets, &context_default(&securities));

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(orders[0].limit_price, Some(dec!(100.00)));
        assert!(orders[0].post_only);
    }

    #[test]
    fn wide_spread_passive_order_falls_back_after_timeout() {
        let mut model = AdaptiveMakerTakerExecutionModel::with_passive_duration(
            dec!(0.001),
            TimeSpan::from_mins(5),
            dec!(0.005),
        );
        let targets = vec![make_target("AAPL", 10.0)];
        let security = make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.00),
            dec!(101.00),
            dec!(0),
            dec!(0),
        );
        let first_securities = securities_map(vec![security]);
        let empty_open_orders = Vec::new();
        let first_context = context_orders(
            DateTime::from_secs(0),
            &first_securities,
            &empty_open_orders,
        );

        let first = model.execute_with_context(&targets, &first_context);
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        let resting = vec![make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.00),
            DateTime::from_secs(0),
            "MakerThenTakerExecutionModel post-only",
        )];
        let before_deadline_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.00),
            dec!(101.00),
            dec!(0),
            dec!(10),
        )]);
        let before_deadline_context = context_orders(
            DateTime::from_secs(60),
            &before_deadline_securities,
            &resting,
        );
        let before_deadline = model.execute_with_context(&[], &before_deadline_context);
        assert!(before_deadline.is_empty());

        let after_deadline_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.00),
            dec!(101.00),
            dec!(0),
            dec!(10),
        )]);
        let after_deadline_context = context_orders(
            DateTime::from_secs(301),
            &after_deadline_securities,
            &resting,
        );
        let second = model.execute_with_context(&[], &after_deadline_context);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Market);
        assert!(second[0].cancel_open_orders);
        assert!(second[0].tag.contains("taker deadline"));
    }
}

// ---------------------------------------------------------------------------
// MakerThenTakerExecutionModel tests
// ---------------------------------------------------------------------------

mod maker_then_taker_execution_tests {
    use super::*;

    fn context_orders<'a>(
        time: DateTime,
        securities: &'a HashMap<u64, SecurityData>,
        open_orders: &'a [ExecutionOpenOrder],
    ) -> ExecutionContext<'a> {
        ExecutionContext::new(time, securities, open_orders, dec!(100000))
    }

    #[test]
    fn posts_post_only_limit_first() {
        let mut model = MakerThenTakerExecutionModel::new(TimeSpan::from_mins(5), dec!(0.005));
        let securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(0),
            dec!(0),
        )]);
        let open_orders = Vec::new();
        let context = context_orders(DateTime::from_secs(1), &securities, &open_orders);

        let orders = model.execute_with_context(&[make_target("AAPL", 10.0)], &context);

        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(orders[0].limit_price, Some(dec!(100.00)));
        assert!(orders[0].post_only);
        assert!(!orders[0].cancel_open_orders);
    }

    #[test]
    fn leaves_resting_order_before_deadline() {
        let mut model = MakerThenTakerExecutionModel::new(TimeSpan::from_mins(5), dec!(0.005));
        let first_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.05),
            dec!(0),
            dec!(0),
        )]);
        let open_orders = Vec::new();
        let first_context = context_orders(DateTime::from_secs(0), &first_securities, &open_orders);
        let first = model.execute_with_context(&[make_target("AAPL", 10.0)], &first_context);
        assert_eq!(first[0].order_type, ExecutionOrderType::Limit);

        let second_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.05),
            dec!(0),
            dec!(10),
        )]);
        let resting = vec![make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.95),
            DateTime::from_secs(0),
            "MakerThenTakerExecutionModel post-only",
        )];
        let second_context = context_orders(DateTime::from_secs(60), &second_securities, &resting);

        let second = model.execute_with_context(&[], &second_context);

        assert!(second.is_empty());
    }

    #[test]
    fn reprices_when_same_side_quote_improves_before_deadline() {
        let mut model = MakerThenTakerExecutionModel::new(TimeSpan::from_mins(5), dec!(0.005));
        let first_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(0),
            dec!(0),
        )]);
        let open_orders = Vec::new();
        let first_context = context_orders(DateTime::from_secs(0), &first_securities, &open_orders);
        model.execute_with_context(&[make_target("AAPL", 10.0)], &first_context);

        let second_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100.25),
            dec!(100.10),
            dec!(100.30),
            dec!(0),
            dec!(10),
        )]);
        let resting = vec![make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.95),
            DateTime::from_secs(0),
            "MakerThenTakerExecutionModel post-only",
        )];
        let second_context = context_orders(DateTime::from_secs(60), &second_securities, &resting);

        let second = model.execute_with_context(&[], &second_context);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Update);
        assert_eq!(second[0].order_id, Some(1));
        assert_eq!(second[0].limit_price, Some(dec!(100.20)));
        assert!(!second[0].cancel_open_orders);
    }

    #[test]
    fn replaces_same_side_limit_when_target_changes_before_deadline() {
        let mut model = MakerThenTakerExecutionModel::new(TimeSpan::from_mins(5), dec!(0.005));
        let first_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.05),
            dec!(0),
            dec!(0),
        )]);
        let open_orders = Vec::new();
        let first_context = context_orders(DateTime::from_secs(0), &first_securities, &open_orders);
        model.execute_with_context(&[make_target("AAPL", 10.0)], &first_context);

        let second_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100.25),
            dec!(100.10),
            dec!(100.30),
            dec!(4),
            dec!(6),
        )]);
        let mut resting_order = make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.95),
            DateTime::from_secs(0),
            "MakerThenTakerExecutionModel post-only",
        );
        resting_order.filled_quantity = dec!(4);
        resting_order.remaining_quantity = dec!(6);
        let resting = vec![resting_order];
        let second_context = context_orders(DateTime::from_secs(60), &second_securities, &resting);

        let second = model.execute_with_context(&[make_target("AAPL", 15.0)], &second_context);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Limit);
        assert_eq!(second[0].order_id, None);
        assert_eq!(second[0].quantity, dec!(11));
        assert_eq!(second[0].limit_price, Some(dec!(100.20)));
        assert!(second[0].cancel_open_orders);
    }

    #[test]
    fn crosses_residual_after_passive_deadline() {
        let mut model = MakerThenTakerExecutionModel::new(TimeSpan::from_mins(5), dec!(0.005));
        let first_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(100.00),
            dec!(100.05),
            dec!(0),
            dec!(0),
        )]);
        let open_orders = Vec::new();
        let first_context = context_orders(DateTime::from_secs(0), &first_securities, &open_orders);
        model.execute_with_context(&[make_target("AAPL", 10.0)], &first_context);

        let second_securities = securities_map(vec![make_security_with_quote(
            "AAPL",
            dec!(100),
            dec!(99.95),
            dec!(100.05),
            dec!(0),
            dec!(10),
        )]);
        let resting = vec![make_open_limit_order(
            1,
            "AAPL",
            dec!(10),
            dec!(99.95),
            DateTime::from_secs(0),
            "MakerThenTakerExecutionModel post-only",
        )];
        let second_context = context_orders(DateTime::from_secs(301), &second_securities, &resting);

        let second = model.execute_with_context(&[], &second_context);

        assert_eq!(second.len(), 1);
        assert_eq!(second[0].order_type, ExecutionOrderType::Market);
        assert_eq!(second[0].quantity, dec!(10));
        assert!(second[0].cancel_open_orders);
        assert!(second[0].tag.contains("deadline"));
    }
}
