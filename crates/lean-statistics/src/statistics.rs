use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatisticsResults {
    pub total_return: Decimal,
    pub annual_return: Decimal,
    pub drawdown: Decimal,
    pub expectancy: Decimal,
    pub net_profit: Decimal,
    pub sharpe_ratio: Decimal,
    pub sortino_ratio: Decimal,
    pub probabilistic_sharpe_ratio: Decimal,
    pub loss_rate: Decimal,
    pub win_rate: Decimal,
    pub profit_loss_ratio: Decimal,
    pub alpha: Decimal,
    pub beta: Decimal,
    pub annual_std: Decimal,
    pub information_ratio: Decimal,
    pub tracking_error: Decimal,
    pub treynor_ratio: Decimal,
    pub total_fees: Decimal,
    pub total_trades: i64,
}

pub struct Statistics;

impl Statistics {
    /// Annualized return from total return and trading days.
    pub fn annual_performance(total_return: Decimal, trading_days: i64) -> Decimal {
        if trading_days <= 0 { return dec!(0); }
        use rust_decimal::prelude::ToPrimitive;
        let total_f = (dec!(1) + total_return).to_f64().unwrap_or(1.0);
        let years = trading_days as f64 / 252.0;
        let annual = total_f.powf(1.0 / years) - 1.0;
        Decimal::from_f64_retain(annual).unwrap_or(dec!(0))
    }

    /// Sharpe ratio from returns and risk-free rate.
    pub fn sharpe_ratio(returns: &[Decimal], risk_free_rate: Decimal) -> Decimal {
        if returns.len() < 2 { return dec!(0); }
        let n = Decimal::from(returns.len());
        let mean = returns.iter().sum::<Decimal>() / n;
        let excess = mean - risk_free_rate;
        let variance = returns.iter()
            .map(|r| (r - mean) * (r - mean))
            .sum::<Decimal>() / (n - dec!(1));

        use rust_decimal::prelude::ToPrimitive;
        let std = variance.to_f64().unwrap_or(0.0).sqrt();
        if std == 0.0 { return dec!(0); }
        let std_dec = Decimal::from_f64_retain(std).unwrap_or(dec!(1));

        (excess / std_dec) * Decimal::from_f64_retain(252_f64.sqrt()).unwrap_or(dec!(1))
    }

    /// Sortino ratio — penalizes only downside deviation.
    pub fn sortino_ratio(returns: &[Decimal], risk_free_rate: Decimal) -> Decimal {
        if returns.len() < 2 { return dec!(0); }
        let n = Decimal::from(returns.len());
        let mean = returns.iter().sum::<Decimal>() / n;
        let excess = mean - risk_free_rate;

        let downside_sq: Decimal = returns.iter()
            .filter(|&&r| r < dec!(0))
            .map(|r| r * r)
            .sum();

        let downside_count = returns.iter().filter(|&&r| r < dec!(0)).count();
        if downside_count == 0 { return dec!(0); }

        use rust_decimal::prelude::ToPrimitive;
        let downside_std = (downside_sq / Decimal::from(downside_count))
            .to_f64().unwrap_or(0.0).sqrt();
        if downside_std == 0.0 { return dec!(0); }

        let std_dec = Decimal::from_f64_retain(downside_std).unwrap_or(dec!(1));
        (excess / std_dec) * Decimal::from_f64_retain(252_f64.sqrt()).unwrap_or(dec!(1))
    }

    /// Maximum drawdown from an equity curve.
    pub fn max_drawdown(equity_curve: &[Decimal]) -> Decimal {
        if equity_curve.is_empty() { return dec!(0); }
        let mut peak = equity_curve[0];
        let mut max_dd = dec!(0);

        for &eq in equity_curve {
            if eq > peak { peak = eq; }
            if peak > dec!(0) {
                let dd = (peak - eq) / peak;
                if dd > max_dd { max_dd = dd; }
            }
        }
        max_dd
    }

    /// Calmar ratio = annual return / max drawdown.
    pub fn calmar_ratio(annual_return: Decimal, max_drawdown: Decimal) -> Decimal {
        if max_drawdown.is_zero() { return dec!(0); }
        annual_return / max_drawdown
    }

    /// Beta of returns vs benchmark returns.
    pub fn beta(returns: &[Decimal], benchmark_returns: &[Decimal]) -> Decimal {
        let n = returns.len().min(benchmark_returns.len());
        if n < 2 { return dec!(1); }

        let n_dec = Decimal::from(n);
        let mean_r = returns[..n].iter().sum::<Decimal>() / n_dec;
        let mean_b = benchmark_returns[..n].iter().sum::<Decimal>() / n_dec;

        let cov: Decimal = returns[..n].iter().zip(benchmark_returns[..n].iter())
            .map(|(r, b)| (r - mean_r) * (b - mean_b))
            .sum::<Decimal>() / (n_dec - dec!(1));

        let var_b: Decimal = benchmark_returns[..n].iter()
            .map(|b| (b - mean_b) * (b - mean_b))
            .sum::<Decimal>() / (n_dec - dec!(1));

        if var_b.is_zero() { dec!(1) } else { cov / var_b }
    }

    /// Alpha = annual_return - (risk_free + beta * (benchmark_annual - risk_free)).
    pub fn alpha(annual_return: Decimal, beta: Decimal, benchmark_annual: Decimal, risk_free: Decimal) -> Decimal {
        annual_return - (risk_free + beta * (benchmark_annual - risk_free))
    }

