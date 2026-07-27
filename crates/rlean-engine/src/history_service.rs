use crate::data_feed::DataFeedContext;
use crate::history_subscription::SubscriptionHistoryProvider;
use anyhow::{anyhow, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use rlean_algorithm::lifecycle::{AlgorithmHistoryService, HistoryColumns};
use rlean_algorithm::qc_algorithm::QcAlgorithm;
use rlean_core::{DataNormalizationMode, DateTime, NanosecondTimestamp, Resolution, Symbol};
use rlean_data::SubscriptionDataConfig;
use rlean_data_sidecar::DataSidecarClient;
use rlean_data_tables::{CustomDataPoint, TradeBar};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::future::Future;
use std::sync::Arc;

#[derive(Clone)]
pub struct AlgorithmHistoryContext {
    pub data_sidecar: Arc<DataSidecarClient>,
}

#[derive(Clone)]
pub struct HistoryService {
    context: AlgorithmHistoryContext,
}

impl HistoryService {
    pub fn new(context: AlgorithmHistoryContext) -> Self {
        Self { context }
    }

    pub fn load_trade_bars_blocking_with_normalization(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        self.load_trade_bars_between_blocking_with_normalization(
            symbol,
            resolution,
            date_to_datetime(start, 0, 0, 0),
            date_to_datetime(end, 23, 59, 59),
            normalization_mode,
        )
    }

    pub fn load_trade_bars_between_blocking_with_normalization(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: DateTime,
        end: DateTime,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        let provider = self.subscription_history_provider();
        let symbol = symbol.clone();
        let bars = block_on_background(async move {
            provider
                .get_trade_bars(symbol, resolution, start, end, normalization_mode)
                .await
                .map_err(|error| anyhow!(error.to_string()))
        })?;
        Ok(bars)
    }

    /// Load the single most-recent known trade bar for `symbol`, applying
    /// LEAN's resolution-dependent lookback window. Language-neutral rule moved
    /// out of the Python binding.
    pub fn load_last_known_trade_bar(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        as_of: DateTime,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        let lookback_days = last_known_lookback_days(resolution);
        let end_date = as_of.date_utc();
        let start_date = end_date - chrono::Duration::days(lookback_days);
        let start = date_to_datetime(start_date, 0, 0, 0);
        // One shared seeding path for live and backtest: block until the
        // query resolves, exactly like every other history read. Waiting on
        // an active provider request is normal operation and is deliberately
        // silent — no periodic WARN, no wall-clock bound. The request ends
        // only by resolving (rows or empty) or by a transport failure, and a
        // no-price resolution is reported loudly by `resolve_seed_price`.
        let provider = self.subscription_history_provider();
        let symbol_owned = symbol.clone();
        let mut bars = block_on_background(async move {
            provider
                .get_trade_bars(symbol_owned, resolution, start, as_of, normalization_mode)
                .await
                .map_err(|error| anyhow!(error.to_string()))
        })?;
        bars.retain(|bar| bar.close > Decimal::ZERO && bar.end_time.0 <= as_of.0);
        bars.sort_by_key(|bar| bar.end_time.0);
        if bars.len() > 1 {
            bars = bars[bars.len() - 1..].to_vec();
        }
        Ok(bars)
    }

    pub fn load_custom_history_blocking(
        &self,
        subscription: &SubscriptionDataConfig,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<CustomDataPoint>> {
        subscription
            .custom
            .as_ref()
            .ok_or_else(|| anyhow!("custom history requested for non-custom subscription"))?;
        let provider = self.subscription_history_provider();
        let config = subscription.clone();
        let start_dt = date_to_datetime(start, 0, 0, 0);
        let end_dt = date_to_datetime(end, 23, 59, 59);
        block_on_background(async move {
            provider
                .get_custom_points(config, start_dt, end_dt)
                .await
                .map_err(|error| anyhow!(error.to_string()))
        })
    }

    fn subscription_history_provider(&self) -> SubscriptionHistoryProvider {
        let context = DataFeedContext::new(self.context.data_sidecar.clone());
        SubscriptionHistoryProvider::new(context)
    }
}

impl AlgorithmHistoryService for HistoryService {
    fn history(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        periods: usize,
        resolution: Resolution,
    ) -> HistoryColumns {
        self.history_count_columns(algorithm, symbol, periods, resolution)
            .unwrap_or_default()
    }

    fn last_known_close_price(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        resolution: Resolution,
    ) -> Option<f64> {
        let as_of = if algorithm.utc_time == DateTime::EPOCH {
            algorithm.start_date
        } else {
            algorithm.utc_time
        };
        let configs = algorithm
            .subscription_manager
            .get_configs_for_symbol(symbol);
        let normalization = matching_normalization_mode(
            &configs
                .iter()
                .map(|config| (**config).clone())
                .collect::<Vec<_>>(),
            Some(resolution),
            DataNormalizationMode::Adjusted,
        );
        let result = self.load_last_known_trade_bar(symbol, resolution, as_of, normalization);
        resolve_seed_price(symbol, result)
    }
}

/// Turn a completed price-seed read into a usable seed price, or `None`.
///
/// Every `None` outcome leaves the security at a zero price, which
/// price-guarded strategies drop without a trace — the 2026-07-27 live
/// incident where two valid entry signals (TRMB, DBX) were lost at the seed
/// step because the seed produced no bars in the lookback window and the empty
/// result was silently mapped to `None`. So each no-price outcome — a failed
/// read, an empty result, or a non-positive/non-finite close — must be an
/// error-level log naming the symbol and the reason, not a silent `None`.
fn resolve_seed_price(symbol: &Symbol, result: Result<Vec<TradeBar>>) -> Option<f64> {
    let bars = match result {
        Ok(bars) => bars,
        Err(error) => {
            tracing::error!(
                symbol = %symbol,
                %error,
                "get_last_known_prices: history read for price seeding failed; \
                 the security keeps a zero price and will be skipped by \
                 price-guarded strategies"
            );
            return None;
        }
    };
    let Some(bar) = bars.last() else {
        tracing::error!(
            symbol = %symbol,
            "get_last_known_prices: price seed returned no bars within the \
             lookback window (stale or missing data); the security keeps a zero \
             price and will be skipped by price-guarded strategies"
        );
        return None;
    };
    match bar.close.to_f64() {
        Some(price) if price > 0.0 && price.is_finite() => Some(price),
        _ => {
            tracing::error!(
                symbol = %symbol,
                close = %bar.close,
                "get_last_known_prices: price seed produced a non-positive or \
                 non-finite close; the security keeps a zero price and will be \
                 skipped by price-guarded strategies"
            );
            None
        }
    }
}

impl HistoryService {
    fn history_count_columns(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        periods: usize,
        resolution: Resolution,
    ) -> Result<HistoryColumns> {
        if periods == 0 {
            return Ok(HistoryColumns::new());
        }

        let end = if algorithm.utc_time == DateTime::EPOCH {
            algorithm.start_date
        } else {
            algorithm.utc_time
        };
        let start = history_count_start(end, periods, resolution);
        let configs = algorithm
            .subscription_manager
            .get_configs_for_symbol(symbol);
        if let Some(custom_config) = configs.iter().find(|config| config.custom.is_some()) {
            let mut points =
                self.load_custom_history_between_blocking(custom_config, start, end)?;
            points.sort_by_key(|point| point.end_time.0);
            if points.len() > periods {
                points = points[points.len() - periods..].to_vec();
            }
            Ok(custom_points_to_columns(&points))
        } else {
            let normalization = matching_normalization_mode(
                &configs
                    .iter()
                    .map(|config| (**config).clone())
                    .collect::<Vec<_>>(),
                Some(resolution),
                DataNormalizationMode::Adjusted,
            );
            let mut bars = self.load_trade_bars_between_blocking_with_normalization(
                symbol,
                resolution,
                start,
                end,
                normalization,
            )?;
            bars.sort_by_key(|bar| bar.end_time.0);
            if bars.len() > periods {
                bars = bars[bars.len() - periods..].to_vec();
            }
            Ok(trade_bars_to_columns(&bars))
        }
    }

    fn load_custom_history_between_blocking(
        &self,
        subscription: &SubscriptionDataConfig,
        start: DateTime,
        end: DateTime,
    ) -> Result<Vec<CustomDataPoint>> {
        subscription
            .custom
            .as_ref()
            .ok_or_else(|| anyhow!("custom history requested for non-custom subscription"))?;
        let provider = self.subscription_history_provider();
        let config = subscription.clone();
        block_on_background(async move {
            provider
                .get_custom_points(config, start, end)
                .await
                .map_err(|error| anyhow!(error.to_string()))
        })
    }
}

fn history_count_start(end: DateTime, periods: usize, resolution: Resolution) -> DateTime {
    let calendar_days = match resolution {
        Resolution::Daily => periods.saturating_mul(3).saturating_add(31),
        Resolution::Hour => periods.saturating_div(6).saturating_add(14),
        Resolution::Minute | Resolution::Second | Resolution::Tick => {
            periods.saturating_div(390).saturating_add(7)
        }
    };
    NanosecondTimestamp(
        end.0.saturating_sub(
            chrono::Duration::days(calendar_days as i64)
                .num_nanoseconds()
                .unwrap_or(i64::MAX),
        ),
    )
}

fn trade_bars_to_columns(bars: &[TradeBar]) -> HistoryColumns {
    let mut columns = HistoryColumns::new();
    columns.insert("time".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("end_time".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("venue".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("open".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("high".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("low".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("close".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("volume".to_string(), Vec::with_capacity(bars.len()));
    for bar in bars {
        columns
            .get_mut("time")
            .unwrap()
            .push(bar.time.to_utc().to_rfc3339());
        columns
            .get_mut("end_time")
            .unwrap()
            .push(bar.end_time.to_utc().to_rfc3339());
        columns
            .get_mut("venue")
            .unwrap()
            .push(bar.venue.clone().unwrap_or_default());
        columns.get_mut("open").unwrap().push(bar.open.to_string());
        columns.get_mut("high").unwrap().push(bar.high.to_string());
        columns.get_mut("low").unwrap().push(bar.low.to_string());
        columns
            .get_mut("close")
            .unwrap()
            .push(bar.close.to_string());
        columns
            .get_mut("volume")
            .unwrap()
            .push(bar.volume.to_string());
    }
    columns
}

fn custom_points_to_columns(points: &[CustomDataPoint]) -> HistoryColumns {
    let mut columns = HistoryColumns::new();
    columns.insert("time".to_string(), Vec::with_capacity(points.len()));
    columns.insert("end_time".to_string(), Vec::with_capacity(points.len()));
    columns.insert("value".to_string(), Vec::with_capacity(points.len()));
    columns.insert("venue".to_string(), Vec::with_capacity(points.len()));
    for point in points {
        columns
            .get_mut("time")
            .unwrap()
            .push(point.time.to_utc().to_rfc3339());
        columns
            .get_mut("end_time")
            .unwrap()
            .push(point.end_time.to_utc().to_rfc3339());
        columns
            .get_mut("value")
            .unwrap()
            .push(point.value.to_string());
        columns
            .get_mut("venue")
            .unwrap()
            .push(point.venue.clone().unwrap_or_default());
    }
    columns
}

/// LEAN's resolution-dependent lookback window for "last known price" lookups.
pub fn last_known_lookback_days(resolution: Resolution) -> i64 {
    match resolution {
        Resolution::Tick | Resolution::Second | Resolution::Minute => 7,
        Resolution::Hour => 14,
        Resolution::Daily => 31,
    }
}

/// Select the data-normalization mode for a history request from the matching
/// subscriptions, mirroring C# Lean's `GetMatchingSubscriptions`: prefer a
/// trade subscription at the requested resolution, then any subscription for
/// the symbol, finally fall back to `fallback` (the universe-settings default).
pub fn matching_normalization_mode(
    configs: &[SubscriptionDataConfig],
    resolution: Option<Resolution>,
    fallback: DataNormalizationMode,
) -> DataNormalizationMode {
    use rlean_core::TickType;
    if let Some(resolution) = resolution {
        if let Some(sub) = configs
            .iter()
            .find(|sub| sub.resolution == resolution && sub.tick_type == TickType::Trade)
        {
            return sub.normalization_mode;
        }
        if let Some(sub) = configs.iter().find(|sub| sub.resolution == resolution) {
            return sub.normalization_mode;
        }
    }
    if let Some(sub) = configs.iter().find(|sub| sub.tick_type == TickType::Trade) {
        return sub.normalization_mode;
    }
    if let Some(sub) = configs.first() {
        return sub.normalization_mode;
    }
    fallback
}

fn date_to_datetime(date: NaiveDate, hour: u32, minute: u32, second: u32) -> DateTime {
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, second).unwrap()))
}

/// Run a history read on a dedicated current-thread runtime on a fresh
/// thread, blocking the caller until the query resolves.
///
/// Deadlock note: reads are answered over the sidecar Flight exchange whose
/// response router runs as a task on the *main* multi-threaded runtime.
/// Parking the calling worker here, even for minutes, cannot starve the
/// router: it keeps making progress on the main runtime's other workers,
/// which is what eventually completes this very read. We never block the
/// worker that services the channel the read answers on, so the wait is
/// bounded by the query itself — it ends when the request resolves or fails
/// at the transport level, never by a wall-clock bound imposed here. The
/// accepted worst case is that a live time step blocks for the full duration
/// of a vendor gap-fill.
fn block_on_background<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let handle = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow!(e))?
            .block_on(future)
    });
    handle
        .join()
        .map_err(|_| anyhow!("history worker panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{Market, Symbol, TimeSpan};
    use rlean_data_tables::{TradeBar, TradeBarData};
    use rust_decimal_macros::dec;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    fn sample_bar(close: Decimal) -> TradeBar {
        TradeBar::new(
            Symbol::create_equity("NVTS", &Market::usa()),
            DateTime::from_secs(1_700_000_000),
            TimeSpan::from_days(1),
            TradeBarData {
                open: close,
                high: close,
                low: close,
                close,
                volume: dec!(1),
            },
        )
    }

    /// A `tracing` writer that captures emitted events into a shared buffer so a
    /// test can assert the no-price seed ERROR was logged (and that waiting is silent).
    #[derive(Clone)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
        type Writer = BufferWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Capture logs emitted while running `resolve_seed_price` at ERROR level.
    fn resolve_seed_price_capturing_logs(
        symbol: &Symbol,
        result: Result<Vec<TradeBar>>,
    ) -> (Option<f64>, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .with_writer(BufferWriter(buffer.clone()))
            .finish();
        let price = {
            let _guard = tracing::subscriber::set_default(subscriber);
            resolve_seed_price(symbol, result)
        };
        let captured = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        (price, captured)
    }

    // Regression for the 2026-07-27 lost-signal incident: a seed that returns
    // NO bars in the lookback window (stale/missing data) must not vanish. It
    // must produce no price AND a LOUD error naming the symbol, so the operator
    // sees which security kept a zero price instead of the signal being dropped
    // in silence.
    #[test]
    fn empty_seed_result_logs_a_loud_error_naming_the_symbol() {
        let symbol = Symbol::create_equity("TRMB", &Market::usa());
        let (price, captured) = resolve_seed_price_capturing_logs(&symbol, Ok(Vec::new()));

        assert!(
            price.is_none(),
            "an empty seed cannot produce a price: {price:?}"
        );
        assert!(
            captured.contains("ERROR"),
            "an empty seed must log at ERROR level, not vanish silently: {captured:?}"
        );
        assert!(
            captured.contains("TRMB"),
            "the error must name the symbol that lost its seed: {captured:?}"
        );
    }

    // A seeded bar with a non-positive close is equally unusable and must be
    // just as loud — a zero/negative price silently disqualifies the security.
    #[test]
    fn non_positive_seed_close_logs_a_loud_error_naming_the_symbol() {
        let symbol = Symbol::create_equity("DBX", &Market::usa());
        let (price, captured) =
            resolve_seed_price_capturing_logs(&symbol, Ok(vec![sample_bar(dec!(0))]));

        assert!(
            price.is_none(),
            "a non-positive close is unusable: {price:?}"
        );
        assert!(
            captured.contains("ERROR"),
            "a non-positive seed close must log at ERROR level: {captured:?}"
        );
        assert!(
            captured.contains("DBX"),
            "the error must name the symbol: {captured:?}"
        );
    }

    // A good seed still returns its price and stays quiet at ERROR level.
    #[test]
    fn usable_seed_returns_price_without_error() {
        let symbol = Symbol::create_equity("NVTS", &Market::usa());
        let (price, captured) =
            resolve_seed_price_capturing_logs(&symbol, Ok(vec![sample_bar(dec!(42))]));

        assert_eq!(price, Some(42.0));
        assert!(
            captured.is_empty(),
            "a usable seed must not log an error: {captured:?}"
        );
    }

    // A failed read stays loud (unchanged behavior, guarded against regression).
    #[test]
    fn failed_seed_read_logs_a_loud_error_naming_the_symbol() {
        let symbol = Symbol::create_equity("TRMB", &Market::usa());
        let (price, captured) =
            resolve_seed_price_capturing_logs(&symbol, Err(anyhow!("sidecar query failed")));

        assert!(price.is_none());
        assert!(captured.contains("ERROR"), "captured: {captured:?}");
        assert!(captured.contains("TRMB"), "captured: {captured:?}");
    }
}
