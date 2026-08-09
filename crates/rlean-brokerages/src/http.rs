use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, Response};
use rlean_core::{DateTime, Market, Symbol};
use rlean_orders::{Order, OrderStatus, OrderType, TimeInForce, UpdateOrderRequest};
use rust_decimal::Decimal;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{Brokerage, BrokerageHolding};

/// Configuration for the language-neutral HTTP execution brokerage contract.
#[derive(Debug, Clone)]
pub struct HttpBrokerageConfig {
    pub base_url: String,
    pub account_id: Option<String>,
}

impl HttpBrokerageConfig {
    pub fn new(base_url: impl Into<String>, account_id: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            account_id,
        }
    }
}

/// Execution brokerage backed by the documented rlean HTTP brokerage API.
pub struct HttpBrokerage {
    config: HttpBrokerageConfig,
    client: Client,
    metadata: BrokerageMetadata,
    connected: bool,
}

impl HttpBrokerage {
    pub fn new(config: HttpBrokerageConfig) -> Result<Self> {
        let base_url = config.base_url.trim_end_matches('/');
        if base_url.is_empty() {
            bail!("HTTP brokerage URL cannot be empty");
        }
        reqwest::Url::parse(base_url).context("parse HTTP brokerage URL")?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .build()?;
        let metadata = decode::<BrokerageMetadata>(
            checked(
                client.get(format!("{base_url}/brokerage")).send()?,
                "metadata",
            )?,
            "metadata",
        )?;
        if metadata.name.trim().is_empty() || metadata.brokerage_model.trim().is_empty() {
            bail!("HTTP brokerage metadata requires name and brokerage_model");
        }
        Ok(Self {
            config: HttpBrokerageConfig {
                base_url: base_url.to_owned(),
                ..config
            },
            client,
            metadata,
            connected: false,
        })
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(self.url(path))
            .query(&self.account_query())
            .send()?;
        decode(checked(response, path)?, path)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url, path)
    }

    fn account_query(&self) -> Vec<(&'static str, &str)> {
        self.config
            .account_id
            .as_deref()
            .map(|account| vec![("account_number", account)])
            .unwrap_or_default()
    }

    fn orders(&self, open_only: bool) -> Result<Vec<HttpOrder>> {
        self.get(if open_only { "/orders/open" } else { "/orders" })
    }

    fn submit(&self, order: &Order) -> Result<String> {
        let order_type = match order.order_type {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
            other => bail!("HTTP brokerage does not support {other:?} orders"),
        };
        let time_in_force = match order.time_in_force {
            TimeInForce::Day => "day",
            TimeInForce::GoodTilCanceled => "gtc",
            ref other => bail!("HTTP brokerage does not support {other:?} time in force"),
        };
        if order.quantity.is_zero() {
            bail!("HTTP brokerage order quantity cannot be zero");
        }
        let request = PlaceOrderRequest {
            symbol: order.symbol.permtick.to_ascii_uppercase(),
            quantity: order.quantity.abs(),
            action: if order.quantity.is_sign_positive() {
                "buy"
            } else {
                "sell"
            },
            order_type,
            time_in_force,
            limit_price: order.limit_price,
            account_number: self.config.account_id.as_deref(),
        };
        let response = checked(
            self.client.post(self.url("/order")).json(&request).send()?,
            "order submission",
        )?;
        let response: OrderResponse = decode(response, "order submission")?;
        if !response.success {
            bail!("HTTP brokerage rejected order: {}", response.message);
        }
        response
            .order
            .map(|order| order.order_id)
            .filter(|id| !id.is_empty())
            .context("HTTP brokerage accepted order without an order id")
    }

    fn cancel(&self, order: &Order) -> Result<()> {
        let order_id = order
            .brokerage_id
            .first()
            .context("HTTP brokerage cancellation requires a brokerage order id")?;
        let response = checked(
            self.client
                .post(self.url("/cancel"))
                .json(&CancelOrderRequest { order_id })
                .send()?,
            "order cancellation",
        )?;
        let response: OrderResponse = decode(response, "order cancellation")?;
        if !response.success {
            bail!("HTTP brokerage rejected cancellation: {}", response.message);
        }
        Ok(())
    }
}

impl Brokerage for HttpBrokerage {
    fn name(&self) -> &str {
        &self.metadata.name
    }

