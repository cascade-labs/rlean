//! Alpha performance analytics: per-alpha Information Coefficient (IC),
//! cross-alpha signal correlation, and a keep/exclude spanning-set ranking.
//!
//! Everything is computed inside the Rust framework from the stream of closed
//! insights. Each insight carries the security price at emission
//! (`reference_value`) and at close (`reference_value_final`), so its realised
//! forward return is `(final - ref) / ref`. Grouping these by `source_model`
//! and emission time reconstructs each alpha's cross-sectional view together
//! with its realised payoff — exactly what the offline `alpha_audit.py`
//! measured, now native to the engine and emitted into the backtest results.

use crate::insight::{Insight, InsightDirection};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Trailing window (in rebalance periods) for the rolling IC line.
const ROLLING_WINDOW: usize = 6;
/// Minimum number of scored periods before an alpha gets summary stats / ranking.
const MIN_PERIODS: usize = 5;
/// Minimum overlapping observations to report a pairwise signal correlation.
const MIN_CORR_OVERLAP: usize = 30;
/// Max pairwise |correlation| an alpha may have with an already-kept alpha
/// before it is treated as a genuine duplicate. Set at the literature
/// "same-signal" line (~0.85): below it, two signals still carry meaningful
/// orthogonal information and the correlation-aware PCM downweights rather
/// than the audit excluding. (alpha_audit.py used 0.85 for redundant clusters.)
const MAX_CORR: f64 = 0.85;
/// Confidence FLOOR on an alpha's own IC: below this |t-stat| the signal is treated
/// as noise and excluded regardless of partial IC (a near-zero-edge signal that looks
/// orthogonal must not be admitted on noise alone).
const IC_T_STAT_FLOOR: f64 = 1.0;
/// Minimum |mean IC| magnitude floor — reject economically negligible edges.
const MIN_IC: f64 = 0.01;
/// KEEP criterion (Grinold-Kahn): an above-floor alpha is kept iff its PARTIAL IC —
/// its mean IC after projecting out the already-kept set through the signal-correlation
/// structure, i.e. the orthogonalized IC that is the increment to the blended IR^2 —
/// clears this floor. This is the additive-information test: a redundant signal
/// (high corr, no extra IC) has partial IC ~0 and is dropped; an orthogonal signal
/// (even a modest standalone IC) keeps most of its IC and is kept for breadth. No
/// portfolio re-runs: computed from the IC vector + correlation matrix in one pass.
const MIN_PARTIAL_IC: f64 = 0.01;
/// Ridge added to the kept-set correlation submatrix diagonal so the SPD solve is
/// stable when kept signals are themselves correlated (near-singular C_SS).
const PARTIAL_IC_RIDGE: f64 = 1e-6;

// ─── Output model (serialised into the backtest results JSON) ──────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlphaIcPoint {
    pub date: String,
    pub ic: f64,
}

/// One alpha's rolling IC over the backtest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlphaIcSeries {
    pub name: String,
    pub points: Vec<AlphaIcPoint>,
}

/// Pairwise signal-correlation matrix over the registered alphas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlphaCorrelationMatrix {
    pub names: Vec<String>,
    /// Row-major NxN; `None` where overlap is insufficient (diagonal = 1.0).
    pub matrix: Vec<Vec<Option<f64>>>,
}

/// One row of the keep/exclude ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlphaRanking {
    pub name: String,
    pub mean_ic: f64,
    pub ic_ir: f64,
    pub t_stat: f64,
    pub hit_rate: f64,
    pub n_periods: usize,
    pub keep: bool,
    pub reason: String,
}

/// Full alpha-framework diagnostics bundle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlphaAnalytics {
    pub ic_series: Vec<AlphaIcSeries>,
    pub correlation: AlphaCorrelationMatrix,
    pub ranking: Vec<AlphaRanking>,
    /// Max |correlation| criterion used for the keep/exclude decision.
    pub max_correlation: f64,
    /// Minimum |t-stat| of mean IC required for an alpha to be kept.
    pub min_t_stat: f64,
    /// Trailing window (periods) used for the rolling IC line.
    pub rolling_window: usize,
}

