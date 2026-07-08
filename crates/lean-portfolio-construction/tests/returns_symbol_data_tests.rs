use std::collections::HashMap;

use lean_core::DateTime;
use lean_portfolio_construction::models::returns_symbol_data::{
    form_returns_matrix, ReturnsSymbolData,
};

fn update_series(data: &mut ReturnsSymbolData, prices: &[f64]) {
    for (idx, price) in prices.iter().enumerate() {
        data.update(DateTime::from_secs(idx as i64), *price);
    }
}

#[test]
fn returns_symbol_data_matrix_matches_lean_shape_and_values() {
    // Mirrors LEAN ReturnsSymbolDataTests shape expectations: returns are
    // chronological and the matrix truncates to the shortest available history.
    let mut spy = ReturnsSymbolData::new(1, 3);
    let mut aapl = ReturnsSymbolData::new(1, 2);
    update_series(&mut spy, &[100.0, 110.0, 121.0, 133.1]);
    update_series(&mut aapl, &[200.0, 220.0, 242.0]);
    let data = HashMap::from([(1_u64, spy), (2_u64, aapl)]);

    let matrix = form_returns_matrix(&data, &[1_u64, 2_u64]).unwrap();

    assert_eq!(matrix.len(), 2);
    assert_eq!(matrix[0].len(), 2);
    assert!((matrix[0][0] - 0.1).abs() < 1e-9);
    assert!((matrix[0][1] - 0.1).abs() < 1e-9);
    assert!((matrix[1][0] - 0.1).abs() < 1e-9);
    assert!((matrix[1][1] - 0.1).abs() < 1e-9);
}

#[test]
fn returns_symbol_data_matrix_returns_none_when_any_asset_has_no_history() {
    let mut spy = ReturnsSymbolData::new(1, 3);
    update_series(&mut spy, &[100.0, 110.0]);
    let data = HashMap::from([(1_u64, spy)]);

    assert!(form_returns_matrix(&data, &[1_u64, 2_u64]).is_none());
}
