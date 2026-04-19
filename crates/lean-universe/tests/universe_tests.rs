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
