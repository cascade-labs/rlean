use crate::contract::OptionContract;
use crate::payoff::{get_exercise_quantity, is_auto_exercised};
use lean_core::{DateTime, SettlementType};
use lean_orders::{OptionExerciseOrder, OrderDirection, OrderEvent, OrderStatus};
use rust_decimal::Decimal;

pub trait IOptionExerciseModel: Send + Sync {
    /// Generate fill events for an option exercise/assignment.
    /// Returns 1-2 OrderEvents:
    ///   1. Always: close the option position (fill price = 0)
    ///   2. If ITM + PhysicalDelivery: open/close the underlying at strike price
    fn option_exercise(
        &self,
        contract: &OptionContract,
        order: &OptionExerciseOrder,
        underlying_price: Decimal,
        settlement: SettlementType,
        utc_time: DateTime,
    ) -> Vec<OrderEvent>;
}

pub struct DefaultExerciseModel;

impl IOptionExerciseModel for DefaultExerciseModel {
    fn option_exercise(
        &self,
        contract: &OptionContract,
        order: &OptionExerciseOrder,
        underlying_price: Decimal,
        settlement: SettlementType,
        utc_time: DateTime,
    ) -> Vec<OrderEvent> {
        let quantity = order.order.quantity;
        let in_the_money = is_auto_exercised(underlying_price, contract.strike, contract.right);
        // Assignment = short holder being assigned (quantity > 0 means we are short)
        let is_assignment = in_the_money && quantity > Decimal::ZERO;

        // Event 1: close the option position at $0
        let option_direction = if quantity >= Decimal::ZERO {
            OrderDirection::Sell // closing short position
        } else {
            OrderDirection::Buy // closing long position
        };

        let mut events = vec![OrderEvent {
            id: 0,
            order_id: order.order.id,
            symbol: contract.symbol.clone(),
            utc_time,
            status: OrderStatus::Filled,
            direction: option_direction,
            fill_price: Decimal::ZERO,
            fill_price_currency: "USD".to_string(),
            fill_quantity: quantity,
            is_assignment,
            is_in_the_money: in_the_money,
            quantity,
            message: format!(
                "Option {} {}",
                if in_the_money {
                    "exercised"
                } else {
                    "expired worthless"
                },
                if is_assignment { "(assignment)" } else { "" }
            ),
            shortable_inventory: None,
            order_fee: Decimal::ZERO,
            limit_price: None,
            stop_price: None,
            trigger_price: None,
            trailing_amount: None,
            trailing_as_percentage: false,
        }];

        // Event 2: physical delivery — create/close underlying position at strike
        if in_the_money && settlement == SettlementType::PhysicalDelivery {
            let exercise_qty =
                get_exercise_quantity(quantity, contract.right, contract.contract_unit_of_trade);
            let underlying_direction = if exercise_qty >= Decimal::ZERO {
                OrderDirection::Buy
            } else {
                OrderDirection::Sell
            };

            // Get the underlying symbol from the option symbol's `underlying` field
            if let Some(ref underlying_sym) = contract.symbol.underlying {
                events.push(OrderEvent {
                    id: 0,
                    order_id: order.order.id,
                    symbol: *underlying_sym.clone(),
                    utc_time,
                    status: OrderStatus::Filled,
                    direction: underlying_direction,
                    fill_price: contract.strike,
                    fill_price_currency: "USD".to_string(),
                    fill_quantity: exercise_qty.abs(),
                    is_assignment,
                    is_in_the_money: true,
                    quantity: exercise_qty.abs(),
                    message: if is_assignment {
                        "Option Assignment".to_string()
                    } else {
                        "Option Exercise".to_string()
                    },
                    shortable_inventory: None,
                    order_fee: Decimal::ZERO,
                    limit_price: None,
                    stop_price: None,
                    trigger_price: None,
                    trailing_amount: None,
                    trailing_as_percentage: false,
                });
            }
        }

        events
    }
}
