use crate::parameter::{ParameterDefinition, ParameterSet, OptimizationResult};
use crate::objective::ObjectiveFunction;

pub struct RandomSearchOptimizer {
    pub parameters: Vec<ParameterDefinition>,
    pub objective: ObjectiveFunction,
    pub n_samples: usize,
    pub seed: u64,
}

impl RandomSearchOptimizer {
    pub fn new(
        parameters: Vec<ParameterDefinition>,
        objective: ObjectiveFunction,
        n_samples: usize,
    ) -> Self {
        Self { parameters, objective, n_samples, seed: 42 }
    }

    pub fn sample_parameters(&self) -> Vec<ParameterSet> {
        // Simple LCG pseudo-random generator (no external deps)
        let mut rng_state = self.seed;
        let lcg_next = |state: &mut u64| -> u64 {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state >> 33
        };

        (0..self.n_samples)
            .map(|_| {
                let mut params = ParameterSet::new();
                for param in &self.parameters {
                    let values = param.values();
                    if values.is_empty() {
                        continue;
                    }
                    let idx = (lcg_next(&mut rng_state) as usize) % values.len();
                    params.insert(param.name.clone(), values[idx]);
                }
                params
            })
            .collect()
    }

    pub fn run<F>(&self, mut backtest_fn: F) -> Vec<OptimizationResult>
    where
        F: FnMut(&ParameterSet) -> OptimizationResult,
    {
        let samples = self.sample_parameters();
        let total = samples.len();
        let mut results: Vec<OptimizationResult> = samples
            .iter()
            .enumerate()
            .map(|(i, params)| {
                tracing::info!("Random sample {}/{}: {:?}", i + 1, total, params);
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
