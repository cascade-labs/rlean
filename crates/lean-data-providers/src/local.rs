/// Local disk-only history provider — reads Parquet trade bars with no network calls.
///
/// Useful as a fallback when data has already been downloaded to the local
/// Parquet store, or in tests.
use chrono::{Datelike, NaiveDate};
use lean_core::{
    exchange_hours::ExchangeHours, Market, OptionRight, OptionStyle, Resolution, SecurityType,
    Symbol, SymbolOptionsExt, TickType,
};
use lean_data::{QuoteBar, Tick, TradeBar};
use lean_storage::{ParquetReader, PathResolver, QueryParams};
use std::collections::HashSet;

use crate::request::HistoryRequest;
use crate::traits::IHistoryProvider;
use async_trait::async_trait;

pub struct LocalHistoryProvider {
    pub(crate) data_root: std::path::PathBuf,
}

impl LocalHistoryProvider {
    pub fn new(data_root: impl AsRef<std::path::Path>) -> Self {
        LocalHistoryProvider {
            data_root: data_root.as_ref().to_path_buf(),
        }
    }
}

#[async_trait]
impl IHistoryProvider for LocalHistoryProvider {
    async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
        use crate::request::DataType;
        // LocalHistoryProvider only serves trade bars from disk.
        // Any other DataType (FactorFile, etc.) must go to a remote provider.
        if request.data_type != DataType::TradeBar {
            return Err(anyhow::anyhow!(
                "NotImplemented: LocalHistoryProvider does not handle {:?}",
                request.data_type
            ));
        }
        let resolver = PathResolver::new(&self.data_root);

        let start_date = request.start.date_utc();
        let end_date = request.end.date_utc();

        let expected_dates = expected_market_dates(&request.symbol, start_date, end_date);

        let mut paths = Vec::new();
        for current in &expected_dates {
            let p = resolver.market_data_partition(
                &request.symbol,
                request.resolution,
                TickType::Trade,
                *current,
            );
            if p.exists() {
                paths.push(p);
            } else {
                return Ok(vec![]);
            }
        }

        if paths.is_empty() {
            return Ok(vec![]);
        }

        let reader = ParquetReader::new();
        let mut params = QueryParams::new().with_time_range(request.start, request.end);
        params.predicate = params.predicate.with_symbols(vec![request.symbol.id.sid]);
        let symbol = request.symbol.clone();

        let mut bars = Vec::new();
        for path in &paths {
            bars.extend(
                reader
                    .read_trade_bar_partition(path, &symbol, &params)
                    .unwrap_or_default(),
            );
        }

        if !local_bars_cover_expected_dates(&bars, &expected_dates) {
            return Ok(vec![]);
        }

