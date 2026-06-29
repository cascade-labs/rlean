use crate::{
    algorithm_manager::AlgorithmManager, data_feed::DataFeedContext, data_manager::DataManager,
    options_service::option_underlying_ticker,
    result_handler::ResultHandler, BacktestProgress, BacktestRunConfig, BacktestRunResult,
};
use anyhow::Result;
use lean_algorithm::lifecycle::{AlgorithmBridge, OptionSubscription};
use lean_core::{DataNormalizationMode, Market, MarketHoursDatabase, Resolution, Symbol};
use lean_data::{
    OptionChainFilterMetadata, OptionChainSubscriptionMetadata, SubscriptionDataConfig, TradeBar,
};
use lean_options::OptionChain;
use lean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
    OrderEvent,
};
use lean_statistics::{PortfolioStatistics, TradeBuilder};
use rust_decimal_macros::dec;
use std::sync::Arc;

/// Engine-owned backtest runner entry point.
///
/// All strategy languages enter through `lean_sdk::AlgorithmBridge`; language
/// crates do not provide runner futures or alternate loops.
pub async fn run_backtest<B>(bridge: B, config: BacktestRunConfig) -> Result<BacktestRunResult>
where
    B: AlgorithmBridge,
{
    let runtime_context = crate::AlgorithmRuntimeContext::new(
        config.data_root.clone(),
        config.data_store.clone(),
        config.history_provider.clone(),
        config.custom_data_sources.clone(),
        config.parameters.clone(),
    );
    run_backtest_with_runtime(bridge, config, runtime_context).await
}

