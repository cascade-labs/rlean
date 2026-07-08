use crate::contract::OptionContract;
use chrono::{NaiveDate, TimeZone, Utc};
use implied_vol::{DefaultSpecialFn, ImpliedBlackVolatility};
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

#[derive(Debug, Clone, Copy)]
pub struct OptionPricingInput {
    pub spot: f64,
    pub forward: f64,
    pub strike: f64,
    pub expiry_years: f64,
    pub market_price: f64,
    pub risk_free_rate: f64,
    pub dividend_yield: f64,
    pub is_call: bool,
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
        let input =
            pricing_input_from_contract(contract, valuation_time, risk_free_rate, dividend_yield);
        let sigma = contract.data.implied_volatility.to_f64().unwrap_or(0.20);
        evaluate_black_scholes_with_iv(input, sigma).unwrap_or_else(|| OptionPriceModelResult {
            theoretical_price: crate::payoff::intrinsic_value(
                contract.data.underlying_last_price,
                contract.strike,
                contract.right,
            ),
            ..Default::default()
        })
    }
}

pub fn pricing_input_from_contract(
    contract: &OptionContract,
    valuation_time: DateTime,
    risk_free_rate: f64,
    dividend_yield: f64,
) -> OptionPricingInput {
    let spot = contract.data.underlying_last_price.to_f64().unwrap_or(0.0);
    let expiry_years = time_to_expiry_years(contract.expiry, valuation_time);
    let forward = if expiry_years > 0.0 && spot > 0.0 {
        spot * ((risk_free_rate - dividend_yield) * expiry_years).exp()
    } else {
        spot
    };
    let market_price = if risk_free_rate != 0.0 && expiry_years > 0.0 {
        contract.mid_price().to_f64().unwrap_or(0.0) * (risk_free_rate * expiry_years).exp()
    } else {
        contract.mid_price().to_f64().unwrap_or(0.0)
    };
    OptionPricingInput {
        spot,
        forward,
        strike: contract.strike.to_f64().unwrap_or(0.0),
        expiry_years,
        market_price,
        risk_free_rate,
        dividend_yield,
        is_call: contract.right == OptionRight::Call,
    }
}

pub fn price_batch(inputs: &[OptionPricingInput]) -> Vec<OptionPriceModelResult> {
    inputs.iter().copied().map(price_one).collect()
}

fn price_one(input: OptionPricingInput) -> OptionPriceModelResult {
    let Some(sigma) = implied_volatility_from_input(input)
        .or_else(|| lean_newton_implied_volatility_from_input(input))
    else {
        return OptionPriceModelResult::default();
    };
    evaluate_black_scholes_with_iv(input, sigma).unwrap_or_default()
}

fn implied_volatility_from_input(input: OptionPricingInput) -> Option<f64> {
    if input.market_price <= 0.0
        || input.forward <= 0.0
        || input.strike <= 0.0
        || input.expiry_years <= 0.0
    {
        return None;
    }
    ImpliedBlackVolatility::builder()
        .option_price(input.market_price)
        .forward(input.forward)
        .strike(input.strike)
        .expiry(input.expiry_years)
        .is_call(input.is_call)
        .build()?
        .calculate::<DefaultSpecialFn>()
        .filter(|sigma| sigma.is_finite() && *sigma > 0.0)
}

fn lean_newton_implied_volatility_from_input(input: OptionPricingInput) -> Option<f64> {
    if input.market_price <= 0.0
        || input.spot <= 0.0
        || input.forward <= 0.0
        || input.strike <= 0.0
        || input.expiry_years <= 0.0
    {
        return None;
    }

    let discounted_price = input.market_price * (-input.risk_free_rate * input.expiry_years).exp();
    let risk_free_discount = (-input.risk_free_rate * input.expiry_years).exp();
    let mut sigma =
        (2.0 * std::f64::consts::PI / input.expiry_years).sqrt() * discounted_price / input.spot;
    if !sigma.is_finite() || sigma <= 0.0 {
        return None;
    }

    const TOLERANCE: f64 = 1e-3;
    const LOWER_BOUND: f64 = 1e-7;
    const UPPER_BOUND: f64 = 4.0;
    let mut error = f64::MAX;
    let mut iterations_remaining = 10;

    while error > TOLERANCE && iterations_remaining > 0 {
        let old_sigma = sigma;
        let std_dev = old_sigma * input.expiry_years.sqrt();
        let price = black_value(
            input.forward,
            input.strike,
            std_dev,
            risk_free_discount,
            input.is_call,
        );
        let vega = black_vega(
            input.forward,
            input.strike,
            std_dev,
            risk_free_discount,
            input.expiry_years.sqrt(),
        );
        if !price.is_finite() || !vega.is_finite() || vega.abs() <= f64::EPSILON {
            return None;
        }

        sigma -= (price - discounted_price) / vega;
        sigma = sigma.clamp(LOWER_BOUND, UPPER_BOUND);
        error = ((sigma - old_sigma) / sigma).abs();
        iterations_remaining -= 1;
    }

    (iterations_remaining > 0 && sigma.is_finite() && sigma > 0.0).then_some(sigma)
}

