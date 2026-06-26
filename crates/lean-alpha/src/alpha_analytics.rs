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
/// Gate 2 floor — minimum REALIZED, friction-adjusted marginal contribution. Partial
/// IC is the frictionless additive-information upper bound (Grinold-Kahn); the marginal
/// contribution haircuts it by signal turnover (arXiv:2105.10306 — Turnover-Adjusted IR:
/// IC volatility + turnover lower realized IR) and by the long-only transfer coefficient
/// (Clarke-de Silva-Thorley — constraints cap how much edge a real book can capture).
/// Set below MIN_PARTIAL_IC because both factors are ≤ 1 and only shrink partial IC.
const MIN_MARGINAL_CONTRIB: f64 = 0.005;
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
    /// Which skill metric `mean_ic`/`t_stat` represent for THIS alpha:
    /// "rank_ic" for scored cross-sectional alphas (multi-name periods → Spearman IC),
    /// "mean_return" for directional/low-breadth alphas (single-name, constant-signal →
    /// IC is undefined, so the per-period realized forward return is used instead).
    pub metric: String,
    /// Mean of the per-period skill series — a Spearman IC ("rank_ic") or the mean realized
    /// forward return ("mean_return"), per `metric`.
    pub mean_ic: f64,
    pub ic_ir: f64,
    pub t_stat: f64,
    pub hit_rate: f64,
    /// Skew of the per-period skill series (color; >0 = fat right tail, e.g. lottery payoffs).
    pub skew: f64,
    pub n_periods: usize,
    /// Gate 1 — Grinold-Kahn partial IC: mean IC orthogonalized against the kept set
    /// via the signal-correlation matrix (the additive-information upper bound).
    pub partial_ic: f64,
    /// Gate 2 — realized friction-adjusted marginal contribution:
    /// `partial_ic * turnover_factor * transfer_factor`.
    pub marginal_contribution: f64,
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

    pub fn from_closed_insights(insights: &[Insight]) -> Self {
        let mut tracker = Self::new();
        tracker.record_expired(insights);
        tracker
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

        // Per-alpha skill series → rolling series + summary stats. Each alpha is scored on the
        // metric appropriate to its breadth: Spearman rank IC for scored cross-sectional alphas,
        // mean realized return for directional/low-breadth (single-name, constant-signal) alphas
        // where IC is undefined. Downstream gates are unchanged — they read the per-alpha mean +
        // t-stat and the cross-alpha signal-correlation matrix, both of which exist either way.
        let mut ic_series = Vec::new();
        let mut stats: HashMap<String, Summary> = HashMap::new();
        let mut metric: HashMap<String, MetricKind> = HashMap::new();
        for name in &names {
            let kind = classify(&self.by_model[name]);
            metric.insert(name.clone(), kind);
            let scores = period_scores(&self.by_model[name], kind);
            stats.insert(name.clone(), summarize(&scores));
            if !scores.is_empty() {
                ic_series.push(AlphaIcSeries {
                    name: name.clone(),
                    points: rolling(&scores, ROLLING_WINDOW),
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
        // Per-alpha decision: (keep, partial_ic, marginal_contribution, reason).
        let mut decision: HashMap<String, (bool, f64, f64, String)> = HashMap::new();
        for name in eligible {
            let s = &stats[name];
            // What the alpha's mean/t-stat represent ("rank_ic" or "mean_return"), for messages.
            let lbl = metric
                .get(name)
                .copied()
                .unwrap_or(MetricKind::RankIc)
                .label();

            // 1. FLOOR — reject noise: must be predictive, economically non-negligible,
            //    and clear a minimum confidence bar. Independence cannot rescue an
            //    alpha below the floor. Below-floor alphas have no meaningful partial
            //    IC / marginal contribution yet (reported as 0.0).
            if s.mean <= 0.0 {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        0.0,
                        0.0,
                        format!("dilutive — {lbl} {:+.3} ≤ 0 (not predictive)", s.mean),
                    ),
                );
                continue;
            }
            if s.mean < MIN_IC {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        0.0,
                        0.0,
                        format!("dilutive — {lbl} {:.3} below floor {:.3}", s.mean, MIN_IC),
                    ),
                );
                continue;
            }
            if s.t_stat.abs() < IC_T_STAT_FLOOR {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        0.0,
                        0.0,
                        format!(
                            "dilutive — below confidence floor (t {:.2}, need |t|≥{:.1})",
                            s.t_stat, IC_T_STAT_FLOOR
                        ),
                    ),
                );
                continue;
            }

            // 2. First floor-passer anchors the set. Its partial IC and marginal
            //    contribution are defined as its own mean IC (nothing to project out).
            if kept.is_empty() {
                kept.push(name);
                decision.insert(
                    name.clone(),
                    (
                        true,
                        s.mean,
                        s.mean,
                        format!(
                            "KEEP — anchor ({lbl} {:.3}, t {:.2}), highest |{lbl}|",
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

            // 5. MARGINAL CONTRIBUTION (Gate 2) — friction-adjust the frictionless
            //    partial IC by the alpha's own signal turnover and its long-only
            //    transfer coefficient, both computed one-pass from this alpha's
            //    per-period observations.
            let turnover_factor = turnover_factor(&self.by_model[name]);
            let transfer_factor = transfer_factor(&self.by_model[name], s.mean);
            let marginal_contribution = partial_ic * turnover_factor * transfer_factor;

            if max_ac >= MAX_CORR {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        partial_ic,
                        marginal_contribution,
                        format!("dilutive — redundant, |corr| {max_ac:.2} with {other}"),
                    ),
                );
            } else if partial_ic < MIN_PARTIAL_IC {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        partial_ic,
                        marginal_contribution,
                        format!(
                            "dilutive — partial IC {partial_ic:.3} < {MIN_PARTIAL_IC:.3} (no information beyond kept set; max |corr| {max_ac:.2} vs {other})"
                        ),
                    ),
                );
            } else if marginal_contribution < MIN_MARGINAL_CONTRIB {
                decision.insert(
                    name.clone(),
                    (
                        false,
                        partial_ic,
                        marginal_contribution,
                        format!(
                            "dilutive — partial IC {partial_ic:.3} ok but marginal contribution {marginal_contribution:.3} < {MIN_MARGINAL_CONTRIB:.3} (turnover×{turnover_factor:.2} / transfer×{transfer_factor:.2} haircut)"
                        ),
                    ),
                );
            } else {
                kept.push(name);
                decision.insert(
                    name.clone(),
                    (
                        true,
                        partial_ic,
                        marginal_contribution,
                        format!(
                            "KEEP — additive: partial IC {partial_ic:.3} (≥{MIN_PARTIAL_IC:.3}), marginal contribution {marginal_contribution:.3} (≥{MIN_MARGINAL_CONTRIB:.3}; turnover×{turnover_factor:.2}, transfer×{transfer_factor:.2}); IC {:.3}, max |corr| {max_ac:.2} vs {other}",
                            s.mean
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
                let (keep, partial_ic, marginal_contribution, reason) =
                    decision.get(name).cloned().unwrap_or_else(|| {
                        (
                            false,
                            0.0,
                            0.0,
                            format!("insufficient data — {} period(s) (<{MIN_PERIODS})", s.n),
                        )
                    });
                AlphaRanking {
                    name: name.clone(),
                    metric: metric
                        .get(name)
                        .copied()
                        .unwrap_or(MetricKind::RankIc)
                        .label()
                        .to_string(),
                    mean_ic: s.mean,
                    ic_ir: s.ir,
                    t_stat: s.t_stat,
                    hit_rate: s.hit_rate,
                    skew: s.skew,
                    n_periods: s.n,
                    partial_ic,
                    marginal_contribution,
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
    skew: f64,
    n: usize,
}

/// Which per-period skill metric an alpha is scored on.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MetricKind {
    /// Scored cross-sectional alpha: Spearman rank IC per multi-name period.
    RankIc,
    /// Directional/low-breadth alpha (single-name periods and/or constant signal): IC is
    /// undefined (no cross-section / no forecast variance), so each period is scored by its
    /// realized forward return — mean return + t-stat is the correct edge measure.
    MeanReturn,
}

impl MetricKind {
    fn label(self) -> &'static str {
        match self {
            MetricKind::RankIc => "rank_ic",
            MetricKind::MeanReturn => "mean_return",
        }
    }
}

/// Classify an alpha by whether a cross-sectional IC is even computable: it needs at least a
/// couple of multi-name periods (a cross-section to correlate) AND signal variance (a forecast
/// that varies across names). A binary long-only selector emitting one name per period with a
/// constant signal (e.g. the sweep monolith) satisfies neither → score it by realized return.
fn classify(obs: &[Observation]) -> MetricKind {
    let mut card: BTreeMap<i64, usize> = BTreeMap::new();
    for o in obs {
        *card.entry(o.period).or_default() += 1;
    }
    let multi_name_periods = card.values().filter(|&&c| c >= 2).count();
    let sigs: Vec<f64> = obs.iter().map(|o| o.signal).collect();
    let sig_mean = sigs.iter().sum::<f64>() / sigs.len().max(1) as f64;
    let sig_var =
        sigs.iter().map(|s| (s - sig_mean).powi(2)).sum::<f64>() / sigs.len().max(1) as f64;
    if multi_name_periods < 2 || sig_var <= 1e-12 {
        MetricKind::MeanReturn
    } else {
        MetricKind::RankIc
    }
}

/// Per-period skill series, sorted chronologically. The metric depends on the alpha type:
/// - `RankIc`: cross-sectional Spearman rank IC over each MULTI-name period (single-name periods
///   carry no cross-section and are skipped — they cannot contribute to a correlation).
/// - `MeanReturn`: the realized forward return of each period (mean across names if >1). For a
///   binary single-name selector this is the per-trade return; its mean + t-stat is the edge,
///   and IC is intentionally NOT computed (it is undefined for a constant, single-name forecast).
fn period_scores(obs: &[Observation], kind: MetricKind) -> Vec<(String, f64)> {
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
        let score = match kind {
            MetricKind::RankIc => {
                if sig.len() < 2 {
                    continue; // no cross-section this period → not an IC observation
                }
                spearman(&sig, &fwd)
            }
            MetricKind::MeanReturn => {
                // Realized return of the period's bet: sign-aligned mean forward return so a
                // short insight's profitable down-move counts positively.
                let n = fwd.len() as f64;
                fwd.iter()
                    .zip(&sig)
                    .map(|(r, s)| if *s < 0.0 { -r } else { *r })
                    .sum::<f64>()
                    / n
            }
        };
        if score.is_finite() {
            out.push((date, score));
        }
    }
    out
}

/// Gate-2 turnover haircut (arXiv:2105.10306, Turnover-Adjusted IR). Turnover is
/// `1 - mean( spearman(signal_t, signal_{t-1}) )` over consecutive periods, on the
/// symbols present in BOTH periods (the alpha's own signal autocorrelation). A signal
/// that reshuffles its cross-section every period (low autocorrelation → high turnover)
/// is costly to harvest, so the realized contribution is haircut by
/// `clamp(1 - turnover, 0, 1)`. With <2 usable consecutive periods → 1.0 (no penalty).
fn turnover_factor(obs: &[Observation]) -> f64 {
    // Per-period symbol→signal maps, in chronological order.
    let mut periods: BTreeMap<i64, HashMap<&str, f64>> = BTreeMap::new();
    for o in obs {
        periods
            .entry(o.period)
            .or_default()
            .insert(o.symbol.as_str(), o.signal);
    }
    let ordered: Vec<&HashMap<&str, f64>> = periods.values().collect();
    let mut autocorrs = Vec::new();
    for w in ordered.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        for (sym, &v_cur) in cur {
            if let Some(&v_prev) = prev.get(sym) {
                xs.push(v_cur);
                ys.push(v_prev);
            }
        }
        let ac = spearman(&xs, &ys);
        if ac.is_finite() {
            autocorrs.push(ac);
        }
    }
    if autocorrs.is_empty() {
        return 1.0;
    }
    let mean_ac = autocorrs.iter().sum::<f64>() / autocorrs.len() as f64;
    let turnover = 1.0 - mean_ac;
    (1.0 - turnover).clamp(0.0, 1.0)
}

