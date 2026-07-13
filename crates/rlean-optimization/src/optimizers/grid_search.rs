use crate::objective::ObjectiveFunction;
use crate::parameter::{OptimizationResult, ParameterDefinition, ParameterSet};

pub struct GridSearchOptimizer {
    pub parameters: Vec<ParameterDefinition>,
    pub objective: ObjectiveFunction,
    pub max_concurrent: usize, // max parallel backtests
}

impl GridSearchOptimizer {
    pub fn new(parameters: Vec<ParameterDefinition>, objective: ObjectiveFunction) -> Self {
        Self {
            parameters,
            objective,
            max_concurrent: 1,
        }
    }

    /// Generate all parameter combinations
    pub fn all_combinations(&self) -> Vec<ParameterSet> {
        // Cartesian product of all parameter value ranges
        let mut result: Vec<ParameterSet> = vec![Default::default()];
        for param in &self.parameters {
            let values = param.values();
            let mut new_result = Vec::new();
            for existing in &result {
                for &val in &values {
                    let mut combo = existing.clone();
                    combo.insert(param.name.clone(), val);
                    new_result.push(combo);
                }
            }
            result = new_result;
        }
        result
    }

    /// Run all combinations and return sorted results (best first)
    pub fn run<F>(&self, mut backtest_fn: F) -> Vec<OptimizationResult>
    where
        F: FnMut(&ParameterSet) -> OptimizationResult,
    {
        let combos = self.all_combinations();
        let total = combos.len();
        let mut results: Vec<OptimizationResult> = combos
            .iter()
            .enumerate()
            .map(|(i, params)| {
                tracing::info!("Running backtest {}/{}: {:?}", i + 1, total, params);
                let mut r = backtest_fn(params);
                r.objective_value = self.objective.evaluate(&r);
                r
            })
            .collect();
        results.sort_by(|a, b| {
            b.objective_value
                .partial_cmp(&a.objective_value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }
}
