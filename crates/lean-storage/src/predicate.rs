use lean_core::DateTime;

/// Predicate pushed down into the Parquet reader to skip row groups and pages
/// without deserializing them. All comparisons are in nanoseconds.
#[derive(Debug, Clone)]
pub struct Predicate {
    pub start_time: Option<DateTime>,
    pub end_time: Option<DateTime>,
    pub symbol_sids: Option<Vec<u64>>,
    pub min_close: Option<i64>,   // scaled price (×1e8)
    pub max_close: Option<i64>,
    pub min_volume: Option<i64>,
}

impl Predicate {
    pub fn new() -> Self {
        Predicate {
            start_time: None,
            end_time: None,
            symbol_sids: None,
            min_close: None,
            max_close: None,
            min_volume: None,
        }
    }

    pub fn with_time_range(mut self, start: DateTime, end: DateTime) -> Self {
        self.start_time = Some(start);
        self.end_time = Some(end);
        self
    }

    pub fn with_symbols(mut self, sids: Vec<u64>) -> Self {
        self.symbol_sids = Some(sids);
        self
    }

    pub fn with_min_close(mut self, price_scaled: i64) -> Self {
        self.min_close = Some(price_scaled);
        self
    }

    pub fn with_max_close(mut self, price_scaled: i64) -> Self {
        self.max_close = Some(price_scaled);
        self
    }

    pub fn with_min_volume(mut self, vol_scaled: i64) -> Self {
        self.min_volume = Some(vol_scaled);
        self
    }

    /// Convert to a DataFusion `Expr` for use with the DataFrame API.
    /// Returns None if predicate is empty (no filtering).
    pub fn to_datafusion_expr(&self) -> Option<datafusion::prelude::Expr> {
        use datafusion::prelude::*;

        let mut exprs: Vec<Expr> = vec![];

        if let Some(start) = self.start_time {
            exprs.push(col("time_ns").gt_eq(lit(start.0)));
        }
        if let Some(end) = self.end_time {
            exprs.push(col("time_ns").lt(lit(end.0)));
        }
        if let Some(ref sids) = self.symbol_sids {
            if sids.len() == 1 {
                exprs.push(col("symbol_sid").eq(lit(sids[0] as i64)));
            } else {
                let sid_exprs: Vec<Expr> = sids.iter()
                    .map(|&s| col("symbol_sid").eq(lit(s as i64)))
                    .collect();
                exprs.push(sid_exprs.into_iter().reduce(|a, b| a.or(b)).unwrap());
            }
        }
        if let Some(min_c) = self.min_close {
            exprs.push(col("close").gt_eq(lit(min_c)));
        }
        if let Some(max_c) = self.max_close {
            exprs.push(col("close").lt_eq(lit(max_c)));
        }
        if let Some(min_v) = self.min_volume {
            exprs.push(col("volume").gt_eq(lit(min_v)));
        }

        exprs.into_iter().reduce(|a, b| a.and(b))
    }
}

impl Default for Predicate {
    fn default() -> Self {
        Predicate::new()
    }
}
