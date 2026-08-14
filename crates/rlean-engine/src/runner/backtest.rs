use crate::{
    algorithm_manager::{AlgorithmManager, OrderEventProcessing},
    data_feed::DataFeedContext,
    data_manager::DataManager,
    options_service::option_underlying_ticker,
    result_handler::ResultHandler,
    BacktestProgress, BacktestRunConfig, BacktestRunResult,
};
use anyhow::Result;
use rlean_algorithm::lifecycle::{AlgorithmBridge, OptionSubscription};
use rlean_core::{
    DataNormalizationMode, Market, MarketHoursDatabase, Resolution, RiskFreeInterestRateModel,
    SecurityType, Symbol, TickType,
};
use rlean_data::{
    OptionChainFilterMetadata, OptionChainSubscriptionMetadata, SubscriptionDataConfig,
    SubscriptionDataKind,
};
use rlean_data_tables::TradeBar;
use rlean_options::OptionChain;
use rlean_orders::{fill_model::ImmediateFillModel, order_processor::OrderProcessor, OrderEvent};
use rlean_statistics::{PortfolioStatistics, TradeBuilder};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use rust_decimal_macros::dec;
use std::sync::Arc;

/// Engine-owned backtest runner entry point.
///
/// All strategy languages enter through `rlean_sdk::AlgorithmBridge`; language
/// crates do not provide runner futures or alternate loops.
pub async fn run_backtest<B>(bridge: B, config: BacktestRunConfig) -> Result<BacktestRunResult>
where
    B: AlgorithmBridge,
{
    let runtime_context = crate::AlgorithmRuntimeContext::new(
        config.historical_provider.clone(),
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
    let engine_time = rlean_core::DateTime::now();
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
    let risk_free_interest_rate_model: Arc<dyn RiskFreeInterestRateModel> = Arc::new(
        crate::risk_free_interest_rate::load_risk_free_interest_rate_model(
            &config.historical_provider,
            end,
        )
        .await?,
    );
    if let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() {
        algorithm_state
            .lock()
            .expect("algorithm state poisoned")
            .set_risk_free_interest_rate_model(risk_free_interest_rate_model.clone());
    }
    let benchmark_symbol = algorithm_manager.benchmark_symbol();
    let benchmark_subscription =
        benchmark_subscription_for_symbol(&benchmark_symbol, algorithm_manager.subscriptions());
    let subscriptions = subscriptions_with_benchmark(
        algorithm_manager.subscriptions(),
        benchmark_subscription.clone(),
    );
    let option_subscriptions = algorithm_manager.option_subscriptions();
    let subscriptions: Vec<Arc<rlean_data::SubscriptionDataConfig>> =
        subscriptions.into_iter().map(Arc::new).collect();
    let subscriptions = subscriptions_with_option_chains(subscriptions, &option_subscriptions);
    // `prepare_data_delivery` receives the full pre-rewrite set, matching the
    // original ordering (it runs before the custom-query rewrite below).
    let subscriptions_owned: Vec<rlean_data::SubscriptionDataConfig> = subscriptions
        .iter()
        .map(|config| (**config).clone())
        .collect();
    algorithm_manager.prepare_data_delivery(&subscriptions_owned)?;
    let feed_subscriptions = lean_style_active_subscriptions(&subscriptions);
    // Owned copy of the feed set for `initialize_feed`, which takes owned configs.
    let feed_subscriptions_owned: Vec<rlean_data::SubscriptionDataConfig> = feed_subscriptions
        .iter()
        .map(|config| (**config).clone())
        .collect();
    // Seed the sync state with the initial feed set. `last_version = None` forces
    // the first sync to run and reconcile it against the bridge's live set.
    let mut sync_state = SubscriptionSyncState {
        active: feed_subscriptions,
        last_version: None,
    };

    let feed_context = DataFeedContext::new(config.historical_provider.clone())
        .with_options(config.data_feed_options)
        .with_market_hours_database(market_hours_database.clone());

    let normal_start = algorithm_manager.local_midnight_utc(start)?;
    let normal_end = algorithm_manager.local_midnight_utc(
        end.succ_opt()
            .ok_or_else(|| anyhow::anyhow!("backtest end date has no following day"))?,
    )? - rlean_core::TimeSpan::from_nanos(1);

    let had_warmup = algorithm_manager.is_warming_up();
    if had_warmup {
        // Bar-count warmups (SetWarmUp(barCount, resolution)) must be sized
        // against each security's exchange calendar so that N trading sessions
        // are replayed — mirrors LEAN's HistoryRequestFactory.GetStartTimeAlgoTz.
        // Fall back to the calendar-span warmup_duration otherwise.
        let warmup_start = if let Some(bar_count) = algorithm_manager.warmup_bar_count() {
            warmup_start_from_bar_count(
                &market_hours_database,
                &subscriptions_owned,
                bar_count,
                start,
            )
            .map(|date| algorithm_manager.local_midnight_utc(date))
            .transpose()?
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
            let warmup_end = normal_start - rlean_core::TimeSpan::from_nanos(1);
            if warmup_start <= warmup_end {
                let warmup_subscriptions = warmup_subscriptions_at_resolution(
                    &feed_subscriptions_owned,
                    algorithm_manager.warmup_resolution(),
                );
                let mut warmup_data_manager = DataManager::from_context(feed_context.clone());
                warmup_data_manager
                    .initialize_feed(&warmup_subscriptions, warmup_start, warmup_end)
                    .await?;
                while let Some(mut slice) = warmup_data_manager.next_slice().await? {
                    algorithm_manager.include_active_option_chains(&mut slice);
                    apply_risk_free_rate_to_option_chains(
                        &mut slice,
                        risk_free_interest_rate_model.as_ref(),
                    );
                    if !slice.has_data {
                        continue;
                    }
                    algorithm_manager.process_warmup_slice(Arc::new(slice), &mut services)?;
                }
            }
        }
        algorithm_manager.warmup_finished(&mut services);
    } else {
        algorithm_manager.warmup_finished(&mut services);
    }
    algorithm_manager.prime_scheduled_events(normal_start - rlean_core::TimeSpan::from_nanos(1));

    let mut data_manager = DataManager::from_context(feed_context);
    data_manager
        .initialize_feed(&feed_subscriptions_owned, normal_start, normal_end)
        .await?;

    let mut result_handler = ResultHandler::new();
    // Engine-owned order processing. The bridge exposes the algorithm's shared
    // `TransactionManager`; fills are settled against the bridge's portfolio so
    // any strategy language gets identical execution semantics.
    let transactions = algorithm_manager.transactions();
    let portfolio = algorithm_manager.portfolio();
    let algorithm_state = algorithm_manager.algorithm.algorithm_state();
    let order_processor = transactions.as_ref().map(|tm| {
        let slippage: Box<dyn rlean_orders::SlippageModel> = algorithm_state
            .clone()
            .map(|state| {
                Box::new(crate::algorithm_manager::SecuritySlippageModel::new(state))
                    as Box<dyn rlean_orders::SlippageModel>
            })
            .unwrap_or_else(|| Box::new(rlean_orders::NullSlippageModel));
        OrderProcessor::new(Box::new(ImmediateFillModel::new(slippage)), tm.clone())
    });
    let mut all_order_events: Vec<OrderEvent> = Vec::new();
    let mut market_slices_after_warmup = 0usize;
    let mut trade_builder = TradeBuilder::new();
    let mut completed_trades = Vec::new();
    let mut insight_events = Vec::new();
    let mut next_margin_call_time = rlean_core::DateTime::EPOCH;

    let mut catalog_order_events = 0usize;
    let mut catalog_trades = 0usize;
    let mut catalog_progress_date = None;

    while let Some(mut slice) = data_manager.next_slice().await? {
        // Match C# LEAN AlgorithmManager.Run: once a backtest portfolio is
        // bankrupt, stop before processing the next time slice. Continuing can
        // manufacture fills, fees, and statistics for capital that no longer
        // exists.
        if portfolio_is_bankrupt(algorithm_manager.portfolio_value()) {
            tracing::error!(
                "AlgorithmManager.Run(): Portfolio value is less than or equal to zero, stopping algorithm."
            );
            break;
        }
        algorithm_manager.include_active_option_chains(&mut slice);
        apply_risk_free_rate_to_option_chains(&mut slice, risk_free_interest_rate_model.as_ref());
        if !slice.has_data {
            continue;
        }
        let has_data_for_algorithm = slice_has_algorithm_data(&slice);
        let has_fill_data = slice_has_fill_data(&slice);
        if has_data_for_algorithm {
            market_slices_after_warmup += 1;
        }
        if has_data_for_algorithm {
            let new_trading_day =
                algorithm_manager.handle_new_trading_day(&slice, &mut services)?;
            let changes =
                algorithm_manager.apply_universe_selection(&slice, new_trading_day, &mut services);
            if changes.has_changes() {
                sync_data_manager_subscriptions(
                    &mut data_manager,
                    &mut sync_state,
                    algorithm_manager.algorithm(),
                    benchmark_subscription.as_ref(),
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

        // C# LEAN AlgorithmManager updates security prices and processes
        // synchronous fills before advancing the real-time handler. Scheduled
        // callbacks must therefore observe the current slice's prices and
        // settled holdings, not the preceding slice.
        let _ = algorithm_manager.scan_scheduled_events(slice.time)?;
        if has_fill_data {
            // BacktestingRealTimeHandler callbacks submit synchronous market
            // orders in LEAN. Settle those orders before OnData/framework so a
            // scheduled liquidation is authoritative for the remainder of the
            // time step.
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

        // Match C# LEAN AlgorithmManager and
        // InsufficientBuyingPowerForAutomaticExerciseRegressionAlgorithm:
        // scan the portfolio every five
        // minutes after scheduled events and synchronously execute generated
        // margin-call orders before OnData. This is particularly important
        // after physical option exercise creates an underlying position.
        if slice.time >= next_margin_call_time {
            next_margin_call_time = slice.time + rlean_core::TimeSpan::from_nanos(300_000_000_000);
            let (requests, issue_warning, exchanges_open) =
                algorithm_manager.margin_call_requests(slice.time);
            let mut executed = 0usize;
            if !requests.is_empty() && exchanges_open && has_fill_data {
                algorithm_manager.notify_margin_call(&requests, slice.time, &mut services);
                for request in &requests {
                    if algorithm_manager
                        .margin_remaining()
                        .is_some_and(|remaining| remaining >= rust_decimal::Decimal::ZERO)
                    {
                        break;
                    }
                    let Some(order_id) = algorithm_manager.submit_margin_call_order(request) else {
                        continue;
                    };
                    let event_count_before = all_order_events.len();
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
                    if all_order_events[event_count_before..]
                        .iter()
                        .any(|event| event.order_id == order_id && event.is_fill())
                    {
                        executed += 1;
                    }
                }
            }
            if executed == 0 && issue_warning {
                algorithm_manager.notify_margin_call_warning(&mut services);
            }
        }

        algorithm_manager.deliver_data(
            rlean_algorithm::algorithm::DataDeliveryPayload {
                slice: slice.clone(),
            },
            &mut services,
        );
        if let Some(error) = algorithm_manager.algorithm().runtime_error() {
            anyhow::bail!("Algorithm runtime error: {error}");
        }
        sync_data_manager_subscriptions(
            &mut data_manager,
            &mut sync_state,
            algorithm_manager.algorithm(),
            benchmark_subscription.as_ref(),
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
        let run_framework_this_slice = should_run_framework_on_slice(
            has_data_for_algorithm,
            had_warmup,
            market_slices_after_warmup,
            slice.as_ref(),
        );
        if run_framework_this_slice {
            algorithm_manager.run_framework(slice.as_ref(), &mut services);
            sync_data_manager_subscriptions(
                &mut data_manager,
                &mut sync_state,
                algorithm_manager.algorithm(),
                benchmark_subscription.as_ref(),
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
        insight_events.extend(
            algorithm_manager
                .framework()
                .lock()
                .expect("framework state poisoned")
                .take_insight_events(),
        );
        algorithm_manager.end_time_step(&mut services);

        if has_data_for_algorithm {
            let portfolio_value = algorithm_manager.portfolio_value();
            let current_date = algorithm_manager
                .current_date()
                .expect("a processed slice must establish an algorithm-local date");
            if !algorithm_manager.is_trading_date(current_date) {
                continue;
            }
            result_handler.record_daily_equity(current_date, portfolio_value);
            let progress_update = BacktestProgress {
                current_date,
                start_date: start,
                end_date: end,
                trading_days: algorithm_manager.trading_days(),
                starting_cash: starting_cash.to_string().parse::<f64>().unwrap_or(0.0),
                portfolio_value: portfolio_value.to_string().parse::<f64>().unwrap_or(0.0),
            };
            if let Some(progress) = &config.progress {
                progress(progress_update.clone());
            }
            let has_catalog_events = all_order_events.len() > catalog_order_events;
            let has_catalog_trades = completed_trades.len() > catalog_trades;
            let is_new_catalog_date = catalog_progress_date != Some(progress_update.current_date);
            if has_catalog_events || has_catalog_trades || is_new_catalog_date {
                if let Some(sender) = &config.stream_updates {
                    sender
                        .send(crate::BacktestStreamUpdate {
                            progress: progress_update,
                            record_daily_progress: is_new_catalog_date,
                            order_events: all_order_events[catalog_order_events..].to_vec(),
                            trades: completed_trades[catalog_trades..].to_vec(),
                            insight_events: Vec::new(),
                            checkpoint_json: None,
                            insight_state_json: None,
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("backtest run-catalog stream closed"))?;
                    catalog_order_events = all_order_events.len();
                    catalog_trades = completed_trades.len();
                    catalog_progress_date = Some(current_date);
                }
            }
            if let Some(benchmark_bar) = benchmark_subscription
                .as_ref()
                .and_then(|config| benchmark_bar(&slice, config))
            {
                result_handler.record_benchmark(slice.time, benchmark_bar.close);
            }
        }
    }

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
    insight_events.extend(
        algorithm_manager
            .framework()
            .lock()
            .expect("framework state poisoned")
            .take_insight_events(),
    );

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

    result_handler.finalize(
        &completed_trades,
        trading_days,
        starting_cash,
        risk_free_interest_rate_model.as_ref(),
    );
    Ok(build_backtest_result(
        result_handler,
        trading_days,
        starting_cash,
        start,
        end,
        all_order_events,
        final_orders,
        completed_trades,
        insight_events,
        total_fees,
        algorithm_manager,
        config,
    ))
}

fn portfolio_is_bankrupt(portfolio_value: Decimal) -> bool {
    portfolio_value <= Decimal::ZERO
}

pub(crate) fn apply_risk_free_rate_to_option_chains(
    slice: &mut rlean_data::Slice,
    model: &dyn RiskFreeInterestRateModel,
) {
    if slice.option_chains.is_empty() {
        return;
    }
    let risk_free_rate = model.get_interest_rate(slice.time).to_f64().unwrap_or(0.0);
    let price_model = rlean_options::BlackScholesPriceModel;
    for chain in slice.option_chains.values_mut() {
        let chain = Arc::make_mut(chain);
        let underlying_sid = chain
            .contracts
            .values()
            .find_map(|contract| contract.symbol.underlying.as_ref())
            .map(|symbol| symbol.id.sid);
        if let Some(underlying_sid) = underlying_sid {
            let underlying_price = slice
                .quote_bars
                .get(&underlying_sid)
                .map(|quote| quote.mid_close())
                .filter(|price| *price > dec!(0))
                .or_else(|| slice.bars.get(&underlying_sid).map(|bar| bar.close))
                .filter(|price| *price > dec!(0))
                .unwrap_or(chain.underlying_price);
            chain.underlying_price = underlying_price;
        }
        for contract in chain.contracts.values_mut() {
            contract.data.underlying_last_price = chain.underlying_price;
            if let Some(quote) = slice.quote_bars.get(&contract.symbol.id.sid) {
                contract.data.bid_price =
                    quote.bid.as_ref().map(|bar| bar.close).unwrap_or_default();
                contract.data.ask_price =
                    quote.ask.as_ref().map(|bar| bar.close).unwrap_or_default();
                contract.data.bid_size = quote.last_bid_size.to_i64().unwrap_or_default();
                contract.data.ask_size = quote.last_ask_size.to_i64().unwrap_or_default();
            }
            if let Some(bar) = slice.bars.get(&contract.symbol.id.sid) {
                contract.data.last_price = bar.close;
                contract.data.volume = bar.volume.to_i64().unwrap_or_default();
            }
            rlean_options::evaluate_contract_with_market_iv(
                &price_model,
                contract,
                slice.time,
                risk_free_rate,
                0.0,
            );
        }
    }
}

fn slice_has_algorithm_data(slice: &rlean_data::Slice) -> bool {
    !slice.bars.is_empty()
        || !slice.quote_bars.is_empty()
        || !slice.ticks.is_empty()
        || !slice.custom_data.is_empty()
        // LEAN's chain-universe data is itself an algorithm/universe event.
        // It commonly arrives at exchange-local midnight before the first
        // underlying bar and must run selection immediately so the concrete
        // contract streams exist when the session opens.
        || !slice.option_chains.is_empty()
        || !slice.order_books.is_empty()
}

fn should_run_framework_on_slice(
    has_data_for_algorithm: bool,
    had_warmup: bool,
    market_slices_after_warmup: usize,
    slice: &rlean_data::Slice,
) -> bool {
    has_data_for_algorithm
        && !(had_warmup
            && market_slices_after_warmup == 1
            // The historical warm-up range is inclusive at its boundary, so
            // its first repeated market-data Slice is suppressed.
            // Chain-universe selection is different: its exchange-local
            // midnight Slice was not processed by the normal feed and must
            // reach the alpha so it can retain today's candidates.
            && slice.option_chains.is_empty())
}

fn slice_has_fill_data(slice: &rlean_data::Slice) -> bool {
    !slice.bars.is_empty()
        || !slice.quote_bars.is_empty()
        || !slice.ticks.is_empty()
        || !slice.order_books.is_empty()
}

/// Backtests run the same subscription-shaped custom data flow as live mode:
/// custom data streams are active subscriptions from feed initialization, and
/// `set_custom_data_symbols` narrows their dynamic query as universe selection
/// changes. For custom streams fed by a custom universe, an unset dynamic
/// symbol filter means "universe not selected yet", so start with an explicit
/// empty filter instead of scanning the full custom dataset.
fn lean_style_active_subscriptions(
    subscriptions: &[Arc<SubscriptionDataConfig>],
) -> Vec<Arc<SubscriptionDataConfig>> {
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
        .map(|config| {
            // Only the custom, non-universe streams fed by a custom universe with
            // an unset dynamic symbol filter are rewritten. Everything else is
            // passed through by cloning the shared `Arc` (a refcount bump, no
            // deep copy) — this replaces the second full deep-clone of the whole
            // list that dominated the sync path (issue #64).
            let needs_empty_filter = config.data_kind != SubscriptionDataKind::Universe
                && config.custom.as_ref().is_some_and(|custom| {
                    custom.dynamic_query.symbols.is_none()
                        && custom_universe_sources
                            .contains(&custom.source_type.to_ascii_lowercase())
                });
            if !needs_empty_filter {
                return config.clone();
            }
            let mut rewritten = (**config).clone();
            if let Some(custom) = rewritten.custom.as_mut() {
                custom.dynamic_query.symbols = Some(Vec::new());
            }
            Arc::new(rewritten)
        })
        .collect()
}

/// Compute the warmup start date for a bar-count warmup by walking back the
/// requested number of trading sessions on each subscribed security's exchange
/// calendar and taking the earliest (min) start — matching LEAN, which selects
/// the minimum start across warmup history requests. Internal/benchmark feeds
/// are ignored so they don't skew the window.
pub(crate) fn warmup_start_from_bar_count(
    market_hours_database: &MarketHoursDatabase,
    subscriptions: &[rlean_data::SubscriptionDataConfig],
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

/// C# LEAN creates warm-up history requests at `Settings.WarmupResolution`
/// when the caller supplied one. Do the same without mutating the algorithm's
/// live subscriptions (for example, a minute SPY clock is replayed as Daily
/// during `SetWarmUp(410, Resolution.Daily)`).
pub(crate) fn warmup_subscriptions_at_resolution(
    subscriptions: &[rlean_data::SubscriptionDataConfig],
    resolution: Option<Resolution>,
) -> Vec<rlean_data::SubscriptionDataConfig> {
    subscriptions
        .iter()
        .filter_map(|config| {
            let mut warmup = config.clone();
            if let Some(resolution) = resolution {
                warmup.resolution = resolution;
                if let Some(custom) = warmup.custom.as_mut() {
                    custom.config.resolution = resolution;
                }
            }
            lean_data_configuration_is_valid(&warmup).then_some(warmup)
        })
        .collect()
}

/// Mirrors LEAN's `LeanData.IsValidConfiguration` constraint for equity bars:
/// hourly and daily equity subscriptions are trade-only. Minute and finer
/// equity subscriptions may carry both trades and quotes.
fn lean_data_configuration_is_valid(config: &SubscriptionDataConfig) -> bool {
    !(config.symbol.security_type() == SecurityType::Equity
        && matches!(config.resolution, Resolution::Hour | Resolution::Daily)
        && config.tick_type == TickType::Quote)
}

fn resolve_backtest_dates(
    start_override: Option<chrono::NaiveDate>,
    end_override: Option<chrono::NaiveDate>,
    algorithm_start: rlean_core::DateTime,
    algorithm_end: rlean_core::DateTime,
    engine_date: chrono::NaiveDate,
) -> Result<(chrono::NaiveDate, chrono::NaiveDate)> {
    let start = start_override.unwrap_or_else(|| algorithm_start.date_utc());
    let end = end_override.unwrap_or_else(|| {
        if algorithm_end == rlean_core::DateTime::MAX {
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

/// Result of diffing the previously-active subscriptions against the current
/// desired set. Empty across all fields means "no change".
struct SubscriptionDiff {
    /// Configs whose stream must be torn down and re-added (id unchanged but the
    /// underlying custom query changed).
    replaced: Vec<rlean_data::SubscriptionDataConfig>,
    /// Configs to subscribe: brand-new ids plus the `replaced` ones.
    added: Vec<rlean_data::SubscriptionDataConfig>,
    /// Ids present in the previous set but not the current one — subscriptions to
    /// tear down.
    removed_ids: std::collections::HashSet<u64>,
}

impl SubscriptionDiff {
    fn is_empty(&self) -> bool {
        self.replaced.is_empty() && self.added.is_empty() && self.removed_ids.is_empty()
    }
}

/// Diff previous vs. current subscriptions in O(N) using a single HashMap of the
/// previous ids, replacing the old O(N²) nested `.find()` scans (issue #39).
///
/// Both sides are shared `Arc` handles. Diffing walks them by the memoized
/// `unique_id()` on the shared instances — no re-hashing and no whole-list deep
/// clone — and materializes owned `SubscriptionDataConfig` values only for the
/// configs that are genuinely added or replaced (issue #64).
fn compute_subscription_diff(
    previous: &[Arc<rlean_data::SubscriptionDataConfig>],
    current: &[Arc<rlean_data::SubscriptionDataConfig>],
) -> SubscriptionDiff {
    use std::collections::{HashMap, HashSet};

    let previous_by_id: HashMap<u64, &Arc<rlean_data::SubscriptionDataConfig>> = previous
        .iter()
        .map(|config| (config.unique_id(), config))
        .collect();
    let current_ids: HashSet<u64> = current.iter().map(|config| config.unique_id()).collect();

    let replaced: Vec<rlean_data::SubscriptionDataConfig> = current
        .iter()
        .filter(|config| {
            previous_by_id
                .get(&config.unique_id())
                .map(|existing| subscription_requires_stream_replacement(existing, config))
                .unwrap_or(false)
        })
        .map(|config| (**config).clone())
        .collect();
    let replaced_ids: HashSet<u64> = replaced.iter().map(|config| config.unique_id()).collect();

    let added: Vec<rlean_data::SubscriptionDataConfig> = current
        .iter()
        .filter(|config| {
            let id = config.unique_id();
            !previous_by_id.contains_key(&id) || replaced_ids.contains(&id)
        })
        .map(|config| (**config).clone())
        .collect();

    let removed_ids: HashSet<u64> = previous
        .iter()
        .map(|config| config.unique_id())
        .filter(|id| !current_ids.contains(id))
        .collect();

    SubscriptionDiff {
        replaced,
        added,
        removed_ids,
    }
}

/// Carries the subscription-sync fast-path state across the (up to three)
/// per-slice `sync_data_manager_subscriptions` calls. `active` is the last
/// synced desired set (shared `Arc`s); `last_version` is the bridge version
/// stamp observed at that sync. `None` forces the first sync to run so the
/// initial feed set (which may include a benchmark not present in the bridge's
/// subscription set) is reconciled exactly as before.
struct SubscriptionSyncState {
    active: Vec<Arc<rlean_data::SubscriptionDataConfig>>,
    last_version: Option<u64>,
}

async fn sync_data_manager_subscriptions<B: AlgorithmBridge>(
    data_manager: &mut DataManager,
    sync_state: &mut SubscriptionSyncState,
    bridge: &B,
    benchmark_subscription: Option<&SubscriptionDataConfig>,
    start: rlean_core::DateTime,
) -> anyhow::Result<()> {
    // Generation short-circuit: if the bridge's subscription set has not changed
    // since the last sync, there is nothing to diff. This runs on every slice
    // (minute resolution → 1000+ calls/day), so the integer compare here is what
    // collapses the sync path from ~73% of the main thread to near-zero (#64).
    let version = bridge.subscriptions_version();
    if sync_state.last_version == Some(version) {
        return Ok(());
    }

    let current_subscriptions = desired_backtest_subscriptions(bridge, benchmark_subscription);

    let diff = compute_subscription_diff(&sync_state.active, &current_subscriptions);

    // Short-circuit: nothing added, removed, or replaced. This is the common case
    // (the sync runs on every slice) and skips all data-manager churn below.
    if diff.is_empty() {
        // Record the version so subsequent unchanged slices skip the diff walk
        // above, and refresh the cached configs to the latest instances — ids are
        // identical, so consumers observe no change.
        sync_state.active = current_subscriptions;
        sync_state.last_version = Some(version);
        return Ok(());
    }

    let replaced_ids: std::collections::HashSet<u64> = diff
        .replaced
        .iter()
        .map(SubscriptionDataConfig::unique_id)
        .collect();
    for config in &diff.replaced {
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

    // Brand-new subscriptions are eligible at the current frontier. Replaced
    // streams are not: the old stream already supplied that frontier, and
    // LEAN's in-place config mutation affects the next data point rather than
    // replaying Current. Start replacements one nanosecond later to preserve
    // that behavior while using rlean's immutable-config stream replacement.
    let added = diff
        .added
        .iter()
        .filter(|config| !replaced_ids.contains(&config.unique_id()))
        .cloned()
        .collect();
    data_manager.add_subscriptions_async(added, start).await?;
    let replacement_start = rlean_core::NanosecondTimestamp(start.0.saturating_add(1));
    data_manager
        .add_subscriptions_async(diff.replaced, replacement_start)
        .await?;
    for config in sync_state.active.iter() {
        if diff.removed_ids.contains(&config.unique_id()) {
            data_manager.remove_subscription(config);
        }
    }
    sync_state.active = current_subscriptions;
    sync_state.last_version = Some(version);
    Ok(())
}

/// Build the complete desired feed set for every backtest subscription sync.
///
/// The benchmark is engine-owned and therefore absent from `bridge.subscriptions()`
/// when the strategy has not separately subscribed to it. It must be re-added on
/// every sync just like option-chain feeds; otherwise the first sync treats it as
/// removed and tears down the provider stream.
fn desired_backtest_subscriptions<B: AlgorithmBridge>(
    bridge: &B,
    benchmark_subscription: Option<&SubscriptionDataConfig>,
) -> Vec<Arc<SubscriptionDataConfig>> {
    desired_backtest_subscriptions_from_parts(
        bridge.subscriptions(),
        benchmark_subscription,
        &bridge.option_subscriptions(),
    )
}

fn desired_backtest_subscriptions_from_parts(
    mut subscriptions: Vec<Arc<SubscriptionDataConfig>>,
    benchmark_subscription: Option<&SubscriptionDataConfig>,
    option_subscriptions: &[OptionSubscription],
) -> Vec<Arc<SubscriptionDataConfig>> {
    if let Some(benchmark) = benchmark_subscription {
        if !subscriptions
            .iter()
            .any(|config| config.unique_id() == benchmark.unique_id())
        {
            subscriptions.push(Arc::new(benchmark.clone()));
        }
    }
    lean_style_active_subscriptions(&subscriptions_with_option_chains(
        subscriptions,
        option_subscriptions,
    ))
}

fn subscription_requires_stream_replacement(
    previous: &rlean_data::SubscriptionDataConfig,
    current: &rlean_data::SubscriptionDataConfig,
) -> bool {
    // C# LEAN's Security.SetDataNormalizationMode mutates the active
    // SubscriptionDataConfig instances in place. rlean keeps configs immutable
    // behind Arc and publishes replacements through SubscriptionManager, so an
    // already-running reader must be restarted to observe the new mode. This is
    // especially important for AddOptionContract: LEAN switches the underlying
    // equity to Raw immediately, even when the contract is added mid-backtest.
    if previous.normalization_mode != current.normalization_mode {
        return true;
    }

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
    mut subscriptions: Vec<Arc<SubscriptionDataConfig>>,
    option_subscriptions: &[OptionSubscription],
) -> Vec<Arc<SubscriptionDataConfig>> {
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
            subscriptions.push(Arc::new(config));
        }
    }
    subscriptions
}

fn benchmark_bar<'a>(
    slice: &'a rlean_data::Slice,
    benchmark: &SubscriptionDataConfig,
) -> Option<&'a TradeBar> {
    slice.bars.get(&benchmark.symbol.id.sid)
}

#[allow(clippy::too_many_arguments)]
fn build_backtest_result(
    result_handler: ResultHandler,
    trading_days: i64,
    starting_cash: rlean_core::Price,
    start_date: chrono::NaiveDate,
    end_date: chrono::NaiveDate,
    order_events: Vec<OrderEvent>,
    orders: Vec<rlean_orders::Order>,
    trades: Vec<rlean_statistics::Trade>,
    insight_events: Vec<rlean_alpha::InsightEvent>,
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
        .map(|time| {
            rlean_core::NanosecondTimestamp(*time)
                .date_utc()
                .to_string()
        })
        .collect();
    let benchmark_curve: Vec<f64> = result_handler
        .benchmark_curve
        .values()
        .map(|value| value.to_string().parse::<f64>().unwrap_or(0.0))
        .collect();
    let benchmark_dates: Vec<String> = result_handler
        .benchmark_curve
        .keys()
        .map(|time| {
            rlean_core::NanosecondTimestamp(*time)
                .date_utc()
                .to_string()
        })
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
        trades,
        insight_events,
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
        apply_risk_free_rate_to_option_chains, benchmark_subscription_for_symbol,
        compute_subscription_diff, desired_backtest_subscriptions_from_parts,
        lean_style_active_subscriptions, portfolio_is_bankrupt, resolve_backtest_dates,
        should_run_framework_on_slice, slice_has_algorithm_data,
        subscription_requires_stream_replacement, warmup_subscriptions_at_resolution,
    };
    use chrono::{NaiveDate, TimeZone, Utc};
    use rlean_core::{
        ConstantRiskFreeInterestRateModel, DataNormalizationMode, DateTime, Market, OptionRight,
        OptionStyle, Resolution, Symbol, SymbolOptionsExt, TimeSpan,
    };
    use rlean_data::{
        CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata, OptionChain, OptionContract,
        SubscriptionDataConfig,
    };
    use rlean_data_tables::{Bar, QuoteBar, TradeBar, TradeBarData};
    use rust_decimal_macros::dec;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    fn equity_config(ticker: &str) -> SubscriptionDataConfig {
        SubscriptionDataConfig::new_equity(
            Symbol::create_equity(ticker, &Market::usa()),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        )
    }

    #[test]
    fn backtest_bankruptcy_matches_lean_algorithm_manager() {
        assert!(!portfolio_is_bankrupt(dec!(0.01)));
        assert!(portfolio_is_bankrupt(dec!(0)));
        assert!(portfolio_is_bankrupt(dec!(-1)));
    }

    #[test]
    fn option_contract_pricing_uses_current_slice_quotes_like_lean() {
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let canonical = Symbol::create_option(
            underlying.clone(),
            &Market::usa(),
            date(2024, 8, 19),
            dec!(0),
            OptionRight::Call,
            OptionStyle::American,
        );
        let option = Symbol::create_option(
            underlying.clone(),
            &Market::usa(),
            date(2024, 8, 19),
            dec!(555),
            OptionRight::Call,
            OptionStyle::American,
        );
        let time = DateTime::from(
            Utc.with_ymd_and_hms(2024, 8, 19, 14, 30, 0)
                .single()
                .unwrap(),
        );
        let mut chain = OptionChain::new(canonical, dec!(554));
        chain.add_contract(OptionContract::new(option.clone()));
        let mut slice = rlean_data::Slice::new(time);
        slice.add_bar(TradeBar::new(
            underlying,
            time,
            TimeSpan::from_mins(1),
            TradeBarData::new(dec!(555), dec!(555), dec!(555), dec!(555), dec!(1000)),
        ));
        slice.add_quote_bar(QuoteBar::new(
            option.clone(),
            time,
            TimeSpan::from_mins(1),
            Some(Bar::from_price(dec!(1.90))),
            Some(Bar::from_price(dec!(2.00))),
            dec!(40),
            dec!(44),
        ));
        slice
            .option_chains
            .insert("SPY".to_string(), Arc::new(chain));

        apply_risk_free_rate_to_option_chains(
            &mut slice,
            &ConstantRiskFreeInterestRateModel::new(dec!(0.01)),
        );

        let contract = slice.option_chains["SPY"].contracts.get(&option).unwrap();
        assert_eq!(contract.data.bid_price, dec!(1.90));
        assert_eq!(contract.data.ask_price, dec!(2.00));
        assert_eq!(contract.data.bid_size, 40);
        assert_eq!(contract.data.ask_size, 44);
        assert_eq!(contract.data.underlying_last_price, dec!(555));
        assert!(contract.data.greeks.delta > dec!(0));
    }

    /// Reference implementation of the diff using the original O(N²) nested-scan
    /// semantics, so the fast `compute_subscription_diff` can be checked against
    /// it. Returns (replaced_ids, added_ids, removed_ids).
    fn reference_diff(
        previous: &[SubscriptionDataConfig],
        current: &[SubscriptionDataConfig],
    ) -> (HashSet<u64>, HashSet<u64>, HashSet<u64>) {
        let previous_ids: HashSet<u64> = previous.iter().map(|c| c.unique_id()).collect();
        let current_ids: HashSet<u64> = current.iter().map(|c| c.unique_id()).collect();

        let replaced: HashSet<u64> = current
            .iter()
            .filter(|config| {
                previous
                    .iter()
                    .find(|existing| existing.unique_id() == config.unique_id())
                    .map(|existing| subscription_requires_stream_replacement(existing, config))
                    .unwrap_or(false)
            })
            .map(|c| c.unique_id())
            .collect();
        let added: HashSet<u64> = current
            .iter()
            .filter(|config| {
                !previous_ids.contains(&config.unique_id())
                    || replaced.contains(&config.unique_id())
            })
            .map(|c| c.unique_id())
            .collect();
        let removed: HashSet<u64> = previous_ids
            .iter()
            .copied()
            .filter(|id| !current_ids.contains(id))
            .collect();
        (replaced, added, removed)
    }

    fn into_arcs(configs: &[SubscriptionDataConfig]) -> Vec<Arc<SubscriptionDataConfig>> {
        configs.iter().cloned().map(Arc::new).collect()
    }

    #[test]
    fn option_chain_only_slice_runs_universe_selection() {
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let canonical = Symbol::create_canonical_option(&underlying, &Market::usa());
        let mut slice = rlean_data::Slice::new(DateTime::from_secs(1_721_281_600));
        slice.add_option_chain(
            canonical.permtick.to_string(),
            Arc::new(rlean_options::OptionChain::new(
                canonical,
                rust_decimal::Decimal::ZERO,
            )),
        );

        assert!(slice_has_algorithm_data(&slice));
        assert!(
            should_run_framework_on_slice(true, true, 1, &slice),
            "the first post-warmup chain slice must reach framework models"
        );
    }

    fn assert_diff_matches_reference(
        previous: &[SubscriptionDataConfig],
        current: &[SubscriptionDataConfig],
    ) {
        let diff = compute_subscription_diff(&into_arcs(previous), &into_arcs(current));
        let (ref_replaced, ref_added, ref_removed) = reference_diff(previous, current);

        let replaced: HashSet<u64> = diff.replaced.iter().map(|c| c.unique_id()).collect();
        let added: HashSet<u64> = diff.added.iter().map(|c| c.unique_id()).collect();
        assert_eq!(replaced, ref_replaced, "replaced mismatch");
        assert_eq!(added, ref_added, "added mismatch");
        assert_eq!(diff.removed_ids, ref_removed, "removed mismatch");
    }

    #[test]
    fn diff_matches_reference_on_representative_cases() {
        let spy = equity_config("SPY");
        let xlk = equity_config("XLK");
        let xlf = equity_config("XLF");

        // No change.
        assert_diff_matches_reference(&[spy.clone(), xlk.clone()], &[spy.clone(), xlk.clone()]);
        // Pure add.
        assert_diff_matches_reference(
            std::slice::from_ref(&spy),
            &[spy.clone(), xlk.clone(), xlf.clone()],
        );
        // Pure remove.
        assert_diff_matches_reference(
            &[spy.clone(), xlk.clone(), xlf.clone()],
            std::slice::from_ref(&spy),
        );
        // Add + remove.
        assert_diff_matches_reference(&[spy.clone(), xlk.clone()], &[spy.clone(), xlf.clone()]);
        // Empty -> populated and populated -> empty.
        assert_diff_matches_reference(&[], &[spy.clone(), xlk.clone()]);
        assert_diff_matches_reference(&[spy.clone(), xlk.clone()], &[]);

        // Replacement: same id, changed dynamic custom query.
        let sweeps = custom_config("sweeps", Resolution::Minute);
        let mut sweeps_changed = sweeps.clone();
        sweeps_changed
            .custom
            .as_mut()
            .unwrap()
            .dynamic_query
            .symbols = Some(vec!["NRG".to_string()]);
        assert_diff_matches_reference(
            std::slice::from_ref(&sweeps),
            std::slice::from_ref(&sweeps_changed),
        );
        // Replacement mixed with an add and a remove.
        assert_diff_matches_reference(
            &[sweeps.clone(), spy.clone(), xlk.clone()],
            &[sweeps_changed, spy.clone(), xlf.clone()],
        );
    }

    #[test]
    fn diff_is_empty_when_unchanged() {
        let configs: Vec<SubscriptionDataConfig> =
            (0..10).map(|i| equity_config(&format!("S{i}"))).collect();
        // Cloning yields the same ids, so the diff must report no change.
        let cloned: Vec<SubscriptionDataConfig> = configs.to_vec();
        let diff = compute_subscription_diff(&into_arcs(&configs), &into_arcs(&cloned));
        assert!(
            diff.is_empty(),
            "unchanged subscription set must diff empty"
        );
    }

    #[test]
    fn desired_subscription_sync_preserves_engine_owned_benchmark() {
        let strategy_subscriptions = vec![equity_config("XLK")];
        let benchmark = benchmark_subscription_for_symbol("SPY", strategy_subscriptions.clone())
            .expect("default SPY benchmark subscription");
        assert!(benchmark.is_internal_feed);

        let initial = desired_backtest_subscriptions_from_parts(
            into_arcs(&strategy_subscriptions),
            Some(&benchmark),
            &[],
        );
        let synced = desired_backtest_subscriptions_from_parts(
            into_arcs(&strategy_subscriptions),
            Some(&benchmark),
            &[],
        );
        let diff = compute_subscription_diff(&initial, &synced);

        assert!(diff.is_empty(), "subscription sync must retain SPY");
        assert!(synced
            .iter()
            .any(|config| config.unique_id() == benchmark.unique_id()));
    }

    #[test]
    fn diff_scales_to_400_configs() {
        // Bench-style: 400 configs, one removed and one added between calls.
        // The O(N) diff must produce exactly one add and one remove and agree
        // with the reference. (The old O(N²) path did ~120k hashes per day at
        // this N; the memoized id keeps this to one hash per config.)
        let previous: Vec<SubscriptionDataConfig> = (0..400)
            .map(|i| equity_config(&format!("SYM{i:04}")))
            .collect();
        let mut current = previous.clone();
        current.remove(0);
        current.push(equity_config("NEWSYM"));

        let diff = compute_subscription_diff(&into_arcs(&previous), &into_arcs(&current));
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed_ids.len(), 1);
        assert!(diff.replaced.is_empty());
        assert_diff_matches_reference(&previous, &current);
    }

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

        let active =
            lean_style_active_subscriptions(&into_arcs(&[snapshot.clone(), sweeps.clone()]));

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
    fn normalization_mode_change_requires_stream_replacement() {
        let previous = equity_config("SPY");
        let mut current = previous.clone();
        current.normalization_mode = DataNormalizationMode::Raw;

        assert_eq!(previous.unique_id(), current.unique_id());
        assert!(subscription_requires_stream_replacement(
            &previous, &current
        ));

        let diff = compute_subscription_diff(
            &into_arcs(std::slice::from_ref(&previous)),
            &into_arcs(std::slice::from_ref(&current)),
        );
        assert_eq!(diff.replaced.len(), 1);
        assert_eq!(diff.added.len(), 1);
        assert!(diff.removed_ids.is_empty());
        assert_eq!(diff.added[0].normalization_mode, DataNormalizationMode::Raw);
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

    #[test]
    fn explicit_daily_warmup_resolution_rewrites_history_requests_only() {
        let minute = equity_config("SPY");
        let warmed = warmup_subscriptions_at_resolution(
            std::slice::from_ref(&minute),
            Some(Resolution::Daily),
        );

        assert_eq!(minute.resolution, Resolution::Minute);
        assert_eq!(warmed[0].resolution, Resolution::Daily);
        assert_ne!(minute.unique_id(), warmed[0].unique_id());
    }

    #[test]
    fn daily_warmup_drops_equity_quote_subscription_like_lean() {
        let trade = equity_config("SPY");
        let mut quote = trade.clone();
        quote.set_tick_type(rlean_core::TickType::Quote);

        let warmed = warmup_subscriptions_at_resolution(&[trade, quote], Some(Resolution::Daily));

        assert_eq!(warmed.len(), 1);
        assert_eq!(warmed[0].tick_type, rlean_core::TickType::Trade);
        assert_eq!(warmed[0].resolution, Resolution::Daily);
    }
}