    /// Annualized tracking error = std(active_returns) * sqrt(252).
    pub fn tracking_error(returns: &[Decimal], benchmark_returns: &[Decimal]) -> Decimal {
        let n = returns.len().min(benchmark_returns.len());
        if n < 2 { return dec!(0); }
        let active: Vec<Decimal> = returns[..n].iter().zip(benchmark_returns[..n].iter())
            .map(|(r, b)| r - b)
            .collect();
        let mean = active.iter().sum::<Decimal>() / Decimal::from(n);
        let var = active.iter().map(|a| (a - mean) * (a - mean)).sum::<Decimal>()
            / Decimal::from(n - 1);
        use rust_decimal::prelude::ToPrimitive;
        let std = var.to_f64().unwrap_or(0.0).sqrt() * 252_f64.sqrt();
        Decimal::from_f64_retain(std).unwrap_or(dec!(0))
    }

    /// Information ratio = annualized active return / tracking error.
    pub fn information_ratio(returns: &[Decimal], benchmark_returns: &[Decimal]) -> Decimal {
        let te = Self::tracking_error(returns, benchmark_returns);
        if te.is_zero() { return dec!(0); }
        let n = returns.len().min(benchmark_returns.len());
        if n < 2 { return dec!(0); }
        let active: Vec<Decimal> = returns[..n].iter().zip(benchmark_returns[..n].iter())
            .map(|(r, b)| r - b)
            .collect();
        let mean_active = active.iter().sum::<Decimal>() / Decimal::from(n);
        use rust_decimal::prelude::ToPrimitive;
        let ann_active = Decimal::from_f64_retain(mean_active.to_f64().unwrap_or(0.0) * 252.0)
            .unwrap_or(dec!(0));
        ann_active / te
    }

    /// Return (skewness, excess_kurtosis) of a daily return series.
    pub fn moments(returns: &[Decimal]) -> (f64, f64) {
        let n = returns.len();
        if n < 4 { return (0.0, 0.0); }
        use rust_decimal::prelude::ToPrimitive;
        let n_f = n as f64;
        let mean = returns.iter().sum::<Decimal>() / Decimal::from(n);
        let mean_f = mean.to_f64().unwrap_or(0.0);
        let returns_f: Vec<f64> = returns.iter().map(|r| r.to_f64().unwrap_or(0.0)).collect();
        let var = returns_f.iter().map(|r| (r - mean_f).powi(2)).sum::<f64>() / (n_f - 1.0);
        let std = var.sqrt();
        if std == 0.0 { return (0.0, 0.0); }
        // Sample skewness (Fisher)
        let skew = returns_f.iter().map(|r| ((r - mean_f) / std).powi(3)).sum::<f64>()
            * n_f / ((n_f - 1.0) * (n_f - 2.0));
        // Excess kurtosis (Fisher), unbiased
        let m4 = returns_f.iter().map(|r| ((r - mean_f) / std).powi(4)).sum::<f64>();
        let kurt = m4 * n_f * (n_f + 1.0) / ((n_f - 1.0) * (n_f - 2.0) * (n_f - 3.0))
            - 3.0 * (n_f - 1.0).powi(2) / ((n_f - 2.0) * (n_f - 3.0));
        (skew, kurt)
    }

    /// Probabilistic Sharpe Ratio — probability that the true SR > `sr_star`.
    ///
    /// Uses the López de Prado (2018) formula with the daily (non-annualized) SR.
    /// `sr_star` should also be expressed as a daily ratio (e.g., 0.0 to test SR > 0).
    pub fn probabilistic_sharpe_ratio(returns: &[Decimal], risk_free_daily: Decimal, sr_star: f64) -> Decimal {
        let n = returns.len();
        if n < 4 { return dec!(0); }
        use rust_decimal::prelude::ToPrimitive;
        let n_f = n as f64;
        let mean = returns.iter().sum::<Decimal>() / Decimal::from(n);
        let excess = mean - risk_free_daily;
        let var = returns.iter().map(|r| (r - mean) * (r - mean)).sum::<Decimal>()
            / Decimal::from(n - 1);
        let std = var.to_f64().unwrap_or(0.0).sqrt();
        if std == 0.0 { return dec!(0); }
        let daily_sr = excess.to_f64().unwrap_or(0.0) / std;
        let (skew, excess_kurt) = Self::moments(returns);
        let denom_sq = 1.0 - skew * daily_sr + (excess_kurt - 1.0) / 4.0 * daily_sr.powi(2);
        if denom_sq <= 0.0 { return dec!(0); }
        let z = (daily_sr - sr_star) * (n_f - 1.0).sqrt() / denom_sq.sqrt();
        let psr = norm_cdf(z);
        Decimal::from_f64_retain(psr).unwrap_or(dec!(0))
    }

    /// Omega ratio: ratio of gains above threshold to losses below threshold.
    pub fn omega_ratio(returns: &[Decimal], threshold: Decimal) -> Decimal {
        if returns.is_empty() { return dec!(0); }
        let gains: Decimal = returns.iter().map(|r| (*r - threshold).max(dec!(0))).sum();
        let losses: Decimal = returns.iter().map(|r| (threshold - *r).max(dec!(0))).sum();
        if losses.is_zero() { return dec!(0); }
        gains / losses
    }

    /// Recovery factor = total net profit / max drawdown in dollars.
    pub fn recovery_factor(total_net_profit: Decimal, max_drawdown_dollars: Decimal) -> Decimal {
        if max_drawdown_dollars.is_zero() { return dec!(0); }
        (total_net_profit / max_drawdown_dollars).abs()
    }
}

/// Standard normal CDF using the Abramowitz & Stegun approximation (max error 1.5×10⁻⁷).
fn norm_cdf(x: f64) -> f64 {
    let a = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * a);
    let poly = t * (0.254829592
        + t * (-0.284496736
        + t * (1.421413741
        + t * (-1.453152027
        + t * 1.061405429))));
    let cdf = 1.0 - poly * (-a * a).exp();
    if x >= 0.0 { cdf } else { 1.0 - cdf }
}
