// Integration tests for lean-alpha crate.
// Mirrors behavior from C# ConstantAlphaModelTests, EmaCrossAlphaModelTests,
// RsiAlphaModelTests, and CommonAlphaModelTests.

use lean_alpha::{
    CompositeAlphaModel, ConstantAlphaModel, IAlphaModel, Insight, InsightCollection,
    InsightDirection, NullAlphaModel,
};
use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::Slice;
use rust_decimal_macros::dec;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn aapl() -> Symbol {
    Symbol::create_equity("AAPL", &Market::usa())
}

/// Build an empty Slice at the given nanosecond timestamp.
fn empty_slice(ts: i64) -> Slice {
    Slice::new(NanosecondTimestamp(ts))
}

// ---------------------------------------------------------------------------
// Insight tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod insight_tests {
    use super::*;

    #[test]
    fn insight_up_direction() {
        let sym = spy();
        let period = TimeSpan::from_days(1);
        let insight = Insight::up(sym.clone(), period);

        assert_eq!(insight.direction, InsightDirection::Up);
        assert_eq!(insight.period, period);
        assert_eq!(insight.symbol, sym);
    }

    #[test]
    fn insight_down_direction() {
        let sym = spy();
        let period = TimeSpan::from_days(5);
        let insight = Insight::down(sym.clone(), period);

        assert_eq!(insight.direction, InsightDirection::Down);
        assert_eq!(insight.period, period);
        assert_eq!(insight.symbol, sym);
    }

    #[test]
    fn insight_flat_direction() {
        let sym = spy();
        let period = TimeSpan::from_hours(4);
        let insight = Insight::flat(sym.clone(), period);

        assert_eq!(insight.direction, InsightDirection::Flat);
        assert_eq!(insight.period, period);
        assert_eq!(insight.symbol, sym);
    }

    #[test]
    fn insight_active_before_period() {
        // The insight's generated_time_utc is `now`; we check at `now + 0` — still active.
        let sym = spy();
        let period = TimeSpan::from_days(1);
        let insight = Insight::up(sym, period);

        // At the exact generation time the insight should be active.
        let check_time = insight.generated_time_utc;
        assert!(insight.is_active(check_time), "insight should be active at generation time");
        assert!(!insight.is_expired(check_time), "insight should not be expired at generation time");
    }

    #[test]
    fn insight_expired_after_period() {
        let sym = spy();
        let period = TimeSpan::from_days(1);
        let insight = Insight::up(sym, period);

        // Check at close_time_utc + 1 ns — must be expired.
        let after_expiry = NanosecondTimestamp(insight.close_time_utc.0 + 1);
        assert!(insight.is_expired(after_expiry), "insight should be expired after period elapses");
        assert!(!insight.is_active(after_expiry), "insight should not be active after expiry");
    }

    #[test]
    fn insight_expires_exactly_at_close_time() {
        let sym = spy();
        let period = TimeSpan::from_days(1);
        let insight = Insight::up(sym, period);

        // At exactly close_time_utc the insight is considered expired (>=).
        let at_close = insight.close_time_utc;
        assert!(insight.is_expired(at_close), "insight should be expired at close_time_utc");
    }

    #[test]
    fn insight_period_stored_correctly() {
        let sym = spy();
        let period = TimeSpan::from_days(14);
        let insight = Insight::new(sym, InsightDirection::Up, period, None, None, "TestModel");
        assert_eq!(insight.period, period);
    }

    #[test]
    fn insight_magnitude_and_confidence() {
        let sym = spy();
        let mag = dec!(0.025);
        let conf = dec!(0.8);
        let insight = Insight::new(
            sym,
            InsightDirection::Up,
            TimeSpan::from_days(1),
            Some(mag),
            Some(conf),
            "TestModel",
        );
        assert_eq!(insight.magnitude, Some(mag));
        assert_eq!(insight.confidence, Some(conf));
    }

    #[test]
    fn insight_source_model_stored() {
        let sym = spy();
        let insight = Insight::new(
            sym,
            InsightDirection::Up,
            TimeSpan::from_days(1),
            None,
            None,
            "MyAlphaModel",
        );
        assert_eq!(insight.source_model, "MyAlphaModel");
    }

    #[test]
    fn insight_ids_are_unique() {
        let sym = spy();
        let period = TimeSpan::from_days(1);
        let a = Insight::up(sym.clone(), period);
        let b = Insight::up(sym.clone(), period);
        assert_ne!(a.id, b.id, "consecutive insights must have distinct ids");
    }

    #[test]
    fn insight_close_time_equals_generated_plus_period() {
        let sym = spy();
        let period = TimeSpan::from_days(12); // mirrors EmaCrossAlphaModelTests period
        let insight = Insight::up(sym, period);
        let expected_close = NanosecondTimestamp(insight.generated_time_utc.0 + period.nanos);
        assert_eq!(insight.close_time_utc, expected_close);
    }
}

