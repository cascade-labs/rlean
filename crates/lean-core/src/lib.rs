pub mod error;
pub mod market;
pub mod resolution;
pub mod security_type;
pub mod symbol;
pub mod time;
pub mod currency;
pub mod decimal_ext;
pub mod period;
pub mod exchange_hours;
pub mod data_normalization;
pub mod tick_type;
pub mod option_right;
pub mod option_style;
pub mod options;

pub use error::{LeanError, Result};
pub use market::Market;
pub use resolution::Resolution;
pub use security_type::SecurityType;
pub use symbol::{Symbol, SymbolProperties, SecurityIdentifier};
pub use time::{DateTime, TimeSpan, NanosecondTimestamp};
pub use currency::CurrencyPair;
pub use period::Period;
pub use data_normalization::DataNormalizationMode;
pub use tick_type::TickType;
pub use option_right::OptionRight;
pub use option_style::OptionStyle;
pub use options::{Greeks, OptionSymbolId, SettlementType, SymbolOptionsExt, format_option_ticker};

use rust_decimal::Decimal;

/// Canonical price type — 18-digit precision, no floating point error.
pub type Price = Decimal;

/// Canonical quantity type.
pub type Quantity = Decimal;
