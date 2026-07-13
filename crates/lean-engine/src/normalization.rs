use crate::data_feed::{CorporateActionResolution, DataFeedContext};
use lean_core::{DataNormalizationMode, DateTime, SecurityType, Symbol};
use lean_data::{QuoteBar, TradeBar};
use lean_storage::{FactorFileEntry, IcebergStore};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;

/// LEAN-style price scale frontier: use `EndTime` when a bar crosses calendar days.
pub fn price_scale_frontier(time: DateTime, end_time: DateTime) -> chrono::NaiveDate {
    let time_date = time.date_utc();
    let end_date = end_time.date_utc();
    if time_date != end_date {
        let end_time_of_day = end_time.to_utc().time();
        if end_time_of_day > chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
            return end_date;
        }
    }
    time_date
}

pub fn read_factor_rows(store: &IcebergStore, symbol: &Symbol) -> Vec<FactorFileEntry> {
    if !matches!(symbol.security_type(), SecurityType::Equity) {
        return Vec::new();
    }
    let store = store.clone();
    let market = symbol.market().as_str().to_string();
    let ticker = symbol.permtick.to_string();
    block_on_background(async move { store.scan_factor_file(&market, &ticker).await })
        .unwrap_or_default()
}

/// Resolve the symbol's corporate-action files (factor + map) for this run,
/// fetching them from the history provider and persisting them into Iceberg if
/// the tables are empty for this symbol.
///
/// This is the framework side of the provider/framework split: providers are
/// pure data sources (`get_factor_file`/`get_map_file` return rows), and the
/// framework owns *all* persistence — writing the rows into Iceberg exactly as
/// it does trade/quote bars. Providers must never write files themselves.
///
/// Resolution is decoupled from bar fetching, mirroring C# LEAN where
/// `SubscriptionDataReader.Initialize` resolves the factor/map file through
/// `IFactorFileProvider.Get` / `IMapFileProvider.ResolveMapFile` at subscription
/// creation regardless of whether the symbol's bars are already on disk
/// (Engine/DataFeeds/SubscriptionDataReader.cs:203-249). A ticker whose bars are
/// already cached in Iceberg still gets its factor/map files fetched here — that
/// is exactly issue #30.
///
/// Cache-first and idempotent, and additionally guarded by a per-run resolution
/// cache (LEAN's per-process `_seededMarket`/`_factorFiles`): repeat
/// subscriptions of a ticker in one run resolve once, never re-hitting the store
/// or provider. Only equities carry corporate actions.
///
/// Returns `true` if the symbol has factor rows available (already cached or
/// freshly fetched), `false` if the factor file is still absent afterwards —
/// including on a fetch error or an empty provider result. A `false` for an
/// Adjusted-mode equity means its prices will flow through unadjusted, so callers
/// should warn. Non-equities always return `true` (nothing to adjust). Nothing
/// durable is written on empty/error, so the fetch is retried on the next run
/// rather than being permanently poisoned.
///
/// This is a synchronous wrapper (the subscription producer builds its state on a
/// blocking worker); it runs the async resolution on a dedicated current-thread
/// runtime, mirroring `read_factor_rows`. Returns the full resolution — the
/// caller uses `factor_rows` for normalization and `map_first_date` for the
/// provider-earliest bound without re-scanning Iceberg.
pub fn ensure_corporate_actions_cached(
    context: &DataFeedContext,
    symbol: &Symbol,
) -> CorporateActionResolution {
    if !matches!(symbol.security_type(), SecurityType::Equity) {
        return CorporateActionResolution {
            factors_available: true,
            factor_rows: Vec::new(),
            map_first_date: None,
        };
    }
    let symbol_value = symbol.value.to_string();
    let context_for_task = context.clone();
    let symbol = symbol.clone();
    match block_on_background(async move {
        Ok::<CorporateActionResolution, anyhow::Error>(
            context_for_task.resolve_corporate_actions(&symbol).await,
        )
    }) {
        Ok(resolution) => resolution,
        Err(err) => {
            tracing::warn!(
                "Corporate-action resolution worker failed for {}: {}",
                symbol_value,
                err
            );
            CorporateActionResolution {
                factors_available: false,
                factor_rows: Vec::new(),
                map_first_date: None,
            }
        }
    }
}

/// The symbol's map-file inception date (first tradable date), if known.
///
/// LEAN uses map files to determine when a security first started trading.
/// Symbols that IPO'd mid-history (e.g. XLC in June 2018) have no data before
/// this date, so the cache-coverage check must not expect it — otherwise the
/// window head is permanently "missing" and re-fetched every run.
pub fn read_map_first_date(store: &IcebergStore, symbol: &Symbol) -> Option<chrono::NaiveDate> {
    if !matches!(symbol.security_type(), SecurityType::Equity) {
        return None;
    }
    let store = store.clone();
    let market = symbol.market().as_str().to_string();
    let ticker = symbol.permtick.to_string();
    let rows =
        block_on_background(async move { store.scan_map_file(&market, &ticker).await }).ok()?;
    rows.into_iter().map(|row| row.date).min()
}

