pub mod forex_holding;
pub mod forex_margin;
pub mod fxcm_model;
pub mod oanda_model;
pub mod pip;
pub mod swap_rates;

pub use forex_holding::ForexHolding;
pub use forex_margin::{max_position_size, required_margin};
pub use fxcm_model::FxcmBrokerageModel;
pub use oanda_model::OandaBrokerageModel;
pub use pip::{
    pip_size, pip_value_usd, pips_to_price, price_to_pips, MICRO_LOT, MINI_LOT, NANO_LOT,
    STANDARD_LOT,
};
pub use swap_rates::{calculate_swap, default_swap_rates, SwapRate};
