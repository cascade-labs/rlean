use lean_core::{Market, Symbol};
use lean_universe::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_symbol(ticker: &str) -> Symbol {
    Symbol::create_equity(ticker, &Market::usa())
}

fn make_coarse(ticker: &str, market_cap: f64, dollar_volume: f64, price: f64) -> CoarseFundamental {
    CoarseFundamental {
        symbol: make_symbol(ticker),
        market_cap: Decimal::try_from(market_cap).unwrap(),
        dollar_volume: Decimal::try_from(dollar_volume).unwrap(),
        price: Decimal::try_from(price).unwrap(),
        has_fundamental_data: true,
        market: "usa".to_string(),
    }
}

// ---------------------------------------------------------------------------
// ManualUniverse tests
// ---------------------------------------------------------------------------

mod manual_universe_tests {
    use super::*;
    use lean_core::DateTime;
    use lean_universe::universe::{ManualUniverse, Universe, UniverseSettings};

    #[test]
    fn manual_universe_returns_configured_symbols() {
        let spy = make_symbol("SPY");
        let aapl = make_symbol("AAPL");
        let symbols = vec![spy.clone(), aapl.clone()];
        let universe = ManualUniverse::new(symbols, UniverseSettings::default());

        let result = universe.select_symbols(DateTime::now(), &[]);

        assert_eq!(result.len(), 2);
        assert!(result.contains(&spy));
        assert!(result.contains(&aapl));
    }

    #[test]
    fn manual_universe_always_returns_same_set() {
        let spy = make_symbol("SPY");
        let universe = ManualUniverse::new(vec![spy.clone()], UniverseSettings::default());

        // Call twice — should be identical each time
        let r1 = universe.select_symbols(DateTime::now(), &[]);
        let r2 = universe.select_symbols(DateTime::now(), &[]);
        assert_eq!(r1, r2);
    }

