use lean_core::Greeks;
use lean_core::OptionRight;
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use crate::contract::OptionContract;

#[derive(Debug, Clone, Default)]
pub struct OptionPriceModelResult {
    pub theoretical_price: Decimal,
    pub implied_volatility: Decimal,
    pub greeks: Greeks,
}

pub trait IOptionPriceModel: Send + Sync {
    fn evaluate(&self, contract: &OptionContract, risk_free_rate: f64, dividend_yield: f64) -> OptionPriceModelResult;
}

/// Returns current bid/ask mid as the theoretical price; no Greeks.
pub struct CurrentPricePriceModel;

impl IOptionPriceModel for CurrentPricePriceModel {
    fn evaluate(&self, contract: &OptionContract, _rf: f64, _dy: f64) -> OptionPriceModelResult {
        OptionPriceModelResult {
            theoretical_price: contract.mid_price(),
            ..Default::default()
        }
    }
}

/// Black-Scholes closed-form price model with full Greeks.
pub struct BlackScholesPriceModel;

impl IOptionPriceModel for BlackScholesPriceModel {
    fn evaluate(&self, contract: &OptionContract, risk_free_rate: f64, dividend_yield: f64) -> OptionPriceModelResult {
        let s = contract.data.underlying_last_price.to_f64().unwrap_or(0.0);
        let k = contract.strike.to_f64().unwrap_or(0.0);
        let today = chrono::Utc::now().date_naive();
        let t = (contract.expiry - today).num_days() as f64 / 365.0;
        let r = risk_free_rate;
        let q = dividend_yield;
        let sigma = contract.data.implied_volatility.to_f64().unwrap_or(0.20);
        let is_call = contract.right == OptionRight::Call;

        if t <= 0.0 || s <= 0.0 || k <= 0.0 || sigma <= 0.0 {
            return OptionPriceModelResult {
                theoretical_price: crate::payoff::intrinsic_value(
                    contract.data.underlying_last_price, contract.strike, contract.right
                ),
                ..Default::default()
            };
        }

        let d1 = (f64::ln(s / k) + (r - q + 0.5 * sigma * sigma) * t) / (sigma * t.sqrt());
        let d2 = d1 - sigma * t.sqrt();

        let is_call_flag = is_call;

        let price = if is_call_flag {
            s * f64::exp(-q * t) * norm_cdf(d1) - k * f64::exp(-r * t) * norm_cdf(d2)
        } else {
            k * f64::exp(-r * t) * norm_cdf(-d2) - s * f64::exp(-q * t) * norm_cdf(-d1)
        };

        let delta = if is_call_flag {
            f64::exp(-q * t) * norm_cdf(d1)
        } else {
            -f64::exp(-q * t) * norm_cdf(-d1)
        };

        let gamma = f64::exp(-q * t) * norm_pdf(d1) / (s * sigma * t.sqrt());
        let vega  = s * f64::exp(-q * t) * norm_pdf(d1) * t.sqrt() / 100.0;
        let theta = if is_call_flag {
            (-s * norm_pdf(d1) * sigma * f64::exp(-q * t) / (2.0 * t.sqrt())
             - r * k * f64::exp(-r * t) * norm_cdf(d2)
             + q * s * f64::exp(-q * t) * norm_cdf(d1)) / 365.0
        } else {
            (-s * norm_pdf(d1) * sigma * f64::exp(-q * t) / (2.0 * t.sqrt())
             + r * k * f64::exp(-r * t) * norm_cdf(-d2)
             - q * s * f64::exp(-q * t) * norm_cdf(-d1)) / 365.0
        };
        let rho = if is_call_flag {
            k * t * f64::exp(-r * t) * norm_cdf(d2) / 100.0
        } else {
            -k * t * f64::exp(-r * t) * norm_cdf(-d2) / 100.0
        };

        let d = |v: f64| Decimal::from_f64(v).unwrap_or(Decimal::ZERO);

        OptionPriceModelResult {
            theoretical_price: d(price.max(0.0)),
            implied_volatility: Decimal::from_f64(sigma).unwrap_or(Decimal::ZERO),
            greeks: Greeks {
                delta: d(delta),
                gamma: d(gamma),
                vega:  d(vega),
                theta: d(theta),
                rho:   d(rho),
                lambda: if price > 0.0 { d(delta * s / price) } else { Decimal::ZERO },
            },
        }
    }
}

/// Compute IV from market price using Newton-Raphson bisection.
pub fn implied_volatility(
    market_price: f64, s: f64, k: f64, t: f64, r: f64, q: f64, right: OptionRight,
) -> f64 {
    if t <= 0.0 || market_price <= 0.0 { return 0.0; }
    let is_call = right == OptionRight::Call;
    let mut lo = 1e-6_f64;
    let mut hi = 10.0_f64;
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        let price = bs_price(s, k, t, r, q, mid, is_call);
        if (price - market_price).abs() < 1e-8 { return mid; }
        if price < market_price { lo = mid; } else { hi = mid; }
    }
    (lo + hi) / 2.0
}

fn bs_price(s: f64, k: f64, t: f64, r: f64, q: f64, sigma: f64, is_call: bool) -> f64 {
    let d1 = (f64::ln(s / k) + (r - q + 0.5 * sigma * sigma) * t) / (sigma * t.sqrt());
    let d2 = d1 - sigma * t.sqrt();
    if is_call {
        s * f64::exp(-q * t) * norm_cdf(d1) - k * f64::exp(-r * t) * norm_cdf(d2)
    } else {
        k * f64::exp(-r * t) * norm_cdf(-d2) - s * f64::exp(-q * t) * norm_cdf(-d1)
    }
}

fn norm_cdf(x: f64) -> f64 {
    let a = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * a);
    let poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    let cdf = 1.0 - poly * (-a * a).exp();
    if x >= 0.0 { cdf } else { 1.0 - cdf }
}

fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}
