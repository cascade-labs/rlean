use crate::parameter::OptimizationResult;

pub struct OptimizationReport {
    pub results: Vec<OptimizationResult>,
    pub total_combinations: usize,
    pub best_parameters: Option<crate::parameter::ParameterSet>,
}

impl OptimizationReport {
    pub fn new(results: Vec<OptimizationResult>, total: usize) -> Self {
        let best_parameters = results.first().map(|r| r.parameters.clone());
        Self { results, total_combinations: total, best_parameters }
    }

    pub fn print_summary(&self, top_n: usize) {
        println!("=== Optimization Results ({} combinations) ===", self.total_combinations);
        println!("{:<8} {:<12} {:<12} {:<12} {:<10}", "Rank", "Sharpe", "Return", "MaxDD", "Params");
        for (i, r) in self.results.iter().take(top_n).enumerate() {
            let params_str: String = r
                .parameters
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "{:<8} {:<12.4} {:<12.4} {:<12.4} {}",
                i + 1,
                r.sharpe_ratio,
                r.total_return,
                r.max_drawdown,
                params_str
            );
        }
    }
}