// ─── Tracker ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Observation {
    /// Emission time in nanos — groups insights into a cross-section.
    period: i64,
    /// Emission date (YYYY-MM-DD) for charting.
    date: String,
    symbol: String,
    /// Signed signal (direction × strength).
    signal: f64,
    /// Realised forward return over the insight's life.
    forward_return: f64,
}

/// Accumulates closed-insight observations across the backtest, keyed by alpha.
#[derive(Debug, Clone, Default)]
pub struct AlphaPerformanceTracker {
    by_model: HashMap<String, Vec<Observation>>,
}

impl AlphaPerformanceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record expired (closed) insights as (signal, forward-return) observations.
    /// Called once per bar with the insights that just closed.
    pub fn record_expired(&mut self, expired: &[Insight]) {
        for ins in expired {
            if ins.source_model.is_empty() {
                continue;
            }
            let (Some(ref_v), Some(final_v)) = (ins.reference_value, ins.reference_value_final)
            else {
                continue;
            };
            let ref_f = ref_v.to_f64().unwrap_or(0.0);
            if ref_f == 0.0 {
                continue;
            }
            let final_f = final_v.to_f64().unwrap_or(0.0);
            let fwd = (final_f - ref_f) / ref_f;
            if !fwd.is_finite() {
                continue;
            }
            let sign = match ins.direction {
                InsightDirection::Up => 1.0,
                InsightDirection::Down => -1.0,
                InsightDirection::Flat => 0.0,
            };
            // Strength: predicted magnitude, else confidence, else unit.
            let strength = ins
                .magnitude
                .and_then(|m| m.to_f64())
                .map(f64::abs)
                .filter(|m| *m > 0.0)
                .or_else(|| {
                    ins.confidence
                        .and_then(|c| c.to_f64())
                        .map(f64::abs)
                        .filter(|c| *c > 0.0)
                })
                .unwrap_or(1.0);
            self.by_model
                .entry(ins.source_model.clone())
                .or_default()
                .push(Observation {
                    period: ins.generated_time_utc.0,
                    date: ins.generated_time_utc.date_utc().to_string(),
                    symbol: ins.symbol.value.clone(),
                    signal: sign * strength,
                    forward_return: fwd,
                });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }

    /// Reduce accumulated observations into the full analytics bundle.
    pub fn compute(&self) -> AlphaAnalytics {
        let mut names: Vec<String> = self.by_model.keys().cloned().collect();
        names.sort();

        // Per-alpha period ICs → rolling series + summary stats.
        let mut ic_series = Vec::new();
        let mut stats: HashMap<String, Summary> = HashMap::new();
        for name in &names {
            let ics = period_ics(&self.by_model[name]);
            stats.insert(name.clone(), summarize(&ics));
            if !ics.is_empty() {
                ic_series.push(AlphaIcSeries {
                    name: name.clone(),
                    points: rolling(&ics, ROLLING_WINDOW),
                });
            }
        }

        // Pairwise signal correlation over aligned (period, symbol) keys.
        let signal_maps: HashMap<&String, HashMap<(i64, &str), f64>> = names
            .iter()
            .map(|name| {
                let m = self.by_model[name]
                    .iter()
                    .map(|o| ((o.period, o.symbol.as_str()), o.signal))
                    .collect();
                (name, m)
            })
            .collect();

        let n = names.len();
        let mut matrix = vec![vec![None; n]; n];
        for i in 0..n {
            matrix[i][i] = Some(1.0);
            for j in (i + 1)..n {
                let a = &signal_maps[&names[i]];
                let b = &signal_maps[&names[j]];
                let (mut xs, mut ys) = (Vec::new(), Vec::new());
                for (k, va) in a {
                    if let Some(vb) = b.get(k) {
                        xs.push(*va);
                        ys.push(*vb);
                    }
                }
                let c = (xs.len() >= MIN_CORR_OVERLAP)
                    .then(|| pearson(&xs, &ys))
                    .filter(|c| c.is_finite());
                matrix[i][j] = c;
                matrix[j][i] = c;
            }
        }
        let name_idx: HashMap<&String, usize> =
            names.iter().enumerate().map(|(i, n)| (n, i)).collect();

        // Greedy spanning set: highest |mean IC| first, drop high-corr duplicates.
        let mut eligible: Vec<&String> = names
            .iter()
            .filter(|name| {
                let s = &stats[*name];
                s.n >= MIN_PERIODS && s.mean.is_finite()
            })
            .collect();
        eligible.sort_by(|a, b| {
            stats[*b]
                .mean
                .abs()
                .partial_cmp(&stats[*a].mean.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut kept: Vec<&String> = Vec::new();
        let mut decision: HashMap<String, (bool, String)> = HashMap::new();
        for name in eligible {
            let s = &stats[name];

            // 1. FLOOR — reject noise: must be predictive, economically non-negligible,
            //    and clear a minimum confidence bar. Independence cannot rescue an
            //    alpha below the floor.
            if s.mean <= 0.0 {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        format!("excluded — mean IC {:+.3} ≤ 0 (not predictive)", s.mean),
                    ),
                );
                continue;
            }
            if s.mean < MIN_IC {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        format!("excluded — mean IC {:.3} below floor {:.3}", s.mean, MIN_IC),
                    ),
                );
                continue;
            }
            if s.t_stat.abs() < IC_T_STAT_FLOOR {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        format!(
                            "excluded — below confidence floor (t {:.2}, need |t|≥{:.1})",
                            s.t_stat, IC_T_STAT_FLOOR
                        ),
                    ),
                );
                continue;
            }

            // 2. First floor-passer anchors the set.
            if kept.is_empty() {
                kept.push(name);
                decision.insert(
                    name.clone(),
                    (
                        true,
                        format!(
                            "kept — anchor (IC {:.3}, t {:.2}), highest |IC|",
                            s.mean, s.t_stat
                        ),
                    ),
                );
                continue;
            }

            // 3. Worst |corr| against the kept set (reporting + hard duplicate cap).
            let i = name_idx[name];
            let mut worst: Option<(f64, &String)> = None;
            for k in &kept {
                if let Some(c) = matrix[i][name_idx[*k]] {
                    let ac = c.abs();
                    if worst.map(|(w, _)| ac > w).unwrap_or(true) {
                        worst = Some((ac, k));
                    }
                }
            }
            let max_ac = worst.map(|(ac, _)| ac).unwrap_or(0.0);
            let other = worst.map(|(_, o)| o.as_str()).unwrap_or("kept set");

            // 4. PARTIAL IC — the candidate's mean IC after projecting out the kept
            //    set through the signal-correlation structure (Grinold-Kahn): the
            //    increment to the blended IR. This is the additive-information test;
            //    it subsumes both significance and correlation-dedup in one number.
            let s_idx: Vec<usize> = kept.iter().map(|k| name_idx[*k]).collect();
            let ic_s: Vec<f64> = kept.iter().map(|k| stats[*k].mean).collect();
            let n = s_idx.len();
            let mut css = vec![vec![0.0_f64; n]; n];
            for a in 0..n {
                css[a][a] = 1.0 + PARTIAL_IC_RIDGE;
                for b in (a + 1)..n {
                    let c = matrix[s_idx[a]][s_idx[b]].unwrap_or(0.0);
                    css[a][b] = c;
                    css[b][a] = c;
                }
            }
            let c_ks: Vec<f64> = s_idx.iter().map(|&j| matrix[i][j].unwrap_or(0.0)).collect();
            // w = C_SS^-1 IC_S ; p = C_SS^-1 c_kS
            let w = solve_spd(&css, &ic_s);
            let p = solve_spd(&css, &c_ks);
            let pred_ic: f64 = c_ks.iter().zip(&w).map(|(c, wi)| c * wi).sum();
            let resid_var: f64 =
                (1.0 - c_ks.iter().zip(&p).map(|(c, pi)| c * pi).sum::<f64>()).max(1e-6);
            let partial_ic = (s.mean - pred_ic) / resid_var.sqrt();

            if max_ac >= MAX_CORR {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        format!("redundant — |corr| {max_ac:.2} with {other}"),
                    ),
                );
            } else if partial_ic >= MIN_PARTIAL_IC {
                kept.push(name);
                decision.insert(
                    name.clone(),
                    (
                        true,
                        format!(
                            "kept — additive: partial IC {partial_ic:.3} (≥{MIN_PARTIAL_IC:.3}) beyond kept set, raises blended IR; IC {:.3}, max |corr| {max_ac:.2} vs {other}",
                            s.mean
                        ),
                    ),
                );
            } else {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        format!(
                            "excluded — partial IC {partial_ic:.3} < floor {MIN_PARTIAL_IC:.3} (no information beyond kept set; max |corr| {max_ac:.2} vs {other})"
                        ),
                    ),
                );
            }
        }

        // Ranking rows for every tracked alpha, sorted by mean IC desc (NaN last).
        let mut ranking: Vec<AlphaRanking> = names
            .iter()
            .map(|name| {
                let s = &stats[name];
                let (keep, reason) = decision.get(name).cloned().unwrap_or_else(|| {
                    (
                        false,
                        format!("insufficient data — {} period(s) (<{MIN_PERIODS})", s.n),
                    )
                });
                AlphaRanking {
                    name: name.clone(),
                    mean_ic: s.mean,
                    ic_ir: s.ir,
                    t_stat: s.t_stat,
                    hit_rate: s.hit_rate,
                    n_periods: s.n,
                    keep,
                    reason,
                }
            })
            .collect();
        ranking.sort_by(
            |a, b| match (a.mean_ic.is_finite(), b.mean_ic.is_finite()) {
                (true, true) => b
                    .mean_ic
                    .partial_cmp(&a.mean_ic)
                    .unwrap_or(std::cmp::Ordering::Equal),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => a.name.cmp(&b.name),
            },
        );

        AlphaAnalytics {
            ic_series,
            correlation: AlphaCorrelationMatrix { names, matrix },
            ranking,
            max_correlation: MAX_CORR,
            min_t_stat: IC_T_STAT_FLOOR,
            rolling_window: ROLLING_WINDOW,
        }
    }
}

