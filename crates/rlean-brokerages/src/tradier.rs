use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use reqwest::blocking::{Client, Response};
use rlean_core::{
    format_option_ticker, DateTime, Market, OptionRight, OptionStyle, SecurityType, Symbol,
    SymbolOptionsExt,
};
use rlean_orders::{Order, OrderStatus, OrderType, TimeInForce};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::{Brokerage, BrokerageHolding, BrokerageOrderSubmission};

const LIVE_BASE: &str = "https://api.tradier.com/v1";
const PAPER_BASE: &str = "https://sandbox.tradier.com/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradierEnvironment {
    Live,
    Paper,
}

#[derive(Debug, Clone)]
pub struct TradierBrokerageConfig {
    pub access_token: String,
    pub account_id: String,
    pub environment: TradierEnvironment,
    pub base_url: String,
}

impl TradierBrokerageConfig {
    pub fn new(
        access_token: impl Into<String>,
        account_id: impl Into<String>,
        environment: TradierEnvironment,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            account_id: account_id.into(),
            environment,
            base_url: match environment {
                TradierEnvironment::Live => LIVE_BASE,
                TradierEnvironment::Paper => PAPER_BASE,
            }
            .to_string(),
        }
    }
}

pub struct TradierBrokerage {
    config: TradierBrokerageConfig,
    client: Client,
    connected: bool,
}

impl TradierBrokerage {
    pub fn new(config: TradierBrokerageConfig) -> Result<Self> {
        if config.access_token.trim().is_empty() {
            bail!("Tradier brokerage access token cannot be empty");
        }
        if config.account_id.trim().is_empty() {
            bail!("Tradier brokerage account id cannot be empty");
        }
        Ok(Self {
            config,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .pool_max_idle_per_host(4)
                .build()?,
            connected: false,
        })
    }

    fn account_get<T: DeserializeOwned>(&self, suffix: &str) -> Result<T> {
        checked(
            self.client
                .get(format!(
                    "{}/accounts/{}/{}",
                    self.config.base_url.trim_end_matches('/'),
                    self.config.account_id,
                    suffix
                ))
                .bearer_auth(&self.config.access_token)
                .header("Accept", "application/json")
                .send()?,
            suffix,
        )?
        .json()
        .context("decode Tradier account response")
    }

    fn positions(&self) -> Result<Vec<TradierPosition>> {
        let response: PositionsResponse = self.account_get("positions")?;
        normalize_collection(response.positions, "position")
    }

    fn orders(&self) -> Result<Vec<TradierOrder>> {
        let response: OrdersResponse = self.account_get("orders")?;
        normalize_collection(response.orders, "order")
    }

    fn balance(&self) -> Result<TradierBalance> {
        Ok(self.account_get::<BalanceResponse>("balances")?.balances)
    }

    fn quotes(&self, symbols: &[String]) -> Result<HashMap<String, Decimal>> {
        if symbols.is_empty() {
            return Ok(HashMap::new());
        }
        let response = checked(
            self.client
                .get(format!(
                    "{}/markets/quotes",
                    self.config.base_url.trim_end_matches('/')
                ))
                .bearer_auth(&self.config.access_token)
                .header("Accept", "application/json")
                .query(&[
                    ("symbols", symbols.join(",")),
                    ("greeks", "false".to_string()),
                ])
                .send()?,
            "position quotes",
        )?;
        let response: QuotesResponse = response.json()?;
        let quotes: Vec<TradierQuote> =
            normalize_collection(response.quotes.map(|quotes| quotes.quote), "quote")?;
        Ok(quotes
            .into_iter()
            .filter_map(|quote| {
                positive_decimal(quote.last)
                    .or_else(|| {
                        Some(
                            (positive_decimal(quote.bid)? + positive_decimal(quote.ask)?)
                                / Decimal::TWO,
                        )
                    })
                    .map(|price| (quote.symbol.to_ascii_uppercase(), price))
            })
            .collect())
    }