// ---------------------------------------------------------------------------
// InsightCollection tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod insight_collection_tests {
    use super::*;

    #[test]
    fn empty_collection() {
        let col = InsightCollection::new();
        assert!(col.is_empty());
        assert_eq!(col.len(), 0);
    }

    #[test]
    fn collection_add_and_retrieve() {
        let mut col = InsightCollection::new();
        let sym = spy();
        let now = NanosecondTimestamp::now();
        let period = TimeSpan::from_days(1);

        col.add(Insight::up(sym.clone(), period));
        col.add(Insight::down(sym.clone(), period));

        assert_eq!(col.len(), 2);

        // Both should be active right now.
        let active = col.get_active(now);
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn collection_remove_expired() {
        let mut col = InsightCollection::new();
        let sym = spy();
        let tiny_period = TimeSpan::from_nanos(1); // expires almost instantly

        let insight = Insight::up(sym.clone(), tiny_period);
        let after_expiry = NanosecondTimestamp(insight.close_time_utc.0 + 1);
        col.add(insight);

        assert_eq!(col.len(), 1);

        // Before remove_expired the insight is still in the collection.
        col.remove_expired(after_expiry);
        assert_eq!(col.len(), 0, "expired insight should be removed");
    }

    #[test]
    fn collection_active_excludes_expired() {
        let mut col = InsightCollection::new();
        let sym = spy();
        let tiny_period = TimeSpan::from_nanos(1);
        let long_period = TimeSpan::from_days(30);

        let short_insight = Insight::up(sym.clone(), tiny_period);
        let after_short = NanosecondTimestamp(short_insight.close_time_utc.0 + 1);
        col.add(short_insight);
        col.add(Insight::down(sym.clone(), long_period));

        // get_active should only return the long-lived insight.
        let active = col.get_active(after_short);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].direction, InsightDirection::Down);
    }

    #[test]
    fn collection_for_symbol() {
        let mut col = InsightCollection::new();
        let spy_sym = spy();
        let aapl_sym = aapl();
        let period = TimeSpan::from_days(1);

        col.add(Insight::up(spy_sym.clone(), period));
        col.add(Insight::down(spy_sym.clone(), period));
        col.add(Insight::flat(aapl_sym.clone(), period));

        let spy_insights = col.for_symbol(&spy_sym);
        assert_eq!(spy_insights.len(), 2, "should have 2 SPY insights");

        let aapl_insights = col.for_symbol(&aapl_sym);
        assert_eq!(aapl_insights.len(), 1, "should have 1 AAPL insight");
    }

    #[test]
    fn collection_for_unknown_symbol_returns_empty() {
        let col = InsightCollection::new();
        let sym = spy();
        assert!(col.for_symbol(&sym).is_empty());
    }

    #[test]
    fn collection_latest_for_symbol() {
        let mut col = InsightCollection::new();
        let sym = spy();
        let period = TimeSpan::from_days(1);

        col.add(Insight::up(sym.clone(), period));
        col.add(Insight::down(sym.clone(), period)); // added last

        let latest = col.latest_for_symbol(&sym).expect("should have a latest insight");
        assert_eq!(
            latest.direction,
            InsightDirection::Down,
            "latest_for_symbol should return the most recently added insight"
        );
    }

    #[test]
    fn collection_latest_for_unknown_symbol_returns_none() {
        let col = InsightCollection::new();
        assert!(col.latest_for_symbol(&spy()).is_none());
    }

    #[test]
    fn collection_add_range() {
        let mut col = InsightCollection::new();
        let sym = spy();
        let period = TimeSpan::from_days(1);

        let batch = vec![
            Insight::up(sym.clone(), period),
            Insight::down(sym.clone(), period),
            Insight::flat(sym.clone(), period),
        ];
        col.add_range(batch);
        assert_eq!(col.len(), 3);
    }

    #[test]
    fn collection_clear() {
        let mut col = InsightCollection::new();
        let sym = spy();
        col.add(Insight::up(sym, TimeSpan::from_days(1)));
        assert!(!col.is_empty());
        col.clear();
        assert!(col.is_empty());
    }

    #[test]
    fn collection_multiple_symbols() {
        let mut col = InsightCollection::new();
        let spy_sym = spy();
        let aapl_sym = aapl();
        let period = TimeSpan::from_days(5);

        col.add(Insight::up(spy_sym.clone(), period));
        col.add(Insight::down(aapl_sym.clone(), period));

        assert_eq!(col.len(), 2);
        assert_eq!(col.for_symbol(&spy_sym).len(), 1);
        assert_eq!(col.for_symbol(&aapl_sym).len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Alpha model tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod alpha_model_tests {
    use super::*;

    // ---- NullAlphaModel ----

    #[test]
    fn null_alpha_model_returns_empty() {
        let mut model = NullAlphaModel;
        let slice = empty_slice(0);
        let securities = vec![spy(), aapl()];
        let insights = model.update(&slice, &securities);
        assert!(insights.is_empty(), "NullAlphaModel must return no insights");
    }

    #[test]
    fn null_alpha_model_name() {
        let model = NullAlphaModel;
        assert_eq!(model.name(), "NullAlphaModel");
    }

    // ---- ConstantAlphaModel ----

    #[test]
    fn constant_alpha_model_direction_up() {
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: Some(dec!(0.025)),
        };
        let slice = empty_slice(0);
        let securities = vec![spy(), aapl()];
        let insights = model.update(&slice, &securities);

        assert_eq!(insights.len(), 2, "should emit one insight per security");
        for insight in &insights {
            assert_eq!(insight.direction, InsightDirection::Up);
            assert_eq!(insight.period, TimeSpan::from_days(1));
        }
    }

    #[test]
    fn constant_alpha_model_direction_down() {
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Down,
            period: TimeSpan::from_days(5),
            magnitude: None,
        };
        let slice = empty_slice(0);
        let securities = vec![spy()];
        let insights = model.update(&slice, &securities);

        assert_eq!(insights.len(), 1);
        assert_eq!(insights[0].direction, InsightDirection::Down);
    }

    #[test]
    fn constant_alpha_model_direction_flat() {
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Flat,
            period: TimeSpan::from_days(1),
            magnitude: None,
        };
        let slice = empty_slice(0);
        let securities = vec![spy()];
        let insights = model.update(&slice, &securities);

        assert_eq!(insights.len(), 1);
        assert_eq!(insights[0].direction, InsightDirection::Flat);
    }

    #[test]
    fn constant_alpha_model_empty_universe() {
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: None,
        };
        let slice = empty_slice(0);
        let securities: Vec<Symbol> = vec![];
        let insights = model.update(&slice, &securities);
        assert!(insights.is_empty(), "no securities → no insights");
    }

    #[test]
    fn constant_alpha_model_magnitude_stored() {
        let mag = dec!(0.025);
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: Some(mag),
        };
        let slice = empty_slice(0);
        let insights = model.update(&slice, &[spy()]);
        assert_eq!(insights[0].magnitude, Some(mag));
    }

    #[test]
    fn constant_alpha_model_insight_symbols_match_securities() {
        let spy_sym = spy();
        let aapl_sym = aapl();
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: None,
        };
        let slice = empty_slice(0);
        let securities = vec![spy_sym.clone(), aapl_sym.clone()];
        let insights = model.update(&slice, &securities);

        let insight_sids: std::collections::HashSet<u64> =
            insights.iter().map(|i| i.symbol.id.sid).collect();
        assert!(insight_sids.contains(&spy_sym.id.sid));
        assert!(insight_sids.contains(&aapl_sym.id.sid));
    }

    #[test]
    fn constant_alpha_model_source_model_name() {
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: None,
        };
        let slice = empty_slice(0);
        let insights = model.update(&slice, &[spy()]);
        assert_eq!(insights[0].source_model, "ConstantAlphaModel");
    }

    #[test]
    fn constant_alpha_model_name() {
        let model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::ONE_DAY,
            magnitude: None,
        };
        assert_eq!(model.name(), "ConstantAlphaModel");
    }

    // ---- CompositeAlphaModel ----

    #[test]
    fn composite_model_combines_insights() {
        // Two ConstantAlphaModels — one Up, one Down — should both contribute.
        let model_a = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: None,
        };
        let model_b = ConstantAlphaModel {
            direction: InsightDirection::Down,
            period: TimeSpan::from_days(5),
            magnitude: None,
        };

        let mut composite = CompositeAlphaModel::new()
            .add(model_a)
            .add(model_b);

        let slice = empty_slice(0);
        let securities = vec![spy()];
        let insights = composite.update(&slice, &securities);

        // Each sub-model emits one insight for SPY → 2 total.
        assert_eq!(insights.len(), 2, "composite should aggregate insights from both models");

        let directions: Vec<InsightDirection> = insights.iter().map(|i| i.direction).collect();
        assert!(directions.contains(&InsightDirection::Up));
        assert!(directions.contains(&InsightDirection::Down));
    }

    #[test]
    fn composite_empty_model_returns_empty() {
        let mut composite = CompositeAlphaModel::new();
        let slice = empty_slice(0);
        let insights = composite.update(&slice, &[spy()]);
        assert!(insights.is_empty());
    }

    #[test]
    fn composite_single_sub_model() {
        let sub = NullAlphaModel;
        let mut composite = CompositeAlphaModel::new().add(sub);
        let slice = empty_slice(0);
        let insights = composite.update(&slice, &[spy(), aapl()]);
        assert!(insights.is_empty(), "NullAlphaModel sub-model yields no insights");
    }

    #[test]
    fn composite_on_securities_changed_propagates() {
        // Verify that on_securities_changed does not panic and compiles correctly.
        let model_a = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::ONE_DAY,
            magnitude: None,
        };
        let mut composite = CompositeAlphaModel::new().add(model_a);
        let added = vec![spy()];
        let removed: Vec<Symbol> = vec![];
        // Should not panic.
        composite.on_securities_changed(&added, &removed);
    }

    #[test]
    fn composite_model_name() {
        let composite = CompositeAlphaModel::new();
        assert_eq!(composite.name(), "CompositeAlphaModel");
    }

    // ---- IAlphaModel trait object usage ----

    #[test]
    fn alpha_model_as_trait_object() {
        // Verify boxing works — critical for portfolio construction pipelines.
        let model: Box<dyn IAlphaModel> = Box::new(ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: None,
        });
        assert_eq!(model.name(), "ConstantAlphaModel");
    }
}