pub async fn run_backtest_with_runtime<B>(
    bridge: B,
    config: BacktestRunConfig,
    runtime_context: crate::AlgorithmRuntimeContext,
) -> Result<BacktestRunResult>
where
    B: AlgorithmBridge,
{
    let engine_time = lean_core::DateTime::now();
    let mut services = crate::EngineAlgorithmServices::new(engine_time, runtime_context.clone());
    let mut algorithm_manager = AlgorithmManager::new(bridge, runtime_context);
    let market_hours_database = MarketHoursDatabase::global();
    algorithm_manager.set_market_hours_database(market_hours_database.clone());
    algorithm_manager.initialize(&mut services)?;

    let (start, end) = resolve_backtest_dates(
        config.start_date_override,
        config.end_date_override,
        algorithm_manager.start_date(),
        algorithm_manager.end_date(),
        engine_time.date_utc(),
    )?;
    let starting_cash = algorithm_manager.starting_cash();
    let benchmark_symbol = algorithm_manager.benchmark_symbol();
    let benchmark_subscription =
        benchmark_subscription_for_symbol(&benchmark_symbol, algorithm_manager.subscriptions());
    let subscriptions = subscriptions_with_benchmark(
        algorithm_manager.subscriptions(),
        benchmark_subscription.clone(),
    );
    let option_subscriptions = algorithm_manager.option_subscriptions();
    let subscriptions = subscriptions_with_option_chains(subscriptions, &option_subscriptions);
    let mut active_subscriptions = subscriptions.clone();
    algorithm_manager.prepare_data_delivery(&subscriptions)?;

    let feed_context = DataFeedContext::new(config.data_store.clone())
        .with_history_provider(config.history_provider.clone())
        .with_custom_data_sources(config.custom_data_sources.clone())
        .with_options(config.data_feed_options)
        .with_market_hours_database(market_hours_database);

    let normal_start = lean_core::NanosecondTimestamp::from(
        start.and_hms_opt(0, 0, 0).expect("valid start of day"),
    );
    let normal_end = lean_core::NanosecondTimestamp::from(
        end.and_hms_opt(23, 59, 59).expect("valid end of day"),
    );

    let had_warmup = algorithm_manager.is_warming_up();
    if had_warmup {
        if let Some(warmup_duration) = algorithm_manager.warmup_duration() {
            let warmup_start = normal_start - warmup_duration;
            let warmup_end = normal_start - lean_core::TimeSpan::from_nanos(1);
            if warmup_start <= warmup_end {
                let mut warmup_data_manager = DataManager::from_context(feed_context.clone());
                warmup_data_manager
                    .initialize_feed(&subscriptions, warmup_start, warmup_end)
                    .await?;
                while let Some(slice) = warmup_data_manager.next_slice().await? {
                    if !slice.has_data {
                        continue;
                    }
                    let slice = Arc::new(slice);
                    algorithm_manager.advance_frontier(slice.as_ref(), &mut services);
                    algorithm_manager.deliver_data(
                        lean_algorithm::algorithm::DataDeliveryPayload {
                            slice: slice.clone(),
                        },
                        &mut services,
                    );
                    if let Some(error) = algorithm_manager.algorithm().runtime_error() {
                        anyhow::bail!("Algorithm runtime error during warm-up: {error}");
                    }
                    algorithm_manager.advance_framework_warmup(slice.as_ref(), &mut services);
                    algorithm_manager.end_time_step(&mut services);
                }
            }
        }
        algorithm_manager.warmup_finished(&mut services);
    } else {
        algorithm_manager.warmup_finished(&mut services);
    }

    let mut data_manager = DataManager::from_context(feed_context);
    data_manager
        .initialize_feed(&subscriptions, normal_start, normal_end)
        .await?;

    let mut result_handler = ResultHandler::new();
    // Engine-owned order processing. The bridge exposes the algorithm's shared
    // `TransactionManager`; fills are settled against the bridge's portfolio so
    // any strategy language gets identical execution semantics.
    let transactions = algorithm_manager.transactions();
    let portfolio = algorithm_manager.portfolio();
    let order_processor = transactions.as_ref().map(|tm| {
        OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            tm.clone(),
        )
    });
    let mut all_order_events: Vec<OrderEvent> = Vec::new();
    let mut market_slices_after_warmup = 0usize;
    let mut trade_builder = TradeBuilder::new();
    let mut completed_trades = Vec::new();

    while let Some(slice) = data_manager.next_slice().await? {
        if !slice.has_data {
            continue;
        }
        let has_market_data = slice_has_market_data(&slice);
        if has_market_data {
            market_slices_after_warmup += 1;
        }
        if has_market_data {
            let new_trading_day = algorithm_manager.handle_new_trading_day(&slice, &mut services);
            let changes =
                algorithm_manager.apply_universe_selection(&slice, new_trading_day, &mut services);
            if changes.has_changes() {
                sync_data_manager_subscriptions(
                    &mut data_manager,
                    &mut active_subscriptions,
                    algorithm_manager.algorithm(),
                    slice.time,
                )
                .await?;
            }
        }

        let slice = Arc::new(slice);
        algorithm_manager.advance_frontier(slice.as_ref(), &mut services);
        let option_chains: Vec<(&str, &OptionChain)> = slice
            .option_chains
            .iter()
            .map(|(key, chain)| (key.as_str(), chain.as_ref()))
            .collect();
        if has_market_data {
            // Settle resting orders against this slice before delivering data, so
            // fills from prior bars are reflected when the strategy sees new data.
            algorithm_manager.process_order_events(
                slice.as_ref(),
                &option_chains,
                order_processor.as_ref(),
                portfolio.as_ref(),
                &mut services,
                &mut all_order_events,
                &mut trade_builder,
                &mut completed_trades,
            );
        }

        algorithm_manager.deliver_data(
            lean_algorithm::algorithm::DataDeliveryPayload {
                slice: slice.clone(),
            },
            &mut services,
        );
        if let Some(error) = algorithm_manager.algorithm().runtime_error() {
            anyhow::bail!("Algorithm runtime error: {error}");
        }
        if has_market_data {
            algorithm_manager.process_order_events(
                slice.as_ref(),
                &option_chains,
                order_processor.as_ref(),
                portfolio.as_ref(),
                &mut services,
                &mut all_order_events,
                &mut trade_builder,
                &mut completed_trades,
            );
            algorithm_manager.process_option_expirations(slice.as_ref(), &mut services);
        }
        let run_framework_this_slice =
            has_market_data && !(had_warmup && market_slices_after_warmup == 1);
        if run_framework_this_slice {
            algorithm_manager.run_framework(slice.as_ref(), &mut services);
            algorithm_manager.process_order_events(
                slice.as_ref(),
                &option_chains,
                order_processor.as_ref(),
                portfolio.as_ref(),
                &mut services,
                &mut all_order_events,
                &mut trade_builder,
                &mut completed_trades,
            );
            algorithm_manager.process_option_expirations(slice.as_ref(), &mut services);
        }
        algorithm_manager.end_time_step(&mut services);

        if has_market_data {
            let portfolio_value = algorithm_manager.portfolio_value();
            result_handler.record_equity(slice.time, portfolio_value);
            if let Some(progress) = &config.progress {
                progress(BacktestProgress {
                    current_date: slice.time.date_utc(),
                    start_date: start,
                    end_date: end,
                    trading_days: algorithm_manager.trading_days(),
                    portfolio_value: portfolio_value.to_string().parse::<f64>().unwrap_or(0.0),
                });
            }
            if let Some(benchmark_bar) = benchmark_subscription
                .as_ref()
                .and_then(|config| benchmark_bar(&slice, config))
            {
                result_handler.record_benchmark(slice.time, benchmark_bar.close);
            }
        }
    }

    algorithm_manager.finish(&mut services);

    let total_fees: f64 = all_order_events
        .iter()
        .map(|event| event.order_fee.to_string().parse::<f64>().unwrap_or(0.0))
        .sum();
    let final_orders = transactions
        .as_ref()
        .map(|tm| tm.get_all_orders())
        .unwrap_or_default();

    let trading_days = algorithm_manager.trading_days();
    result_handler.finalize(&completed_trades, trading_days, starting_cash);
    Ok(build_backtest_result(
        result_handler,
        trading_days,
        starting_cash,
        start,
        end,
        all_order_events,
        final_orders,
        total_fees,
        algorithm_manager,
        config,
    ))
}