    fn submit(&self, order: &Order) -> Result<String> {
        let position = self
            .positions()?
            .into_iter()
            .find(|position| {
                position
                    .symbol
                    .eq_ignore_ascii_case(&broker_symbol(&order.symbol))
            })
            .map(|position| position.quantity)
            .unwrap_or_default();
        let params = order_params(order, position)?;
        let response: OrderResponse = checked(
            self.client
                .post(format!(
                    "{}/accounts/{}/orders",
                    self.config.base_url.trim_end_matches('/'),
                    self.config.account_id
                ))
                .bearer_auth(&self.config.access_token)
                .header("Accept", "application/json")
                .form(&params)
                .send()?,
            "order submission",
        )?
        .json()?;
        validate_order_response(&response)?;
        Ok(response.order.id.to_string())
    }

    fn modify(&self, order: &Order) -> Result<()> {
        let id = order
            .brokerage_id
            .first()
            .context("Tradier update requires a brokerage order id")?;
        let mut params = vec![
            ("type", order_type(order.order_type)?.to_string()),
            ("duration", duration(&order.time_in_force)?.to_string()),
        ];
        if let Some(price) = order.limit_price {
            params.push(("price", price.to_string()));
        }
        if let Some(stop) = order.stop_price {
            params.push(("stop", stop.to_string()));
        }
        let response: OrderResponse = checked(
            self.client
                .put(format!(
                    "{}/accounts/{}/orders/{id}",
                    self.config.base_url.trim_end_matches('/'),
                    self.config.account_id
                ))
                .bearer_auth(&self.config.access_token)
                .header("Accept", "application/json")
                .form(&params)
                .send()?,
            "order update",
        )?
        .json()?;
        validate_order_response(&response)
    }

    fn cancel(&self, order: &Order) -> Result<()> {
        let id = order
            .brokerage_id
            .first()
            .context("Tradier cancellation requires a brokerage order id")?;
        let response: OrderResponse = checked(
            self.client
                .delete(format!(
                    "{}/accounts/{}/orders/{id}",
                    self.config.base_url.trim_end_matches('/'),
                    self.config.account_id
                ))
                .bearer_auth(&self.config.access_token)
                .header("Accept", "application/json")
                .send()?,
            "order cancellation",
        )?
        .json()?;
        validate_order_response(&response)
    }
}