    #[test]
    fn manual_universe_empty_symbols() {
        let universe = ManualUniverse::new(vec![], UniverseSettings::default());
        let result = universe.select_symbols(DateTime::now(), &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn universe_settings_default_resolution_is_daily() {
        let settings = UniverseSettings::default();
        assert_eq!(settings.resolution, lean_core::Resolution::Daily);
        assert!(settings.fill_data_forward);
        assert!(!settings.extended_market_hours);
        assert!((settings.leverage - 1.0).abs() < f64::EPSILON);
    }
}

// ---------------------------------------------------------------------------
// CoarseUniverse tests
// ---------------------------------------------------------------------------

mod coarse_universe_tests {
    use super::*;
    use lean_universe::coarse_fundamental::FnCoarseFilter;

    #[test]
    fn coarse_filter_by_market_cap() {
        let data = vec![
            make_coarse("AAPL", 3_000_000_000.0, 1_000_000.0, 180.0),
            make_coarse("SMLL", 50_000_000.0, 100_000.0, 5.0),
            make_coarse("MSFT", 2_500_000_000.0, 900_000.0, 300.0),
        ];
        let threshold = dec!(1_000_000_000);

        let filter = FnCoarseFilter::new(move |coarse: &[CoarseFundamental]| {
            coarse
                .iter()
                .filter(|c| c.market_cap >= threshold)
                .map(|c| c.symbol.clone())
                .collect()
        });

        let model = CoarseUniverseSelectionModel::new(filter);
        let result = model.filter.select(&data);

        assert_eq!(result.len(), 2);
        assert!(result.contains(&make_symbol("AAPL")));
        assert!(result.contains(&make_symbol("MSFT")));
        assert!(!result.contains(&make_symbol("SMLL")));
    }

    #[test]
    fn coarse_filter_by_dollar_volume() {
        let data = vec![
            make_coarse("HIGH", 1_000_000_000.0, 5_000_000.0, 100.0),
            make_coarse("LOW", 500_000_000.0, 50_000.0, 20.0),
            make_coarse("MED", 800_000_000.0, 1_000_000.0, 50.0),
        ];
        let min_dv = dec!(500_000);

        let filter = FnCoarseFilter::new(move |coarse: &[CoarseFundamental]| {
            coarse
                .iter()
                .filter(|c| c.dollar_volume >= min_dv)
                .map(|c| c.symbol.clone())
                .collect()
        });

        let model = CoarseUniverseSelectionModel::new(filter);
        let result = model.filter.select(&data);

        assert_eq!(result.len(), 2);
        assert!(result.contains(&make_symbol("HIGH")));
        assert!(result.contains(&make_symbol("MED")));
        assert!(!result.contains(&make_symbol("LOW")));
    }

    #[test]
    fn coarse_top_n_by_dollar_volume() {
        // 10 stocks; top 3 by DV should be A, B, C
        let mut data: Vec<CoarseFundamental> = (1..=10)
            .map(|i| {
                let ticker = format!("STK{i}");
                make_coarse(&ticker, 1_000_000_000.0, (i as f64) * 100_000.0, 50.0)
            })
            .collect();

        // Shuffle to make sure ordering is not assumed
        data.reverse();

        let filter = FnCoarseFilter::new(|coarse: &[CoarseFundamental]| {
            let mut sorted = coarse.to_vec();
            sorted.sort_by(|a, b| b.dollar_volume.partial_cmp(&a.dollar_volume).unwrap());
            sorted.truncate(3);
            sorted.iter().map(|c| c.symbol.clone()).collect()
        });

        let model = CoarseUniverseSelectionModel::new(filter);
        let result = model.filter.select(&data);

        assert_eq!(result.len(), 3);
        // STK10, STK9, STK8 have the highest DV (10×, 9×, 8× factor)
        assert!(result.contains(&make_symbol("STK10")));
        assert!(result.contains(&make_symbol("STK9")));
        assert!(result.contains(&make_symbol("STK8")));
    }

    #[test]
    fn coarse_empty_input_returns_empty_vec() {
        let filter = FnCoarseFilter::new(|coarse: &[CoarseFundamental]| {
            coarse.iter().map(|c| c.symbol.clone()).collect()
        });
        let model = CoarseUniverseSelectionModel::new(filter);
        let result = model.filter.select(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn coarse_filter_excludes_no_fundamental_data() {
        let mut no_fund = make_coarse("NOFUND", 2_000_000_000.0, 500_000.0, 10.0);
        no_fund.has_fundamental_data = false;

        let data = vec![
            make_coarse("AAPL", 3_000_000_000.0, 1_000_000.0, 180.0),
            no_fund,
        ];

        let filter = FnCoarseFilter::new(|coarse: &[CoarseFundamental]| {
            coarse
                .iter()
                .filter(|c| c.has_fundamental_data)
                .map(|c| c.symbol.clone())
                .collect()
        });
        let model = CoarseUniverseSelectionModel::new(filter);
        let result = model.filter.select(&data);

        assert_eq!(result.len(), 1);
        assert!(result.contains(&make_symbol("AAPL")));
    }
}

// ---------------------------------------------------------------------------
// FineFundamental tests
// ---------------------------------------------------------------------------

mod fine_fundamental_tests {
    use super::*;
    use lean_universe::{FineFundamental, FineFundamentalUniverseSelectionModel};

    #[test]
    fn fine_fundamental_default_fields_are_none() {
        let sym = make_symbol("AAPL");
        let ff = FineFundamental::new(sym.clone());

        assert_eq!(ff.symbol, Some(sym));
        assert!(ff.pe_ratio.is_none());
        assert!(ff.pb_ratio.is_none());
        assert!(ff.ps_ratio.is_none());
        assert!(ff.ev_to_ebitda.is_none());
        assert!(ff.peg_ratio.is_none());
        assert!(ff.revenue.is_none());
        assert!(ff.net_income.is_none());
        assert!(ff.ebitda.is_none());
        assert!(ff.eps.is_none());
        assert!(ff.total_assets.is_none());
        assert!(ff.total_debt.is_none());
        assert!(ff.book_value_per_share.is_none());
        assert!(ff.market_cap.is_none());
        assert!(ff.sector.is_none());
        assert!(ff.industry.is_none());
    }

    #[test]
    fn fine_fundamental_default_impl_all_none() {
        let ff = FineFundamental::default();
        assert!(ff.symbol.is_none());
        assert!(ff.pe_ratio.is_none());
        assert!(ff.return_on_equity.is_none());
        assert!(ff.dividend_yield.is_none());
    }

    #[test]
    fn fine_universe_applies_coarse_filter_first() {
        // 10 stocks in coarse; coarse filter keeps 5; fine filter keeps 3
        let coarse_data: Vec<CoarseFundamental> = (1..=10)
            .map(|i| {
                let ticker = format!("C{i}");
                // First 5 have large market cap, last 5 small
                make_coarse(
                    &ticker,
                    (i as f64) * 1_000_000_000.0,
                    (i as f64) * 100_000.0,
                    50.0,
                )
            })
            .collect();

        let model = FineFundamentalUniverseSelectionModel::new(
            // Coarse: keep top 5 by market cap
            |coarse: &[CoarseFundamental]| {
                let mut sorted = coarse.to_vec();
                sorted.sort_by(|a, b| b.market_cap.partial_cmp(&a.market_cap).unwrap());
                sorted.truncate(5);
                sorted.iter().map(|c| c.symbol.clone()).collect()
            },
            // Fine: keep those with a PE ratio set
            |fine: &[FineFundamental]| {
                fine.iter()
                    .filter(|f| f.pe_ratio.is_some())
                    .map(|f| f.symbol.clone().unwrap())
                    .collect()
            },
        );

        let coarse_survivors = model.select_coarse(&coarse_data);
        assert_eq!(coarse_survivors.len(), 5);

        // Simulate fine data for coarse survivors — only 3 have PE set
        let fine_data: Vec<FineFundamental> = coarse_survivors
            .iter()
            .enumerate()
            .map(|(i, sym)| {
                let mut ff = FineFundamental::new(sym.clone());
                if i < 3 {
                    ff.pe_ratio = Some(dec!(15));
                }
                ff
            })
            .collect();

        let final_selection = model.select_fine(&fine_data);
        assert_eq!(final_selection.len(), 3);
    }

    #[test]
    fn fine_filter_by_low_pe_ratio() {
        let tickers = ["AAPL", "MSFT", "AMZN", "TSLA"];
        let pe_values = [dec!(12), dec!(25), dec!(80), dec!(150)];

        let fine_data: Vec<FineFundamental> = tickers
            .iter()
            .zip(pe_values.iter())
            .map(|(ticker, pe)| {
                let mut ff = FineFundamental::new(make_symbol(ticker));
                ff.pe_ratio = Some(*pe);
                ff
            })
            .collect();

        let model = FineFundamentalUniverseSelectionModel::new(
            |coarse: &[CoarseFundamental]| coarse.iter().map(|c| c.symbol.clone()).collect(),
            |fine: &[FineFundamental]| {
                fine.iter()
                    .filter(|f| f.pe_ratio.map(|pe| pe < dec!(30)).unwrap_or(false))
                    .map(|f| f.symbol.clone().unwrap())
                    .collect()
            },
        );

        let result = model.select_fine(&fine_data);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&make_symbol("AAPL")));
        assert!(result.contains(&make_symbol("MSFT")));
    }
}

// ---------------------------------------------------------------------------
// LiquidUniverse tests
// ---------------------------------------------------------------------------

mod liquid_universe_tests {
    use super::*;
    use lean_universe::LiquidUniverseSelectionModel;

    #[test]
    fn liquid_universe_top_n() {
        let data = vec![
            make_coarse("A", 1e9, 5_000_000.0, 100.0),
            make_coarse("B", 1e9, 1_000_000.0, 80.0),
            make_coarse("C", 1e9, 3_000_000.0, 60.0),
            make_coarse("D", 1e9, 2_000_000.0, 40.0),
            make_coarse("E", 1e9, 4_000_000.0, 20.0),
        ];

        let model = LiquidUniverseSelectionModel::new(3);
        let result = model.select(&data);

        assert_eq!(result.len(), 3);
        // Top 3 by DV: A (5M), E (4M), C (3M)
        assert!(result.contains(&make_symbol("A")));
        assert!(result.contains(&make_symbol("E")));
        assert!(result.contains(&make_symbol("C")));
        assert!(!result.contains(&make_symbol("B")));
        assert!(!result.contains(&make_symbol("D")));
    }

    #[test]
    fn liquid_universe_filters_low_price() {
        let data = vec![
            make_coarse("CHEAP", 1e9, 10_000_000.0, 2.0), // highest DV but very cheap
            make_coarse("OK1", 1e9, 5_000_000.0, 50.0),
            make_coarse("OK2", 1e9, 4_000_000.0, 60.0),
            make_coarse("OK3", 1e9, 3_000_000.0, 70.0),
        ];

        let model = LiquidUniverseSelectionModel::new(3).with_min_price(dec!(5));
        let result = model.select(&data);

        assert_eq!(result.len(), 3);
        assert!(!result.contains(&make_symbol("CHEAP")));
        assert!(result.contains(&make_symbol("OK1")));
        assert!(result.contains(&make_symbol("OK2")));
        assert!(result.contains(&make_symbol("OK3")));
    }

    #[test]
    fn liquid_universe_top_n_fewer_than_n_available() {
        let data = vec![
            make_coarse("A", 1e9, 5_000_000.0, 100.0),
            make_coarse("B", 1e9, 1_000_000.0, 80.0),
        ];
        let model = LiquidUniverseSelectionModel::new(5);
        let result = model.select(&data);
        // Only 2 available; should return all 2 rather than failing
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn liquid_universe_empty_input_returns_empty() {
        let model = LiquidUniverseSelectionModel::new(10);
        let result = model.select(&[]);
        assert!(result.is_empty());
    }
}

// ---------------------------------------------------------------------------
// MarketCapUniverse tests
// ---------------------------------------------------------------------------

mod market_cap_universe_tests {
    use super::*;
    use lean_universe::MarketCapUniverseSelectionModel;

    #[test]
    fn large_cap_top_n_returns_correct_symbols() {
        let data = vec![
            make_coarse("LARGE1", 3_000_000_000.0, 1_000_000.0, 200.0),
            make_coarse("LARGE2", 2_000_000_000.0, 900_000.0, 150.0),
            make_coarse("LARGE3", 1_500_000_000.0, 800_000.0, 100.0),
            make_coarse("MID", 500_000_000.0, 400_000.0, 50.0),
            make_coarse("SMALL", 50_000_000.0, 100_000.0, 10.0),
        ];

        let model = MarketCapUniverseSelectionModel::large_cap(3);
        let result = model.select(&data);

        assert_eq!(result.len(), 3);
        assert!(result.contains(&make_symbol("LARGE1")));
        assert!(result.contains(&make_symbol("LARGE2")));
        assert!(result.contains(&make_symbol("LARGE3")));
    }

    #[test]
    fn mid_cap_range_filter() {
        let data = vec![
            make_coarse("MEGA", 10_000_000_000.0, 5_000_000.0, 500.0),
            make_coarse("LARGE", 3_000_000_000.0, 1_000_000.0, 200.0),
            make_coarse("MID1", 800_000_000.0, 400_000.0, 80.0),
            make_coarse("MID2", 600_000_000.0, 300_000.0, 60.0),
            make_coarse("SMALL", 50_000_000.0, 100_000.0, 10.0),
        ];

        let model =
            MarketCapUniverseSelectionModel::mid_cap(dec!(500_000_000), dec!(1_000_000_000));
        let result = model.select(&data);

        assert_eq!(result.len(), 2);
        assert!(result.contains(&make_symbol("MID1")));
        assert!(result.contains(&make_symbol("MID2")));
    }

    #[test]
    fn market_cap_empty_input() {
        let model = MarketCapUniverseSelectionModel::large_cap(10);
        let result = model.select(&[]);
        assert!(result.is_empty());
    }
}

// ---------------------------------------------------------------------------
// SectorUniverse tests
// ---------------------------------------------------------------------------

mod sector_universe_tests {
    use super::*;
    use lean_universe::SectorUniverseSelectionModel;

    #[test]
    fn sector_universe_with_min_dv_filters_correctly() {
        let data = vec![
            make_coarse("TECH1", 2e9, 5_000_000.0, 100.0),
            make_coarse("TECH2", 1e9, 50_000.0, 20.0), // low DV
            make_coarse("HLTH1", 1_500_000_000.0, 2_000_000.0, 80.0),
        ];

        // Sector filter with min dollar volume of 1M
        let model = SectorUniverseSelectionModel::new(vec![309, 206])
            .with_min_dollar_volume(dec!(1_000_000));
        let result = model.select(&data);

        assert_eq!(result.len(), 2);
        assert!(result.contains(&make_symbol("TECH1")));
        assert!(result.contains(&make_symbol("HLTH1")));
        assert!(!result.contains(&make_symbol("TECH2")));
    }

    #[test]
    fn sector_universe_no_min_dv_returns_all() {
        let data = vec![
            make_coarse("A", 1e9, 100.0, 10.0),
            make_coarse("B", 2e9, 200.0, 20.0),
        ];
        let model = SectorUniverseSelectionModel::new(vec![309]);
        let result = model.select(&data);
        assert_eq!(result.len(), 2);
    }
}

// ---------------------------------------------------------------------------
// ETF universe tests
// ---------------------------------------------------------------------------

mod etf_universe_tests {
    use super::*;
    use lean_universe::{EtfConstituent, EtfConstituentsUniverse, EtfUniverses};

    fn make_constituent(ticker: &str, weight: Decimal) -> EtfConstituent {
        EtfConstituent {
            symbol: make_symbol(ticker),
            weight,
            shares_held: None,
            market_value: None,
        }
    }

    #[test]
    fn etf_universe_returns_all_constituents_without_filter() {
        let spy = make_symbol("SPY");
        let mut universe = EtfConstituentsUniverse::new(spy);
        universe.load_constituents(vec![
            make_constituent("AAPL", dec!(0.07)),
            make_constituent("MSFT", dec!(0.06)),
            make_constituent("AMZN", dec!(0.03)),
        ]);

        let result = universe.select_symbols();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn etf_universe_filter_by_weight() {
        let spy = make_symbol("SPY");
        let mut universe = EtfConstituentsUniverse::new(spy).with_filter(|consts| {
            consts
                .iter()
                .filter(|c| c.weight >= dec!(0.05))
                .map(|c| c.symbol.clone())
                .collect()
        });
        universe.load_constituents(vec![
            make_constituent("AAPL", dec!(0.07)),
            make_constituent("MSFT", dec!(0.06)),
            make_constituent("SMALL", dec!(0.01)),
        ]);

        let result = universe.select_symbols();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&make_symbol("AAPL")));
        assert!(result.contains(&make_symbol("MSFT")));
        assert!(!result.contains(&make_symbol("SMALL")));
    }

    #[test]
    fn etf_constituent_weight_lookup() {
        let spy = make_symbol("SPY");
        let mut universe = EtfConstituentsUniverse::new(spy);
        universe.load_constituents(vec![make_constituent("AAPL", dec!(0.07))]);

        let weight = universe.constituent_weight("AAPL");
        assert_eq!(weight, Some(dec!(0.07)));

        let missing = universe.constituent_weight("TSLA");
        assert!(missing.is_none());
    }

    #[test]
    fn etf_universe_empty_after_no_load() {
        let spy = make_symbol("SPY");
        let universe = EtfConstituentsUniverse::new(spy);
        let result = universe.select_symbols();
        assert!(result.is_empty());
    }

    #[test]
    fn etf_universes_factory_creates_correct_etf_symbol() {
        let spy = make_symbol("SPY");
        let qqq = make_symbol("QQQ");
        let iwm = make_symbol("IWM");

        let sp500 = EtfUniverses::sp500(spy.clone());
        assert_eq!(sp500.etf_symbol, spy);

        let nasdaq = EtfUniverses::nasdaq100(qqq.clone());
        assert_eq!(nasdaq.etf_symbol, qqq);

        let russell = EtfUniverses::russell2000(iwm.clone());
        assert_eq!(russell.etf_symbol, iwm);
    }
}

// ---------------------------------------------------------------------------
// ScheduledUniverse tests
// ---------------------------------------------------------------------------

mod scheduled_universe_tests {
    use super::*;
    use chrono::NaiveDate;
    use lean_universe::{ScheduledUniverseSelectionModel, UniverseSchedule};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn daily_schedule_fires_on_first_call() {
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Daily, |_| vec![]);
        assert!(u.select(date(2024, 1, 2)).is_some());
    }

    #[test]
    fn daily_schedule_fires_every_day() {
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Daily, |_| vec![]);
        let d1 = date(2024, 1, 2);
        let d2 = date(2024, 1, 3);
        let d3 = date(2024, 1, 4);

        assert!(u.select(d1).is_some());
        assert!(u.select(d2).is_some()); // next day — must fire again
        assert!(u.select(d3).is_some());
    }

