/// Minimal dense matrix math using Vec<Vec<f64>> (row-major).
/// All matrices are represented as Vec<Vec<f64>> where mat[i] is row i.
/// Used internally by Black-Litterman and Risk-Parity implementations.

/// Create an n×n identity matrix.
pub fn identity(n: usize) -> Vec<Vec<f64>> {
    let mut m = vec![vec![0.0; n]; n];
    for i in 0..n {
        m[i][i] = 1.0;
    }
    m
}

/// Matrix multiply: A (r×k) × B (k×c) → C (r×c).
pub fn mat_mul(a: &[Vec<f64>], b: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let r = a.len();
    let k = b.len();
    let c = if k == 0 { 0 } else { b[0].len() };
    let mut out = vec![vec![0.0; c]; r];
    for i in 0..r {
        for j in 0..c {
            let mut s = 0.0;
            for l in 0..k {
                s += a[i][l] * b[l][j];
            }
            out[i][j] = s;
        }
    }
    out
}

/// Transpose of matrix A (r×c) → (c×r).
pub fn transpose(a: &[Vec<f64>]) -> Vec<Vec<f64>> {
    if a.is_empty() {
        return vec![];
    }
    let r = a.len();
    let c = a[0].len();
    let mut out = vec![vec![0.0; r]; c];
    for i in 0..r {
        for j in 0..c {
            out[j][i] = a[i][j];
        }
    }
    out
}

/// Add two matrices element-wise.
pub fn mat_add(a: &[Vec<f64>], b: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let r = a.len();
    let c = if r == 0 { 0 } else { a[0].len() };
    let mut out = vec![vec![0.0; c]; r];
    for i in 0..r {
        for j in 0..c {
            out[i][j] = a[i][j] + b[i][j];
        }
    }
    out
}

/// Subtract two matrices element-wise: A - B.
pub fn mat_sub(a: &[Vec<f64>], b: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let r = a.len();
    let c = if r == 0 { 0 } else { a[0].len() };
    let mut out = vec![vec![0.0; c]; r];
    for i in 0..r {
        for j in 0..c {
            out[i][j] = a[i][j] - b[i][j];
        }
    }
    out
}

/// Scale matrix by scalar.
pub fn mat_scale(a: &[Vec<f64>], s: f64) -> Vec<Vec<f64>> {
    a.iter()
        .map(|row| row.iter().map(|x| x * s).collect())
        .collect()
}

/// Matrix-vector product: A (r×c) × v (c) → w (r).
pub fn mat_vec_mul(a: &[Vec<f64>], v: &[f64]) -> Vec<f64> {
    a.iter()
        .map(|row| row.iter().zip(v.iter()).map(|(a, b)| a * b).sum())
        .collect()
}

/// Invert an n×n matrix using Gauss-Jordan elimination.
/// Returns None if the matrix is singular (|det| < 1e-14).
pub fn mat_inv(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    // Build augmented matrix [A | I]
    let mut aug: Vec<Vec<f64>> = (0..n)
        .map(|i| {
            let mut row = a[i].clone();
            let mut eye_row = vec![0.0; n];
            eye_row[i] = 1.0;
            row.extend(eye_row);
            row
        })
        .collect();

    for col in 0..n {
        // Find pivot (max absolute value in column)
        let pivot_row = (col..n)
            .max_by(|&i, &j| aug[i][col].abs().partial_cmp(&aug[j][col].abs()).unwrap())?;

        if aug[pivot_row][col].abs() < 1e-14 {
            return None; // singular
        }

        aug.swap(col, pivot_row);

        let pivot = aug[col][col];
        for j in 0..2 * n {
            aug[col][j] /= pivot;
        }

        for row in 0..n {
            if row != col {
                let factor = aug[row][col];
                for j in 0..2 * n {
                    let v = aug[col][j];
                    aug[row][j] -= factor * v;
                }
            }
        }
    }

    // Extract right half → inverse
    Some(
        aug.into_iter()
            .map(|row| row[n..].to_vec())
            .collect(),
    )
}

/// Compute the sample covariance matrix from a returns matrix.
/// `returns` is a slice of rows, each row is a snapshot of per-asset returns.
/// Returns an N×N covariance matrix where N = number of assets.
/// Uses ddof=1 (unbiased estimator).
pub fn covariance_matrix(returns: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let t = returns.len();
    if t < 2 {
        let n = if t == 0 { 0 } else { returns[0].len() };
        return vec![vec![0.0; n]; n];
    }
    let n = returns[0].len();

    // Compute column means
    let means: Vec<f64> = (0..n)
        .map(|j| returns.iter().map(|row| row[j]).sum::<f64>() / t as f64)
        .collect();

    // Compute covariance
    let mut cov = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in i..n {
            let s: f64 = returns
                .iter()
                .map(|row| (row[i] - means[i]) * (row[j] - means[j]))
                .sum();
            let v = s / (t - 1) as f64;
            cov[i][j] = v;
            cov[j][i] = v;
        }
    }
    cov
}

/// Dot product of two vectors.
pub fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Element-wise vector addition.
pub fn vec_add(a: &[f64], b: &[f64]) -> Vec<f64> {
    a.iter().zip(b.iter()).map(|(x, y)| x + y).collect()
}

/// Element-wise vector subtraction.
pub fn vec_sub(a: &[f64], b: &[f64]) -> Vec<f64> {
    a.iter().zip(b.iter()).map(|(x, y)| x - y).collect()
}

/// Scale a vector by scalar.
pub fn vec_scale(a: &[f64], s: f64) -> Vec<f64> {
    a.iter().map(|x| x * s).collect()
}

/// Build a diagonal matrix from a vector.
pub fn diag(v: &[f64]) -> Vec<Vec<f64>> {
    let n = v.len();
    let mut m = vec![vec![0.0; n]; n];
    for i in 0..n {
        m[i][i] = v[i];
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mat_mul_identity() {
        let i2 = identity(2);
        let a = vec![vec![1.0, 2.0], vec![3.0, 4.0]];
        let result = mat_mul(&a, &i2);
        assert!((result[0][0] - 1.0).abs() < 1e-10);
        assert!((result[1][1] - 4.0).abs() < 1e-10);
    }

    #[test]
    fn test_mat_inv_2x2() {
        let a = vec![vec![4.0, 7.0], vec![2.0, 6.0]];
        let inv = mat_inv(&a).unwrap();
        let prod = mat_mul(&a, &inv);
        for i in 0..2 {
            for j in 0..2 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!((prod[i][j] - expected).abs() < 1e-10);
            }
        }
    }

    #[test]
    fn test_covariance_matrix() {
        // Simple: two perfectly correlated assets
        let returns = vec![
            vec![0.01, 0.02],
            vec![0.02, 0.04],
            vec![0.03, 0.06],
        ];
        let cov = covariance_matrix(&returns);
        assert!(cov[0][0] > 0.0);
        // cov[0][1] should equal cov[1][0]
        assert!((cov[0][1] - cov[1][0]).abs() < 1e-14);
        // Correlation = 1: cov[0][1]^2 = cov[0][0] * cov[1][1]
        let corr_sq = cov[0][1] * cov[0][1];
        let var_prod = cov[0][0] * cov[1][1];
        assert!((corr_sq - var_prod).abs() < 1e-12);
    }
}