impl Brokerage for TradierBrokerage {
    fn name(&self) -> &str {
        "Tradier"
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn connect(&mut self) -> rlean_core::Result<()> {
        self.balance()?;
        self.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) {
        self.connected = false;
    }

    fn place_order(&mut self, order: Order) -> rlean_core::Result<bool> {
        Ok(matches!(
            self.place_order_with_brokerage_ids(order)?,
            BrokerageOrderSubmission::Accepted(_)
        ))
    }

    fn place_order_with_brokerage_ids(
        &mut self,
        order: Order,
    ) -> rlean_core::Result<BrokerageOrderSubmission> {
        Ok(BrokerageOrderSubmission::Accepted(vec![
            self.submit(&order)?
        ]))
    }

    fn update_order(&mut self, order: &Order) -> rlean_core::Result<bool> {
        self.modify(order)?;
        Ok(true)
    }

    fn cancel_order(&mut self, order: &Order) -> rlean_core::Result<bool> {
        self.cancel(order)?;
        Ok(true)
    }

    fn get_open_orders(&self) -> Vec<Order> {
        self.orders()
            .unwrap_or_else(|error| {
                tracing::error!(%error, "Tradier open-order query failed");
                Vec::new()
            })
            .into_iter()
            .filter(|order| status(&order.status).is_open())
            .filter_map(to_order)
            .collect()
    }

    fn get_account_orders(&self) -> Vec<Order> {
        self.orders()
            .unwrap_or_else(|error| {
                tracing::error!(%error, "Tradier account-order query failed");
                Vec::new()
            })
            .into_iter()
            .filter_map(to_order)
            .collect()
    }

    fn get_cash_balance(&self) -> Vec<(String, Decimal)> {
        self.balance()
            .map(|balance| vec![("USD".to_string(), decimal(balance.total_cash))])
            .unwrap_or_else(|error| {
                tracing::error!(%error, "Tradier balance query failed");
                Vec::new()
            })
    }

    fn get_account_holdings(&self) -> HashMap<Symbol, Decimal> {
        self.get_account_detailed_holdings()
            .into_iter()
            .map(|holding| (holding.symbol, holding.quantity))
            .collect()
    }

    fn get_account_detailed_holdings(&self) -> Vec<BrokerageHolding> {
        let positions = self.positions().unwrap_or_else(|error| {
            tracing::error!(%error, "Tradier positions query failed");
            Vec::new()
        });
        let symbols = positions
            .iter()
            .map(|position| position.symbol.clone())
            .collect::<Vec<_>>();
        let prices = self.quotes(&symbols).unwrap_or_default();
        positions
            .into_iter()
            .map(|position| {
                let symbol = parse_symbol(&position.symbol)
                    .unwrap_or_else(|| Symbol::create_equity(&position.symbol, &Market::usa()));
                let multiplier = if matches!(
                    symbol.security_type(),
                    SecurityType::Option | SecurityType::IndexOption
                ) {
                    Decimal::from(100)
                } else {
                    Decimal::ONE
                };
                let average_price = if position.quantity.is_zero() {
                    Decimal::ZERO
                } else {
                    decimal(position.cost_basis).abs() / position.quantity.abs() / multiplier
                };
                BrokerageHolding {
                    symbol,
                    quantity: position.quantity,
                    average_price,
                    market_price: prices
                        .get(&position.symbol.to_ascii_uppercase())
                        .copied()
                        .unwrap_or_default(),
                }
            })
            .collect()
    }
}

fn order_params(order: &Order, position: Decimal) -> Result<Vec<(&'static str, String)>> {
    if order.quantity.is_zero() {
        bail!("Tradier order quantity cannot be zero");
    }
    let buy = order.quantity > Decimal::ZERO;
    let (class, symbol, option_symbol, side) = match order.symbol.security_type() {
        SecurityType::Equity => {
            let side = if buy {
                if position < Decimal::ZERO {
                    "buy_to_cover"
                } else {
                    "buy"
                }
            } else if position > Decimal::ZERO {
                "sell"
            } else {
                "sell_short"
            };
            (
                "equity",
                order.symbol.permtick.to_ascii_uppercase(),
                None,
                side,
            )
        }
        SecurityType::Option | SecurityType::IndexOption => {
            let option = order
                .symbol
                .option_symbol_id()
                .context("Tradier option is missing contract metadata")?;
            let root = option.underlying.permtick.to_ascii_uppercase();
            let contract = format_option_ticker(&root, option.strike, option.expiry, option.right);
            let side = if buy {
                if position < Decimal::ZERO {
                    "buy_to_close"
                } else {
                    "buy_to_open"
                }
            } else if position > Decimal::ZERO {
                "sell_to_close"
            } else {
                "sell_to_open"
            };
            ("option", root, Some(contract), side)
        }
        other => bail!("Tradier does not support {other:?} execution"),
    };
    let mut params = vec![
        ("class", class.to_string()),
        ("symbol", symbol),
        ("side", side.to_string()),
        ("quantity", order.quantity.abs().to_string()),
        ("type", order_type(order.order_type)?.to_string()),
        ("duration", duration(&order.time_in_force)?.to_string()),
    ];
    if let Some(option_symbol) = option_symbol {
        params.push(("option_symbol", option_symbol));
    }
    if let Some(price) = order.limit_price {
        params.push(("price", price.to_string()));
    }
    if let Some(stop) = order.stop_price {
        params.push(("stop", stop.to_string()));
    }
    Ok(params)
}

fn order_type(value: OrderType) -> Result<&'static str> {
    match value {
        OrderType::Market => Ok("market"),
        OrderType::Limit => Ok("limit"),
        OrderType::StopMarket => Ok("stop"),
        OrderType::StopLimit => Ok("stop_limit"),
        other => bail!("Tradier does not support {other:?} orders"),
    }
}