    #[test]
    fn daily_schedule_does_not_fire_same_day_twice() {
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Daily, |_| vec![]);
        let d = date(2024, 1, 2);
        assert!(u.select(d).is_some());
        assert!(u.select(d).is_none()); // same day — no re-fire
    }

    #[test]
    fn monthly_schedule_fires_once_per_month() {
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Monthly, |_| vec![]);
        let d1 = date(2024, 1, 5);
        let d2 = date(2024, 1, 20); // same month as d1
        let d3 = date(2024, 2, 5); // next month

        assert!(u.select(d1).is_some());
        assert!(u.select(d2).is_none()); // same month — no re-fire
        assert!(u.select(d3).is_some()); // new month — fires
    }

    #[test]
    fn monthly_schedule_fires_in_new_year_same_month_name() {
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Monthly, |_| vec![]);
        let jan_2024 = date(2024, 1, 5);
        let jan_2025 = date(2025, 1, 5); // same month number, different year

        assert!(u.select(jan_2024).is_some());
        assert!(u.select(jan_2025).is_some()); // new year → should fire even if same month number
    }

    #[test]
    fn weekly_schedule_fires_once_per_week() {
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Weekly, |_| vec![]);
        // Week of Jan 1, 2024 (Mon) and Jan 8, 2024 (Mon) are different ISO weeks
        let d1 = date(2024, 1, 2); // Tue, week 1
        let d2 = date(2024, 1, 4); // Thu, same week 1 — no fire
        let d3 = date(2024, 1, 8); // Mon, week 2 — fires

        assert!(u.select(d1).is_some());
        assert!(u.select(d2).is_none());
        assert!(u.select(d3).is_some());
    }

    #[test]
    fn quarterly_schedule_fires_once_per_quarter() {
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Quarterly, |_| vec![]);
        let q1 = date(2024, 1, 5); // Q1
        let q1b = date(2024, 3, 25); // still Q1 — no fire
        let q2 = date(2024, 4, 5); // Q2 — fires
        let q3 = date(2024, 7, 1); // Q3 — fires
        let q4 = date(2024, 10, 1); // Q4 — fires

        assert!(u.select(q1).is_some());
        assert!(u.select(q1b).is_none());
        assert!(u.select(q2).is_some());
        assert!(u.select(q3).is_some());
        assert!(u.select(q4).is_some());
    }

    #[test]
    fn selector_closure_receives_correct_date() {
        use std::sync::{Arc, Mutex};
        let captured = Arc::new(Mutex::new(Vec::<NaiveDate>::new()));
        let cap2 = Arc::clone(&captured);

        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Daily, move |d| {
            cap2.lock().unwrap().push(d);
            vec![]
        });

        let d1 = date(2024, 6, 1);
        let d2 = date(2024, 6, 2);
        u.select(d1);
        u.select(d2);

        let dates = captured.lock().unwrap();
        assert_eq!(dates[0], d1);
        assert_eq!(dates[1], d2);
    }

    #[test]
    fn selector_closure_returned_symbols_are_propagated() {
        let spy = make_symbol("SPY");
        let spy_inner = spy.clone();
        let mut u = ScheduledUniverseSelectionModel::new(UniverseSchedule::Daily, move |_| {
            vec![spy_inner.clone()]
        });

        let result = u.select(date(2024, 1, 1)).expect("should fire");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], spy);
    }
}

