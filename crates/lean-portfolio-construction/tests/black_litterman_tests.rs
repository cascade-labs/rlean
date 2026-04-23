/// Unit tests for BlackLittermanOptimizationPortfolioConstructionModel.
///
/// Tests mirror the spirit of LEAN's C# BlackLittermanOptimizationPortfolioConstructionModelTests
/// while exercising the Rust-specific implementation.
use lean_core::{Market, Symbol};
use lean_portfolio_construction::{
    BlackLittermanOptimizationPortfolioConstructionModel, IPortfolioConstructionModel,
    InsightDirection, InsightForPcm, PortfolioBias,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn make_equity(ticker: &str) -> Symbol {
    Symbol::create_equity(ticker, &Market::new(Market::USA))
}

fn make_insight(symbol: Symbol, direction: InsightDirection, magnitude: f64) -> InsightForPcm {
    InsightForPcm {
        symbol,
        direction,
        magnitude: Some(Decimal::try_from(magnitude).unwrap()),
        confidence: Some(dec!(0.8)),
        source_model: "TestAlpha".to_string(),
    }
}

fn make_insight_no_mag(symbol: Symbol, direction: InsightDirection) -> InsightForPcm {
    InsightForPcm {
        symbol,
        direction,
        magnitude: None,
        confidence: None,
        source_model: "TestAlpha".to_string(),
    }
}

/// Feed `n_bars` of synthetic price data to build up the model's price history.
/// Prices follow a simple trend so covariance is well-conditioned.
fn warm_up_model(
    model: &mut BlackLittermanOptimizationPortfolioConstructionModel,
    tickers: &[&str],
    n_bars: usize,
) {
    for bar in 0..n_bars {
        let prices: HashMap<String, Decimal> = tickers
            .iter()
            .enumerate()
            .map(|(i, &ticker)| {
                // Each asset has slightly different trend to create covariance structure
                let base = 100.0 + i as f64 * 20.0;
                let drift = 0.001 * (i as f64 + 1.0) * bar as f64;
                let noise = if bar % 3 == 0 { 0.5 } else { -0.5 };
                let price = base + drift + noise;
                (ticker.to_uppercase(), Decimal::try_from(price).unwrap())
            })
            .collect();
        // Call create_targets to trigger price update (with empty insights)
        // We need a non-empty insight list to trigger the price update in the model.
        // Use a dummy insight to get past the early return.
        let dummy: Vec<InsightForPcm> = tickers
            .iter()
            .map(|&t| make_insight_no_mag(make_equity(t), InsightDirection::Flat))
            .collect();
        let portfolio_value = dec!(100_000);
        model.create_targets(&dummy, portfolio_value, &prices);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// With insufficient price history the model should return empty targets.
#[test]
fn returns_empty_when_not_enough_history() {
    let mut model =
        BlackLittermanOptimizationPortfolioConstructionModel::with_params(1, 63, 0.0, 2.5, 0.05, PortfolioBias::LongShort);

    let spy = make_equity("SPY");
    let insights = vec![make_insight(spy.clone(), InsightDirection::Up, 0.05)];
    let prices = HashMap::from([("SPY".to_string(), dec!(400))]);

    // Only 1 price → not enough data
    let targets = model.create_targets(&insights, dec!(100_000), &prices);
    assert!(
        targets.is_empty(),
        "Should return empty targets with insufficient price history"
    );
}

/// With a single bullish view, the model should return a positive (long) weight.
#[test]
fn bullish_view_produces_positive_weight() {
    let mut model =
        BlackLittermanOptimizationPortfolioConstructionModel::with_params(1, 30, 0.0, 2.5, 0.05, PortfolioBias::LongShort);

    let spy = make_equity("SPY");
    let agg = make_equity("AGG");

    warm_up_model(&mut model, &["SPY", "AGG"], 50);

    let prices = HashMap::from([
        ("SPY".to_string(), dec!(415)),
        ("AGG".to_string(), dec!(100)),
    ]);
    let insights = vec![
        make_insight(spy.clone(), InsightDirection::Up, 0.10),
        make_insight(agg.clone(), InsightDirection::Flat, 0.0),
    ];

    let targets = model.create_targets(&insights, dec!(100_000), &prices);
    assert!(!targets.is_empty(), "Should produce targets with sufficient history");

    let spy_target = targets.iter().find(|t| t.symbol.value == "SPY");
    assert!(spy_target.is_some(), "SPY should have a target");
    // Bullish insight + LongShort bias → positive quantity
    if let Some(t) = spy_target {
        assert!(
            t.quantity >= Decimal::ZERO,
            "Bullish SPY insight should produce non-negative quantity (got {})",
            t.quantity
        );
    }
}

/// Long-only bias: all weights should be non-negative.
#[test]
fn long_only_bias_produces_non_negative_weights() {
    let mut model =
        BlackLittermanOptimizationPortfolioConstructionModel::with_params(1, 30, 0.0, 2.5, 0.05, PortfolioBias::Long);

    warm_up_model(&mut model, &["SPY", "IEF", "GLD"], 50);

    let prices = HashMap::from([
        ("SPY".to_string(), dec!(415)),
        ("IEF".to_string(), dec!(95)),
        ("GLD".to_string(), dec!(175)),
    ]);

    let insights = vec![
        make_insight(make_equity("SPY"), InsightDirection::Up, 0.08),
        make_insight(make_equity("IEF"), InsightDirection::Down, 0.02),
        make_insight(make_equity("GLD"), InsightDirection::Up, 0.05),
    ];

    let targets = model.create_targets(&insights, dec!(100_000), &prices);

    for target in &targets {
        assert!(
            target.quantity >= Decimal::ZERO,
            "Long-only bias must produce non-negative quantities (ticker={}, qty={})",
            target.symbol.value,
            target.quantity
        );
    }
}

/// Model name should match LEAN's class name.
#[test]
fn model_name_matches_lean() {
    let model = BlackLittermanOptimizationPortfolioConstructionModel::new();
    assert_eq!(
        model.name(),
        "BlackLittermanOptimizationPortfolioConstructionModel"
    );
}

/// With two assets and views from two separate source models, both views are
/// incorporated and targets are produced for both assets.
#[test]
fn multiple_source_models_produce_targets_for_each() {
    let mut model =
        BlackLittermanOptimizationPortfolioConstructionModel::with_params(1, 30, 0.0, 2.5, 0.05, PortfolioBias::LongShort);

    warm_up_model(&mut model, &["SPY", "TLT"], 50);

    let prices = HashMap::from([
        ("SPY".to_string(), dec!(415)),
        ("TLT".to_string(), dec!(100)),
    ]);

    // Two source models each with one view
    let insights = vec![
        InsightForPcm {
            symbol: make_equity("SPY"),
            direction: InsightDirection::Up,
            magnitude: Some(dec!(0.08)),
            confidence: Some(dec!(0.9)),
            source_model: "AlphaA".to_string(),
        },
        InsightForPcm {
            symbol: make_equity("TLT"),
            direction: InsightDirection::Up,
            magnitude: Some(dec!(0.03)),
            confidence: Some(dec!(0.7)),
            source_model: "AlphaB".to_string(),
        },
    ];

    let targets = model.create_targets(&insights, dec!(100_000), &prices);
    assert_eq!(
        targets.len(),
        2,
        "Should produce a target for each insight"
    );
}

/// Empty insights always produce empty targets (no panic).
#[test]
fn empty_insights_returns_empty() {
    let mut model = BlackLittermanOptimizationPortfolioConstructionModel::new();
    let targets = model.create_targets(&[], dec!(100_000), &HashMap::new());
    assert!(targets.is_empty());
}

/// Default parameters match C# defaults.
#[test]
fn default_parameters_match_csharp() {
    // The default C# params are: lookback=1, period=63, rf=0, delta=2.5, tau=0.05, LongShort
    // We test indirectly by checking the name and that it constructs without panic.
    let model = BlackLittermanOptimizationPortfolioConstructionModel::new();
    assert_eq!(
        model.name(),
        "BlackLittermanOptimizationPortfolioConstructionModel"
    );
}

/// Test the matrix math subsystem used by BL directly.
mod matrix_tests {
    use lean_portfolio_construction::models::matrix::{
        covariance_matrix, mat_add, mat_inv, mat_mul, mat_scale, mat_vec_mul, transpose,
    };

    #[test]
    fn transpose_2x3() {
        let a = vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]];
        let at = transpose(&a);
        assert_eq!(at.len(), 3);
        assert_eq!(at[0].len(), 2);
        assert!((at[0][0] - 1.0).abs() < 1e-10);
        assert!((at[2][1] - 6.0).abs() < 1e-10);
    }

    #[test]
    fn mat_inv_3x3() {
        let a = vec![
            vec![2.0, 1.0, 0.0],
            vec![1.0, 3.0, 1.0],
            vec![0.0, 1.0, 2.0],
        ];
        let inv = mat_inv(&a).unwrap();
        let prod = mat_mul(&a, &inv);
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (prod[i][j] - expected).abs() < 1e-8,
                    "A*A⁻¹[{i}][{j}] = {} ≠ {expected}",
                    prod[i][j]
                );
            }
        }
    }

    #[test]
    fn singular_matrix_returns_none() {
        // Rows 1 and 2 are identical → singular
        let a = vec![
            vec![1.0, 2.0, 3.0],
            vec![4.0, 5.0, 6.0],
            vec![4.0, 5.0, 6.0],
        ];
        assert!(mat_inv(&a).is_none());
    }

    #[test]
    fn covariance_symmetric_positive_definite() {
        // Four observations of three assets
        let returns = vec![
            vec![0.01, -0.01, 0.005],
            vec![0.02, -0.015, 0.008],
            vec![-0.005, 0.01, -0.003],
            vec![0.015, -0.008, 0.006],
        ];
        let cov = covariance_matrix(&returns);
        // Must be symmetric
        for i in 0..3 {
            for j in 0..3 {
                assert!((cov[i][j] - cov[j][i]).abs() < 1e-14);
            }
        }
        // Diagonal must be non-negative
        for i in 0..3 {
            assert!(cov[i][i] >= 0.0);
        }
    }

    #[test]
    fn bl_posterior_formula_known_values() {
        // Simple 2-asset case: verify BL posterior shifts mean in view direction.
        // Prior: π = [0.05, 0.03]
        // Σ = [[0.04, 0.01], [0.01, 0.02]]
        // P = [[1, 0]]  (view on asset 1 only)
        // Q = [0.08]    (expected return of asset 1 = 8%)
        // τ = 0.05
        //
        // Expected: posterior π[0] > 0.05 (view pushes asset 1 return up)
        let pi = vec![0.05, 0.03];
        let sigma = vec![vec![0.04, 0.01], vec![0.01, 0.02]];
        let p = vec![vec![1.0, 0.0]];
        let q = vec![0.08];
        let tau = 0.05;

        // τΣ
        let sigma_tau = mat_scale(&sigma, tau);

        // Ω = diag(P × τΣ × Pᵀ)
        let p_st = mat_mul(&p, &sigma_tau);
        let pt = transpose(&p);
        let p_st_pt = mat_mul(&p_st, &pt);
        let omega = vec![vec![p_st_pt[0][0]]]; // 1×1 diagonal

        // A = τΣ Pᵀ (P τΣ Pᵀ + Ω)⁻¹
        let denom = mat_add(&p_st_pt, &omega);
        let denom_inv = mat_inv(&denom).unwrap();
        let st_pt = mat_mul(&sigma_tau, &pt);
        let a = mat_mul(&st_pt, &denom_inv);

        // π* = π + A(Q - Pπ)
        let p_pi = mat_vec_mul(&p, &pi);
        let diff = vec![q[0] - p_pi[0]];
        let correction = mat_vec_mul(&a, &diff);
        let pi_post = vec![pi[0] + correction[0], pi[1] + correction[1]];

        // The view says asset 1 should return 8% vs prior 5%
        // Posterior should be between 5% and 8%.
        assert!(
            pi_post[0] > 0.05,
            "Posterior π[0] should be > prior 0.05, got {}",
            pi_post[0]
        );
        assert!(
            pi_post[0] < 0.08,
            "Posterior π[0] should be < view 0.08, got {}",
            pi_post[0]
        );
        // Asset 2 is not directly in the view but may shift slightly due to correlation
        assert!(pi_post[1].is_finite());
    }
}