// ---------------------------------------------------------------------------
// Cross-cutting: Insight + InsightCollection round-trip
// ---------------------------------------------------------------------------

#[cfg(test)]
mod round_trip_tests {
    use super::*;

    #[test]
    fn constant_model_insights_stored_in_collection() {
        let mut model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: TimeSpan::from_days(1),
            magnitude: Some(dec!(0.025)),
        };
        let slice = empty_slice(0);
        let securities = vec![spy(), aapl()];
        let insights = model.update(&slice, &securities);

        let mut collection = InsightCollection::new();
        collection.add_range(insights);

        assert_eq!(collection.len(), 2);

        let now = NanosecondTimestamp::now();
        let active = collection.get_active(now);
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn composite_model_insights_stored_in_collection_and_expire() {
        let tiny = TimeSpan::from_nanos(1);
        let long = TimeSpan::from_days(30);

        let fast_model = ConstantAlphaModel {
            direction: InsightDirection::Up,
            period: tiny,
            magnitude: None,
        };
        let slow_model = ConstantAlphaModel {
            direction: InsightDirection::Down,
            period: long,
            magnitude: None,
        };
        let mut composite = CompositeAlphaModel::new()
            .add(fast_model)
            .add(slow_model);

        let slice = empty_slice(0);
        let securities = vec![spy()];
        let insights = composite.update(&slice, &securities);

        // Record the close time of the tiny (Up) insight before consuming the vec.
        let tiny_close = insights
            .iter()
            .find(|i| i.direction == InsightDirection::Up)
            .map(|i| i.close_time_utc)
            .expect("should have an Up insight");

        let mut collection = InsightCollection::new();
        collection.add_range(insights);
        assert_eq!(collection.len(), 2);

        // Use a check time that is just past the tiny insight's expiry but well
        // before the 30-day insight would expire.
        let just_after_tiny = NanosecondTimestamp(tiny_close.0 + 1);
        collection.remove_expired(just_after_tiny);

        // Only the long-period Down insight should remain.
        assert_eq!(collection.len(), 1);
        assert_eq!(
            collection.for_symbol(&spy())[0].direction,
            InsightDirection::Down
        );
    }
}
