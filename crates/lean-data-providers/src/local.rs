/// Local disk-only history provider — reads Parquet trade bars with no network calls.
///
/// Useful as a fallback when data has already been downloaded to the local
/// Parquet store, or in tests.
use lean_core::{Market, OptionRight, OptionStyle, Resolution, Symbol, SymbolOptionsExt};
use lean_data::{QuoteBar, Tick, TradeBar};
use lean_storage::{ParquetReader, PathResolver, QueryParams};

use crate::request::HistoryRequest;
use crate::traits::IHistoryProvider;

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

impl IHistoryProvider for LocalHistoryProvider {
    fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
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

        let paths: Vec<std::path::PathBuf> = if request.resolution.is_high_resolution() {
            let mut current = start_date;
            let mut v = Vec::new();
            while current <= end_date {
                let dp = resolver.trade_bar(&request.symbol, request.resolution, current);
                let p = dp.to_path();
                if p.exists() {
                    v.push(p);
                }
                current = current.succ_opt().unwrap_or(current);
            }
            v
        } else {
            let dp = resolver.trade_bar(&request.symbol, request.resolution, start_date);
            let p = dp.to_path();
            if p.exists() {
                vec![p]
            } else {
                vec![]
            }
        };

        if paths.is_empty() {
            return Ok(vec![]);
        }

        // ParquetReader::read_trade_bars is async; run it on a current-thread
        // runtime since get_history is called from spawn_blocking (no outer
        // runtime context is active on this thread).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build local runtime: {e}"))?;

        let reader = ParquetReader::new();
        let params = QueryParams::new().with_time_range(request.start, request.end);
        let symbol = request.symbol.clone();

        let bars = rt
            .block_on(reader.read_trade_bars(&paths, symbol, &params))
            .unwrap_or_default();

        Ok(bars)
    }

    fn get_quote_bars(&self, request: &HistoryRequest) -> anyhow::Result<Vec<QuoteBar>> {
        let resolver = PathResolver::new(&self.data_root);
        let start_date = request.start.date_utc();
        let end_date = request.end.date_utc();

        let paths: Vec<std::path::PathBuf> = if request.resolution.is_high_resolution() {
            let mut current = start_date;
            let mut v = Vec::new();
            while current <= end_date {
                let p = resolver
                    .quote_bar(&request.symbol, request.resolution, current)
                    .to_path();
                if p.exists() {
                    v.push(p);
                }
                current = current.succ_opt().unwrap_or(current);
            }
            v
        } else {
            let p = resolver
                .quote_bar(&request.symbol, request.resolution, start_date)
                .to_path();
            if p.exists() {
                vec![p]
            } else {
                vec![]
            }
        };

        if paths.is_empty() {
            return Ok(vec![]);
        }

        let reader = ParquetReader::new();
        let params = QueryParams::new().with_time_range(request.start, request.end);
        Ok(reader.read_quote_bars(&paths, request.symbol.clone(), &params)?)
    }

    fn get_ticks(&self, request: &HistoryRequest) -> anyhow::Result<Vec<Tick>> {
        let resolver = PathResolver::new(&self.data_root);
        let start_date = request.start.date_utc();
        let end_date = request.end.date_utc();

        let mut current = start_date;
        let mut paths = Vec::new();
        while current <= end_date {
            let p = resolver.tick(&request.symbol, current).to_path();
            if p.exists() {
                paths.push(p);
            }
            current = current.succ_opt().unwrap_or(current);
        }

        if paths.is_empty() {
            return Ok(vec![]);
        }

        let reader = ParquetReader::new();
        let params = QueryParams::new().with_time_range(request.start, request.end);
        Ok(reader.read_ticks(&paths, request.symbol.clone(), &params)?)
    }

    fn get_option_universe(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_storage::OptionUniverseRow>> {
        let resolver = PathResolver::new(&self.data_root);
        let underlying = Symbol::create_equity(ticker, &Market::usa());
        let path = resolver.option_universe(&underlying, date).to_path();
        if !path.exists() {
            return Ok(vec![]);
        }
        Ok(ParquetReader::new().read_option_universe(&[path])?)
    }

    fn get_option_trade_bars(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<TradeBar>> {
        let resolver = PathResolver::new(&self.data_root);
        let underlying = Symbol::create_equity(ticker, &Market::usa());
        let path = resolver
            .option_trade_bar(&underlying, resolution, date)
            .to_path();
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

    fn get_option_quote_bars(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<QuoteBar>> {
        let resolver = PathResolver::new(&self.data_root);
        let underlying = Symbol::create_equity(ticker, &Market::usa());
        let path = resolver
            .option_quote_bar(&underlying, resolution, date)
            .to_path();
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

    fn get_option_ticks(&self, ticker: &str, date: chrono::NaiveDate) -> anyhow::Result<Vec<Tick>> {
        let resolver = PathResolver::new(&self.data_root);
        let underlying = Symbol::create_equity(ticker, &Market::usa());
        let path = resolver.option_tick(&underlying, date).to_path();
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

fn load_option_symbols(
    resolver: &PathResolver,
    ticker: &str,
    date: chrono::NaiveDate,
) -> anyhow::Result<std::collections::HashMap<String, Symbol>> {
    let underlying = Symbol::create_equity(ticker, &Market::usa());
    let universe_path = resolver.option_universe(&underlying, date).to_path();
    if !universe_path.exists() {
        return Ok(std::collections::HashMap::new());
    }

    let universe_rows = ParquetReader::new().read_option_universe(&[universe_path])?;
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
