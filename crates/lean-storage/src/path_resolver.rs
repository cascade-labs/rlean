use chrono::NaiveDate;
use lean_core::{Resolution, Symbol, TickType};
use std::path::{Path, PathBuf};

/// Compatibility path resolver for legacy provider plugins.
#[derive(Debug, Clone)]
pub struct PathResolver {
    pub data_root: PathBuf,
}

impl PathResolver {
    pub fn new(data_root: impl AsRef<Path>) -> Self {
        Self {
            data_root: data_root.as_ref().to_path_buf(),
        }
    }

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
}