fn duration(value: &TimeInForce) -> Result<&'static str> {
    match value {
        TimeInForce::Day => Ok("day"),
        TimeInForce::GoodTilCanceled => Ok("gtc"),
        other => bail!("Tradier does not support {other:?} time in force"),
    }
}

fn checked(response: Response, operation: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().unwrap_or_default();
    bail!("Tradier {operation} failed with HTTP {status}: {body}")
}

fn validate_order_response(response: &OrderResponse) -> Result<()> {
    if let Some(errors) = &response.errors {
        if !errors.error.is_empty() {
            bail!("Tradier order rejected: {}", errors.error.join("; "));
        }
    }
    if response.order.id <= 0 || response.order.status.eq_ignore_ascii_case("rejected") {
        bail!("Tradier rejected the order");
    }
    Ok(())
}

fn status(value: &str) -> OrderStatus {
    match value.to_ascii_lowercase().as_str() {
        "filled" => OrderStatus::Filled,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "canceled" | "cancelled" | "expired" => OrderStatus::Canceled,
        "rejected" => OrderStatus::Invalid,
        _ => OrderStatus::Submitted,
    }
}

fn to_order(raw: TradierOrder) -> Option<Order> {
    let ticker = raw
        .option_symbol
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&raw.symbol);
    let symbol =
        parse_symbol(ticker).unwrap_or_else(|| Symbol::create_equity(ticker, &Market::usa()));
    let sign = if matches!(
        raw.side.as_str(),
        "buy" | "buy_to_cover" | "buy_to_open" | "buy_to_close"
    ) {
        Decimal::ONE
    } else {
        -Decimal::ONE
    };
    let mut order = Order::market(raw.id, symbol, raw.quantity * sign, DateTime::now(), "");
    order.brokerage_id = vec![raw.id.to_string()];
    order.order_type = match raw.order_type.as_str() {
        "market" => OrderType::Market,
        "limit" => OrderType::Limit,
        "stop" => OrderType::StopMarket,
        "stop_limit" => OrderType::StopLimit,
        _ => return None,
    };
    order.status = status(&raw.status);
    order.filled_quantity = decimal(raw.exec_quantity) * sign;
    order.average_fill_price = decimal(raw.avg_fill_price);
    order.limit_price = (raw.price > 0.0).then(|| decimal(raw.price));
    order.stop_price = (raw.stop_price > 0.0).then(|| decimal(raw.stop_price));
    Some(order)
}

fn broker_symbol(symbol: &Symbol) -> String {
    symbol
        .option_symbol_id()
        .map(|id| format_option_ticker(&id.underlying.permtick, id.strike, id.expiry, id.right))
        .unwrap_or_else(|| symbol.permtick.to_ascii_uppercase())
}

fn parse_symbol(value: &str) -> Option<Symbol> {
    let value = value.trim().to_ascii_uppercase();
    let suffix_start = value.len().checked_sub(15)?;
    let root = &value[..suffix_start];
    let suffix = &value[suffix_start..];
    if root.is_empty()
        || !suffix[..6].chars().all(|c| c.is_ascii_digit())
        || !suffix[7..].chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let expiry = NaiveDate::parse_from_str(&suffix[..6], "%y%m%d").ok()?;
    let right = match &suffix[6..7] {
        "C" => OptionRight::Call,
        "P" => OptionRight::Put,
        _ => return None,
    };
    let strike = Decimal::from_i128_with_scale(suffix[7..].parse().ok()?, 3);
    if matches!(
        root,
        "SPX" | "NDX" | "VIX" | "RUT" | "RUTW" | "SPXW" | "VIXW" | "NDXP" | "NQX"
    ) {
        Some(Symbol::create_index_option_osi(
            Symbol::create_index(root, &Market::usa()),
            strike,
            expiry,
            right,
            OptionStyle::European,
            &Market::usa(),
        ))
    } else {
        Some(Symbol::create_option_osi(
            Symbol::create_equity(root, &Market::usa()),
            strike,
            expiry,
            right,
            OptionStyle::American,
            &Market::usa(),
        ))
    }
}

