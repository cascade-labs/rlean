use lean_optimization::*;
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use rust_decimal::Decimal;
use std::collections::HashMap;

fn simple_result(params: &ParameterSet, sharpe: f64, ret: f64, dd: f64) -> OptimizationResult {
    OptimizationResult {
        parameters: params.clone(),
        sharpe_ratio: Decimal::from_f64_retain(sharpe).unwrap_or(dec!(0)),
        total_return: Decimal::from_f64_retain(ret).unwrap_or(dec!(0)),
        max_drawdown: Decimal::from_f64_retain(dd).unwrap_or(dec!(0)),
        win_rate: dec!(0.5),
        total_trades: 10,
        objective_value: Decimal::from_f64_retain(sharpe).unwrap_or(dec!(0)),
    }
}

// ---------------------------------------------------------------------------
// ParameterDefinition tests
// ---------------------------------------------------------------------------
mod parameter_definition_tests {
    use super::*;

    #[test]
    fn values_correct_count() {
        // min=1, max=5, step=1 → [1, 2, 3, 4, 5]  (5 values)
        let p = ParameterDefinition::new("x", dec!(1), dec!(5), dec!(1));
        let vals = p.values();
        assert_eq!(vals.len(), 5);
        assert_eq!(vals[0], dec!(1));
        assert_eq!(vals[4], dec!(5));
    }

    #[test]
    fn values_with_decimal_step() {
        // min=0.1, max=0.5, step=0.1 → [0.1, 0.2, 0.3, 0.4, 0.5]
        let p = ParameterDefinition::new("x", dec!(0.1), dec!(0.5), dec!(0.1));
        let vals = p.values();
        assert_eq!(vals.len(), 5);
        assert_eq!(vals[0], dec!(0.1));
        assert_eq!(vals[4], dec!(0.5));
    }

    #[test]
    fn single_value_range() {
        // min == max → exactly one value
        let p = ParameterDefinition::new("x", dec!(7), dec!(7), dec!(1));
        let vals = p.values();
        assert_eq!(vals.len(), 1);
        assert_eq!(vals[0], dec!(7));
    }

    #[test]
    fn values_are_in_ascending_order() {
        let p = ParameterDefinition::new("x", dec!(10), dec!(30), dec!(5));
        let vals = p.values();
        // should be [10, 15, 20, 25, 30]
        assert_eq!(vals.len(), 5);
        for w in vals.windows(2) {
            assert!(w[0] < w[1], "values must be strictly ascending");
        }
    }

    #[test]
    fn step_larger_than_range_yields_single_value() {
        // step bigger than (max - min): only min should be produced
        let p = ParameterDefinition::new("x", dec!(1), dec!(2), dec!(10));
        let vals = p.values();
        assert_eq!(vals.len(), 1);
        assert_eq!(vals[0], dec!(1));
    }
}

// ---------------------------------------------------------------------------
// GridSearchOptimizer tests
// ---------------------------------------------------------------------------
mod grid_search_tests {
    use super::*;

