use anyhow::{anyhow, bail, Context};
use chrono::{Duration, NaiveDate};
use rlean_core::{
    Market, NanosecondTimestamp, OptionRight, OptionStyle, SecurityIdentifier, SecurityType, Symbol,
};
use rlean_orders::order::OrderProperties;
use rlean_orders::{Order, OrderStatus, OrderType, TimeInForce};
use rust_decimal::Decimal;

use crate::{WireOrder, WireSymbol};

impl From<&Symbol> for WireSymbol {
    fn from(symbol: &Symbol) -> Self {
        Self {
            sid: symbol.id.sid,
            value: symbol.value.to_string(),
            permanent_ticker: symbol.permtick.to_string(),
            identifier_ticker: symbol.id.ticker.clone(),
            security_type: symbol.id.security_type as i32,
            market: symbol.id.market.as_str().to_string(),
            expiry_day: symbol.id.expiry.map(days_since_epoch),
            strike: symbol.id.strike.map(|value| value.to_string()),
            option_right: symbol.id.option_right.map(|value| value as i32),
            option_style: symbol.id.option_style.map(|value| value as i32),
            underlying: symbol
                .underlying()
                .map(|underlying| Box::new(WireSymbol::from(underlying))),
        }
    }
}

pub fn symbol_from_wire(symbol: WireSymbol) -> anyhow::Result<Symbol> {
    let security_type = SecurityType::from_repr(
        u8::try_from(symbol.security_type).context("negative security type")?,
    )
    .ok_or_else(|| anyhow!("unknown security type {}", symbol.security_type))?;
    let expiry = symbol.expiry_day.map(date_from_days).transpose()?;
    let strike = symbol.strike.as_deref().map(parse_decimal).transpose()?;
    let option_right = symbol
        .option_right
        .map(|value| {
            OptionRight::from_repr(u8::try_from(value).context("negative option right")?)
                .ok_or_else(|| anyhow!("unknown option right {value}"))
        })
        .transpose()?;
    let option_style = symbol
        .option_style
        .map(|value| {
            OptionStyle::from_repr(u8::try_from(value).context("negative option style")?)
                .ok_or_else(|| anyhow!("unknown option style {value}"))
        })
        .transpose()?;
    let underlying = symbol
        .underlying
        .map(|value| symbol_from_wire(*value))
        .transpose()?;
    Ok(Symbol::from_parts(
        SecurityIdentifier {
            ticker: symbol.identifier_ticker,
            market: Market::new(symbol.market),
            security_type,
            expiry,
            strike,
            option_right,
            option_style,
            sid: symbol.sid,
        },
        symbol.value,
        symbol.permanent_ticker,
        underlying,
    ))
}

impl From<&Order> for WireOrder {
    fn from(order: &Order) -> Self {
        let (time_in_force, good_til_time_ns) = match order.time_in_force {
            TimeInForce::GoodTilCanceled => (0, None),
            TimeInForce::Day => (1, None),
            TimeInForce::GoodTilDate(time) => (2, Some(time.0)),
            TimeInForce::ImmediateOrCancel => (3, None),
            TimeInForce::FillOrKill => (4, None),
        };
        Self {
            engine_order_id: order.id,
            contingent_id: order.contingent_id,
            brokerage_order_ids: order.brokerage_id.clone(),
            symbol: Some(WireSymbol::from(&order.symbol)),
            order_type: order_type_to_wire(order.order_type),
            status: order_status_to_wire(order.status),
            quantity: order.quantity.to_string(),
            filled_quantity: order.filled_quantity.to_string(),
            average_fill_price: order.average_fill_price.to_string(),
            limit_price: order.limit_price.map(|value| value.to_string()),
            stop_price: order.stop_price.map(|value| value.to_string()),
            trailing_amount: order.trailing_amount.map(|value| value.to_string()),
            trailing_as_percentage: order.trailing_as_percent,
            time_ns: order.time.0,
            created_time_ns: order.created_time.0,
            time_in_force,
            good_til_time_ns,
            price_currency: order.price_currency.clone(),
            tag: order.tag.clone(),
            exchange: order.properties.exchange.clone(),
            outside_regular_trading_hours: order.properties.outside_regular_trading_hours,
            post_only: order.properties.post_only,
            price: order.price.to_string(),
        }
    }
}

