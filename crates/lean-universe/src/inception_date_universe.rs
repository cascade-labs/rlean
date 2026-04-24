use chrono::NaiveDate;
use std::collections::{BTreeMap, HashSet};

/// Universe selection model that adds a ticker to the active set on or after
/// its inception date, and never removes it.
///
/// Mirrors C# `InceptionDateUniverseSelectionModel`.
///
/// # Example
/// ```rust
/// use chrono::NaiveDate;
/// use lean_universe::InceptionDateUniverseSelectionModel;
///
/// let mut model = InceptionDateUniverseSelectionModel::new("my-basket");
/// model.add("AMZN", NaiveDate::from_ymd_opt(1997, 5, 15).unwrap());
/// model.add("GOOG", NaiveDate::from_ymd_opt(2004, 8, 19).unwrap());
///
/// let active = model.select(NaiveDate::from_ymd_opt(2000, 1, 1).unwrap());
/// assert_eq!(active, Some(vec!["AMZN".to_string()]));
/// ```
pub struct InceptionDateUniverseSelectionModel {
    /// Friendly name for this universe (e.g. "qc-energy-etf-basket").
    pub name: String,
    /// Tickers ordered by their inception date (allows efficient scan).
    /// BTreeMap<date, Vec<ticker>> handles multiple tickers on same date.
    pending: BTreeMap<NaiveDate, Vec<String>>,
    /// Currently active tickers (added on or after inception date).
    active: Vec<String>,
    /// For dedup tracking.
    active_set: HashSet<String>,
}

impl InceptionDateUniverseSelectionModel {
    /// Create an empty model with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pending: BTreeMap::new(),
            active: Vec::new(),
            active_set: HashSet::new(),
        }
    }

    /// Register a ticker with its inception date.
    pub fn add(&mut self, ticker: impl Into<String>, inception: NaiveDate) {
        self.pending
            .entry(inception)
            .or_default()
            .push(ticker.into());
    }

    /// Build from a collection of (ticker, inception_date) pairs.
    pub fn from_pairs(
        name: impl Into<String>,
        pairs: impl IntoIterator<Item = (String, NaiveDate)>,
    ) -> Self {
        let mut model = Self::new(name);
        for (ticker, date) in pairs {
            model.add(ticker, date);
        }
        model
    }

    /// Returns the set of active tickers for the given date.
    ///
    /// All tickers whose inception date is <= `date` are included.
    /// Once a ticker is active it stays active — there is no removal.
    ///
    /// Returns `None` if no new tickers were added since the last call
    /// (i.e. the universe is unchanged). Returns `Some(tickers)` on any
    /// change or on the very first call.
    pub fn select(&mut self, date: NaiveDate) -> Option<Vec<String>> {
        // Drain all pending tickers whose inception date has passed.
        let ready_dates: Vec<NaiveDate> = self.pending.range(..=date).map(|(d, _)| *d).collect();

        if ready_dates.is_empty() && !self.active.is_empty() {
            return None; // Universe unchanged
        }

        let mut changed = false;
        for d in ready_dates {
            if let Some(tickers) = self.pending.remove(&d) {
                for ticker in tickers {
                    if self.active_set.insert(ticker.clone()) {
                        self.active.push(ticker);
                        changed = true;
                    }
                }
            }
        }

        if !changed && !self.active.is_empty() {
            return None;
        }

        if self.active.is_empty() {
            return None;
        }

        Some(self.active.clone())
    }

    /// Returns all currently active tickers without advancing state.
    pub fn active_tickers(&self) -> &[String] {
        &self.active
    }
}

// ─── Preset ETF baskets ───────────────────────────────────────────────────────

/// All preset ETF universe baskets. Each method returns a fully-populated
/// `InceptionDateUniverseSelectionModel` ready for use.
pub struct LiquidEtfUniverse;

