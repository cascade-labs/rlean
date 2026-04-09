use crate::parameter::OptimizationResult;
use rust_decimal::Decimal;

/// What metric to optimize
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectiveFunction {
    MaximizeSharpeRatio,
    MaximizeTotalReturn,
    MinimizeMaxDrawdown,
    MaximizeCalmarRatio,    // return / max_drawdown
    MaximizeProfitFactor,   // win_rate * avg_win / (loss_rate * avg_loss)
    Custom,
}

impl ObjectiveFunction {
    pub fn evaluate(&self, result: &OptimizationResult) -> Decimal {
        use rust_decimal_macros::dec;
        match self {
            Self::MaximizeSharpeRatio => result.sharpe_ratio,
            Self::MaximizeTotalReturn => result.total_return,
            Self::MinimizeMaxDrawdown => -result.max_drawdown,
            Self::MaximizeCalmarRatio => {
                if result.max_drawdown.is_zero() { dec!(0) }
                else { result.total_return / result.max_drawdown }
            },
            Self::MaximizeProfitFactor => result.objective_value,
            Self::Custom => result.objective_value,
        }
    }
}