// ─── Stats helpers ─────────────────────────────────────────────────────────────

struct Summary {
    mean: f64,
    ir: f64,
    t_stat: f64,
    hit_rate: f64,
    n: usize,
}

/// Cross-sectional Spearman rank IC per emission period, sorted chronologically.
fn period_ics(obs: &[Observation]) -> Vec<(String, f64)> {
    let mut groups: BTreeMap<i64, (String, Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for o in obs {
        let e = groups
            .entry(o.period)
            .or_insert_with(|| (o.date.clone(), Vec::new(), Vec::new()));
        e.1.push(o.signal);
        e.2.push(o.forward_return);
    }
    let mut out = Vec::new();
    for (_, (date, sig, fwd)) in groups {
        // Per-period skill, cardinality-agnostic: a single-name call scores by
        // directional agreement (±1, "did its long/short go the right way");
        // multi-name periods score by cross-sectional rank IC. Both aggregate
        // over all the alpha's periods into its IC + t-stat.
        let ic = if sig.len() == 1 {
            let agree = sig[0].signum() * fwd[0].signum();
            if agree == 0.0 {
                continue;
            }
            agree
        } else {
            spearman(&sig, &fwd)
        };
        if ic.is_finite() {
            out.push((date, ic));
        }
    }
    out
}

fn rolling(ics: &[(String, f64)], window: usize) -> Vec<AlphaIcPoint> {
    (0..ics.len())
        .map(|i| {
            let lo = i.saturating_sub(window.saturating_sub(1));
            let slice = &ics[lo..=i];
            let mean = slice.iter().map(|(_, v)| *v).sum::<f64>() / slice.len() as f64;
            AlphaIcPoint {
                date: ics[i].0.clone(),
                ic: mean,
            }
        })
        .collect()
}

fn summarize(ics: &[(String, f64)]) -> Summary {
    let n = ics.len();
    if n == 0 {
        return Summary {
            mean: f64::NAN,
            ir: f64::NAN,
            t_stat: f64::NAN,
            hit_rate: f64::NAN,
            n: 0,
        };
    }
    let vals: Vec<f64> = ics.iter().map(|(_, v)| *v).collect();
    let mean = vals.iter().sum::<f64>() / n as f64;
    let hit_rate = vals.iter().filter(|v| **v > 0.0).count() as f64 / n as f64;
    let (ir, t_stat) = if n >= 2 {
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
        let sd = var.sqrt();
        if sd > 0.0 {
            let ir = mean / sd;
            (ir, ir * (n as f64).sqrt())
        } else {
            (f64::NAN, f64::NAN)
        }
    } else {
        (f64::NAN, f64::NAN)
    };
    Summary {
        mean,
        ir,
        t_stat,
        hit_rate,
        n,
    }
}

fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 2 || n != y.len() {
        return f64::NAN;
    }
    let nf = n as f64;
    let mx = x.iter().sum::<f64>() / nf;
    let my = y.iter().sum::<f64>() / nf;
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let dx = x[i] - mx;
        let dy = y[i] - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 1e-12 || syy <= 1e-12 {
        return f64::NAN;
    }
    sxy / (sxx.sqrt() * syy.sqrt())
}

