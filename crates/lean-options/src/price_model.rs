use crate::contract::OptionContract;
use chrono::{NaiveDate, TimeZone, Utc};
use lean_core::time::tz;
use lean_core::{DateTime, Greeks, OptionRight};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Default)]
pub struct OptionPriceModelResult {
    pub theoretical_price: Decimal,
    pub implied_volatility: Decimal,
    pub greeks: Greeks,
}

pub trait IOptionPriceModel: Send + Sync {
    fn evaluate(
        &self,
        contract: &OptionContract,
        valuation_time: DateTime,
        risk_free_rate: f64,
        dividend_yield: f64,
    ) -> OptionPriceModelResult;
}

/// Returns current bid/ask mid as the theoretical price; no Greeks.
pub struct CurrentPricePriceModel;

impl IOptionPriceModel for CurrentPricePriceModel {
    fn evaluate(
        &self,
        contract: &OptionContract,
        _valuation_time: DateTime,
        _rf: f64,
        _dy: f64,
    ) -> OptionPriceModelResult {
        OptionPriceModelResult {
            theoretical_price: contract.mid_price(),
            ..Default::default()
        }
    }
}

/// Black-Scholes closed-form price model with full Greeks.
pub struct BlackScholesPriceModel;

impl IOptionPriceModel for BlackScholesPriceModel {
    fn evaluate(
        &self,
        contract: &OptionContract,
        valuation_time: DateTime,
        risk_free_rate: f64,
        dividend_yield: f64,
    ) -> OptionPriceModelResult {
        let s = contract.data.underlying_last_price.to_f64().unwrap_or(0.0);
        let k = contract.strike.to_f64().unwrap_or(0.0);
        let t = time_to_expiry_years(contract.expiry, valuation_time);
        let r = risk_free_rate;
        let q = dividend_yield;
        let sigma = contract.data.implied_volatility.to_f64().unwrap_or(0.20);
        let is_call = contract.right == OptionRight::Call;

        if t <= 0.0 || s <= 0.0 || k <= 0.0 || sigma <= 0.0 {
            return OptionPriceModelResult {
                theoretical_price: crate::payoff::intrinsic_value(
                    contract.data.underlying_last_price,
                    contract.strike,
                    contract.right,
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
        let vega = s * f64::exp(-q * t) * norm_pdf(d1) * t.sqrt() / 100.0;
        let theta = if is_call_flag {
            (-s * norm_pdf(d1) * sigma * f64::exp(-q * t) / (2.0 * t.sqrt())
                - r * k * f64::exp(-r * t) * norm_cdf(d2)
                + q * s * f64::exp(-q * t) * norm_cdf(d1))
                / 365.0
        } else {
            (-s * norm_pdf(d1) * sigma * f64::exp(-q * t) / (2.0 * t.sqrt())
                + r * k * f64::exp(-r * t) * norm_cdf(-d2)
                - q * s * f64::exp(-q * t) * norm_cdf(-d1))
                / 365.0
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
                vega: d(vega),
                theta: d(theta),
                rho: d(rho),
                lambda: if price > 0.0 {
                    d(delta * s / price)
                } else {
                    Decimal::ZERO
                },
            },
        }
    }
}

pub fn time_to_expiry_years(expiry: NaiveDate, valuation_time: DateTime) -> f64 {
    let expiry_local = expiry.and_hms_opt(16, 0, 0).unwrap();
    let expiry_dt = match tz::NEW_YORK.from_local_datetime(&expiry_local) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc),
        chrono::LocalResult::None => return 0.0,
    };

    let Ok(duration) = (expiry_dt - valuation_time.to_utc()).to_std() else {
        return 0.0;
    };

    duration.as_secs_f64() / (365.0 * 24.0 * 60.0 * 60.0)
}

