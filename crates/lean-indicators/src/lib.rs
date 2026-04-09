pub mod indicator;
pub mod window;
pub mod sma;
pub mod ema;
pub mod rsi;
pub mod macd;
pub mod bb;
pub mod atr;
pub mod adx;
pub mod stochastic;
pub mod roc;
pub mod cci;
pub mod williams_r;
pub mod donchian;
pub mod keltner;
pub mod vwap;
pub mod obv;
pub mod mfi;
pub mod aroon;

// Group 1 — Moving Averages
pub mod wma;
pub mod dema;
pub mod tema;
pub mod hma;
pub mod alma;
pub mod kama;
pub mod wilder_ma;
pub mod t3;
pub mod zlema;
pub mod trima;
pub mod mcginley;

// Group 2 — Trend / Overlay
pub mod supertrend;
pub mod psar;
pub mod ichimoku;
pub mod heikin_ashi;

// Group 3 — Momentum / Oscillators
pub mod momentum;
pub mod momentum_pct;
pub mod awesome_oscillator;
pub mod trix;
pub mod tsi;
pub mod ultimate_oscillator;
pub mod cmo;
pub mod dpo;
pub mod kst;
pub mod schaff;
pub mod stoch_rsi;
pub mod connors_rsi;
pub mod demarker;
pub mod balance_of_power;
pub mod relative_vigor_index;

// Group 4 — Volume
pub mod chaikin_money_flow;
pub mod chaikin_oscillator;
pub mod accumulation_distribution;
pub mod force_index;
pub mod ease_of_movement;
pub mod mass_index;

// Group 5 — Volatility / Trend Strength
pub mod vortex;
pub mod variance;
pub mod standard_deviation;
pub mod natr;

// Group 6 — Statistical / Regression
pub mod hurst_exponent;
pub mod log_return;
pub mod lsma;
pub mod tsf;
pub mod regression_channel;
pub mod correlation;
pub mod covariance;

// Group 7 — Misc / Utility
pub mod maximum;
pub mod minimum;
pub mod sum;
pub mod fisher_transform;
pub mod midpoint;
pub mod mid_price;
pub mod true_range;
pub mod average_range;

pub use indicator::{Indicator, IndicatorStatus, IndicatorResult};
pub use window::RollingWindow;
pub use sma::Sma;
pub use ema::Ema;
pub use rsi::Rsi;
pub use macd::Macd;
pub use bb::BollingerBands;
pub use atr::Atr;
pub use adx::Adx;
pub use stochastic::Stochastic;
pub use roc::Roc;
pub use cci::Cci;
pub use donchian::DonchianChannel;
pub use keltner::KeltnerChannel;
pub use vwap::Vwap;
pub use obv::Obv;
pub use mfi::MoneyFlowIndex;
pub use aroon::Aroon;

// Group 1
pub use wma::Wma;
pub use dema::Dema;
pub use tema::Tema;
pub use hma::Hma;
pub use alma::Alma;
pub use kama::Kama;
pub use wilder_ma::WilderMa;
pub use t3::T3;
pub use zlema::Zlema;
pub use trima::Trima;
pub use mcginley::McGinley;

// Group 2
pub use supertrend::SuperTrend;
pub use psar::Psar;
pub use ichimoku::{Ichimoku, IchimokuResult};
pub use heikin_ashi::{HeikinAshi, HeikinAshiBar};

// Group 3
pub use momentum::Momentum;
pub use momentum_pct::MomentumPct;
pub use awesome_oscillator::AwesomeOscillator;
pub use trix::Trix;
pub use tsi::Tsi;
pub use ultimate_oscillator::UltimateOscillator;
pub use cmo::Cmo;
pub use dpo::Dpo;
pub use kst::Kst;
pub use schaff::SchaffTrendCycle;
pub use stoch_rsi::StochasticRsi;
pub use connors_rsi::ConnorsRsi;
pub use demarker::DeMarker;
pub use balance_of_power::BalanceOfPower;
pub use relative_vigor_index::RelativeVigorIndex;

// Group 4
pub use chaikin_money_flow::ChaikinMoneyFlow;
pub use chaikin_oscillator::ChaikinOscillator;
pub use accumulation_distribution::AccumulationDistribution;
pub use force_index::ForceIndex;
pub use ease_of_movement::EaseOfMovement;
pub use mass_index::MassIndex;

// Group 5
pub use vortex::{Vortex, VortexResult};
pub use variance::Variance;
pub use standard_deviation::StandardDeviation;
pub use natr::Natr;

// Group 6
pub use hurst_exponent::HurstExponent;
pub use log_return::LogReturn;
pub use lsma::Lsma;
pub use tsf::Tsf;
pub use regression_channel::{RegressionChannel, RegressionChannelResult};
pub use correlation::Correlation;
pub use covariance::Covariance;

// Group 7
pub use maximum::Maximum;
pub use minimum::Minimum;
pub use sum::Sum;
pub use fisher_transform::FisherTransform;
pub use midpoint::MidPoint;
pub use mid_price::MidPrice;
pub use true_range::TrueRange;
pub use average_range::AverageRange;