impl TryFrom<WireOrder> for Order {
    type Error = anyhow::Error;

    fn try_from(order: WireOrder) -> Result<Self, Self::Error> {
        let symbol = symbol_from_wire(order.symbol.context("wire order is missing symbol")?)?;
        let time_in_force = match order.time_in_force {
            0 => TimeInForce::GoodTilCanceled,
            1 => TimeInForce::Day,
            2 => TimeInForce::GoodTilDate(NanosecondTimestamp(
                order
                    .good_til_time_ns
                    .context("GoodTilDate order is missing its expiry")?,
            )),
            3 => TimeInForce::ImmediateOrCancel,
            4 => TimeInForce::FillOrKill,
            value => bail!("unknown time-in-force {value}"),
        };
        Ok(Order {
            id: order.engine_order_id,
            contingent_id: order.contingent_id,
            brokerage_id: order.brokerage_order_ids,
            symbol,
            price: parse_decimal(&order.price)?,
            price_currency: order.price_currency,
            time: NanosecondTimestamp(order.time_ns),
            created_time: NanosecondTimestamp(order.created_time_ns),
            last_fill_time: None,
            last_update_time: None,
            canceled_time: None,
            quantity: parse_decimal(&order.quantity)?,
            filled_quantity: parse_decimal(&order.filled_quantity)?,
            average_fill_price: parse_decimal(&order.average_fill_price)?,
            order_type: order_type_from_wire(order.order_type)?,
            status: order_status_from_wire(order.status)?,
            time_in_force: time_in_force.clone(),
            tag: order.tag,
            properties: OrderProperties {
                exchange: order.exchange,
                time_in_force: Some(time_in_force),
                outside_regular_trading_hours: order.outside_regular_trading_hours,
                post_only: order.post_only,
            },
            order_submission_data: None,
            limit_price: order
                .limit_price
                .as_deref()
                .map(parse_decimal)
                .transpose()?,
            stop_price: order.stop_price.as_deref().map(parse_decimal).transpose()?,
            trailing_amount: order
                .trailing_amount
                .as_deref()
                .map(parse_decimal)
                .transpose()?,
            trailing_as_percent: order.trailing_as_percentage,
        })
    }
}

fn order_type_to_wire(value: OrderType) -> i32 {
    value as i32
}

fn order_type_from_wire(value: i32) -> anyhow::Result<OrderType> {
    Ok(match value {
        0 => OrderType::Market,
        1 => OrderType::Limit,
        2 => OrderType::StopMarket,
        3 => OrderType::StopLimit,
        4 => OrderType::MarketOnOpen,
        5 => OrderType::MarketOnClose,
        6 => OrderType::TrailingStop,
        7 => OrderType::LimitIfTouched,
        8 => OrderType::ComboMarket,
        9 => OrderType::ComboLimit,
        10 => OrderType::ComboLegLimit,
        11 => OrderType::OptionExercise,
        _ => bail!("unknown order type {value}"),
    })
}

fn order_status_to_wire(value: OrderStatus) -> i32 {
    value as i32
}

pub fn order_status_from_wire(value: i32) -> anyhow::Result<OrderStatus> {
    Ok(match value {
        0 => OrderStatus::New,
        1 => OrderStatus::Submitted,
        2 => OrderStatus::PartiallyFilled,
        3 => OrderStatus::Filled,
        5 => OrderStatus::Canceled,
        6 => OrderStatus::None,
        7 => OrderStatus::Invalid,
        8 => OrderStatus::CancelPending,
        9 => OrderStatus::UpdateSubmitted,
        _ => bail!("unknown order status {value}"),
    })
}

fn parse_decimal(value: &str) -> anyhow::Result<Decimal> {
    value
        .parse()
        .with_context(|| format!("invalid decimal '{value}'"))
}

fn days_since_epoch(date: NaiveDate) -> i32 {
    date.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
        .num_days() as i32
}

fn date_from_days(days: i32) -> anyhow::Result<NaiveDate> {
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .checked_add_signed(Duration::days(i64::from(days)))
        .context("wire symbol expiry is outside the supported date range")
}
