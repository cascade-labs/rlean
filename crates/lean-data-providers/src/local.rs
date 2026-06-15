/// Local disk-only history provider — reads Parquet trade bars with no network calls.
///
/// Useful as a fallback when data has already been downloaded to the local
/// Parquet store, or in tests.
use chrono::{Datelike, NaiveDate};
use lean_core::{
    exchange_hours::ExchangeHours, Market, OptionRight, OptionStyle, Resolution, SecurityType,
    Symbol, SymbolOptionsExt, TickType,
};
use lean_data::{MarginInterestRate, QuoteBar, Tick, TradeBar};
use lean_storage::{OptionUniverseRow, ParquetReader, PathResolver, QueryParams};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::request::HistoryRequest;
use crate::traits::IHistoryProvider;
use async_trait::async_trait;

pub struct LocalHistoryProvider {
    pub(crate) data_root: std::path::PathBuf,
    option_universe_cache: Mutex<Option<OptionUniverseCacheEntry>>,
}

struct OptionUniverseCacheEntry {
    date: NaiveDate,
    rows: Arc<Vec<OptionUniverseRow>>,
    loaded_underlyings: HashSet<String>,
}

impl LocalHistoryProvider {
    pub fn new(data_root: impl AsRef<std::path::Path>) -> Self {
        LocalHistoryProvider {
            data_root: data_root.as_ref().to_path_buf(),
            option_universe_cache: Mutex::new(None),
        }
    }