        Ok(bars)
    }

    async fn get_quote_bars(&self, request: &HistoryRequest) -> anyhow::Result<Vec<QuoteBar>> {
        let resolver = PathResolver::new(&self.data_root);
        let start_date = request.start.date_utc();
        let end_date = request.end.date_utc();
        let expected_dates = expected_market_dates(&request.symbol, start_date, end_date);

        let mut paths = Vec::new();
        for current in &expected_dates {
            let p = resolver.market_data_partition(
                &request.symbol,
                request.resolution,
                TickType::Quote,
                *current,
            );
            if p.exists() {
                paths.push(p);
            } else {
                return Ok(vec![]);
            }
        }

        if paths.is_empty() {
            return Ok(vec![]);
        }

        let reader = ParquetReader::new();
        let mut params = QueryParams::new().with_time_range(request.start, request.end);
        params.predicate = params.predicate.with_symbols(vec![request.symbol.id.sid]);
        let mut bars = Vec::new();
        for path in &paths {
            bars.extend(reader.read_quote_bar_partition(path, &request.symbol, &params)?);
        }
        if !local_quote_bars_cover_expected_dates(&bars, &expected_dates) {
            return Ok(vec![]);
        }
        Ok(bars)
    }

    async fn get_ticks(&self, request: &HistoryRequest) -> anyhow::Result<Vec<Tick>> {
        let resolver = PathResolver::new(&self.data_root);
        let start_date = request.start.date_utc();
        let end_date = request.end.date_utc();
        let expected_dates = expected_market_dates(&request.symbol, start_date, end_date);

        let mut paths = Vec::new();
        for current in &expected_dates {
            let p = resolver.market_data_partition(
                &request.symbol,
                Resolution::Tick,
                TickType::Trade,
                *current,
            );
            if p.exists() {
                paths.push(p);
            } else {
                return Ok(vec![]);
            }
        }

        if paths.is_empty() {
            return Ok(vec![]);
        }

        let reader = ParquetReader::new();
        let mut params = QueryParams::new().with_time_range(request.start, request.end);
        params.predicate = params.predicate.with_symbols(vec![request.symbol.id.sid]);
        let mut ticks = Vec::new();
        for path in &paths {
            ticks.extend(reader.read_tick_partition(path, &request.symbol, &params)?);
        }
        if !local_ticks_cover_expected_dates(&ticks, &expected_dates) {
            return Ok(vec![]);
        }
        Ok(ticks)
    }

    async fn get_option_universe(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_storage::OptionUniverseRow>> {
        let path = option_partition_path(&self.data_root, Resolution::Daily, "universe", date);
        if !path.exists() {
            return Ok(vec![]);
        }
        Ok(ParquetReader::new()
            .read_option_universe(&[path])?
            .into_iter()
            .filter(|row| row.underlying.eq_ignore_ascii_case(ticker))
            .collect())
    }

    async fn get_option_trade_bars(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<TradeBar>> {
        let resolver = PathResolver::new(&self.data_root);
        let path = option_partition_path(&self.data_root, resolution, "trade", date);
        if !path.exists() {
            return Ok(vec![]);
        }

        let symbols_by_value = load_option_symbols(&resolver, ticker, date)?;
        if symbols_by_value.is_empty() {
            return Ok(vec![]);
        }

        let params = day_params(date, resolution);
        Ok(ParquetReader::new().read_trade_bars_with_symbols(
            &[path],
            &symbols_by_value,
            &params,
        )?)
    }

    async fn get_option_quote_bars(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<QuoteBar>> {
        let resolver = PathResolver::new(&self.data_root);
        let path = option_partition_path(&self.data_root, resolution, "quote", date);
        if !path.exists() {
            return Ok(vec![]);
        }

        let symbols_by_value = load_option_symbols(&resolver, ticker, date)?;
        if symbols_by_value.is_empty() {
            return Ok(vec![]);
        }

        let params = day_params(date, resolution);
        Ok(ParquetReader::new().read_quote_bars_with_symbols(
            &[path],
            &symbols_by_value,
            &params,
        )?)
    }

    async fn get_option_ticks(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<Tick>> {
        let resolver = PathResolver::new(&self.data_root);
        let path = option_partition_path(&self.data_root, Resolution::Tick, "tick", date);
        if !path.exists() {
            return Ok(vec![]);
        }

        let symbols_by_value = load_option_symbols(&resolver, ticker, date)?;
        if symbols_by_value.is_empty() {
            return Ok(vec![]);
        }

        let params = day_params(date, Resolution::Tick);
        Ok(ParquetReader::new().read_ticks_with_symbols(&[path], &symbols_by_value, &params)?)
    }
}

fn local_bars_cover_expected_dates(bars: &[TradeBar], expected_dates: &[NaiveDate]) -> bool {
    if expected_dates.is_empty() {
        return true;
    }
    let available: HashSet<NaiveDate> = bars.iter().map(|bar| bar.time.date_utc()).collect();
    expected_dates.iter().all(|date| available.contains(date))
}

fn local_quote_bars_cover_expected_dates(bars: &[QuoteBar], expected_dates: &[NaiveDate]) -> bool {
    if expected_dates.is_empty() {
        return true;
    }
    let available: HashSet<NaiveDate> = bars.iter().map(|bar| bar.time.date_utc()).collect();
    expected_dates.iter().all(|date| available.contains(date))
}

fn local_ticks_cover_expected_dates(ticks: &[Tick], expected_dates: &[NaiveDate]) -> bool {
    if expected_dates.is_empty() {
        return true;
    }
    let available: HashSet<NaiveDate> = ticks.iter().map(|tick| tick.time.date_utc()).collect();
    expected_dates.iter().all(|date| available.contains(date))
}

fn expected_market_dates(symbol: &Symbol, start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    let mut current = start;
    while current <= end {
        if is_expected_market_date(symbol, current) {
            dates.push(current);
        }
        current += chrono::Duration::days(1);
    }
    dates
}

fn is_expected_market_date(symbol: &Symbol, date: NaiveDate) -> bool {
    match symbol.security_type() {
        SecurityType::Equity | SecurityType::Option | SecurityType::IndexOption => {
            let hours = ExchangeHours::us_equity();
            let dow = date.weekday().num_days_from_sunday() as usize;
            hours.schedule[dow].is_open() && !hours.holidays.contains(&date)
        }
        SecurityType::Crypto => true,
        _ => !matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun),
    }
}

fn load_option_symbols(
    resolver: &PathResolver,
    ticker: &str,
    date: chrono::NaiveDate,
) -> anyhow::Result<std::collections::HashMap<String, Symbol>> {
    let underlying = Symbol::create_equity(ticker, &Market::usa());
    let universe_path =
        option_partition_path(&resolver.data_root, Resolution::Daily, "universe", date);
    if !universe_path.exists() {
        return Ok(std::collections::HashMap::new());
    }

    let universe_rows = ParquetReader::new()
        .read_option_universe(&[universe_path])?
        .into_iter()
        .filter(|row| row.underlying.eq_ignore_ascii_case(ticker));
    let mut out = std::collections::HashMap::new();
    for row in universe_rows {
        let right = match row.right.to_ascii_uppercase().as_str() {
            "C" | "CALL" => OptionRight::Call,
            "P" | "PUT" => OptionRight::Put,
            _ => continue,
        };
        let sym = Symbol::create_option_osi(
            underlying.clone(),
            row.strike,
            row.expiration,
            right,
            OptionStyle::American,
            &Market::usa(),
        );
        out.insert(row.symbol_value, sym);
    }
    Ok(out)
}

fn option_partition_path(
    data_root: &std::path::Path,
    resolution: Resolution,
    tick_type: &str,
    date: chrono::NaiveDate,
) -> std::path::PathBuf {
    data_root
        .join("option")
        .join("usa")
        .join(resolution.folder_name())
        .join(tick_type)
        .join(format!("date={date}"))
        .join("data.parquet")
}

fn day_params(date: chrono::NaiveDate, resolution: Resolution) -> QueryParams {
    let start = lean_core::DateTime::from(chrono::DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0).unwrap(),
        chrono::Utc,
    ));
    let _ = resolution;
    let end = lean_core::DateTime::from(chrono::DateTime::from_naive_utc_and_offset(
        date.and_hms_opt(23, 59, 59).unwrap(),
        chrono::Utc,
    ));
    QueryParams::new().with_time_range(start, end)
}
