use chrono::NaiveDate;
use lean_core::{Resolution, Symbol, TickType};
use std::path::{Path, PathBuf};

/// High-level helper that knows the data root and resolves canonical parquet paths.
#[derive(Debug, Clone)]
pub struct PathResolver {
    pub data_root: PathBuf,
}

impl PathResolver {
    pub fn new(data_root: impl AsRef<Path>) -> Self {
        PathResolver {
            data_root: data_root.as_ref().to_path_buf(),
        }
    }

    /// Canonical partition path for market data stored as all-symbol daily
    /// Parquet datasets:
    /// `{root}/{security_type}/{market}/{resolution}/{tick_type}/date=YYYY-MM-DD/data.parquet`.
    pub fn market_data_partition(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        tick_type: TickType,
        date: NaiveDate,
    ) -> PathBuf {
        self.partition(
            &format!("{}", symbol.security_type()).to_lowercase(),
            &symbol.market().as_str().to_lowercase(),
            resolution.folder_name(),
            &format!("{tick_type}").to_lowercase(),
            date,
        )
    }

    pub fn option_partition(
        &self,
        resolution: Resolution,
        tick_type: TickType,
        date: NaiveDate,
    ) -> PathBuf {
        self.partition(
            "option",
            "usa",
            resolution.folder_name(),
            &format!("{tick_type}").to_lowercase(),
            date,
        )
    }

    pub fn option_universe_partition(&self, date: NaiveDate) -> PathBuf {
        self.partition("option", "usa", "daily", "universe", date)
    }

    fn partition(
        &self,
        security_type: &str,
        market: &str,
        resolution: &str,
        tick_type: &str,
        date: NaiveDate,
    ) -> PathBuf {
        self.data_root
            .join(security_type)
            .join(market)
            .join(resolution)
            .join(tick_type)
            .join(format!("date={date}"))
            .join("data.parquet")
    }

    /// Path to the factor file for the given market and ticker.
    ///
    /// Layout: `{data_root}/equity/{market}/factor_files/{ticker_lower}.parquet`
    pub fn factor_file(&self, market: &str, ticker: &str) -> PathBuf {
        factor_file_path(&self.data_root, market, ticker)
    }

    /// Path to the map file for the given market and ticker.
    ///
    /// Layout: `{data_root}/equity/{market}/map_files/{ticker_lower}.parquet`
    pub fn map_file(&self, market: &str, ticker: &str) -> PathBuf {
        map_file_path(&self.data_root, market, ticker)
    }
}

/// Canonical path for a custom data point cache file.
///
/// Layout: `{root}/custom/{source_type}/{ticker_lower}/{YYYYMMDD}.parquet`
///
/// One file per trading date per source per ticker; reads return all rows for that date.
pub fn custom_data_path(
    root: impl AsRef<Path>,
    source_type: &str,
    ticker: &str,
    date: NaiveDate,
) -> PathBuf {
    let mut p = root.as_ref().to_path_buf();
    p.push("custom");
    p.push(source_type.to_lowercase());
    p.push(ticker.to_lowercase());
    p.push(format!("{}.parquet", date.format("%Y%m%d")));
    p
}

/// Canonical path for the full-history cache of a custom data series.
///
/// Used when `ICustomDataSource::is_full_history_source()` is `true`.
///
/// Layout: `{root}/custom/{source_type_lower}/{ticker_lower}/history.parquet`
pub fn custom_data_history_path(
    root: impl AsRef<Path>,
    source_type: &str,
    ticker: &str,
) -> PathBuf {
    let mut p = root.as_ref().to_path_buf();
    p.push("custom");
    p.push(source_type.to_lowercase());
    p.push(ticker.to_lowercase());
    p.push("history.parquet");
    p
}

/// Canonical path for a factor file.
///
/// Layout: `{root}/equity/{market}/factor_files/{ticker_lower}.parquet`
pub fn factor_file_path(root: impl AsRef<Path>, market: &str, ticker: &str) -> PathBuf {
    let mut p = root.as_ref().to_path_buf();
    p.push("equity");
    p.push(market.to_lowercase());
    p.push("factor_files");
    p.push(format!("{}.parquet", ticker.to_lowercase()));
    p
}

/// Canonical path for a map file.
///
/// Layout: `{root}/equity/{market}/map_files/{ticker_lower}.parquet`
pub fn map_file_path(root: impl AsRef<Path>, market: &str, ticker: &str) -> PathBuf {
    let mut p = root.as_ref().to_path_buf();
    p.push("equity");
    p.push(market.to_lowercase());
    p.push("map_files");
    p.push(format!("{}.parquet", ticker.to_lowercase()));
    p
}