/// Gate-2 long-only transfer coefficient (Clarke-de Silva-Thorley). `long_side_ic` is
/// the mean over periods of the per-period Spearman(signal, forward_return) computed
/// ONLY over the names at/above that period's cross-sectional signal median — the names
/// a long-only book can actually overweight. If an alpha's edge lives on the names it
/// ranks LOW (which long-only can't short), `long_side_ic << mean_ic` and the factor is
/// small. `transfer_factor = clamp(long_side_ic / max(|mean_ic|, 1e-6), 0, 1)`. Periods
/// with <3 top-half names are skipped; no usable periods → 1.0.
fn transfer_factor(obs: &[Observation], mean_ic: f64) -> f64 {
    let mut periods: BTreeMap<i64, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for o in obs {
        let e = periods.entry(o.period).or_default();
        e.0.push(o.signal);
        e.1.push(o.forward_return);
    }
    let mut top_ics = Vec::new();
    for (sig, fwd) in periods.values() {
        let ranks = rank(sig);
        let n = sig.len() as f64;
        // 1-based ranks; median line at (n+1)/2, "at/above median" = rank ≥ that.
        let median_rank = (n + 1.0) / 2.0;
        let (mut top_sig, mut top_fwd) = (Vec::new(), Vec::new());
        for i in 0..sig.len() {
            if ranks[i] >= median_rank {
                top_sig.push(sig[i]);
                top_fwd.push(fwd[i]);
            }
        }
        if top_sig.len() < 3 {
            continue;
        }
        let ic = spearman(&top_sig, &top_fwd);
        if ic.is_finite() {
            top_ics.push(ic);
        }
    }
    if top_ics.is_empty() {
        return 1.0;
    }
    let long_side_ic = top_ics.iter().sum::<f64>() / top_ics.len() as f64;
    (long_side_ic / mean_ic.abs().max(1e-6)).clamp(0.0, 1.0)
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
            skew: f64::NAN,
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
    // Population skew of the per-period series (color: >0 = fat right tail / lottery payoffs).
    let skew = if n >= 2 {
        let psd = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
        if psd > 0.0 {
            vals.iter().map(|v| ((v - mean) / psd).powi(3)).sum::<f64>() / n as f64
        } else {
            0.0
        }
    } else {
        f64::NAN
    };
    Summary {
        mean,
        ir,
        t_stat,
        hit_rate,
        skew,
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
        let pivot = m[col].clone();
        for (r, row) in m.iter_mut().enumerate() {
            if r == col {
                continue;
            }
            let f = row[col] / d;
            if f != 0.0 {
                for (c, &pv) in pivot.iter().enumerate().skip(col) {
                    row[c] -= f * pv;
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

    fn obs(period: i64, symbol: &str, signal: f64, fwd: f64) -> Observation {
        Observation {
            period,
            date: "2026-01-01".to_string(),
            symbol: symbol.to_string(),
            signal,
            forward_return: fwd,
        }
    }

    #[test]
    fn turnover_factor_stable_signal_no_penalty() {
        // Same cross-sectional ranking every period → autocorrelation ≈ 1 →
        // turnover ≈ 0 → factor ≈ 1 (no haircut).
        let mut o = Vec::new();
        for p in 0..4 {
            o.push(obs(p, "A", 3.0, 0.0));
            o.push(obs(p, "B", 2.0, 0.0));
            o.push(obs(p, "C", 1.0, 0.0));
        }
        assert!((turnover_factor(&o) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn turnover_factor_reshuffling_signal_penalized() {
        // Ranking flips every period (perfect inverse autocorrelation = -1) →
        // turnover = 2 → clamp(1 - 2, 0, 1) = 0 (fully penalized).
        let mut o = Vec::new();
        for p in 0..4 {
            let (a, b, c) = if p % 2 == 0 {
                (3.0, 2.0, 1.0)
            } else {
                (1.0, 2.0, 3.0)
            };
            o.push(obs(p, "A", a, 0.0));
            o.push(obs(p, "B", b, 0.0));
            o.push(obs(p, "C", c, 0.0));
        }
        assert!(turnover_factor(&o) < 1e-9);
    }

    #[test]
    fn turnover_factor_single_period_no_penalty() {
        let o = vec![obs(0, "A", 1.0, 0.0), obs(0, "B", 2.0, 0.0)];
        assert!((turnover_factor(&o) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn transfer_factor_long_side_edge_full() {
        // Edge is monotonic across the whole cross-section, so the top-half names
        // are also well-ordered → long_side_ic ≈ mean_ic → factor ≈ 1.
        let mut o = Vec::new();
        for p in 0..3 {
            for k in 0..6 {
                let s = k as f64;
                o.push(obs(p, &format!("S{k}"), s, s)); // signal predicts return on every name
            }
        }
        let mean_ic = 1.0; // perfect cross-sectional IC
        let tf = transfer_factor(&o, mean_ic);
        assert!(tf > 0.95, "expected ~1.0, got {tf}");
    }

    #[test]
    fn transfer_factor_short_side_edge_haircut() {
        // Symmetric V: signal predicts return ONLY on the low-signal (short) names;
        // among the top-half (long-side) names the relationship is reversed, so
        // long_side_ic is negative → transfer_factor clamps to ~0.
        // signal s in {0..5}; forward return = -(s - 2.5) shape on top half.
        let returns = [5.0, 3.0, 1.0, -1.0, -3.0, -5.0]; // top-half (high signal) does worst
        let mut o = Vec::new();
        for p in 0..3 {
            for (k, &ret) in returns.iter().enumerate() {
                o.push(obs(p, &format!("S{k}"), k as f64, ret));
            }
        }
        // Whole-book IC is strongly negative; mean_ic.abs() is large. The top-half
        // (signals 3,4,5) returns are -1,-3,-5: signal up ⇒ return down ⇒ long-side
        // IC negative ⇒ factor clamps to 0.
        let mean_ic = 0.5;
        let tf = transfer_factor(&o, mean_ic);
        assert!(tf < 1e-9, "expected ~0 (short-side edge), got {tf}");
    }

    #[test]
    fn transfer_factor_too_few_top_names_no_data() {
        // 4 names → top half = 2 names (<3) → period skipped → no usable periods → 1.0.
        let o = vec![
            obs(0, "A", 1.0, 1.0),
            obs(0, "B", 2.0, 2.0),
            obs(0, "C", 3.0, 3.0),
            obs(0, "D", 4.0, 4.0),
        ];
        assert!((transfer_factor(&o, 1.0) - 1.0).abs() < 1e-9);
    }
}