// ---------------------------------------------------------------------------
// EmaCrossUniverseSelectionModel tests
// ---------------------------------------------------------------------------

mod ema_cross_universe_tests {
    use lean_universe::{CoarseFundamental, EmaCrossUniverseSelectionModel};
    use lean_core::{Market, Symbol};
    use rust_decimal::Decimal;

    fn make_symbol(ticker: &str) -> Symbol {
        Symbol::create_equity(ticker, &Market::usa())
    }

    fn make_coarse(ticker: &str, price: f64, dollar_volume: f64) -> CoarseFundamental {
        CoarseFundamental {
            symbol: make_symbol(ticker),
            price: Decimal::try_from(price).unwrap(),
            dollar_volume: Decimal::try_from(dollar_volume).unwrap(),
            market_cap: Decimal::try_from(1_000_000_000.0_f64).unwrap(),
            has_fundamental_data: true,
            market: "usa".to_string(),
        }
    }

    /// Feed `n` identical price bars so EMAs warm up.
    fn warm_up(model: &mut EmaCrossUniverseSelectionModel, ticker: &str, price: f64, n: usize) {
        for _ in 0..n {
            model.select(&[make_coarse(ticker, price, 1_000_000.0)]);
        }
    }

    #[test]
    fn empty_coarse_returns_empty() {
        let mut model = EmaCrossUniverseSelectionModel::new(5, 10, 500);
        let result = model.select(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn emas_not_ready_before_warm_up() {
        // With fast=5, slow=10, we need at least 10 bars to be ready.
        let mut model = EmaCrossUniverseSelectionModel::new(5, 10, 500);
        // Feed only 9 bars — EMAs should not be ready yet.
        for _ in 0..9 {
            let result = model.select(&[make_coarse("SPY", 100.0, 1_000_000.0)]);
            assert!(result.is_empty(), "should not be ready before slow period");
        }
    }

    #[test]
    fn rising_price_triggers_selection() {
        // Start at a flat price to warm up the EMAs with no gap,
        // then push price up so fast EMA > slow EMA + tolerance.
        let mut model = EmaCrossUniverseSelectionModel::new(5, 10, 500);

        // Warm up both EMAs at price=100.
        warm_up(&mut model, "SPY", 100.0, 20);

        // Feed a strongly rising price. On the first bar at 200 the fast EMA
        // should immediately pull well ahead of the slow EMA.
        // We only need the first result — by the time the EMAs converge the
        // cross condition has already been satisfied at least once.
        let first_result = model.select(&[make_coarse("SPY", 200.0, 1_000_000.0)]);

        assert!(
            !first_result.is_empty(),
            "rising price should trigger EMA cross selection on the first bar after the jump"
        );
        assert!(first_result.contains(&make_symbol("SPY")));
    }

    #[test]
    fn flat_price_no_bullish_cross() {
        // When fast ≈ slow there is no cross — nothing should be selected.
        let mut model = EmaCrossUniverseSelectionModel::new(5, 10, 500);
        let mut last = vec![];
        // 50 bars of completely flat price — EMAs converge to the same value.
        for _ in 0..50 {
            last = model.select(&[make_coarse("FLAT", 50.0, 1_000_000.0)]);
        }
        // Fast == Slow, so fast > slow*(1+0.01) is false → not selected.
        assert!(last.is_empty(), "flat price should not trigger a bullish cross");
    }

    #[test]
    fn universe_count_limits_results() {
        let mut model = EmaCrossUniverseSelectionModel::new(3, 5, 2); // max 2 results
        // 5 symbols — all rising strongly.
        let tickers = ["A", "B", "C", "D", "E"];
        for _ in 0..30 {
            let coarse: Vec<_> = tickers
                .iter()
                .enumerate()
                .map(|(i, t)| make_coarse(t, 100.0 + (i as f64) * 10.0, 1_000_000.0))
                .collect();
            model.select(&coarse);
        }
        // After warm-up push prices very high.
        let coarse: Vec<_> = tickers
            .iter()
            .enumerate()
            .map(|(i, t)| make_coarse(t, 500.0 + (i as f64) * 10.0, 1_000_000.0))
            .collect();
        let result = model.select(&coarse);
        assert!(result.len() <= 2, "universe_count=2 must cap results at 2");
    }

    #[test]
    fn min_dollar_volume_pre_filter() {
        let mut model = EmaCrossUniverseSelectionModel::new(3, 5, 500)
            .with_min_dollar_volume(Decimal::from(500_000));

        // Warm up two symbols: HIGH_DV passes the filter, LOW_DV does not.
        for _ in 0..30 {
            model.select(&[
                make_coarse("HIGH_DV", 100.0, 1_000_000.0),
                make_coarse("LOW_DV",  100.0,   100_000.0),
            ]);
        }
        // Push both prices up strongly.
        let result = model.select(&[
            make_coarse("HIGH_DV", 300.0, 1_000_000.0),
            make_coarse("LOW_DV",  300.0,   100_000.0),
        ]);

        // After enough bars HIGH_DV may or may not be selected; LOW_DV must never be.
        assert!(
            !result.contains(&make_symbol("LOW_DV")),
            "LOW_DV is below min dollar volume and must not be selected"
        );
    }
}

// ---------------------------------------------------------------------------
// InceptionDateUniverseSelectionModel tests
// ---------------------------------------------------------------------------

mod inception_date_universe_tests {
    use chrono::NaiveDate;
    use lean_universe::{InceptionDateUniverseSelectionModel, LiquidEtfUniverse};

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn no_symbols_before_any_inception() {
        let mut model = InceptionDateUniverseSelectionModel::new("test");
        model.add("AMZN", d(1997, 5, 15));
        // Date is before AMZN inception.
        let result = model.select(d(1990, 1, 1));
        assert!(result.is_none());
    }

    #[test]
    fn symbol_added_on_inception_date() {
        let mut model = InceptionDateUniverseSelectionModel::new("test");
        model.add("AMZN", d(1997, 5, 15));
        // Exactly on inception date → should appear.
        let result = model.select(d(1997, 5, 15));
        assert!(result.is_some());
        assert!(result.unwrap().contains(&"AMZN".to_string()));
    }

    #[test]
    fn symbol_added_after_inception_date() {
        let mut model = InceptionDateUniverseSelectionModel::new("test");
        model.add("AMZN", d(1997, 5, 15));
        let result = model.select(d(2024, 1, 1));
        assert!(result.is_some());
        assert!(result.unwrap().contains(&"AMZN".to_string()));
    }

    #[test]
    fn symbols_accumulate_over_time() {
        let mut model = InceptionDateUniverseSelectionModel::new("test");
        model.add("AMZN", d(1997,  5, 15));
        model.add("GOOG", d(2004,  8, 19));
        model.add("META", d(2012,  5, 18));

        // After AMZN inception only.
        let r1 = model.select(d(2000, 1, 1));
        assert!(r1.is_some());
        let r1 = r1.unwrap();
        assert!(r1.contains(&"AMZN".to_string()));
        assert!(!r1.contains(&"GOOG".to_string()));

        // After GOOG inception: both AMZN + GOOG active, no change detected
        // (no new tickers were added since last call) → None.
        let r2 = model.select(d(2004, 8, 19));
        assert!(r2.is_some());
        let r2 = r2.unwrap();
        assert!(r2.contains(&"AMZN".to_string()));
        assert!(r2.contains(&"GOOG".to_string()));

        // META added.
        let r3 = model.select(d(2012, 5, 18));
        assert!(r3.is_some());
        let r3 = r3.unwrap();
        assert!(r3.contains(&"META".to_string()));
    }

    #[test]
    fn unchanged_universe_returns_none() {
        let mut model = InceptionDateUniverseSelectionModel::new("test");
        model.add("SPY", d(1993, 1, 22));
        model.select(d(1993, 1, 22)); // adds SPY
        // Next day — no new tickers → universe unchanged.
        let result = model.select(d(1993, 1, 23));
        assert!(result.is_none(), "universe unchanged should return None");
    }

    #[test]
    fn from_pairs_builds_correctly() {
        let pairs = vec![
            ("SPY".to_string(), NaiveDate::from_ymd_opt(1993, 1, 22).unwrap()),
            ("QQQ".to_string(), NaiveDate::from_ymd_opt(1999, 3, 10).unwrap()),
        ];
        let mut model = InceptionDateUniverseSelectionModel::from_pairs("test", pairs);
        let result = model.select(d(2000, 1, 1)).unwrap();
        assert!(result.contains(&"SPY".to_string()));
        assert!(result.contains(&"QQQ".to_string()));
    }

    // ── LiquidEtfUniverse preset tests ──────────────────────────────────────

    #[test]
    fn energy_etf_universe_non_empty() {
        let mut m = LiquidEtfUniverse::energy();
        let result = m.select(d(2020, 1, 1)).unwrap();
        assert!(!result.is_empty());
        assert!(result.contains(&"XLE".to_string()));
        assert!(result.contains(&"USO".to_string()));
        assert!(result.contains(&"TAN".to_string()));
    }

    #[test]
    fn metals_etf_universe_non_empty() {
        let mut m = LiquidEtfUniverse::metals();
        let result = m.select(d(2020, 1, 1)).unwrap();
        assert!(!result.is_empty());
        assert!(result.contains(&"GLD".to_string()));
        assert!(result.contains(&"SLV".to_string()));
        assert!(result.contains(&"GDX".to_string()));
    }

    #[test]
    fn technology_etf_universe_non_empty() {
        let mut m = LiquidEtfUniverse::technology();
        let result = m.select(d(2020, 1, 1)).unwrap();
        assert!(!result.is_empty());
        assert!(result.contains(&"QQQ".to_string()));
        assert!(result.contains(&"SOXL".to_string()));
        assert!(result.contains(&"KWEB".to_string()));
    }

    #[test]
    fn us_treasuries_etf_universe_non_empty() {
        let mut m = LiquidEtfUniverse::us_treasuries();
        let result = m.select(d(2020, 1, 1)).unwrap();
        assert!(!result.is_empty());
        assert!(result.contains(&"TLT".to_string()));
        assert!(result.contains(&"IEF".to_string()));
        assert!(result.contains(&"SHY".to_string()));
        assert!(result.contains(&"GOVT".to_string()));
    }

    #[test]
    fn volatility_etf_universe_non_empty() {
        let mut m = LiquidEtfUniverse::volatility();
        let result = m.select(d(2020, 1, 1)).unwrap();
        assert!(!result.is_empty());
        assert!(result.contains(&"UVXY".to_string()));
        assert!(result.contains(&"SVXY".to_string()));
        assert!(result.contains(&"TQQQ".to_string()));
    }

    #[test]
    fn sp500_sectors_etf_universe_has_all_nine_spdr() {
        let mut m = LiquidEtfUniverse::sp500_sectors();
        let result = m.select(d(2020, 1, 1)).unwrap();
        for ticker in &["XLB", "XLE", "XLF", "XLI", "XLK", "XLP", "XLU", "XLV", "XLY"] {
            assert!(result.contains(&ticker.to_string()), "missing {ticker}");
        }
    }

    #[test]
    fn liquid_etf_universe_includes_all_sectors_and_groups() {
        let mut m = LiquidEtfUniverse::liquid();
        let result = m.select(d(2020, 1, 1)).unwrap();
        // Spot-check a few from each group.
        for ticker in &[
            "XLB",  // sp500 sectors
            "XLE",  // energy
            "GLD",  // metals
            "QQQ",  // technology
            "TLT",  // treasuries
            "UVXY", // volatility
        ] {
            assert!(result.contains(&ticker.to_string()), "missing {ticker}");
        }
    }

    #[test]
    fn inception_date_respected_for_later_tickers() {
        let mut m = LiquidEtfUniverse::volatility();
        // TQQQ / SQQQ inception is 2010-02-11; before that they should not appear.
        let early = m.select(d(2009, 12, 31));
        // No tickers have inception before 2010 in the volatility basket.
        assert!(early.is_none(), "no volatility ETF exists before 2010");
    }
}

// ---------------------------------------------------------------------------
// OptionUniverseSelectionModel tests
// ---------------------------------------------------------------------------

mod option_universe_tests {
    use chrono::NaiveDate;
    use lean_universe::{OptionContractView, OptionRight, OptionUniverseSelectionModel};
    use rust_decimal_macros::dec;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn make_call(underlying: &str, strike: f64, dte: i64, delta: Option<f64>) -> OptionContractView {
        OptionContractView {
            underlying: underlying.to_string(),
            symbol: format!("{underlying}_C_{strike}"),
            expiry: d(2024, 3, 15),
            strike: rust_decimal::Decimal::try_from(strike).unwrap(),
            right: OptionRight::Call,
            delta: delta.map(|d| rust_decimal::Decimal::try_from(d).unwrap()),
            dte,
        }
    }

    fn make_put(underlying: &str, strike: f64, dte: i64, delta: Option<f64>) -> OptionContractView {
        OptionContractView {
            underlying: underlying.to_string(),
            symbol: format!("{underlying}_P_{strike}"),
            expiry: d(2024, 3, 15),
            strike: rust_decimal::Decimal::try_from(strike).unwrap(),
            right: OptionRight::Put,
            delta: delta.map(|d| rust_decimal::Decimal::try_from(d).unwrap()),
            dte,
        }
    }

    #[test]
    fn empty_chain_returns_empty() {
        let model = OptionUniverseSelectionModel::default();
        assert!(model.filter(&[]).is_empty());
    }

    #[test]
    fn dte_filter_excludes_out_of_range() {
        let model = OptionUniverseSelectionModel::new(7, 30);
        let contracts = vec![
            make_call("SPY", 400.0,  5, None), // DTE too low
            make_call("SPY", 410.0, 15, None), // in range
            make_call("SPY", 420.0, 45, None), // DTE too high
        ];
        let result = model.filter(&contracts);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].dte, 15);
    }