fn block_on_background<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!(e))?
            .block_on(future)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("factor reader worker panicked"))?
}

pub fn factor_for_entry(rows: &[FactorFileEntry], date: chrono::NaiveDate) -> (f64, f64) {
    if rows.is_empty() {
        return (1.0, 1.0);
    }
    if let Some(row) = rows
        .iter()
        .filter(|row| row.date < date)
        .max_by_key(|row| row.date)
    {
        return (row.price_factor, row.split_factor);
    }
    rows.iter()
        .min_by_key(|row| row.date)
        .map(|row| (row.price_factor, row.split_factor))
        .unwrap_or((1.0, 1.0))
}

pub fn normalize_trade_bar(
    bar: &mut TradeBar,
    normalization_mode: DataNormalizationMode,
    factor_rows: &[FactorFileEntry],
) {
    if normalization_mode == DataNormalizationMode::Raw
        || !matches!(bar.symbol.security_type(), SecurityType::Equity)
    {
        return;
    }
    let frontier = price_scale_frontier(bar.time, bar.end_time);
    apply_factor_to_trade_bar(bar, factor_rows, frontier, normalization_mode);
}

pub fn normalize_quote_bar(
    bar: &mut QuoteBar,
    normalization_mode: DataNormalizationMode,
    factor_rows: &[FactorFileEntry],
) {
    if normalization_mode == DataNormalizationMode::Raw
        || !matches!(bar.symbol.security_type(), SecurityType::Equity)
    {
        return;
    }
    let frontier = price_scale_frontier(bar.time, bar.end_time);
    let (price_factor, split_factor) = factor_for_entry(factor_rows, frontier);
    let scale = normalization_scale(normalization_mode, price_factor, split_factor);
    if (scale - 1.0).abs() < 1e-9 {
        return;
    }
    let price_scale = Decimal::from_f64(scale).unwrap_or(Decimal::ONE);
    if let Some(bid) = bar.bid.as_mut() {
        bid.open *= price_scale;
        bid.high *= price_scale;
        bid.low *= price_scale;
        bid.close *= price_scale;
    }
    if let Some(ask) = bar.ask.as_mut() {
        ask.open *= price_scale;
        ask.high *= price_scale;
        ask.low *= price_scale;
        ask.close *= price_scale;
    }
}

fn normalization_scale(
    normalization_mode: DataNormalizationMode,
    price_factor: f64,
    split_factor: f64,
) -> f64 {
    match normalization_mode {
        DataNormalizationMode::Raw => 1.0,
        DataNormalizationMode::SplitAdjusted => split_factor,
        DataNormalizationMode::Adjusted
        | DataNormalizationMode::TotalReturn
        | DataNormalizationMode::ForwardPanamaCanal
        | DataNormalizationMode::BackwardPanamaCanal => price_factor * split_factor,
    }
}