    fn merge_option_universe_cache(
        &self,
        date: NaiveDate,
        loaded_underlyings: &HashSet<String>,
        rows: &[OptionUniverseRow],
    ) {
        if loaded_underlyings.is_empty() {
            return;
        }

        let mut guard = self
            .option_universe_cache
            .lock()
            .expect("local option universe cache poisoned");
        if let Some(entry) = guard.as_mut().filter(|entry| entry.date == date) {
            let mut merged = entry.rows.as_ref().clone();
            merged
                .retain(|row| !loaded_underlyings.contains(&normalize_underlying(&row.underlying)));
            merged.extend_from_slice(rows);
            entry
                .loaded_underlyings
                .extend(loaded_underlyings.iter().cloned());
            entry.rows = Arc::new(merged);
        } else {
            *guard = Some(OptionUniverseCacheEntry {
                date,
                rows: Arc::new(rows.to_vec()),
                loaded_underlyings: loaded_underlyings.clone(),
            });
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

    async fn get_margin_interest_rates(
        &self,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<MarginInterestRate>> {
        let resolver = PathResolver::new(&self.data_root);
        let start_date = request.start.date_utc();
        let end_date = request.end.date_utc();
        let expected_dates = expected_market_dates(&request.symbol, start_date, end_date);

        let mut paths = Vec::new();
        for current in &expected_dates {
            let p = resolver.margin_interest_partition(&request.symbol, *current);
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
        let mut rates = Vec::new();
        for path in &paths {
            rates.extend(reader.read_margin_interest_rate_partition(
                path,
                &request.symbol,
                &params,
            )?);
        }
        Ok(rates)
    }

    async fn get_option_universe(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_storage::OptionUniverseRow>> {
        let ticker_key = normalize_underlying(ticker);
        if let Some(rows) = self
            .option_universe_cache
            .lock()
            .expect("local option universe cache poisoned")
            .as_ref()
            .filter(|entry| entry.date == date && entry.loaded_underlyings.contains(&ticker_key))
            .map(|entry| Arc::clone(&entry.rows))
        {
            return Ok(filter_option_universe_rows(&rows, &ticker_key));
        }

        let path = option_partition_path(&self.data_root, Resolution::Daily, "universe", date);
        if !path.exists() {
            return Ok(vec![]);
        }

        let rows = ParquetReader::new()
            .read_option_universe_filtered(&[path], std::slice::from_ref(&ticker_key))
            .await?;
        self.merge_option_universe_cache(date, &HashSet::from([ticker_key.clone()]), &rows);

        Ok(filter_option_universe_rows(&rows, &ticker_key))
    }

    async fn get_option_universes(
        &self,
        tickers: &[String],
        date: chrono::NaiveDate,
    ) -> anyhow::Result<HashMap<String, Vec<lean_storage::OptionUniverseRow>>> {
        let requested = normalized_underlyings(tickers);
        if requested.is_empty() {
            return Ok(HashMap::new());
        }

        let mut out = HashMap::new();
        let mut missing = Vec::new();
        if let Some(rows) = self
            .option_universe_cache
            .lock()
            .expect("local option universe cache poisoned")
            .as_ref()
            .filter(|entry| entry.date == date)
            .map(|entry| (Arc::clone(&entry.rows), entry.loaded_underlyings.clone()))
        {
            for ticker in &requested {
                if rows.1.contains(ticker) {
                    out.insert(ticker.clone(), filter_option_universe_rows(&rows.0, ticker));
                } else {
                    missing.push(ticker.clone());
                }
            }
        } else {
            missing.extend(requested.iter().cloned());
        }

        if missing.is_empty() {
            return Ok(out);
        }

        let path = option_partition_path(&self.data_root, Resolution::Daily, "universe", date);
        if !path.exists() {
            for ticker in missing {
                out.entry(ticker).or_insert_with(Vec::new);
            }
            return Ok(out);
        }

        let loaded_rows = ParquetReader::new()
            .read_option_universe_filtered(&[path], &missing)
            .await?;
        let loaded_underlyings = missing.iter().cloned().collect::<HashSet<_>>();
        self.merge_option_universe_cache(date, &loaded_underlyings, &loaded_rows);

        for ticker in missing {
            out.insert(
                ticker.clone(),
                filter_option_universe_rows(&loaded_rows, &ticker),
            );
        }

        Ok(out)
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

    async fn get_option_trade_bars_filtered(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<TradeBar>> {
        let path = option_partition_path(&self.data_root, resolution, "trade", date);
        if !path.exists() {
            return Ok(vec![]);
        }

        let symbols_by_value = option_symbols_by_value_from_contracts(ticker, contracts);
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

    async fn get_option_quote_bars_filtered(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<QuoteBar>> {
        let path = option_partition_path(&self.data_root, resolution, "quote", date);
        if !path.exists() {
            return Ok(vec![]);
        }

        let symbols_by_value = option_symbols_by_value_from_contracts(ticker, contracts);
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
        let paths = [
            resolver.option_partition(Resolution::Tick, TickType::Trade, date),
            resolver.option_partition(Resolution::Tick, TickType::Quote, date),
            resolver.option_partition(Resolution::Tick, TickType::OpenInterest, date),
        ];
        let existing_paths = paths
            .into_iter()
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        if existing_paths.is_empty() {
            return Ok(vec![]);
        }

        let symbols_by_value = load_option_symbols(&resolver, ticker, date)?;
        if symbols_by_value.is_empty() {
            return Ok(vec![]);
        }

        let params = day_params(date, Resolution::Tick);
        let mut ticks = ParquetReader::new().read_ticks_with_symbols(
            &existing_paths,
            &symbols_by_value,
            &params,
        )?;
        ticks.sort_by_key(|tick| (tick.time.0, tick.symbol.id.sid, tick.tick_type as u8));
        Ok(ticks)
    }

    async fn get_option_ticks_filtered(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<Tick>> {
        let resolver = PathResolver::new(&self.data_root);
        let paths = [
            resolver.option_partition(Resolution::Tick, TickType::Trade, date),
            resolver.option_partition(Resolution::Tick, TickType::Quote, date),
            resolver.option_partition(Resolution::Tick, TickType::OpenInterest, date),
        ];
        let existing_paths = paths
            .into_iter()
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        if existing_paths.is_empty() {
            return Ok(vec![]);
        }

        let symbols_by_value = option_symbols_by_value_from_contracts(ticker, contracts);
        if symbols_by_value.is_empty() {
            return Ok(vec![]);
        }

        let params = day_params(date, Resolution::Tick);
        let mut ticks = ParquetReader::new().read_ticks_with_symbols(
            &existing_paths,
            &symbols_by_value,
            &params,
        )?;
        ticks.sort_by_key(|tick| (tick.time.0, tick.symbol.id.sid, tick.tick_type as u8));
        Ok(ticks)
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
        SecurityType::Crypto | SecurityType::CryptoFuture => true,
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
        out.insert(row.symbol_value, sym.clone());
        out.insert(sym.value.clone(), sym);
    }
    Ok(out)
}

fn option_symbols_by_value_from_contracts(
    ticker: &str,
    contracts: &[lean_storage::OptionUniverseRow],
) -> HashMap<String, Symbol> {
    let underlying = Symbol::create_equity(ticker, &Market::usa());
    let mut out = HashMap::new();
    for row in contracts {
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
        out.insert(row.symbol_value.clone(), sym.clone());
        out.insert(sym.value.clone(), sym);
    }
    out
}

fn normalize_underlying(ticker: &str) -> String {
    ticker.trim().trim_start_matches('?').to_ascii_uppercase()
}

fn normalized_underlyings(tickers: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for ticker in tickers {
        let normalized = normalize_underlying(ticker);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        out.push(normalized);
    }
    out
}

fn filter_option_universe_rows(rows: &[OptionUniverseRow], ticker: &str) -> Vec<OptionUniverseRow> {
    rows.iter()
        .filter(|row| row.underlying.eq_ignore_ascii_case(ticker))
        .cloned()
        .collect()
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