    fn brokerage_model(&self) -> Option<&str> {
        Some(&self.metadata.brokerage_model)
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn connect(&mut self) -> rlean_core::Result<()> {
        let response = self
            .client
            .get(self.url("/ready"))
            .send()
            .context("HTTP brokerage readiness request")?;
        checked(response, "readiness")?;
        self.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) {
        self.connected = false;
    }

    fn place_order(&mut self, order: Order) -> rlean_core::Result<bool> {
        Ok(self.place_order_with_brokerage_ids(order)?.is_some())
    }

    fn place_order_with_brokerage_ids(
        &mut self,
        order: Order,
    ) -> rlean_core::Result<Option<Vec<String>>> {
        Ok(Some(vec![self.submit(&order)?]))
    }

    fn update_order(&mut self, _order: &Order) -> rlean_core::Result<bool> {
        Ok(false)
    }

    fn can_update_order(&self, _order: &Order, _request: &UpdateOrderRequest) -> bool {
        false
    }

    fn cancel_order(&mut self, order: &Order) -> rlean_core::Result<bool> {
        self.cancel(order)?;
        Ok(true)
    }

    fn get_open_orders(&self) -> Vec<Order> {
        self.orders(true)
            .unwrap_or_else(|error| {
                tracing::error!(%error, "HTTP brokerage open-order query failed");
                Vec::new()
            })
            .into_iter()
            .filter_map(to_order)
            .collect()
    }

    fn get_account_orders(&self) -> Vec<Order> {
        self.orders(false)
            .unwrap_or_else(|error| {
                tracing::error!(%error, "HTTP brokerage account-order query failed");
                Vec::new()
            })
            .into_iter()
            .filter_map(to_order)
            .collect()
    }

    fn get_cash_balance(&self) -> Vec<(String, Decimal)> {
        self.get::<Vec<CashBalance>>("/cash")
            .map(|rows| {
                rows.into_iter()
                    .map(|row| (row.currency, row.amount))
                    .collect()
            })
            .unwrap_or_else(|error| {
                tracing::error!(%error, "HTTP brokerage cash query failed");
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
        self.get::<Vec<HttpHolding>>("/holdings")
            .map(|rows| rows.into_iter().map(to_holding).collect())
            .unwrap_or_else(|error| {
                tracing::error!(%error, "HTTP brokerage holdings query failed");
                Vec::new()
            })
    }
}

fn checked(response: Response, operation: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().unwrap_or_default();
    bail!("HTTP brokerage {operation} failed with HTTP {status}: {body}")
}

fn decode<T: DeserializeOwned>(response: Response, operation: &str) -> Result<T> {
    response
        .json()
        .with_context(|| format!("decode HTTP brokerage {operation} response"))
}

fn to_holding(row: HttpHolding) -> BrokerageHolding {
    BrokerageHolding {
        symbol: Symbol::create_equity(&row.symbol, &Market::usa()),
        quantity: row.quantity,
        average_price: row.average_buy_price,
        market_price: row.last_price,
    }
}

fn to_order(raw: HttpOrder) -> Option<Order> {
    let sign = if raw.side.eq_ignore_ascii_case("buy") {
        Decimal::ONE
    } else {
        -Decimal::ONE
    };
    let time = raw
        .updated_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| DateTime::from(value.to_utc()))
        .unwrap_or_else(DateTime::now);
    let mut order = Order::market(
        stable_order_id(&raw.order_id),
        Symbol::create_equity(&raw.symbol, &Market::usa()),
        raw.quantity * sign,
        time,
        "",
    );
    order.brokerage_id = vec![raw.order_id];
    order.order_type = match raw.order_type.as_str() {
        "market" => OrderType::Market,
        "limit" => OrderType::Limit,
        _ => return None,
    };
    order.time_in_force = match raw.time_in_force.as_str() {
        "gfd" | "day" => TimeInForce::Day,
        "gtc" => TimeInForce::GoodTilCanceled,
        _ => TimeInForce::Day,
    };
    order.status = order_status(&raw.state);
    order.filled_quantity = raw.filled_quantity * sign;
    order.average_fill_price = raw.average_price.unwrap_or_default();
    order.limit_price = raw.limit_price;
    Some(order)
}

fn stable_order_id(value: &str) -> i64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    i64::from_ne_bytes(hash.to_ne_bytes())
}

fn order_status(value: &str) -> OrderStatus {
    match value.to_ascii_lowercase().as_str() {
        "filled" => OrderStatus::Filled,
        "partially_filled" | "partially-filled" => OrderStatus::PartiallyFilled,
        "cancelled" | "canceled" | "expired" | "failed" => OrderStatus::Canceled,
        "rejected" => OrderStatus::Invalid,
        "cancel_pending" | "pending_cancel" => OrderStatus::CancelPending,
        _ => OrderStatus::Submitted,
    }
}

#[derive(Debug, Deserialize)]
struct BrokerageMetadata {
    name: String,
    brokerage_model: String,
}

#[derive(Debug, Serialize)]
struct PlaceOrderRequest<'a> {
    symbol: String,
    quantity: Decimal,
    action: &'a str,
    order_type: &'a str,
    time_in_force: &'a str,
    limit_price: Option<Decimal>,
    account_number: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct CancelOrderRequest<'a> {
    order_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct OrderResponse {
    success: bool,
    message: String,
    order: Option<HttpOrder>,
}

#[derive(Debug, Deserialize)]
struct HttpOrder {
    order_id: String,
    symbol: String,
    side: String,
    order_type: String,
    time_in_force: String,
    state: String,
    quantity: Decimal,
    filled_quantity: Decimal,
    limit_price: Option<Decimal>,
    average_price: Option<Decimal>,
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpHolding {
    symbol: String,
    quantity: Decimal,
    average_buy_price: Decimal,
    last_price: Decimal,
}

#[derive(Debug, Deserialize)]
struct CashBalance {
    currency: String,
    amount: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_http_order_into_lean_order() {
        let order = to_order(HttpOrder {
            order_id: "broker-123".to_owned(),
            symbol: "SPY".to_owned(),
            side: "sell".to_owned(),
            order_type: "limit".to_owned(),
            time_in_force: "gfd".to_owned(),
            state: "partially_filled".to_owned(),
            quantity: Decimal::from(10),
            filled_quantity: Decimal::from(4),
            limit_price: Some(Decimal::new(50_025, 2)),
            average_price: Some(Decimal::new(50_010, 2)),
            updated_at: Some("2026-08-05T14:31:00Z".to_owned()),
        })
        .expect("supported order");

        assert_eq!(order.quantity, Decimal::from(-10));
        assert_eq!(order.filled_quantity, Decimal::from(-4));
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
        assert_eq!(order.time_in_force, TimeInForce::Day);
        assert_eq!(order.brokerage_id, ["broker-123"]);
    }

    #[test]
    fn stable_broker_order_ids_are_repeatable_and_distinct() {
        assert_eq!(stable_order_id("abc"), stable_order_id("abc"));
        assert_ne!(stable_order_id("abc"), stable_order_id("def"));
    }
}