fn black_value(forward: f64, strike: f64, std_dev: f64, discount: f64, is_call: bool) -> f64 {
    if std_dev <= 0.0 || forward <= 0.0 || strike <= 0.0 || discount <= 0.0 {
        return 0.0;
    }
    let d1 = (forward / strike).ln() / std_dev + 0.5 * std_dev;
    let d2 = d1 - std_dev;
    if is_call {
        discount * (forward * norm_cdf(d1) - strike * norm_cdf(d2))
    } else {
        discount * (strike * norm_cdf(-d2) - forward * norm_cdf(-d1))
    }
}

fn black_vega(forward: f64, strike: f64, std_dev: f64, discount: f64, sqrt_time: f64) -> f64 {
    if std_dev <= 0.0 || forward <= 0.0 || strike <= 0.0 || discount <= 0.0 {
        return 0.0;
    }
    let d1 = (forward / strike).ln() / std_dev + 0.5 * std_dev;
    discount * forward * norm_pdf(d1) * sqrt_time
}

fn evaluate_black_scholes_with_iv(
    input: OptionPricingInput,
    sigma: f64,
) -> Option<OptionPriceModelResult> {
    let s = input.spot;
    let k = input.strike;
    let t = input.expiry_years;
    let r = input.risk_free_rate;
    let q = input.dividend_yield;
    if t <= 0.0 || s <= 0.0 || k <= 0.0 || sigma <= 0.0 {
        return None;
    }

    let sqrt_t = t.sqrt();
    let d1 = (f64::ln(s / k) + (r - q + 0.5 * sigma * sigma) * t) / (sigma * sqrt_t);
    let d2 = d1 - sigma * sqrt_t;
    let discount_q = f64::exp(-q * t);
    let discount_r = f64::exp(-r * t);
    let pdf_d1 = norm_pdf(d1);

    let price = if input.is_call {
        s * discount_q * norm_cdf(d1) - k * discount_r * norm_cdf(d2)
    } else {
        k * discount_r * norm_cdf(-d2) - s * discount_q * norm_cdf(-d1)
    };

    let delta = if input.is_call {
        discount_q * norm_cdf(d1)
    } else {
        -discount_q * norm_cdf(-d1)
    };

    let gamma = discount_q * pdf_d1 / (s * sigma * sqrt_t);
    let vega = s * discount_q * pdf_d1 * sqrt_t / 100.0;
    let theta = if input.is_call {
        (-s * pdf_d1 * sigma * discount_q / (2.0 * sqrt_t) - r * k * discount_r * norm_cdf(d2)
            + q * s * discount_q * norm_cdf(d1))
            / 365.0
    } else {
        (-s * pdf_d1 * sigma * discount_q / (2.0 * sqrt_t) + r * k * discount_r * norm_cdf(-d2)
            - q * s * discount_q * norm_cdf(-d1))
            / 365.0
    };
    let rho = if input.is_call {
        k * t * discount_r * norm_cdf(d2) / 100.0
    } else {
        -k * t * discount_r * norm_cdf(-d2) / 100.0
    };

    let d = |v: f64| Decimal::from_f64(v).unwrap_or(Decimal::ZERO);
    Some(OptionPriceModelResult {
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
    })
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

/// Compute IV from market price using Peter Jaeckel's Lets Be Rational algorithm.
pub fn implied_volatility(
    market_price: f64,
    s: f64,
    k: f64,
    t: f64,
    r: f64,
    q: f64,
    right: OptionRight,
) -> f64 {
    let forward = if t > 0.0 && s > 0.0 {
        s * ((r - q) * t).exp()
    } else {
        s
    };
    let option_price = if r != 0.0 && t > 0.0 {
        market_price * (r * t).exp()
    } else {
        market_price
    };
    implied_volatility_from_input(OptionPricingInput {
        spot: s,
        forward,
        strike: k,
        expiry_years: t,
        market_price: option_price,
        risk_free_rate: r,
        dividend_yield: q,
        is_call: right == OptionRight::Call,
    })
    .or_else(|| {
        lean_newton_implied_volatility_from_input(OptionPricingInput {
            spot: s,
            forward,
            strike: k,
            expiry_years: t,
            market_price: option_price,
            risk_free_rate: r,
            dividend_yield: q,
            is_call: right == OptionRight::Call,
        })
    })
    .unwrap_or(0.0)
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

    #[test]
    fn implied_volatility_recovers_known_sigma() {
        let s = 100.0;
        let k = 105.0;
        let t = 45.0 / 365.0;
        let sigma = 0.32;
        let price = evaluate_black_scholes_with_iv(
            OptionPricingInput {
                spot: s,
                forward: s,
                strike: k,
                expiry_years: t,
                market_price: 0.0,
                risk_free_rate: 0.0,
                dividend_yield: 0.0,
                is_call: true,
            },
            sigma,
        )
        .unwrap()
        .theoretical_price
        .to_f64()
        .unwrap();

        let recovered = implied_volatility(price, s, k, t, 0.0, 0.0, OptionRight::Call);
        assert!((recovered - sigma).abs() < 1e-4);
    }

    #[test]
    fn batch_pricing_matches_scalar_contract_evaluation() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let expiry = NaiveDate::from_ymd_opt(2024, 2, 16).unwrap();
        let valuation_time = DateTime::from(
            Utc.with_ymd_and_hms(2024, 1, 19, 19, 0, 0)
                .single()
                .unwrap(),
        );
        let symbol = Symbol::create_option_osi(
            underlying,
            dec!(430),
            expiry,
            OptionRight::Put,
            OptionStyle::American,
            &market,
        );
        let mut contract = OptionContract::new(symbol);
        contract.data.underlying_last_price = dec!(450);
        contract.data.bid_price = dec!(3.80);
        contract.data.ask_price = dec!(4.10);

        let input = pricing_input_from_contract(&contract, valuation_time, 0.0, 0.0);
        let batch_result = price_batch(&[input]).remove(0);
        let model = BlackScholesPriceModel;
        let scalar_result =
            evaluate_contract_with_market_iv(&model, &mut contract, valuation_time, 0.0, 0.0);

        assert!(
            (batch_result.implied_volatility - scalar_result.implied_volatility).abs()
                < dec!(0.0000000001)
        );
        assert!(
            (batch_result.theoretical_price - scalar_result.theoretical_price).abs()
                < dec!(0.0000000001)
        );
        assert!(
            (batch_result.greeks.delta - scalar_result.greeks.delta).abs() < dec!(0.0000000001)
        );
        assert!(
            (batch_result.greeks.gamma - scalar_result.greeks.gamma).abs() < dec!(0.0000000001)
        );
    }

    #[test]
    fn batch_pricing_supports_multiple_underlyings() {
        let inputs = [
            OptionPricingInput {
                spot: 100.0,
                forward: 100.0,
                strike: 105.0,
                expiry_years: 30.0 / 365.0,
                market_price: 1.20,
                risk_free_rate: 0.0,
                dividend_yield: 0.0,
                is_call: true,
            },
            OptionPricingInput {
                spot: 45.0,
                forward: 45.0,
                strike: 42.0,
                expiry_years: 60.0 / 365.0,
                market_price: 0.95,
                risk_free_rate: 0.0,
                dividend_yield: 0.0,
                is_call: false,
            },
        ];
        let results = price_batch(&inputs);

        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|result| result.implied_volatility > Decimal::ZERO));
        assert!(results
            .iter()
            .all(|result| result.greeks.gamma > Decimal::ZERO));
    }
}
