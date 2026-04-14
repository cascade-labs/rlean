use lean_core::{Resolution, SecurityType, Symbol};
use chrono::NaiveDate;
use std::path::{Path, PathBuf};

/// Returns the canonical path for a date-partitioned option EOD Parquet file.
///
/// Layout: `{root}/option/usa/daily/{underlying_lower}/{YYYYMMDD}_eod.parquet`
///
/// One file per trading date per underlying — cache check is a single
/// `file.exists()` syscall; reads open the file and return all rows with no
/// predicate filtering needed.
pub fn option_eod_path(root: &Path, underlying: &str, date: NaiveDate) -> PathBuf {
    let ul = underlying.to_lowercase();
    let mut p = root.to_path_buf();
    p.push("option");
    p.push("usa");
    p.push("daily");
    p.push(&ul);
    p.push(format!("{}_{}.parquet", date.format("%Y%m%d"), "eod"));
    p
}

/// Glob pattern to list all cached EOD dates for an underlying.
///
/// Matches: `{root}/option/usa/daily/{underlying_lower}/*_eod.parquet`
pub fn option_eod_glob(root: &Path, underlying: &str) -> String {
    let ul = underlying.to_lowercase();
    format!("{}/option/usa/daily/{}/*_eod.parquet", root.display(), ul)
}

/// Canonical data path for a symbol + date + resolution combination.
/// Mirrors LEAN's `LeanDataPathComponents` convention but outputs `.parquet`.
#[derive(Debug, Clone)]
pub struct DataPath {
    pub root: PathBuf,
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub date: NaiveDate,
    pub suffix: &'static str,
    /// When set, this path uses option-style layout (keyed by underlying ticker).
    /// The string holds the underlying ticker in lowercase.
    pub option_underlying: Option<String>,
    /// When true, this path is for the option universe file.
    pub is_universe: bool,
}

impl DataPath {
    pub fn trade_bar(root: impl AsRef<Path>, symbol: &Symbol, resolution: Resolution, date: NaiveDate) -> Self {
        DataPath {
            root: root.as_ref().to_path_buf(),
            symbol: symbol.clone(),
            resolution,
            date,
            suffix: "trade",
            option_underlying: None,
            is_universe: false,
        }
    }

    pub fn quote_bar(root: impl AsRef<Path>, symbol: &Symbol, resolution: Resolution, date: NaiveDate) -> Self {
        DataPath {
            root: root.as_ref().to_path_buf(),
            symbol: symbol.clone(),
            resolution,
            date,
            suffix: "quote",
            option_underlying: None,
            is_universe: false,
        }
    }

    pub fn tick(root: impl AsRef<Path>, symbol: &Symbol, date: NaiveDate) -> Self {
        DataPath {
            root: root.as_ref().to_path_buf(),
            symbol: symbol.clone(),
            resolution: Resolution::Tick,
            date,
            suffix: "tick",
            option_underlying: None,
            is_universe: false,
        }
    }

    pub fn open_interest(root: impl AsRef<Path>, symbol: &Symbol, date: NaiveDate) -> Self {
        DataPath {
            root: root.as_ref().to_path_buf(),
            symbol: symbol.clone(),
            resolution: Resolution::Daily,
            date,
            suffix: "openinterest",
            option_underlying: None,
            is_universe: false,
        }
    }

    /// Path for option EOD bar data.
    ///
    /// `underlying_symbol` must be the **equity** symbol whose ticker names
    /// the directory (e.g. the SPY equity symbol for SPY options).
    /// `resolution` is typically `Daily` for ThetaData EOD, but callers can
    /// pass `Minute` for intraday option data.
    ///
    /// Produced paths mirror LEAN's canonical option layout:
    /// - Daily:   `{root}/option/{market}/daily/{ticker}_{suffix}.parquet`  (flat)
    /// - Minute+: `{root}/option/{market}/minute/{ticker}/{YYYYMMDD}_{suffix}.parquet`
    pub fn option_eod_bar(
        root: impl AsRef<Path>,
        underlying_symbol: &Symbol,
        resolution: Resolution,
        date: NaiveDate,
    ) -> Self {
        let ticker = underlying_symbol.value.to_lowercase();
        DataPath {
            root: root.as_ref().to_path_buf(),
            symbol: underlying_symbol.clone(),
            resolution,
            date,
            suffix: "trade",
            option_underlying: Some(ticker),
            is_universe: false,
        }
    }