/// Solve the small (ridge-regularized, symmetric) system `a x = b` by Gauss-Jordan
/// elimination with partial pivoting. `a` is n×n with `n` = kept-set size; returns x
/// of length n. Used for the partial-IC / blended-IR computation (Grinold-Kahn).
fn solve_spd(a: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    let n = b.len();
    if n == 0 {
        return Vec::new();
    }
    let mut m: Vec<Vec<f64>> = (0..n)
        .map(|r| {
            let mut row = a[r].clone();
            row.push(b[r]);
            row
        })
        .collect();
    for col in 0..n {
        let mut piv = col;
        for r in (col + 1)..n {
            if m[r][col].abs() > m[piv][col].abs() {
                piv = r;
            }
        }
        m.swap(col, piv);
        let d = m[col][col];
        if d.abs() < 1e-12 {
            continue; // singular pivot; leave this coordinate at ~0
        }
        for r in 0..n {
            if r == col {
                continue;
            }
            let f = m[r][col] / d;
            if f != 0.0 {
                for c in col..=n {
                    m[r][c] -= f * m[col][c];
                }
            }
        }
    }
    (0..n)
        .map(|r| {
            let d = m[r][r];
            if d.abs() < 1e-12 {
                0.0
            } else {
                m[r][n] / d
            }
        })
        .collect()
}