    #[test]
    fn grid_search_all_combinations() {
        // 2 params: A=[1,2], B=[3,4] → 4 combinations
        let params = vec![
            ParameterDefinition::new("A", dec!(1), dec!(2), dec!(1)),
            ParameterDefinition::new("B", dec!(3), dec!(4), dec!(1)),
        ];
        let opt = GridSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio);
        let combos = opt.all_combinations();
        assert_eq!(combos.len(), 4);
    }

    #[test]
    fn grid_search_three_params_cartesian_product() {
        // A=[1,2], B=[1,2], C=[1,2] → 8 combinations
        let params = vec![
            ParameterDefinition::new("A", dec!(1), dec!(2), dec!(1)),
            ParameterDefinition::new("B", dec!(1), dec!(2), dec!(1)),
            ParameterDefinition::new("C", dec!(1), dec!(2), dec!(1)),
        ];
        let opt = GridSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio);
        assert_eq!(opt.all_combinations().len(), 8);
    }

    #[test]
    fn grid_search_results_sorted_best_first() {
        // Results should be sorted by objective_value descending
        let params = vec![ParameterDefinition::new("X", dec!(1), dec!(3), dec!(1))];
        let opt = GridSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio);
        // Assign sharpe values that correspond inversely to X values so ordering is non-trivial
        let results = opt.run(|p| {
            let x: f64 = p["X"].to_f64().unwrap();
            simple_result(p, x, 0.1, 0.05)
        });
        assert!(!results.is_empty());
        for w in results.windows(2) {
            assert!(
                w[0].objective_value >= w[1].objective_value,
                "results must be sorted descending by objective_value"
            );
        }
    }

    #[test]
    fn grid_search_runs_backtest_fn_once_per_combination() {
        // backtest_fn must be called exactly once per combination
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let call_count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&call_count);

        let params = vec![ParameterDefinition::new("X", dec!(1), dec!(3), dec!(1))];
        let opt = GridSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio);
        opt.run(|p| {
            counter.fetch_add(1, Ordering::SeqCst);
            simple_result(p, 1.0, 0.1, 0.05)
        });

        assert_eq!(call_count.load(Ordering::SeqCst), 3); // X=[1,2,3]
    }

    #[test]
    fn grid_search_single_param_single_value() {
        let params = vec![ParameterDefinition::new("X", dec!(5), dec!(5), dec!(1))];
        let opt = GridSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio);
        let combos = opt.all_combinations();
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0]["X"], dec!(5));
    }

    #[test]
    fn grid_search_all_param_values_present_in_combinations() {
        // Every value of A must appear in at least one combination
        let params = vec![
            ParameterDefinition::new("A", dec!(1), dec!(3), dec!(1)),
            ParameterDefinition::new("B", dec!(10), dec!(10), dec!(1)),
        ];
        let opt = GridSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio);
        let combos = opt.all_combinations();
        for expected_a in [dec!(1), dec!(2), dec!(3)] {
            assert!(
                combos.iter().any(|c| c["A"] == expected_a),
                "A={} not found in combinations",
                expected_a
            );
        }
    }

    #[test]
    fn grid_search_objective_value_overwritten_by_run() {
        // run() recomputes objective_value via the objective function
        // Supply a backtest_fn that sets sharpe=0 but objective_value=999 —
        // the optimizer must overwrite with evaluate(result) = sharpe = 0
        let params = vec![ParameterDefinition::new("X", dec!(1), dec!(1), dec!(1))];
        let opt = GridSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio);
        let results = opt.run(|p| OptimizationResult {
            parameters: p.clone(),
            sharpe_ratio: dec!(0),
            total_return: dec!(0),
            max_drawdown: dec!(0),
            win_rate: dec!(0),
            total_trades: 0,
            objective_value: dec!(999), // should be overwritten
        });
        assert_eq!(results[0].objective_value, dec!(0));
    }
}

// ---------------------------------------------------------------------------
// RandomSearchOptimizer tests
// ---------------------------------------------------------------------------
mod random_search_tests {
    use super::*;

    #[test]
    fn random_search_correct_sample_count() {
        let params = vec![ParameterDefinition::new("X", dec!(1), dec!(100), dec!(1))];
        let opt = RandomSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio, 10);
        let samples = opt.sample_parameters();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn random_samples_within_bounds() {
        // All sampled values must lie within [min, max]
        let params = vec![
            ParameterDefinition::new("X", dec!(5), dec!(20), dec!(1)),
            ParameterDefinition::new("Y", dec!(0.1), dec!(0.9), dec!(0.1)),
        ];
        let opt = RandomSearchOptimizer::new(params.clone(), ObjectiveFunction::MaximizeSharpeRatio, 50);
        let samples = opt.sample_parameters();
        for sample in &samples {
            for p in &params {
                let v = sample[&p.name];
                assert!(v >= p.min, "sampled {} < min {}", v, p.min);
                assert!(v <= p.max, "sampled {} > max {}", v, p.max);
            }
        }
    }