pub fn infer_implied_volatility(
    contract: &OptionContract,
    valuation_time: DateTime,
    risk_free_rate: f64,
    dividend_yield: f64,
) -> Option<Decimal> {
    let market_price = contract.mid_price().to_f64().unwrap_or(0.0);
    let s = contract.data.underlying_last_price.to_f64().unwrap_or(0.0);
    let k = contract.strike.to_f64().unwrap_or(0.0);
    let t = time_to_expiry_years(contract.expiry, valuation_time);

    if market_price <= 0.0 || s <= 0.0 || k <= 0.0 || t <= 0.0 {
        return None;
    }

    let sigma = implied_volatility(
        market_price,
        s,
        k,
        t,
        risk_free_rate,
        dividend_yield,
        contract.right,
    );

    if sigma.is_finite() && sigma > 0.0 {
        Decimal::from_f64(sigma)
    } else {
        None
    }
}

pub fn evaluate_contract_with_market_iv<M: IOptionPriceModel>(
    price_model: &M,
    contract: &mut OptionContract,
    valuation_time: DateTime,
    risk_free_rate: f64,
    dividend_yield: f64,
) -> OptionPriceModelResult {
    if let Some(iv) =
        infer_implied_volatility(contract, valuation_time, risk_free_rate, dividend_yield)
    {
        contract.data.implied_volatility = iv;
    }

    let result = price_model.evaluate(contract, valuation_time, risk_free_rate, dividend_yield);
    contract.data.theoretical_price = result.theoretical_price;
    contract.data.implied_volatility = result.implied_volatility;
    contract.data.greeks = result.greeks.clone();
    result
}

/// Compute IV from market price using Newton-Raphson bisection.
pub fn implied_volatility(
    market_price: f64,
    s: f64,
    k: f64,
    t: f64,
    r: f64,
    q: f64,
    right: OptionRight,
) -> f64 {
    if t <= 0.0 || market_price <= 0.0 {
        return 0.0;
    }
    let is_call = right == OptionRight::Call;
    let mut lo = 1e-6_f64;
    let mut hi = 10.0_f64;
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        let price = bs_price(s, k, t, r, q, mid, is_call);
        if (price - market_price).abs() < 1e-8 {
            return mid;
        }
        if price < market_price {
            lo = mid;
        } else {
            hi = mid;
        }
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
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * z);
    let poly = (((((1.061405429 * t) - 1.453152027) * t + 1.421413741) * t - 0.284496736) * t
        + 0.254829592)
        * t;
    let erf = 1.0 - poly * (-z * z).exp();
    let signed_erf = if x >= 0.0 { erf } else { -erf };
    0.5 * (1.0 + signed_erf)
}

fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, OptionStyle, Symbol, SymbolOptionsExt};
    use rust_decimal_macros::dec;

    #[test]
    fn black_scholes_uses_supplied_backtest_time() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let expiry = NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let symbol = Symbol::create_option_osi(
            underlying,
            dec!(100),
            expiry,
            OptionRight::Call,
            OptionStyle::American,
            &market,
        );

        let mut contract = OptionContract::new(symbol);
        contract.data.underlying_last_price = dec!(100);
        contract.data.bid_price = dec!(2.40);
        contract.data.ask_price = dec!(2.60);

        let valuation_time = DateTime::from(
            Utc.with_ymd_and_hms(2024, 1, 18, 19, 0, 0)
                .single()
                .unwrap(),
        );

        let model = BlackScholesPriceModel;
        let result =
            evaluate_contract_with_market_iv(&model, &mut contract, valuation_time, 0.0, 0.0);

        assert!(result.implied_volatility > Decimal::ZERO);
        assert!(result.theoretical_price > Decimal::ZERO);
        assert!(result.greeks.delta > Decimal::ZERO);
        assert!(result.greeks.gamma > Decimal::ZERO);
    }

    #[test]
    fn time_to_expiry_keeps_intraday_value_on_expiry_date() {
        let expiry = NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let valuation_time = DateTime::from(
            Utc.with_ymd_and_hms(2024, 1, 19, 17, 0, 0)
                .single()
                .unwrap(),
        );

        assert!(time_to_expiry_years(expiry, valuation_time) > 0.0);
    }

    #[test]
    fn norm_cdf_is_centered_at_half() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-9);
    }
}
