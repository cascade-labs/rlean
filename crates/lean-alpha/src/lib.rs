pub mod alpha_model;
pub mod insight;
pub mod insight_collection;
pub mod models;

pub use alpha_model::{CompositeAlphaModel, ConstantAlphaModel, IAlphaModel, NullAlphaModel};
pub use insight::{Insight, InsightDirection, InsightType};
pub use insight_collection::InsightCollection;
pub use models::ema_cross::EmaCrossAlphaModel;
pub use models::historical_returns::HistoricalReturnsAlphaModel;
pub use models::macd_alpha::MacdAlphaModel;
pub use models::momentum_alpha::MomentumAlphaModel;
pub use models::pairs_alpha::PairsTradingAlphaModel;
pub use models::pearson_pairs::PearsonCorrelationPairsTradingAlphaModel;
pub use models::rsi_alpha::RsiAlphaModel;