    #[test]
    fn random_samples_are_discrete_step_multiples() {
        // Each sampled value must equal min + k*step for some non-negative integer k
        let params = vec![ParameterDefinition::new("X", dec!(2), dec!(10), dec!(2))];
        let valid: Vec<Decimal> = params[0].values();
        let opt = RandomSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio, 20);
        for sample in opt.sample_parameters() {
            assert!(
                valid.contains(&sample["X"]),
                "sampled value {} not in valid set {:?}",
                sample["X"],
                valid
            );
        }
    }

    #[test]
    fn random_search_deterministic_with_same_seed() {
        // Two optimizers with identical seed must produce identical samples
        let params = vec![ParameterDefinition::new("X", dec!(1), dec!(50), dec!(1))];
        let opt1 = RandomSearchOptimizer::new(params.clone(), ObjectiveFunction::MaximizeSharpeRatio, 15);
        let opt2 = RandomSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio, 15);
        assert_eq!(opt1.sample_parameters(), opt2.sample_parameters());
    }

    #[test]
    fn random_search_zero_samples() {
        let params = vec![ParameterDefinition::new("X", dec!(1), dec!(10), dec!(1))];
        let opt = RandomSearchOptimizer::new(params, ObjectiveFunction::MaximizeSharpeRatio, 0);
        assert!(opt.sample_parameters().is_empty());
    }
}

// ---------------------------------------------------------------------------
// WalkForwardOptimizer tests
// ---------------------------------------------------------------------------
mod walk_forward_tests {
    use super::*;
    use chrono::NaiveDate;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn walk_forward_generates_windows() {
        let start = date(2020, 1, 1);
        let end = date(2022, 1, 1);
        let opt = WalkForwardOptimizer::new(start, end, 6, 3); // 6-mo in-sample, 3-mo OOS
        let windows = opt.windows();
        assert!(!windows.is_empty());
    }

    #[test]
    fn walk_forward_in_sample_end_equals_oos_start() {
        // For every window: in_sample_end == out_of_sample_start
        let opt = WalkForwardOptimizer::new(date(2020, 1, 1), date(2023, 1, 1), 6, 3);
        for w in opt.windows() {
            assert_eq!(
                w.in_sample_end, w.out_of_sample_start,
                "in_sample_end must equal out_of_sample_start"
            );
        }
    }

    #[test]
    fn walk_forward_windows_do_not_exceed_end_date() {
        let end = date(2022, 1, 1);
        let opt = WalkForwardOptimizer::new(date(2020, 1, 1), end, 6, 3);
        for w in opt.windows() {
            assert!(
                w.out_of_sample_end <= end,
                "oos_end {} exceeds backtest_end {}",
                w.out_of_sample_end,
                end
            );
        }
    }

    #[test]
    fn walk_forward_in_sample_duration() {
        use chrono::Datelike;
        let opt = WalkForwardOptimizer::new(date(2020, 1, 1), date(2023, 1, 1), 6, 3);
        for w in opt.windows() {
            // in-sample span must be approximately 6 months (within a day of month boundary)
            let months_span = (w.in_sample_end.year() - w.in_sample_start.year()) * 12
                + w.in_sample_end.month() as i32
                - w.in_sample_start.month() as i32;
            assert_eq!(months_span, 6, "in-sample window should span 6 months");
        }
    }

    #[test]
    fn walk_forward_oos_duration() {
        use chrono::Datelike;
        let opt = WalkForwardOptimizer::new(date(2020, 1, 1), date(2023, 1, 1), 6, 3);
        for w in opt.windows() {
            let months_span = (w.out_of_sample_end.year() - w.out_of_sample_start.year()) * 12
                + w.out_of_sample_end.month() as i32
                - w.out_of_sample_start.month() as i32;
            assert_eq!(months_span, 3, "out-of-sample window should span 3 months");
        }
    }

    #[test]
    fn walk_forward_window_starts_advance_by_oos_months() {
        // Each successive window's in_sample_start should advance by out_of_sample_months
        use chrono::Datelike;
        let opt = WalkForwardOptimizer::new(date(2020, 1, 1), date(2023, 1, 1), 6, 3);
        let windows = opt.windows();
        for pair in windows.windows(2) {
            let prev = &pair[0];
            let next = &pair[1];
            let advance_months = (next.in_sample_start.year() - prev.in_sample_start.year()) * 12
                + next.in_sample_start.month() as i32
                - prev.in_sample_start.month() as i32;
            assert_eq!(advance_months, 3, "window start should advance by oos_months=3");
        }
    }

    #[test]
    fn walk_forward_range_too_short_yields_empty() {
        // If the range is shorter than in_sample + oos, no windows should be generated
        let opt = WalkForwardOptimizer::new(date(2020, 1, 1), date(2020, 6, 1), 6, 3);
        assert!(opt.windows().is_empty());
    }
}