fn slice_has_market_data(slice: &lean_data::Slice) -> bool {
    !slice.bars.is_empty()
        || !slice.quote_bars.is_empty()
        || !slice.ticks.is_empty()
        || !slice.custom_data.is_empty()
        || !slice.order_books.is_empty()
        || !slice.perpetual_contexts.is_empty()
}

fn resolve_backtest_dates(
    start_override: Option<chrono::NaiveDate>,
    end_override: Option<chrono::NaiveDate>,
    algorithm_start: lean_core::DateTime,
    algorithm_end: lean_core::DateTime,
    engine_date: chrono::NaiveDate,
) -> Result<(chrono::NaiveDate, chrono::NaiveDate)> {
    let start = start_override.unwrap_or_else(|| algorithm_start.date_utc());
    let end = end_override.unwrap_or_else(|| {
        if algorithm_end == lean_core::DateTime::MAX {
            engine_date
        } else {
            algorithm_end.date_utc()
        }
    });
    if start > end {
        anyhow::bail!("backtest start date {start} is after end date {end}");
    }
    Ok((start, end))
}

async fn sync_data_manager_subscriptions<B: AlgorithmBridge>(
    data_manager: &mut DataManager,
    active_subscriptions: &mut Vec<lean_data::SubscriptionDataConfig>,
    bridge: &B,
    start: lean_core::DateTime,
) -> anyhow::Result<()> {
    let previous = active_subscriptions
        .iter()
        .map(|config| config.unique_id())
        .collect::<std::collections::HashSet<_>>();
    let current_subscriptions =
        subscriptions_with_option_chains(bridge.subscriptions(), &bridge.option_subscriptions());
    let current = current_subscriptions
        .iter()
        .map(|config| config.unique_id())
        .collect::<std::collections::HashSet<_>>();

    for config in &current_subscriptions {
        if !previous.contains(&config.unique_id()) {
            data_manager
                .add_subscription_async(config.clone(), start)
                .await?;
        }
    }
    for config in active_subscriptions.iter() {
        if !current.contains(&config.unique_id()) {
            data_manager.remove_subscription(config);
        }
    }
    *active_subscriptions = current_subscriptions;
    Ok(())
}

pub(crate) fn benchmark_subscription_for_symbol(
    ticker: &str,
    subscriptions: Vec<SubscriptionDataConfig>,
) -> Option<SubscriptionDataConfig> {
    let ticker = ticker.trim().to_ascii_uppercase();
    if ticker.is_empty() {
        return None;
    }
    subscriptions
        .iter()
        .find(|config| config.symbol.permtick.eq_ignore_ascii_case(&ticker))
        .cloned()
        .or_else(|| {
            let symbol = Symbol::create_equity(&ticker, &Market::usa());
            let mut config = SubscriptionDataConfig::new_equity(
                symbol,
                Resolution::Daily,
                DataNormalizationMode::Adjusted,
            );
            config.is_internal_feed = true;
            Some(config)
        })
}

pub(crate) fn subscriptions_with_benchmark(
    mut subscriptions: Vec<SubscriptionDataConfig>,
    benchmark: Option<SubscriptionDataConfig>,
) -> Vec<SubscriptionDataConfig> {
    let Some(benchmark) = benchmark else {
        return subscriptions;
    };
    if !subscriptions
        .iter()
        .any(|config| config.unique_id() == benchmark.unique_id())
    {
        subscriptions.push(benchmark);
    }
    subscriptions
}

pub(crate) fn subscriptions_with_option_chains(
    mut subscriptions: Vec<SubscriptionDataConfig>,
    option_subscriptions: &[OptionSubscription],
) -> Vec<SubscriptionDataConfig> {
    for subscription in option_subscriptions {
        let metadata = OptionChainSubscriptionMetadata {
            canonical_permtick: subscription.canonical.permtick.to_string(),
            underlying_ticker: option_underlying_ticker(&subscription.canonical),
            filter: OptionChainFilterMetadata {
                min_strike_rank: subscription.filter.min_strike_rank,
                max_strike_rank: subscription.filter.max_strike_rank,
                min_expiry_days: subscription.filter.min_expiry_days,
                max_expiry_days: subscription.filter.max_expiry_days,
            },
        };
        let config = SubscriptionDataConfig::new_option_chain(
            subscription.canonical.clone(),
            subscription.resolution,
            metadata,
        );
        if !subscriptions
            .iter()
            .any(|existing| existing.unique_id() == config.unique_id())
        {
            subscriptions.push(config);
        }
    }
    subscriptions
}