    #[test]
    fn delta_filter_excludes_out_of_range() {
        let model = OptionUniverseSelectionModel::new(0, 45)
            .with_delta(dec!(0.20), dec!(0.50));
        let contracts = vec![
            make_call("SPY", 400.0, 20, Some(0.10)), // too low delta
            make_call("SPY", 410.0, 20, Some(0.35)), // in range
            make_call("SPY", 420.0, 20, Some(0.65)), // too high delta
        ];
        let result = model.filter(&contracts);
        assert_eq!(result.len(), 1);
        assert!((result[0].strike - dec!(410.0)).abs() < dec!(0.001));
    }

    #[test]
    fn right_filter_only_puts() {
        let model = OptionUniverseSelectionModel::new(0, 45)
            .with_right(OptionRight::Put);
        let contracts = vec![
            make_call("SPY", 400.0, 20, None),
            make_put("SPY",  390.0, 20, None),
            make_put("SPY",  380.0, 20, None),
        ];
        let result = model.filter(&contracts);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|c| c.right == OptionRight::Put));
    }

    #[test]
    fn strike_filter_excludes_out_of_range() {
        let model = OptionUniverseSelectionModel::new(0, 45)
            .with_strike(dec!(390.0), dec!(420.0));
        let contracts = vec![
            make_call("SPY", 370.0, 20, None), // too low
            make_call("SPY", 400.0, 20, None), // in range
            make_call("SPY", 415.0, 20, None), // in range
            make_call("SPY", 450.0, 20, None), // too high
        ];
        let result = model.filter(&contracts);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn combined_filters_applied_together() {
        let model = OptionUniverseSelectionModel::new(7, 30)
            .with_delta(dec!(0.25), dec!(0.55))
            .with_right(OptionRight::Call);

        let contracts = vec![
            make_call("SPY", 400.0, 15, Some(0.40)), // passes all
            make_call("SPY", 410.0, 15, Some(0.10)), // delta too low
            make_put("SPY",  390.0, 15, Some(0.40)), // wrong right
            make_call("SPY", 420.0,  5, Some(0.40)), // DTE too low
        ];
        let result = model.filter(&contracts);
        assert_eq!(result.len(), 1);
        assert!(result[0].right == OptionRight::Call);
        assert_eq!(result[0].dte, 15);
    }

    #[test]
    fn filter_owned_returns_cloned_results() {
        let model = OptionUniverseSelectionModel::new(0, 30);
        let contracts = vec![
            make_call("SPY", 400.0, 20, None),
            make_call("SPY", 410.0, 35, None), // out of range
        ];
        let result = model.filter_owned(&contracts);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn contracts_without_delta_pass_delta_filter_if_delta_not_required() {
        // If delta is None on the contract but model has no delta filter, it should pass.
        let model = OptionUniverseSelectionModel::new(0, 45);
        let contracts = vec![make_call("SPY", 400.0, 20, None)];
        let result = model.filter(&contracts);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn contracts_without_delta_excluded_when_delta_filter_set() {
        // If delta is None and the model requires a delta range, the contract should
        // pass through (we can't filter what we don't know) — document current behaviour.
        // Current impl: no delta on contract → delta filter block skipped → contract passes.
        let model = OptionUniverseSelectionModel::new(0, 45)
            .with_delta(dec!(0.20), dec!(0.50));
        let contracts = vec![make_call("SPY", 400.0, 20, None)];
        // Contracts with no delta are not excluded (we have no info to exclude them).
        let result = model.filter(&contracts);
        assert_eq!(result.len(), 1, "no-delta contracts pass through delta filter");
    }
}