/// Fractional ranks with ties averaged.
fn rank(v: &[f64]) -> Vec<f64> {
    let n = v.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap_or(std::cmp::Ordering::Equal));
    let mut ranks = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && v[idx[j]] == v[idx[i]] {
            j += 1;
        }
        // 1-based ranks of the tie block [i, j) average to ((i+1)+j)/2.
        let avg = ((i + 1 + j) as f64) / 2.0;
        for &k in &idx[i..j] {
            ranks[k] = avg;
        }
        i = j;
    }
    ranks
}

fn spearman(x: &[f64], y: &[f64]) -> f64 {
    if x.len() != y.len() || x.len() < 2 {
        return f64::NAN;
    }
    pearson(&rank(x), &rank(y))
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spearman_perfect_monotonic() {
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let y = [10.0, 20.0, 25.0, 40.0, 41.0]; // monotonic, non-linear
        assert!((spearman(&x, &y) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn spearman_perfect_inverse() {
        let x = [1.0, 2.0, 3.0, 4.0, 5.0];
        let y = [5.0, 4.0, 3.0, 2.0, 1.0];
        assert!((spearman(&x, &y) + 1.0).abs() < 1e-9);
    }

    #[test]
    fn pearson_constant_is_nan() {
        let x = [1.0, 1.0, 1.0];
        let y = [1.0, 2.0, 3.0];
        assert!(pearson(&x, &y).is_nan());
    }

    #[test]
    fn rank_handles_ties() {
        let r = rank(&[10.0, 10.0, 20.0]);
        assert_eq!(r, vec![1.5, 1.5, 3.0]);
    }
}
