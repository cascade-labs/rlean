pub mod continuous_contract;
pub mod contract_series;
pub mod expiry;
pub mod known_futures;
pub mod settlement;

pub use continuous_contract::{ContinuousContract, ContinuousContractType};
pub use contract_series::{
    month_code, month_from_code, FuturesContract, FuturesContractSeries, MONTH_CODES,
};
pub use expiry::{compute_expiry, ExpiryRule};
pub use known_futures::{cl, es, gc, nq, zb};
pub use settlement::{settle_futures_position, SettlementMethod, SettlementResult};