    /// Path for the option universe file for a given underlying + date.
    ///
    /// Produced path:
    /// `{root}/option/{market}/universes/{ticker}/{YYYYMMDD}_universe.parquet`
    pub fn option_universe(
        root: impl AsRef<Path>,
        underlying_symbol: &Symbol,
        date: NaiveDate,
    ) -> Self {
        let ticker = underlying_symbol.value.to_lowercase();
        DataPath {
            root: root.as_ref().to_path_buf(),
            symbol: underlying_symbol.clone(),
            // Resolution::Daily is a placeholder; universe paths don't use the
            // resolution folder in the same way — the "universes" segment is
            // hard-coded in `to_path()` for universe files.
            resolution: Resolution::Daily,
            date,
            suffix: "universe",
            option_underlying: Some(ticker),
            is_universe: true,
        }
    }

    /// Full path to the parquet file.
    ///
    /// For standard (equity/forex/crypto) data:
    /// `{root}/{security_type}/{market}/{resolution}/{ticker}/{YYYYMMDD}_{suffix}.parquet`  (high-res)
    /// `{root}/{security_type}/{market}/{resolution}/{ticker}_{suffix}.parquet`             (daily)
    ///
    /// For option EOD bars (option_underlying is Some):
    /// `{root}/option/{market}/daily/{ticker}/{YYYYMMDD}_{suffix}.parquet`                 (daily, date-partitioned)
    /// `{root}/option/{market}/{resolution}/{ticker}/{YYYYMMDD}_{suffix}.parquet`          (intraday)
    ///
    /// For option universes (is_universe = true):
    /// `{root}/option/{market}/universes/{ticker}/{YYYYMMDD}_universe.parquet`
    pub fn to_path(&self) -> PathBuf {
        let market = self.symbol.market().as_str().to_lowercase();
        let mut p = self.root.clone();

        if let Some(ref ticker) = self.option_underlying {
            // Option path layout
            p.push("option");
            p.push(&market);

            if self.is_universe {
                p.push("universes");
                p.push(ticker);
                p.push(format!("{}_universe.parquet", self.date.format("%Y%m%d")));
            } else {
                let res = self.resolution.folder_name();
                p.push(res);
                // All resolutions use ticker subdirectory + date prefix
                // (daily is date-partitioned: one file per date per underlying)
                p.push(ticker);
                p.push(format!("{}_{}.parquet", self.date.format("%Y%m%d"), self.suffix));
            }
        } else {
            // Standard path layout
            let sec_type = format!("{}", self.symbol.security_type()).to_lowercase();
            let ticker = self.symbol.value.to_lowercase();
            let res = self.resolution.folder_name();

            p.push(&sec_type);
            p.push(&market);
            p.push(res);

            if self.resolution.is_high_resolution() {
                p.push(&ticker);
                p.push(format!("{}_{}.parquet", self.date.format("%Y%m%d"), self.suffix));
            } else {
                p.push(format!("{}_{}.parquet", ticker, self.suffix));
            }
        }

        p
    }

    /// Directory containing the file (for glob-based scanning).
    pub fn dir(&self) -> PathBuf {
        let mut p = self.to_path();
        p.pop();
        p
    }

    /// Glob pattern to read all dates for this symbol at this resolution.
    pub fn glob_all_dates(&self) -> String {
        let market = self.symbol.market().as_str().to_lowercase();

        if let Some(ref ticker) = self.option_underlying {
            if self.is_universe {
                format!(
                    "{}/option/{}/universes/{}/*_universe.parquet",
                    self.root.display(), market, ticker
                )
            } else {
                let res = self.resolution.folder_name();
                // All resolutions: ticker subdirectory + date-partitioned files
                format!(
                    "{}/option/{}/{}/{}/*_{}.parquet",
                    self.root.display(), market, res, ticker, self.suffix
                )
            }
        } else {
            let sec_type = format!("{}", self.symbol.security_type()).to_lowercase();
            let ticker = self.symbol.value.to_lowercase();
            let res = self.resolution.folder_name();

            if self.resolution.is_high_resolution() {
                format!(
                    "{}/{}/{}/{}/{}/**/*_{}.parquet",
                    self.root.display(), sec_type, market, res, ticker, self.suffix
                )
            } else {
                format!(
                    "{}/{}/{}/{}/{}_{}.parquet",
                    self.root.display(), sec_type, market, res, ticker, self.suffix
                )
            }
        }
    }
}

