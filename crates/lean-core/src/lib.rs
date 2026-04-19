pub mod currency;
pub mod data_normalization;
pub mod decimal_ext;
pub mod error;
pub mod exchange_hours;
pub mod market;
pub mod option_right;
pub mod option_style;
pub mod options;
pub mod period;
pub mod resolution;
pub mod security_type;
pub mod symbol;
pub mod tick_type;
pub mod time;

pub use currency::CurrencyPair;
pub use data_normalization::DataNormalizationMode;
pub use error::{LeanError, Result};
pub use market::Market;
pub use option_right::OptionRight;
pub use option_style::OptionStyle;
pub use options::{format_option_ticker, Greeks, OptionSymbolId, SettlementType, SymbolOptionsExt};
pub use period::Period;
pub use resolution::Resolution;
pub use security_type::SecurityType;
pub use symbol::{SecurityIdentifier, Symbol, SymbolProperties};
pub use tick_type::TickType;
pub use time::{DateTime, NanosecondTimestamp, TimeSpan};

use rust_decimal::Decimal;

/// Canonical price type — 18-digit precision, no floating point error.
pub type Price = Decimal;

/// Canonical quantity type.
pub type Quantity = Decimal;
