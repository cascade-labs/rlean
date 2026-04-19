use lean_core::DateTime;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Path to the Parquet data directory.
    pub data_root: PathBuf,
    pub start_date: Option<DateTime>,
    pub end_date: Option<DateTime>,
    pub starting_cash: rust_decimal::Decimal,
    pub benchmark_symbol: Option<String>,
    /// Maximum number of open orders.
    pub max_orders: usize,
    /// Whether to use real-time clock for live trading.
    pub live_mode: bool,
    /// Parallelism for data loading.
    pub threads: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            data_root: PathBuf::from("data"),
            start_date: None,
            end_date: None,
            starting_cash: rust_decimal_macros::dec!(100_000),
            benchmark_symbol: Some("SPY".to_string()),
            max_orders: 10_000,
            live_mode: false,
            threads: num_cpus::get(),
        }
    }
}
