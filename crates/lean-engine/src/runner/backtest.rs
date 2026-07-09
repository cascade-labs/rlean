use crate::{
    algorithm_manager::{AlgorithmManager, OrderEventProcessing},
    data_feed::DataFeedContext,
    data_manager::DataManager,
    options_service::option_underlying_ticker,
    result_handler::ResultHandler,
    BacktestProgress, BacktestRunConfig, BacktestRunResult,
};
use anyhow::Result;
use lean_algorithm::lifecycle::{AlgorithmBridge, OptionSubscription};
use lean_core::{DataNormalizationMode, Market, MarketHoursDatabase, Resolution, Symbol};
use lean_data::{
    OptionChainFilterMetadata, OptionChainSubscriptionMetadata, SubscriptionDataConfig,
    SubscriptionDataKind, TradeBar,
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
    algorithm_manager.prepare_data_delivery(&subscriptions)?;
    let feed_subscriptions = lean_style_active_subscriptions(&subscriptions);
    let mut active_subscriptions = feed_subscriptions.clone();

    let feed_context = DataFeedContext::new(config.data_store.clone())
        .with_history_provider(config.history_provider.clone())
        .with_custom_data_sources(config.custom_data_sources.clone())
        .with_options(config.data_feed_options)
        .with_market_hours_database(market_hours_database.clone());

    // Warm the history-provider plugin's lazy `OnceLock` exactly once, up front
    // and on the blocking pool. The first `earliest_date()`/`get_history()`
    // triggers a plugin `dlopen` (which pulls in Python and other heavy
    // libraries); if that first load happens concurrently across the hundreds
    // of subscription producers spawned by universe selection, they all
    // serialize on the plugin `OnceLock` while occupying worker threads and
    // deadlock the runtime. Warming it here makes that load single and cheap.
    if let Some(provider) = config.history_provider.clone() {
        let _ = tokio::task::spawn_blocking(move || provider.earliest_date()).await;
    }

    let normal_start = lean_core::NanosecondTimestamp::from(
        start.and_hms_opt(0, 0, 0).expect("valid start of day"),
    );
    let normal_end = lean_core::NanosecondTimestamp::from(
        end.and_hms_opt(23, 59, 59).expect("valid end of day"),
    );

    let had_warmup = algorithm_manager.is_warming_up();
    if had_warmup {
        // Bar-count warmups (SetWarmUp(barCount, resolution)) must be sized
        // against each security's exchange calendar so that N trading sessions
        // are replayed — mirrors LEAN's HistoryRequestFactory.GetStartTimeAlgoTz.
        // Fall back to the calendar-span warmup_duration otherwise.
        let warmup_start = if let Some(bar_count) = algorithm_manager.warmup_bar_count() {
            warmup_start_from_bar_count(&market_hours_database, &subscriptions, bar_count, start)
                .map(|date| {
                    lean_core::NanosecondTimestamp::from(
                        date.and_hms_opt(0, 0, 0).expect("valid warmup start"),
                    )
                })
        } else {
            algorithm_manager
                .warmup_duration()
                .map(|duration| normal_start - duration)
        };
        tracing::debug!(
            warmup_bar_count = ?algorithm_manager.warmup_bar_count(),
            warmup_start = ?warmup_start.map(|ts| ts.to_string()),
            "computed warmup window"
        );
        if let Some(warmup_start) = warmup_start {
            let warmup_end = normal_start - lean_core::TimeSpan::from_nanos(1);
            if warmup_start <= warmup_end {
                let mut warmup_data_manager = DataManager::from_context(feed_context.clone());
                warmup_data_manager
                    .initialize_feed(&feed_subscriptions, warmup_start, warmup_end)
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
                feed_context.flush_market_cache_writes().await?;
                feed_context.flush_corporate_action_cache_writes().await?;
            }
        }
        algorithm_manager.warmup_finished(&mut services);
    } else {
        algorithm_manager.warmup_finished(&mut services);
    }

    let mut data_manager = DataManager::from_context(feed_context);
    data_manager
        .initialize_feed(&feed_subscriptions, normal_start, normal_end)
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

    // Optional incremental result streamer. When an output directory is set the
    // runner appends order events / trades and rewrites progress.json while the
    // backtest is still running, matching the live path's streaming sidecars.
    let mut stream_writer = config.output_dir.as_ref().map(|dir| {
        crate::runner::stream_writer::BacktestStreamWriter::new(dir.clone(), start, end)
    });
    let mut streamed_order_events = 0usize;
    let mut streamed_trades = 0usize;

    while let Some(slice) = data_manager.next_slice().await? {
        if !slice.has_data {
            continue;
        }
        let has_data_for_algorithm = slice_has_algorithm_data(&slice);
        let has_fill_data = slice_has_fill_data(&slice);
        if has_data_for_algorithm {
            market_slices_after_warmup += 1;
        }
        if has_data_for_algorithm {
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
        if has_fill_data {
            // Settle resting orders against this slice before delivering data, so
            // fills from prior bars are reflected when the strategy sees new data.
            algorithm_manager.process_order_events(OrderEventProcessing {
                slice: slice.as_ref(),
                option_chains: &option_chains,
                order_processor: order_processor.as_ref(),
                portfolio: portfolio.as_ref(),
                services: &mut services,
                all_order_events: &mut all_order_events,
                trade_builder: &mut trade_builder,
                completed_trades: &mut completed_trades,
            });
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
        sync_data_manager_subscriptions(
            &mut data_manager,
            &mut active_subscriptions,
            algorithm_manager.algorithm(),
            slice.time,
        )
        .await?;
        if has_fill_data {
            algorithm_manager.process_order_events(OrderEventProcessing {
                slice: slice.as_ref(),
                option_chains: &option_chains,
                order_processor: order_processor.as_ref(),
                portfolio: portfolio.as_ref(),
                services: &mut services,
                all_order_events: &mut all_order_events,
                trade_builder: &mut trade_builder,
                completed_trades: &mut completed_trades,
            });
            algorithm_manager.process_option_expirations(slice.as_ref(), &mut services);
        }
        let run_framework_this_slice =
            has_data_for_algorithm && !(had_warmup && market_slices_after_warmup == 1);
        if run_framework_this_slice {
            algorithm_manager.run_framework(slice.as_ref(), &mut services);
            sync_data_manager_subscriptions(
                &mut data_manager,
                &mut active_subscriptions,
                algorithm_manager.algorithm(),
                slice.time,
            )
            .await?;
            algorithm_manager.process_order_events(OrderEventProcessing {
                slice: slice.as_ref(),
                option_chains: &option_chains,
                order_processor: order_processor.as_ref(),
                portfolio: portfolio.as_ref(),
                services: &mut services,
                all_order_events: &mut all_order_events,
                trade_builder: &mut trade_builder,
                completed_trades: &mut completed_trades,
            });
            algorithm_manager.process_option_expirations(slice.as_ref(), &mut services);
        }
        algorithm_manager.end_time_step(&mut services);

        if has_data_for_algorithm {
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
            if let Some(writer) = stream_writer.as_mut() {
                if all_order_events.len() > streamed_order_events {
                    writer.append_order_events(&all_order_events[streamed_order_events..]);
                    streamed_order_events = all_order_events.len();
                }
                if completed_trades.len() > streamed_trades {
                    writer.append_trades(&completed_trades[streamed_trades..]);
                    streamed_trades = completed_trades.len();
                }
                writer.record_progress(
                    slice.time.date_utc(),
                    algorithm_manager.trading_days(),
                    portfolio_value,
                    all_order_events.len(),
                    completed_trades.len(),
                );
            }
            if let Some(benchmark_bar) = benchmark_subscription
                .as_ref()
                .and_then(|config| benchmark_bar(&slice, config))
            {
                result_handler.record_benchmark(slice.time, benchmark_bar.close);
            }
        }
    }
    data_manager.context().flush_market_cache_writes().await?;
    data_manager
        .context()
        .flush_corporate_action_cache_writes()
        .await?;

    // End-of-run summary: surface every Adjusted-mode equity that traded with no
    // factor rows, so silent unadjusted symbols (issue #27) are impossible to
    // miss even if the per-symbol WARNs scrolled past.
    let unadjusted = data_manager.context().take_unadjusted_equities();
    if !unadjusted.is_empty() {
        tracing::warn!(
            "{} equit{} ran WITHOUT factor files (split/dividend adjustment \
             skipped — prices were raw, corporate actions produce phantom P&L; \
             issue #27): {}",
            unadjusted.len(),
            if unadjusted.len() == 1 { "y" } else { "ies" },
            unadjusted.join(", "),
        );
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

    // Flush any trailing order events / trades and finalize the streaming
    // progress file before the batch report writers run.
    if let Some(writer) = stream_writer.as_mut() {
        if all_order_events.len() > streamed_order_events {
            writer.append_order_events(&all_order_events[streamed_order_events..]);
        }
        if completed_trades.len() > streamed_trades {
            writer.append_trades(&completed_trades[streamed_trades..]);
        }
        let final_value = algorithm_manager.portfolio_value();
        writer.mark_completed(
            trading_days,
            final_value,
            all_order_events.len(),
            completed_trades.len(),
        );
    }

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

fn slice_has_algorithm_data(slice: &lean_data::Slice) -> bool {
    !slice.bars.is_empty()
        || !slice.quote_bars.is_empty()
        || !slice.ticks.is_empty()
        || !slice.custom_data.is_empty()
        || !slice.order_books.is_empty()
        || !slice.perpetual_contexts.is_empty()
}

fn slice_has_fill_data(slice: &lean_data::Slice) -> bool {
    !slice.bars.is_empty()
        || !slice.quote_bars.is_empty()
        || !slice.ticks.is_empty()
        || !slice.order_books.is_empty()
        || !slice.perpetual_contexts.is_empty()
}

/// Backtests run the same subscription-shaped custom data flow as live mode:
/// custom data streams are active subscriptions from feed initialization, and
/// `set_custom_data_symbols` narrows their dynamic query as universe selection
/// changes. For custom streams fed by a custom universe, an unset dynamic
/// symbol filter means "universe not selected yet", so start with an explicit
/// empty filter instead of scanning the full custom dataset.
fn lean_style_active_subscriptions(
    subscriptions: &[SubscriptionDataConfig],
) -> Vec<SubscriptionDataConfig> {
    let custom_universe_sources = subscriptions
        .iter()
        .filter(|config| config.data_kind == SubscriptionDataKind::Universe)
        .filter_map(|config| {
            config
                .custom
                .as_ref()
                .map(|custom| custom.source_type.to_ascii_lowercase())
        })
        .collect::<std::collections::HashSet<_>>();

    subscriptions
        .iter()
        .cloned()
        .map(|mut config| {
            let Some(custom) = config.custom.as_mut() else {
                return config;
            };
            if config.data_kind == SubscriptionDataKind::Universe {
                return config;
            }
            if custom_universe_sources.contains(&custom.source_type.to_ascii_lowercase())
                && custom.dynamic_query.symbols.is_none()
            {
                custom.dynamic_query.symbols = Some(Vec::new());
            }
            config
        })
        .collect()
}

/// Compute the warmup start date for a bar-count warmup by walking back the
/// requested number of trading sessions on each subscribed security's exchange
/// calendar and taking the earliest (min) start — matching LEAN, which selects
/// the minimum start across warmup history requests. Internal/benchmark feeds
/// are ignored so they don't skew the window.
fn warmup_start_from_bar_count(
    market_hours_database: &MarketHoursDatabase,
    subscriptions: &[lean_data::SubscriptionDataConfig],
    bar_count: usize,
    normal_start_date: chrono::NaiveDate,
) -> Option<chrono::NaiveDate> {
    subscriptions
        .iter()
        .filter(|config| !config.is_internal_feed)
        .map(|config| {
            market_hours_database.warmup_start_date(&config.symbol, bar_count, normal_start_date)
        })
        .min()
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
    let current_subscriptions = lean_style_active_subscriptions(&subscriptions_with_option_chains(
        bridge.subscriptions(),
        &bridge.option_subscriptions(),
    ));
    let current = current_subscriptions
        .iter()
        .map(|config| config.unique_id())
        .collect::<std::collections::HashSet<_>>();

    let replaced = current_subscriptions
        .iter()
        .filter(|config| {
            active_subscriptions
                .iter()
                .find(|existing| existing.unique_id() == config.unique_id())
                .map(|existing| subscription_requires_stream_replacement(existing, config))
                .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();
    for config in &replaced {
        if let Some(custom) = &config.custom {
            tracing::debug!(
                "replacing custom subscription {}:{} with dynamic symbols={}",
                custom.source_type,
                custom.ticker,
                custom
                    .dynamic_query
                    .symbols
                    .as_ref()
                    .map(|symbols| symbols.len())
                    .unwrap_or(0)
            );
        }
        data_manager.remove_subscription(config);
    }

    let added = current_subscriptions
        .iter()
        .filter(|config| {
            !previous.contains(&config.unique_id())
                || replaced
                    .iter()
                    .any(|replacement| replacement.unique_id() == config.unique_id())
        })
        .cloned()
        .collect::<Vec<_>>();
    data_manager.add_subscriptions_async(added, start).await?;
    for config in active_subscriptions.iter() {
        if !current.contains(&config.unique_id()) {
            data_manager.remove_subscription(config);
        }
    }
    *active_subscriptions = current_subscriptions;
    Ok(())
}

fn subscription_requires_stream_replacement(
    previous: &lean_data::SubscriptionDataConfig,
    current: &lean_data::SubscriptionDataConfig,
) -> bool {
    match (previous.custom.as_ref(), current.custom.as_ref()) {
        (Some(previous_custom), Some(current_custom)) => {
            previous_custom.dynamic_query != current_custom.dynamic_query
                || previous_custom.config.query != current_custom.config.query
                || previous_custom.config.properties != current_custom.config.properties
        }
        _ => false,
    }
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
    use super::{
        lean_style_active_subscriptions, resolve_backtest_dates,
        subscription_requires_stream_replacement,
    };
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{DateTime, Market, Resolution, Symbol};
    use lean_data::{
        CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata, SubscriptionDataConfig,
    };
    use std::collections::HashMap;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn dt(year: i32, month: u32, day: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date(year, month, day).and_hms_opt(0, 0, 0).unwrap()))
    }

    fn custom_config(ticker: &str, resolution: Resolution) -> SubscriptionDataConfig {
        let symbol = Symbol::create_base("tradealert", ticker, &Market::usa());
        let metadata = CustomSubscriptionMetadata {
            source_type: "tradealert".to_string(),
            ticker: ticker.to_string(),
            config: CustomDataConfig {
                ticker: ticker.to_string(),
                source_type: "tradealert".to_string(),
                resolution,
                properties: HashMap::new(),
                query: CustomDataQuery::default(),
            },
            dynamic_query: CustomDataQuery::default(),
        };
        SubscriptionDataConfig::new_custom(symbol, resolution, metadata)
    }

    #[test]
    fn active_subscriptions_keep_empty_dynamic_custom_streams() {
        let snapshot_symbol = Symbol::create_base("tradealert", "snapshot", &Market::usa());
        let snapshot_metadata = CustomSubscriptionMetadata {
            source_type: "tradealert".to_string(),
            ticker: "snapshot".to_string(),
            config: CustomDataConfig {
                ticker: "snapshot".to_string(),
                source_type: "tradealert".to_string(),
                resolution: Resolution::Daily,
                properties: HashMap::new(),
                query: CustomDataQuery::default(),
            },
            dynamic_query: CustomDataQuery::default(),
        };
        let snapshot = SubscriptionDataConfig::new_custom_universe(
            snapshot_symbol,
            Resolution::Daily,
            snapshot_metadata,
        );
        let sweeps = custom_config("sweeps", Resolution::Minute);

        let active = lean_style_active_subscriptions(&[snapshot.clone(), sweeps.clone()]);

        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|config| {
            config
                .custom
                .as_ref()
                .map(|custom| custom.ticker == "sweeps")
                .unwrap_or(false)
                && config.resolution == Resolution::Minute
        }));
        let active_sweeps = active
            .iter()
            .find(|config| {
                config
                    .custom
                    .as_ref()
                    .map(|custom| custom.ticker == "sweeps")
                    .unwrap_or(false)
            })
            .expect("sweeps subscription");
        assert_eq!(
            active_sweeps
                .custom
                .as_ref()
                .expect("custom metadata")
                .dynamic_query
                .symbols,
            Some(Vec::new())
        );
    }

    #[test]
    fn dynamic_custom_query_change_requires_stream_replacement() {
        let previous = custom_config("sweeps", Resolution::Minute);
        let mut current = previous.clone();
        current
            .custom
            .as_mut()
            .expect("custom metadata")
            .dynamic_query
            .symbols = Some(vec!["NRG".to_string(), "FXI".to_string()]);

        assert!(subscription_requires_stream_replacement(
            &previous, &current
        ));
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