impl LiquidEtfUniverse {
    /// Energy ETF basket (19 tickers, 1998-12-22 through 2012-02-15).
    /// Mirrors C# `EnergyETFUniverse`.
    pub fn energy() -> InceptionDateUniverseSelectionModel {
        let mut m = InceptionDateUniverseSelectionModel::new("qc-energy-etf-basket");
        let d = NaiveDate::from_ymd_opt;
        m.add("XLE", d(1998, 12, 22).unwrap());
        m.add("IYE", d(2000, 6, 16).unwrap());
        m.add("VDE", d(2004, 9, 29).unwrap());
        m.add("USO", d(2006, 4, 10).unwrap());
        m.add("XES", d(2006, 6, 22).unwrap());
        m.add("XOP", d(2006, 6, 22).unwrap());
        m.add("UNG", d(2007, 4, 18).unwrap());
        m.add("ICLN", d(2008, 6, 25).unwrap());
        m.add("ERX", d(2008, 11, 6).unwrap());
        m.add("ERY", d(2008, 11, 6).unwrap());
        m.add("SCO", d(2008, 11, 25).unwrap());
        m.add("UCO", d(2008, 11, 25).unwrap());
        m.add("AMJ", d(2009, 6, 2).unwrap());
        m.add("BNO", d(2010, 6, 2).unwrap());
        m.add("AMLP", d(2010, 8, 25).unwrap());
        m.add("OIH", d(2011, 12, 21).unwrap());
        m.add("DGAZ", d(2012, 2, 8).unwrap());
        m.add("UGAZ", d(2012, 2, 8).unwrap());
        m.add("TAN", d(2012, 2, 15).unwrap());
        m
    }

    /// Metals ETF basket (13 tickers, 2004-11-18 through 2013-10-03).
    /// Mirrors C# `MetalsETFUniverse`.
    pub fn metals() -> InceptionDateUniverseSelectionModel {
        let mut m = InceptionDateUniverseSelectionModel::new("qc-metals-etf-basket");
        let d = NaiveDate::from_ymd_opt;
        m.add("GLD", d(2004, 11, 18).unwrap());
        m.add("IAU", d(2005, 1, 28).unwrap());
        m.add("SLV", d(2006, 4, 28).unwrap());
        m.add("GDX", d(2006, 5, 22).unwrap());
        m.add("AGQ", d(2008, 12, 4).unwrap());
        m.add("GDXJ", d(2009, 11, 11).unwrap());
        m.add("PPLT", d(2010, 1, 8).unwrap());
        m.add("NUGT", d(2010, 12, 8).unwrap());
        m.add("DUST", d(2010, 12, 8).unwrap());
        m.add("USLV", d(2011, 10, 17).unwrap());
        m.add("UGLD", d(2011, 10, 17).unwrap());
        m.add("JNUG", d(2013, 10, 3).unwrap());
        m.add("JDST", d(2013, 10, 3).unwrap());
        m
    }

    /// Technology ETF basket (16 tickers, 1998-12-22 through 2013-10-24).
    /// Mirrors C# `TechnologyETFUniverse`.
    pub fn technology() -> InceptionDateUniverseSelectionModel {
        let mut m = InceptionDateUniverseSelectionModel::new("qc-technology-etf-basket");
        let d = NaiveDate::from_ymd_opt;
        m.add("XLK", d(1998, 12, 22).unwrap());
        m.add("QQQ", d(1999, 3, 10).unwrap());
        m.add("SOXX", d(2001, 7, 13).unwrap());
        m.add("IGV", d(2001, 7, 13).unwrap());
        m.add("VGT", d(2004, 1, 30).unwrap());
        m.add("QTEC", d(2006, 4, 25).unwrap());
        m.add("FDN", d(2006, 6, 23).unwrap());
        m.add("FXL", d(2007, 5, 10).unwrap());
        m.add("TECL", d(2008, 12, 17).unwrap());
        m.add("TECS", d(2008, 12, 17).unwrap());
        m.add("SOXL", d(2010, 3, 11).unwrap());
        m.add("SOXS", d(2010, 3, 11).unwrap());
        m.add("SKYY", d(2011, 7, 6).unwrap());
        m.add("SMH", d(2011, 12, 21).unwrap());
        m.add("KWEB", d(2013, 8, 1).unwrap());
        m.add("FTEC", d(2013, 10, 24).unwrap());
        m
    }