// ---------------------------------------------------------------------------
// ObjectiveFunction tests
// ---------------------------------------------------------------------------
mod objective_function_tests {
    use super::*;

    fn make_result(sharpe: f64, ret: f64, dd: f64) -> OptimizationResult {
        simple_result(&HashMap::new(), sharpe, ret, dd)
    }

    #[test]
    fn maximize_sharpe_returns_sharpe_ratio() {
        let r = make_result(1.5, 0.2, 0.1);
        assert_eq!(
            ObjectiveFunction::MaximizeSharpeRatio.evaluate(&r),
            r.sharpe_ratio
        );
    }

    #[test]
    fn maximize_total_return_returns_total_return() {
        let r = make_result(1.0, 0.35, 0.1);
        assert_eq!(
            ObjectiveFunction::MaximizeTotalReturn.evaluate(&r),
            r.total_return
        );
    }

    #[test]
    fn minimize_drawdown_negates_max_drawdown() {
        let r = make_result(1.0, 0.1, 0.3);
        let score = ObjectiveFunction::MinimizeMaxDrawdown.evaluate(&r);
        assert!(score < dec!(0), "MinimizeMaxDrawdown score must be negative");
        assert_eq!(score, -r.max_drawdown);
    }

    #[test]
    fn calmar_ratio_return_over_drawdown() {
        // calmar = total_return / max_drawdown
        let r = make_result(1.0, 0.6, 0.2);
        let calmar = ObjectiveFunction::MaximizeCalmarRatio.evaluate(&r);
        // 0.6 / 0.2 = 3.0
        assert_eq!(calmar, r.total_return / r.max_drawdown);
    }

    #[test]
    fn calmar_ratio_zero_drawdown_returns_zero() {
        // Guard against division by zero: if drawdown is 0, calmar should be 0
        let r = make_result(1.0, 0.5, 0.0);
        let calmar = ObjectiveFunction::MaximizeCalmarRatio.evaluate(&r);
        assert_eq!(calmar, dec!(0));
    }

    #[test]
    fn higher_sharpe_gives_higher_score() {
        let r1 = make_result(2.0, 0.1, 0.05);
        let r2 = make_result(0.5, 0.1, 0.05);
        let obj = ObjectiveFunction::MaximizeSharpeRatio;
        assert!(obj.evaluate(&r1) > obj.evaluate(&r2));
    }

    #[test]
    fn lower_drawdown_gives_higher_minimize_score() {
        let r_low_dd = make_result(1.0, 0.1, 0.05);
        let r_high_dd = make_result(1.0, 0.1, 0.30);
        let obj = ObjectiveFunction::MinimizeMaxDrawdown;
        assert!(obj.evaluate(&r_low_dd) > obj.evaluate(&r_high_dd));
    }
}

// ---------------------------------------------------------------------------
// OptimizationReport tests
// ---------------------------------------------------------------------------
mod optimization_report_tests {
    use super::*;

    fn make_results(sharpes: &[f64]) -> Vec<OptimizationResult> {
        sharpes
            .iter()
            .map(|&s| {
                let mut r = simple_result(&HashMap::new(), s, 0.1, 0.05);
                r.objective_value = Decimal::from_f64_retain(s).unwrap_or(dec!(0));
                r
            })
            .collect()
    }

    #[test]
    fn report_best_parameters_is_first_result() {
        let results = make_results(&[1.5, 1.0, 0.5]);
        let first_params = results[0].parameters.clone();
        let report = OptimizationReport::new(results, 3);
        assert_eq!(report.best_parameters, Some(first_params));
    }

    #[test]
    fn report_empty_results_best_parameters_none() {
        let report = OptimizationReport::new(vec![], 0);
        assert!(report.best_parameters.is_none());
    }

    #[test]
    fn report_total_combinations_correct() {
        let results = make_results(&[1.0, 0.5]);
        let report = OptimizationReport::new(results, 42);
        assert_eq!(report.total_combinations, 42);
    }

    #[test]
    fn report_results_count_matches_input() {
        let results = make_results(&[1.0, 2.0, 3.0]);
        let count = results.len();
        let report = OptimizationReport::new(results, count);
        assert_eq!(report.results.len(), 3);
    }
}