/// High-level helper that knows the data root and resolves paths.
#[derive(Debug, Clone)]
pub struct PathResolver {
    pub data_root: PathBuf,
}

impl PathResolver {
    pub fn new(data_root: impl AsRef<Path>) -> Self {
        PathResolver { data_root: data_root.as_ref().to_path_buf() }
    }

    pub fn trade_bar(&self, symbol: &Symbol, resolution: Resolution, date: NaiveDate) -> DataPath {
        DataPath::trade_bar(&self.data_root, symbol, resolution, date)
    }

    pub fn quote_bar(&self, symbol: &Symbol, resolution: Resolution, date: NaiveDate) -> DataPath {
        DataPath::quote_bar(&self.data_root, symbol, resolution, date)
    }

    pub fn tick(&self, symbol: &Symbol, date: NaiveDate) -> DataPath {
        DataPath::tick(&self.data_root, symbol, date)
    }

    pub fn open_interest(&self, symbol: &Symbol, date: NaiveDate) -> DataPath {
        DataPath::open_interest(&self.data_root, symbol, date)
    }

    /// Path for option EOD bar data.  `underlying_symbol` should be the equity
    /// (or index) symbol whose ticker names the storage directory.
    pub fn option_eod_bar(
        &self,
        underlying_symbol: &Symbol,
        resolution: Resolution,
        date: NaiveDate,
    ) -> DataPath {
        DataPath::option_eod_bar(&self.data_root, underlying_symbol, resolution, date)
    }

    /// Path for the option universe snapshot for a given underlying + date.
    pub fn option_universe(&self, underlying_symbol: &Symbol, date: NaiveDate) -> DataPath {
        DataPath::option_universe(&self.data_root, underlying_symbol, date)
    }

    /// Path to the factor file for the given market and ticker.
    ///
    /// Layout: `{data_root}/equity/{market}/factor_files/{ticker_lower}.parquet`
    pub fn factor_file(&self, market: &str, ticker: &str) -> PathBuf {
        factor_file_path(&self.data_root, market, ticker)
    }

    /// Path to the map file for the given market and ticker.
    ///
    /// Layout: `{data_root}/equity/{market}/map_files/{ticker_lower}.parquet`
    pub fn map_file(&self, market: &str, ticker: &str) -> PathBuf {
        map_file_path(&self.data_root, market, ticker)
    }
}

/// Canonical path for a factor file.
///
/// Layout: `{root}/equity/{market}/factor_files/{ticker_lower}.parquet`
///
/// Mirrors LEAN's `LeanData.GenerateRelativeFactorFilePath` convention.
pub fn factor_file_path(root: impl AsRef<Path>, market: &str, ticker: &str) -> PathBuf {
    let mut p = root.as_ref().to_path_buf();
    p.push("equity");
    p.push(market.to_lowercase());
    p.push("factor_files");
    p.push(format!("{}.parquet", ticker.to_lowercase()));
    p
}

/// Canonical path for a map file.
///
/// Layout: `{root}/equity/{market}/map_files/{ticker_lower}.parquet`
///
/// We store all LEAN data as Parquet (converted from LEAN's CSV sources)
/// to enable fast predicate-pushdown reads.
pub fn map_file_path(root: impl AsRef<Path>, market: &str, ticker: &str) -> PathBuf {
    let mut p = root.as_ref().to_path_buf();
    p.push("equity");
    p.push(market.to_lowercase());
    p.push("map_files");
    p.push(format!("{}.parquet", ticker.to_lowercase()));
    p
}
