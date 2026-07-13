pub mod algorithm;
pub mod charting;
pub mod data;
pub mod data_views;
pub mod framework;
pub mod history;
pub mod indicators;
pub mod interrupt;
pub mod lifecycle;
pub mod market_data;
pub mod native_bridge;
pub mod options;
pub mod orders;
pub mod portfolio;
#[cfg(feature = "python")]
pub mod python_framework;
pub mod research;
pub mod securities;
pub mod types;
pub mod universe;

pub use algorithm::AlgorithmApi;
pub use lifecycle::{
    AlgorithmBridge, AlgorithmServices, AlgorithmStateAccess, LifecycleBridge,
    NoopAlgorithmServices, OptionSubscription, UniverseSelection,
};
pub use native_bridge::QcAlgorithmNativeBridge;

use rlean_core::Price;
use rust_decimal_macros::dec;

pub fn zero_price() -> Price {
    dec!(0)
}
