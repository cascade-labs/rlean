use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single parameter definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterDefinition {
    pub name: String,
    pub min: Decimal,
    pub max: Decimal,
    pub step: Decimal,
}

impl ParameterDefinition {
    pub fn new(name: &str, min: Decimal, max: Decimal, step: Decimal) -> Self {
        Self { name: name.to_string(), min, max, step }
    }
    /// All discrete values in this parameter's range
    pub fn values(&self) -> Vec<Decimal> {
        let mut v = Vec::new();
        let mut cur = self.min;
        while cur <= self.max {
            v.push(cur);
            cur += self.step;
        }
        v
    }
}

/// A concrete parameter set (one point in the parameter space)
pub type ParameterSet = HashMap<String, Decimal>;

/// Result of one backtest run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationResult {
    pub parameters: ParameterSet,
    pub sharpe_ratio: Decimal,
    pub total_return: Decimal,
    pub max_drawdown: Decimal,
    pub win_rate: Decimal,
    pub total_trades: usize,
    pub objective_value: Decimal, // the value being maximized
}