fn apply_factor_to_trade_bar(
    bar: &mut TradeBar,
    rows: &[FactorFileEntry],
    date: chrono::NaiveDate,
    normalization_mode: DataNormalizationMode,
) {
    let (price_factor, split_factor) = factor_for_entry(rows, date);
    let scale = normalization_scale(normalization_mode, price_factor, split_factor);
    if (scale - 1.0).abs() < 1e-9 {
        return;
    }
    let price_scale = Decimal::from_f64(scale).unwrap_or(Decimal::ONE);
    bar.open *= price_scale;
    bar.high *= price_scale;
    bar.low *= price_scale;
    bar.close *= price_scale;
    if split_factor != 0.0 && (split_factor - 1.0).abs() > 1e-9 {
        let volume_scale = Decimal::from_f64(1.0 / split_factor).unwrap_or(Decimal::ONE);
        bar.volume *= volume_scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{Market, Symbol, TimeSpan};
    use lean_data::{TradeBar, TradeBarData};
    use rust_decimal_macros::dec;

    fn dt(date: NaiveDate, hour: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, 0, 0).unwrap()))
    }

    #[test]
    fn price_scale_frontier_uses_end_time_for_cross_midnight_daily_bar() {
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let time = dt(day1, 20);
        let end_time = dt(day2, 20);
        assert_eq!(price_scale_frontier(time, end_time), day2);
    }

    #[test]
    fn price_scale_frontier_uses_time_for_same_day_bar() {
        let day = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let time = dt(day, 10);
        let end_time = dt(day, 11);
        assert_eq!(price_scale_frontier(time, end_time), day);
    }

    #[test]
    fn normalize_trade_bar_applies_factor_from_frontier_date() {
        let day0 = NaiveDate::from_ymd_opt(2024, 1, 14).unwrap();
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut bar = TradeBar::new(
            symbol,
            dt(day1, 20),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
        );
        let rows = vec![
            FactorFileEntry {
                date: day0,
                price_factor: 1.0,
                split_factor: 1.0,
                reference_price: 0.0,
            },
            FactorFileEntry {
                date: day1,
                price_factor: 2.0,
                split_factor: 1.0,
                reference_price: 0.0,
            },
            FactorFileEntry {
                date: day2,
                price_factor: 4.0,
                split_factor: 1.0,
                reference_price: 0.0,
            },
        ];
        normalize_trade_bar(&mut bar, DataNormalizationMode::Adjusted, &rows);
        assert_eq!(bar.close, dec!(200));
    }

    /// Issue #27: with the refetched DPST factor file, a pre-split bar must be
    /// scaled up by the 1:10 reverse-split factor so it lines up with post-split
    /// prices (no phantom 10x jump). Missing factors would leave it unadjusted.
    #[test]
    fn dpst_reverse_split_scales_pre_split_bar() {
        // Reverse split effective 2023-06-05: split_factor 10 applies to all
        // dates before it (newest-first factor file, base row far in the past).
        let split_date = NaiveDate::from_ymd_opt(2023, 6, 5).unwrap();
        let pre_split = NaiveDate::from_ymd_opt(2023, 5, 30).unwrap();
        let rows = vec![
            FactorFileEntry {
                date: split_date,
                price_factor: 1.0,
                split_factor: 1.0,
                reference_price: 0.0,
            },
            FactorFileEntry {
                date: NaiveDate::from_ymd_opt(1900, 1, 1).unwrap(),
                price_factor: 1.0,
                split_factor: 10.0,
                reference_price: 0.0,
            },
        ];
        // A pre-split raw close of ~$5 must become ~$50 (× 10) after adjustment.
        let symbol = Symbol::create_equity("DPST", &Market::usa());
        let mut bar = TradeBar::new(
            symbol,
            dt(pre_split, 20),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(5), dec!(5), dec!(5), dec!(5), dec!(1000)),
        );
        normalize_trade_bar(&mut bar, DataNormalizationMode::Adjusted, &rows);
        assert_eq!(
            bar.close,
            dec!(50),
            "pre-split bar must scale up by the 10x reverse split"
        );

        // Empty factor rows (the bug) leave the price raw — no adjustment.
        let mut raw_bar = TradeBar::new(
            Symbol::create_equity("DPST", &Market::usa()),
            dt(pre_split, 20),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(5), dec!(5), dec!(5), dec!(5), dec!(1000)),
        );
        normalize_trade_bar(&mut raw_bar, DataNormalizationMode::Adjusted, &[]);
        assert_eq!(
            raw_bar.close,
            dec!(5),
            "no factor rows leaves the price unadjusted"
        );
    }

    /// Validation for issue #27 against the *backfilled* shared warehouse: read
    /// DPST's refetched factor rows via the real engine path and confirm a
    /// pre-2023-06-05 bar is scaled up by the 1:10 reverse split (no phantom
    /// 10x). Ignored by default (needs a REST catalog with the backfilled
    /// tables):
    ///   RLEAN_TEST_CATALOG=... RLEAN_TEST_WAREHOUSE=... \
    ///     cargo test -p lean-engine --lib -- --ignored --nocapture \
    ///     dpst_backfilled_factor_file_adjusts_pre_split_bar
    #[test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    fn dpst_backfilled_factor_file_adjusts_pre_split_bar() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let Some(store) = rt.block_on(crate::test_support::connect_test_store()) else {
            return;
        };
        let symbol = Symbol::create_equity("DPST", &Market::usa());
        let rows = read_factor_rows(&store, &symbol);
        assert!(!rows.is_empty(), "DPST must have backfilled factor rows");

        // A pre-split raw close from spring 2023 (~$5) must adjust upward by the
        // 10x reverse split; without the backfill it would pass through raw.
        let pre_split = NaiveDate::from_ymd_opt(2023, 5, 30).unwrap();
        let mut bar = TradeBar::new(
            symbol,
            dt(pre_split, 20),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(5), dec!(5), dec!(5), dec!(5), dec!(1000)),
        );
        normalize_trade_bar(&mut bar, DataNormalizationMode::Adjusted, &rows);
        // 10x split factor × dividend price factor (~0.96) => close well above $40.
        assert!(
            bar.close > dec!(40),
            "pre-split DPST bar should be adjusted up by the reverse split, got {}",
            bar.close
        );
    }
}