fn decimal(value: f64) -> Decimal {
    Decimal::from_f64(value).unwrap_or_default()
}
fn positive_decimal(value: f64) -> Option<Decimal> {
    Decimal::from_f64(value).filter(|value| *value > Decimal::ZERO)
}

fn normalize_collection<T: DeserializeOwned>(value: Option<Value>, field: &str) -> Result<Vec<T>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null()
        || value
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case("null"))
    {
        return Ok(Vec::new());
    }
    if let Some(items) = value.get(field) {
        if items.is_array() {
            return Ok(serde_json::from_value(items.clone())?);
        }
        return Ok(vec![serde_json::from_value(items.clone())?]);
    }
    if value.is_array() {
        return Ok(serde_json::from_value(value)?);
    }
    Ok(vec![serde_json::from_value(value)?])
}

fn decimal_from_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Decimal, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_f64().map(|value| value.to_string()))
        .ok_or_else(|| serde::de::Error::custom("invalid decimal"))?
        .parse()
        .map_err(serde::de::Error::custom)
}

fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct PositionsResponse {
    positions: Option<Value>,
}
#[derive(Deserialize)]
struct OrdersResponse {
    orders: Option<Value>,
}
#[derive(Deserialize)]
struct BalanceResponse {
    balances: TradierBalance,
}
#[derive(Deserialize)]
struct TradierBalance {
    #[serde(default)]
    total_cash: f64,
}
#[derive(Deserialize)]
struct QuotesResponse {
    quotes: Option<QuoteContainer>,
}
#[derive(Deserialize)]
struct QuoteContainer {
    quote: Value,
}
#[derive(Deserialize)]
struct TradierQuote {
    symbol: String,
    #[serde(default, deserialize_with = "null_default")]
    last: f64,
    #[serde(default, deserialize_with = "null_default")]
    bid: f64,
    #[serde(default, deserialize_with = "null_default")]
    ask: f64,
}
#[derive(Deserialize)]
struct TradierPosition {
    symbol: String,
    #[serde(deserialize_with = "decimal_from_value")]
    quantity: Decimal,
    #[serde(default)]
    cost_basis: f64,
}
#[derive(Deserialize)]
struct TradierOrder {
    id: i64,
    #[serde(rename = "type")]
    order_type: String,
    symbol: String,
    #[serde(default)]
    option_symbol: Option<String>,
    side: String,
    #[serde(deserialize_with = "decimal_from_value")]
    quantity: Decimal,
    status: String,
    #[serde(default, deserialize_with = "null_default")]
    price: f64,
    #[serde(default, alias = "stop", deserialize_with = "null_default")]
    stop_price: f64,
    #[serde(default, deserialize_with = "null_default")]
    avg_fill_price: f64,
    #[serde(default, deserialize_with = "null_default")]
    exec_quantity: f64,
}
#[derive(Deserialize)]
struct OrderResponse {
    order: OrderResponseOrder,
    #[serde(default)]
    errors: Option<TradierErrors>,
}
#[derive(Deserialize)]
struct OrderResponseOrder {
    id: i64,
    status: String,
}
#[derive(Deserialize)]
struct TradierErrors {
    #[serde(default)]
    error: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn equity_close_uses_sell_side() {
        let order = Order::market(
            1,
            Symbol::create_equity("SPY", &Market::usa()),
            dec!(-10),
            DateTime::now(),
            "",
        );
        let params = order_params(&order, dec!(20)).unwrap();
        assert!(params.contains(&("side", "sell".to_string())));
        assert!(params.contains(&("duration", "gtc".to_string())));
    }
}