    /// US Treasuries ETF basket (20 tickers, 2002-07-26 through 2012-02-24).
    /// Mirrors C# `USTreasuriesETFUniverse`.
    pub fn us_treasuries() -> InceptionDateUniverseSelectionModel {
        let mut m = InceptionDateUniverseSelectionModel::new("qc-us-treasuries-etf-basket");
        let d = NaiveDate::from_ymd_opt;
        m.add("IEF", d(2002, 7, 26).unwrap());
        m.add("SHY", d(2002, 7, 26).unwrap());
        m.add("TLT", d(2002, 7, 26).unwrap());
        m.add("IEI", d(2007, 1, 11).unwrap());
        m.add("SHV", d(2007, 1, 11).unwrap());
        m.add("TLH", d(2007, 1, 11).unwrap());
        m.add("EDV", d(2007, 12, 10).unwrap());
        m.add("BIL", d(2007, 5, 30).unwrap());
        m.add("SPTL", d(2007, 5, 30).unwrap());
        m.add("TBT", d(2008, 5, 1).unwrap());
        m.add("TMF", d(2009, 4, 16).unwrap());
        m.add("TMV", d(2009, 4, 16).unwrap());
        m.add("TBF", d(2009, 8, 20).unwrap());
        m.add("VGSH", d(2009, 11, 23).unwrap());
        m.add("VGIT", d(2009, 11, 23).unwrap());
        m.add("VGLT", d(2009, 11, 24).unwrap());
        m.add("SCHO", d(2010, 8, 6).unwrap());
        m.add("SCHR", d(2010, 8, 6).unwrap());
        m.add("SPTS", d(2011, 12, 1).unwrap());
        m.add("GOVT", d(2012, 2, 24).unwrap());
        m
    }

    /// Volatility ETF basket (10 tickers, 2010-02-11 through 2011-10-20).
    /// Mirrors C# `VolatilityETFUniverse`.
    pub fn volatility() -> InceptionDateUniverseSelectionModel {
        let mut m = InceptionDateUniverseSelectionModel::new("qc-volatility-etf-basket");
        let d = NaiveDate::from_ymd_opt;
        m.add("SQQQ", d(2010, 2, 11).unwrap());
        m.add("TQQQ", d(2010, 2, 11).unwrap());
        m.add("TVIX", d(2010, 11, 30).unwrap());
        m.add("VIXY", d(2011, 1, 4).unwrap());
        m.add("SPLV", d(2011, 5, 5).unwrap());
        m.add("SVXY", d(2011, 10, 4).unwrap());
        m.add("UVXY", d(2011, 10, 4).unwrap());
        m.add("EEMV", d(2011, 10, 20).unwrap());
        m.add("EFAV", d(2011, 10, 20).unwrap());
        m.add("USMV", d(2011, 10, 20).unwrap());
        m
    }

    /// S&P 500 Sector SPDR ETF basket (9 tickers, all 1998-12-22).
    /// Mirrors C# `SP500SectorsETFUniverse`.
    pub fn sp500_sectors() -> InceptionDateUniverseSelectionModel {
        let mut m = InceptionDateUniverseSelectionModel::new("qc-sp500-sectors-etf-basket");
        let inception = NaiveDate::from_ymd_opt(1998, 12, 22).unwrap();
        for ticker in &[
            "XLB", "XLE", "XLF", "XLI", "XLK", "XLP", "XLU", "XLV", "XLY",
        ] {
            m.add(*ticker, inception);
        }
        m
    }

    /// Full liquid ETF basket: SP500 sectors + energy + metals + technology +
    /// treasuries + volatility. Mirrors C# `LiquidETFUniverse`.
    pub fn liquid() -> InceptionDateUniverseSelectionModel {
        let mut m = InceptionDateUniverseSelectionModel::new("qc-liquid-etf-basket");

        // Merge from all sub-baskets by draining their pending maps.
        for sub in [
            Self::sp500_sectors(),
            Self::energy(),
            Self::metals(),
            Self::technology(),
            Self::us_treasuries(),
            Self::volatility(),
        ] {
            for (date, tickers) in sub.pending {
                for ticker in tickers {
                    m.add(ticker, date);
                }
            }
        }
        m
    }
}
