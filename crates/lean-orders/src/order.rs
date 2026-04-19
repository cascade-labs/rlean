use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
    StopMarket,
    StopLimit,
    MarketOnOpen,
    MarketOnClose,
    TrailingStop,
    LimitIfTouched,
    ComboMarket,
    ComboLimit,
    ComboLegLimit,
    OptionExercise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderStatus {
    New = 0,
    Submitted = 1,
    PartiallyFilled = 2,
    Filled = 3,
    Canceled = 5,
    None = 6,
    Invalid = 7,
    CancelPending = 8,
    UpdateSubmitted = 9,
}

impl OrderStatus {
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            OrderStatus::New
                | OrderStatus::Submitted
                | OrderStatus::PartiallyFilled
                | OrderStatus::UpdateSubmitted
        )
    }

    pub fn is_closed(&self) -> bool {
        !self.is_open()
    }

    /// Mirrors C# LEAN's `OrderStatus.IsFill()` extension method.
    pub fn is_fill(&self) -> bool {
        matches!(self, OrderStatus::Filled | OrderStatus::PartiallyFilled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderDirection {
    Buy,
    Sell,
    Hold,
}

impl OrderDirection {
    pub fn from_quantity(qty: Quantity) -> Self {
        if qty > dec!(0) {
            OrderDirection::Buy
        } else if qty < dec!(0) {
            OrderDirection::Sell
        } else {
            OrderDirection::Hold
        }
    }

    pub fn opposite(&self) -> Self {
        match self {
            OrderDirection::Buy => OrderDirection::Sell,
            OrderDirection::Sell => OrderDirection::Buy,
            OrderDirection::Hold => OrderDirection::Hold,
        }
    }
}

/// Time-in-force controls how long an order stays active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum TimeInForce {
    /// Good until canceled (default).
    #[default]
    GoodTilCanceled,
    /// Day — cancel at end of session.
    Day,
    /// Good until a specific date/time.
    GoodTilDate(DateTime),
    /// Immediate or cancel — fill what you can, cancel the rest.
    ImmediateOrCancel,
    /// Fill or kill — fill completely or cancel entirely.
    FillOrKill,
}

/// Core order struct. Immutable after submission; updates create new versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: i64,
    pub contingent_id: i64,
    pub brokerage_id: Vec<String>,
    pub symbol: Symbol,
    pub price: Price,
    pub price_currency: String,
    pub time: DateTime,
    pub created_time: DateTime,
    pub last_fill_time: Option<DateTime>,
    pub last_update_time: Option<DateTime>,
    pub canceled_time: Option<DateTime>,
    pub quantity: Quantity,
    pub filled_quantity: Quantity,
    pub average_fill_price: Price,
    pub order_type: OrderType,
    pub status: OrderStatus,
    pub time_in_force: TimeInForce,
    pub tag: String,
    pub properties: OrderProperties,
    // For limit/stop orders
    pub limit_price: Option<Price>,
    pub stop_price: Option<Price>,
    pub trailing_amount: Option<Price>,
    pub trailing_as_percent: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrderProperties {
    pub exchange: Option<String>,
    pub time_in_force: Option<TimeInForce>,
}

impl Order {
    pub fn market(id: i64, symbol: Symbol, quantity: Quantity, time: DateTime, tag: &str) -> Self {
        Order {
            id,
            contingent_id: 0,
            brokerage_id: vec![],
            symbol,
            price: dec!(0),
            price_currency: "USD".into(),
            time,
            created_time: time,
            last_fill_time: None,
            last_update_time: None,
            canceled_time: None,
            quantity,
            filled_quantity: dec!(0),
            average_fill_price: dec!(0),
            order_type: OrderType::Market,
            status: OrderStatus::New,
            time_in_force: TimeInForce::GoodTilCanceled,
            tag: tag.to_string(),
            properties: Default::default(),
            limit_price: None,
            stop_price: None,
            trailing_amount: None,
            trailing_as_percent: false,
        }
    }

    pub fn limit(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        limit_price: Price,
        time: DateTime,
        tag: &str,
    ) -> Self {
        let mut o = Self::market(id, symbol, quantity, time, tag);
        o.order_type = OrderType::Limit;
        o.price = limit_price;
        o.limit_price = Some(limit_price);
        o
    }

    pub fn stop_market(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        stop_price: Price,
        time: DateTime,
        tag: &str,
    ) -> Self {
        let mut o = Self::market(id, symbol, quantity, time, tag);
        o.order_type = OrderType::StopMarket;
        o.stop_price = Some(stop_price);
        o
    }

    pub fn stop_limit(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        stop_price: Price,
        limit_price: Price,
        time: DateTime,
        tag: &str,
    ) -> Self {
        let mut o = Self::market(id, symbol, quantity, time, tag);
        o.order_type = OrderType::StopLimit;
        o.stop_price = Some(stop_price);
        o.limit_price = Some(limit_price);
        o
    }

    pub fn direction(&self) -> OrderDirection {
        OrderDirection::from_quantity(self.quantity)
    }

    pub fn abs_quantity(&self) -> Quantity {
        self.quantity.abs()
    }

    pub fn remaining_quantity(&self) -> Quantity {
        self.quantity - self.filled_quantity
    }

    pub fn is_filled(&self) -> bool {
        self.status == OrderStatus::Filled
    }

    pub fn is_open(&self) -> bool {
        self.status.is_open()
    }

    pub fn value(&self) -> Price {
        self.quantity * self.price
    }
}