fn benchmark_bar<'a>(
    slice: &'a lean_data::Slice,
    benchmark: &SubscriptionDataConfig,
) -> Option<&'a TradeBar> {
    slice.bars.get(&benchmark.symbol.id.sid)
}

#[allow(clippy::too_many_arguments)]
fn build_backtest_result(
    result_handler: ResultHandler,
    trading_days: i64,
    starting_cash: lean_core::Price,
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
    order_events: Vec<OrderEvent>,
    orders: Vec<lean_orders::Order>,
    total_fees: f64,
    algorithm_manager: AlgorithmManager<impl AlgorithmBridge>,
    _config: BacktestRunConfig,
) -> BacktestRunResult {
    let equity_curve: Vec<f64> = result_handler
        .equity_curve
        .values()
        .map(|value| value.to_string().parse::<f64>().unwrap_or(0.0))
        .collect();
    let daily_dates: Vec<String> = result_handler
        .equity_curve
        .keys()
        .map(|time| lean_core::NanosecondTimestamp(*time).date_utc().to_string())
        .collect();
    let benchmark_curve: Vec<f64> = result_handler
        .benchmark_curve
        .values()
        .map(|value| value.to_string().parse::<f64>().unwrap_or(0.0))
        .collect();
    let benchmark_dates: Vec<String> = result_handler
        .benchmark_curve
        .keys()
        .map(|time| lean_core::NanosecondTimestamp(*time).date_utc().to_string())
        .collect();
    let final_value = equity_curve
        .last()
        .copied()
        .unwrap_or_else(|| starting_cash.to_string().parse::<f64>().unwrap_or(0.0));
    let starting_cash_f64 = starting_cash.to_string().parse::<f64>().unwrap_or(0.0);
    let total_return = if starting_cash_f64 == 0.0 {
        0.0
    } else {
        (final_value - starting_cash_f64) / starting_cash_f64
    };

    let observed_trading_days = daily_dates
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len() as i64;
    let trading_days = trading_days.max(observed_trading_days);

    BacktestRunResult {
        trading_days,
        final_value,
        total_return,
        starting_cash: starting_cash_f64,
        total_fees,
        total_funding: 0.0,
        start_date,
        end_date,
        equity_curve,
        daily_dates,
        benchmark_curve,
        benchmark_dates,
        statistics: result_handler.portfolio_stats.unwrap_or_else(|| {
            PortfolioStatistics::compute(&[], &[], &[], trading_days, starting_cash, dec!(0))
        }),
        charts: algorithm_manager.charts(),
        order_events,
        orders,
        succeeded_data_requests: Vec::new(),
        failed_data_requests: Vec::new(),
        backtest_id: chrono::Utc::now().timestamp(),
        benchmark_symbol: algorithm_manager.benchmark_symbol(),
        alpha_analytics: algorithm_manager.alpha_analytics(),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_backtest_dates;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::DateTime;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn dt(year: i32, month: u32, day: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date(year, month, day).and_hms_opt(0, 0, 0).unwrap()))
    }

    #[test]
    fn unset_algorithm_end_date_resolves_to_engine_date() {
        let resolved =
            resolve_backtest_dates(None, None, dt(2022, 1, 1), DateTime::MAX, date(2026, 6, 28))
                .unwrap();

        assert_eq!(resolved, (date(2022, 1, 1), date(2026, 6, 28)));
    }

    #[test]
    fn explicit_end_date_and_cli_overrides_are_preserved() {
        let resolved = resolve_backtest_dates(
            Some(date(2023, 1, 1)),
            Some(date(2023, 12, 31)),
            dt(2022, 1, 1),
            dt(2026, 1, 1),
            date(2026, 6, 28),
        )
        .unwrap();

        assert_eq!(resolved, (date(2023, 1, 1), date(2023, 12, 31)));
    }

    #[test]
    fn start_after_end_is_rejected() {
        let error = resolve_backtest_dates(
            Some(date(2024, 1, 2)),
            Some(date(2024, 1, 1)),
            dt(2022, 1, 1),
            DateTime::MAX,
            date(2026, 6, 28),
        )
        .unwrap_err();

        assert!(error.to_string().contains("start date 2024-01-02"));
    }
}
