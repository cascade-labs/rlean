use crate::{
    algorithm_manager::{AlgorithmManager, OrderEventProcessing},
    data_feed::DataFeedContext,
    data_manager::DataManager,
    runner::backtest::{
        apply_risk_free_rate_to_option_chains, benchmark_subscription_for_symbol,
        subscriptions_with_benchmark, subscriptions_with_option_chains,
        warmup_start_from_bar_count, warmup_subscriptions_at_resolution,
    },
    LiveRunConfig, LiveRunResult,
};
use anyhow::Result;
use crossbeam_channel::RecvTimeoutError;
use futures::StreamExt;
use rlean_algorithm::lifecycle::{AlgorithmBridge, AlgorithmServices};
use rlean_core::MarketHoursDatabase;
use rlean_data::{LiveDataItem, LiveDataSubscription, SubscriptionDataConfig};
use rlean_data_sidecar::{
    decode_batch, CanonicalDataBatch, DataSidecarClient, SubscriptionSpec, WireDataType,
};
use rlean_live::{is_transient_sidecar_error, LiveSliceAssembler};
use rlean_orders::{fill_model::ImmediateFillModel, order_processor::OrderProcessor, OrderEvent};
use rlean_statistics::{Trade, TradeBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// LEAN-style real-time pulse source. LiveSynchronizer advances the algorithm
/// clock from wall time after warmup even when no subscription produced data;
/// the pulse is a clock event, not a Slice delivered to `OnData`.
struct LiveTimePulse {
    last_second: i64,
}

impl LiveTimePulse {
    fn new(now: rlean_core::DateTime) -> Self {
        Self {
            last_second: now.as_secs(),
        }
    }

    fn next(&mut self, now: rlean_core::DateTime) -> Option<rlean_core::DateTime> {
        let second = now.as_secs();
        if second <= self.last_second {
            return None;
        }
        self.last_second = second;
        Some(rlean_core::DateTime::from_secs(second))
    }
}

/// Engine-owned live runner entry point.
///
/// All strategy languages enter through `rlean_algorithm::lifecycle::AlgorithmBridge`; language
/// crates do not provide runner futures or alternate loops.
pub async fn run_live<B>(bridge: B, config: LiveRunConfig) -> Result<LiveRunResult>
where
    B: AlgorithmBridge,
{
    let runtime_context =
        crate::AlgorithmRuntimeContext::new(config.data_sidecar.clone(), config.parameters.clone());
    run_live_with_runtime(bridge, config, runtime_context).await
}

pub async fn run_live_with_runtime<B>(
    bridge: B,
    mut config: LiveRunConfig,
    runtime_context: crate::AlgorithmRuntimeContext,
) -> Result<LiveRunResult>
where
    B: AlgorithmBridge,
{
    let started_at = chrono::Utc::now();
    let history_service = runtime_context.history_service();
    let mut services =
        crate::EngineAlgorithmServices::new(rlean_core::DateTime::now(), runtime_context.clone());
    let mut algorithm_manager = AlgorithmManager::new(bridge, runtime_context);
    let market_hours_database = MarketHoursDatabase::global();
    algorithm_manager.set_market_hours_database(market_hours_database.clone());
    algorithm_manager.set_brokerage_model(config.brokerage_model);

    // Execution is independent from the live-data feed. A sidecar brokerage
    // connection enables real routing; without one, fills remain local paper
    // fills even though market data still arrives through the sidecar.
    let real_routing = config.brokerage.is_some();
    let mut snapshot = crate::live::snapshots::LiveDeploymentSnapshot::new(
        config
            .output_dir
            .as_ref()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("live")
            .to_string(),
    );

    // LEAN's BrokerageSetupHandler sets the live algorithm clock to UTC now
    // before calling Initialize. Security seeders invoked by add_equity during
    // Initialize must therefore resolve history relative to the live frontier,
    // not the strategy's backtest start date.
    if let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() {
        let mut algorithm = algorithm_state.lock().expect("algorithm state poisoned");
        algorithm.live_mode = true;
        crate::algorithm_services::advance_algorithm_time(
            &mut algorithm,
            rlean_core::DateTime::now(),
        );
    }
    algorithm_manager.initialize(&mut services)?;
    // The deployment selects execution independently from strategy source.
    // Strategies often set a brokerage model for backtests in Initialize;
    // re-apply the live deployment model so the same strategy can be deployed
    // through Fidelity, Robinhood, Tradier, or paper without source edits.
    algorithm_manager.set_brokerage_model(config.brokerage_model);
    let risk_free_interest_rate_model: Arc<dyn rlean_core::RiskFreeInterestRateModel> = Arc::new(
        crate::risk_free_interest_rate::load_risk_free_interest_rate_model(
            &config.data_sidecar,
            chrono::Utc::now().date_naive(),
        )
        .await?,
    );
    if let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() {
        algorithm_state
            .lock()
            .expect("algorithm state poisoned")
            .set_risk_free_interest_rate_model(risk_free_interest_rate_model.clone());
    }

    if let Some(dir) = config.output_dir.as_deref() {
        if let Some((active, closed)) = crate::live::deployment_writer::restore_live_insights(
            &algorithm_manager.framework(),
            dir,
        ) {
            tracing::info!(
                "Restored live framework insights from deploy dir: active={active} closed={closed}"
            );
        }
    }

    // Feature A: securities referenced by restored (persisted) insights are not
    // re-added by `initialize` when they were originally added mid-session (e.g.
    // an alpha model calling add_equity on a data event). Register a security +
    // subscription for each active-insight symbol so the live data feed follows,
    // the PCM sees prices, and restored insights become actionable. This must run
    // before the initial subscription set is built so the new subscriptions are
    // included from the first data feed. Prices are seeded from the history
    // provider so the startup rebalance sees valid prices; symbols whose seed
    // failed are tracked and re-notified once live data prices them.
    let mut pending_price_restores =
        restore_securities_for_active_insights(&mut algorithm_manager, &history_service);

    let benchmark_subscription = benchmark_subscription_for_symbol(
        &algorithm_manager.benchmark_symbol(),
        algorithm_manager.subscriptions(),
    );
    let subscriptions = subscriptions_with_benchmark(
        algorithm_manager.subscriptions(),
        benchmark_subscription.clone(),
    );
    let subscriptions = subscriptions_with_option_chains(
        subscriptions.into_iter().map(Arc::new).collect(),
        &algorithm_manager.option_subscriptions(),
    );
    // Live consumers below take owned config slices; live is not the per-slice
    // hot path (it has its own id-set short-circuit), so materialize once here.
    let subscriptions: Vec<SubscriptionDataConfig> = subscriptions
        .iter()
        .map(|config| (**config).clone())
        .collect();
    algorithm_manager.prepare_data_delivery(&subscriptions)?;
    let feed_context = DataFeedContext::new(config.data_sidecar.clone());
    if algorithm_manager.is_warming_up() {
        let live_frontier = rlean_core::DateTime::now();
        let warmup_start = if let Some(bar_count) = algorithm_manager.warmup_bar_count() {
            warmup_start_from_bar_count(
                &market_hours_database,
                &subscriptions,
                bar_count,
                live_frontier.date_utc(),
            )
            .map(|date| {
                rlean_core::DateTime::from(
                    date.and_hms_opt(0, 0, 0).expect("valid live warmup start"),
                )
            })
        } else {
            algorithm_manager
                .warmup_duration()
                .map(|duration| live_frontier - duration)
        };
        if let Some(warmup_start) = warmup_start {
            let warmup_end = live_frontier - rlean_core::TimeSpan::from_nanos(1);
            if warmup_start <= warmup_end {
                tracing::info!(
                    %warmup_start,
                    %warmup_end,
                    warmup_bar_count = ?algorithm_manager.warmup_bar_count(),
                    "replaying live algorithm warm-up history"
                );
                let warmup_subscriptions = warmup_subscriptions_at_resolution(
                    &subscriptions,
                    algorithm_manager.warmup_resolution(),
                );
                let mut warmup_data_manager = DataManager::from_context(feed_context.clone());
                warmup_data_manager
                    .initialize_feed(&warmup_subscriptions, warmup_start, warmup_end)
                    .await?;
                while let Some(mut slice) = warmup_data_manager.next_slice().await? {
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
    }
    algorithm_manager.warmup_finished(&mut services);
    // Match C# LEAN's live real-time handler: scheduled events that elapsed
    // while the deployment was offline or warming up are skipped, not replayed.
    algorithm_manager.prime_scheduled_events(rlean_core::DateTime::now());

    // The sidecar owns every live subscription and pushes canonical batches on
    // the persistent exchange; the engine only assembles them into slices.
    let sidecar = config.data_sidecar.clone();
    let transport: Arc<dyn LiveFeedTransport> = sidecar.clone();
    let mut live_subscriptions =
        LiveSubscriptionSet::subscribe_initial(transport, &subscriptions).await?;

    let transactions = algorithm_manager.transactions();
    let portfolio = algorithm_manager.portfolio();
    // The order processor drives local paper fills and also exposes the
    // transaction manager to the deployment-writer snapshot. It is always built
    // (so snapshots keep working), but under real brokerage routing it is *not*
    // passed to `process_order_events`, so it never generates local fills.
    let algorithm_state = algorithm_manager.algorithm.algorithm_state();
    let order_processor = transactions.as_ref().map(|tm| {
        OrderProcessor::new(
            Box::new(ImmediateFillModel::new(
                algorithm_state
                    .clone()
                    .map(|state| {
                        Box::new(crate::algorithm_manager::SecuritySlippageModel::new(state))
                            as Box<dyn rlean_orders::SlippageModel>
                    })
                    .unwrap_or_else(|| Box::new(rlean_orders::NullSlippageModel)),
            )),
            tm.clone(),
        )
    });

    // Real brokerage routing: connect + sync the account, then hand the
    // brokerage to a dedicated worker thread that submits orders and polls for
    // fills/status changes. Under paper/local mode this stays `None` and the
    // local paper-fill path is used unchanged.
    let mut brokerage_router = None;
    if real_routing {
        if let Some(brokerage) = config.brokerage.take() {
            let sync = match crate::live::transaction_handler::startup_sidecar_account_sync(
                &brokerage,
                portfolio.as_ref(),
                transactions.as_ref(),
            )
            .await
            {
                Ok(sync) => {
                    if let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() {
                        let mut algorithm = algorithm_state
                            .lock()
                            .expect("algorithm state poisoned during brokerage account sync");
                        ensure_brokerage_holding_securities(&mut algorithm, &sync.holdings);
                        seed_missing_brokerage_holding_prices(
                            &mut algorithm,
                            &sync.holdings,
                            &history_service,
                        );
                    }
                    // Brokerage holdings are unrequested LEAN securities. They
                    // must join the live feed immediately so a holding whose
                    // brokerage and history prices are both unavailable can
                    // receive a quote and release its deferred liquidation.
                    sync_live_subscriptions(
                        &algorithm_manager,
                        &mut live_subscriptions,
                        benchmark_subscription.as_ref(),
                        &feed_context,
                    )
                    .await?;
                    if let Some(portfolio) = portfolio.as_ref() {
                        crate::live::deployment_writer::set_live_starting_portfolio_value_from_synced_account(portfolio);
                    }
                    tracing::info!(
                        "Live brokerage routing active: cash={} holdings={} open_orders={}",
                        sync.cash,
                        sync.holdings.len(),
                        sync.open_orders.len(),
                    );
                    sync
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "brokerage startup account sync failed: {error}"
                    ));
                }
            };
            let mut router =
                crate::live::transaction_handler::LiveBrokerageRouter::spawn_sidecar(brokerage);
            // Feature B: unconditionally liquidate synced account holdings that no
            // active (restored) framework insight covers. This is the default,
            // always-on convergence behavior under real brokerage routing: live
            // startup drives the brokerage account to the framework's insight
            // state — unmanaged positions closed here, insight-backed positions
            // kept, and missing insight-backed positions bought by the framework
            // rebalance (Feature A). Runs after the account sync and after Feature
            // A's insight/security restore, so the managed set reflects every
            // restored insight. Idempotent across restarts: once liquidated the
            // holding disappears from the next sync.
            liquidate_unmanaged_holdings(
                &algorithm_manager,
                transactions.as_ref(),
                &sync.holdings,
                &mut router,
                rlean_core::DateTime::now(),
            );
            // A live insight checkpoint restores the source of portfolio
            // targets, not the execution model's transient target collection.
            // Once actual brokerage holdings/orders are known, rebuild those
            // targets once from the complete active insight set. This mirrors
            // C# PortfolioConstructionModel.CreateTargets, which evaluates the
            // active Algorithm.Insights collection when reconciliation is due,
            // without pretending the restored insights are newly emitted alpha.
            algorithm_manager
                .framework()
                .lock()
                .expect("framework poisoned during startup reconciliation")
                .request_rebalance();
            brokerage_router = Some(router);
        }
    }
    // Prefer an artifact sink (mirrors snapshots to S3 per its mode); fall back
    // to a plain local writer when only an output dir is set.
    let live_writer = match config.artifact_sink.clone() {
        Some(sink) => Some(crate::live::deployment_writer::LiveDeploymentWriter::with_sink(sink)),
        None => config
            .output_dir
            .as_ref()
            .map(|dir| crate::live::deployment_writer::LiveDeploymentWriter::new(dir.clone())),
    };
    let restored = if config.paper_trading {
        config
            .output_dir
            .as_deref()
            .and_then(crate::live::deployment_writer::load_deploy_restore)
    } else {
        None
    };
    let mut all_order_events: Vec<OrderEvent> = match &restored {
        Some(restore) => restore.order_events.clone(),
        None => Vec::new(),
    };
    let mut trade_builder = TradeBuilder::new();
    let mut completed_trades = match &restored {
        Some(restore) => restore.trades.clone(),
        None => Vec::new(),
    };
    if let (Some(restore), Some(algorithm_state)) = (
        restored.as_ref(),
        algorithm_manager.algorithm().algorithm_state(),
    ) {
        crate::live::deployment_writer::apply_initial_brokerage_account_state(
            &algorithm_state,
            &restore.account_state,
        );
        if let Some(portfolio) = portfolio.as_ref() {
            let starting_value = crate::live::deployment_writer::set_live_starting_portfolio_value_from_synced_account(
                portfolio,
            );
            tracing::info!(
                "Restored paper account from deploy dir: cash={} holdings={} open_orders={} order_events={} trades={} starting_value={starting_value}",
                restore.account_state.cash,
                restore.account_state.holdings.len(),
                restore.account_state.open_orders.len(),
                restore.order_events.len(),
                restore.trades.len(),
            );
        }
        let algorithm = algorithm_state.lock().unwrap();
        for holding in &restore.account_state.holdings {
            if holding.quantity.is_zero() {
                continue;
            }
            let multiplier = algorithm.contract_multiplier_for_symbol(&holding.symbol);
            let _ = trade_builder.record_fill(
                &holding.symbol,
                rlean_core::DateTime::now(),
                holding.average_price,
                holding.quantity,
                multiplier,
                rust_decimal::Decimal::ZERO,
            );
        }
    }
    if let (Some(writer), Some(processor), Some(portfolio)) = (
        live_writer.as_ref(),
        order_processor.as_ref(),
        portfolio.as_ref(),
    ) {
        writer.record_snapshot(
            rlean_core::DateTime::now(),
            portfolio,
            processor,
            Some(&algorithm_manager.framework()),
            crate::live::deployment_writer::LiveSnapshotCounts {
                slices_processed: algorithm_manager.slices_processed() as usize,
                order_events: all_order_events.len(),
                trades: completed_trades.len(),
            },
        );
    }
    let run_started = Instant::now();
    let mut assembler = LiveSliceAssembler::new();
    assembler.set_subscriptions(live_subscriptions.configs.values());
    let mut time_pulse = LiveTimePulse::new(rlean_core::DateTime::now());
    let mut live_reconnect: Option<tokio::task::JoinHandle<Result<()>>> = None;

    'live: loop {
        if should_stop(
            algorithm_manager.slices_processed() as usize,
            config.max_slices,
            run_started,
            config.max_runtime,
        ) {
            break;
        }

        if live_reconnect
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            live_reconnect
                .take()
                .expect("finished reconnect task is present")
                .await??;
            live_subscriptions.finish_reconnect().await?;
        }

        // C# LEAN's LiveTradingRealTimeHandler is driven by wall time, not by
        // subscription enumerators. A pulse advances algorithm time and scans
        // scheduled events, but is never delivered to OnData.
        let now = rlean_core::DateTime::now();
        if let Some(pulse_time) = time_pulse.next(now) {
            let scheduled_event_times = algorithm_manager.scan_scheduled_events(pulse_time)?;
            if let Some(trigger_time) = scheduled_event_times.last().copied() {
                // Give the framework one empty scheduled time-step so a PCM
                // explicitly gated by the callback can produce targets.
                let scheduled_slice = rlean_data::Slice::new(trigger_time);
                algorithm_manager.run_framework(&scheduled_slice, &mut services);
            }
            // Scheduled callbacks set the clock to their exact trigger. Restore
            // the current wall-clock frontier afterwards without invoking
            // OnData, universe selection, indicators, or slice counters.
            algorithm_manager.advance_frontier(&rlean_data::Slice::new(pulse_time), &mut services);
            if let (Some(writer), Some(portfolio)) = (live_writer.as_ref(), portfolio.as_ref()) {
                writer.record_time_pulse(
                    pulse_time,
                    portfolio,
                    crate::live::deployment_writer::LiveSnapshotCounts {
                        slices_processed: algorithm_manager.slices_processed() as usize,
                        order_events: all_order_events.len(),
                        trades: completed_trades.len(),
                    },
                );
            }
            if let (Some(router), Some(transactions)) =
                (brokerage_router.as_mut(), transactions.as_ref())
            {
                router.request_cash_sync_if_due(pulse_time);
                service_live_brokerage(
                    &mut algorithm_manager,
                    &mut services,
                    router,
                    transactions,
                    portfolio.as_ref(),
                    live_writer.as_ref(),
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                );
            }
        }

        let poll = if live_reconnect.is_none() {
            poll_live_item(&mut live_subscriptions, Duration::from_millis(250))
        } else {
            std::thread::sleep(Duration::from_millis(250));
            LivePoll::Idle
        };
        match poll {
            LivePoll::Item(item) => {
                assembler.enqueue(*item);
                loop {
                    match poll_live_item(&mut live_subscriptions, Duration::ZERO) {
                        LivePoll::Item(item) => assembler.enqueue(*item),
                        LivePoll::Idle => break,
                        // A live subscription stream dropped: re-establish the
                        // sidecar session in the background. Wall-clock pulses,
                        // schedules and brokerage servicing must remain live.
                        LivePoll::StreamLost => {
                            live_reconnect = Some(live_subscriptions.begin_reconnect());
                            break;
                        }
                    }
                }
            }
            LivePoll::Idle => {}
            LivePoll::StreamLost => {
                live_reconnect = Some(live_subscriptions.begin_reconnect());
            }
        }
        // Universe and strategy code can add/remove subscriptions while live.
        // Keep the assembler's LEAN-style fill-forward enumerators aligned with
        // the exact active sidecar subscription set.
        assembler.set_subscriptions(live_subscriptions.configs.values());
        let ready_slices: Vec<_> = assembler
            .advance(rlean_core::DateTime::now())
            .into_iter()
            .collect();

        // Brokerage fills are an independent event source. Service them even
        // when no market-data slice is ready so cash-account sell proceeds can
        // release a deferred replacement buy without waiting for the next bar.
        if ready_slices.is_empty() {
            if let (Some(router), Some(transactions)) =
                (brokerage_router.as_mut(), transactions.as_ref())
            {
                service_live_brokerage(
                    &mut algorithm_manager,
                    &mut services,
                    router,
                    transactions,
                    portfolio.as_ref(),
                    live_writer.as_ref(),
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                );
            }
        }

        for mut slice in ready_slices {
            apply_risk_free_rate_to_option_chains(
                &mut slice,
                risk_free_interest_rate_model.as_ref(),
            );
            process_live_slice(
                &mut algorithm_manager,
                &mut services,
                &mut live_subscriptions,
                benchmark_subscription.as_ref(),
                &order_processor,
                brokerage_router.as_mut(),
                transactions.as_ref(),
                portfolio.as_ref(),
                live_writer.as_ref(),
                &mut all_order_events,
                &mut trade_builder,
                &mut completed_trades,
                &mut pending_price_restores,
                &feed_context,
                &slice,
            )
            .await?;

            if algorithm_manager.algorithm().terminal_status().is_some()
                || algorithm_manager.algorithm().runtime_error().is_some()
            {
                break 'live;
            }
            if should_stop(
                algorithm_manager.slices_processed() as usize,
                config.max_slices,
                run_started,
                config.max_runtime,
            ) {
                break;
            }
        }
    }

    if algorithm_manager.algorithm().terminal_status().is_none()
        && algorithm_manager.algorithm().runtime_error().is_none()
    {
        if let Some(mut slice) = assembler.advance(rlean_core::DateTime::now()) {
            apply_risk_free_rate_to_option_chains(
                &mut slice,
                risk_free_interest_rate_model.as_ref(),
            );
            if !should_stop(
                algorithm_manager.slices_processed() as usize,
                config.max_slices,
                run_started,
                config.max_runtime,
            ) {
                process_live_slice(
                    &mut algorithm_manager,
                    &mut services,
                    &mut live_subscriptions,
                    benchmark_subscription.as_ref(),
                    &order_processor,
                    brokerage_router.as_mut(),
                    transactions.as_ref(),
                    portfolio.as_ref(),
                    live_writer.as_ref(),
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                    &mut pending_price_restores,
                    &feed_context,
                    &slice,
                )
                .await?;
            }
        }
    }

    if let Some(router) = brokerage_router.as_mut() {
        router.shutdown();
    }
    if let Some(reconnect) = live_reconnect.take() {
        reconnect.abort();
    }
    algorithm_manager.finish(&mut services);
    live_subscriptions.unsubscribe_all().await;
    sidecar
        .close_live_data_feed(config.live_data_feed_connection_id)
        .await?;
    snapshot.slices_processed = algorithm_manager.slices_processed() as usize;
    snapshot.final_value = algorithm_manager
        .portfolio_value()
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    snapshot.recent_order_events = all_order_events.clone();

    // Clean shutdown: drain any queued S3 uploads so the final snapshot lands.
    if let Some(writer) = live_writer.as_ref() {
        writer.flush();
    }

    Ok(LiveRunResult {
        slices_processed: algorithm_manager.slices_processed() as usize,
        final_value: algorithm_manager
            .portfolio_value()
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0),
        order_events: all_order_events,
        started_at,
        stopped_at: chrono::Utc::now(),
    })
}

/// C# LEAN's brokerage setup creates securities for every brokerage holding
/// before transaction processing starts. The portfolio-only sync is not enough:
/// buying-power validation resolves the holding's security model before it can
/// submit a reducing order.
fn ensure_brokerage_holding_securities(
    algorithm: &mut rlean_algorithm::qc_algorithm::QcAlgorithm,
    holdings: &[rlean_brokerages::BrokerageHolding],
) {
    for holding in holdings {
        if !algorithm.securities.contains(&holding.symbol) {
            algorithm.add_security_symbol(holding.symbol.clone(), rlean_core::Resolution::Minute);
        }
        if holding.market_price > rust_decimal::Decimal::ZERO {
            algorithm
                .securities
                .update_price(&holding.symbol, holding.market_price);
            algorithm
                .portfolio
                .update_prices(&holding.symbol, holding.market_price);
        }
    }
}

/// Match C# LEAN BrokerageSetupHandler.LoadExistingHoldingsAndOrders: after
/// creating an unrequested security and applying the brokerage holding, resolve
/// a zero brokerage MarketPrice through GetLastKnownPrice and seed both the
/// security and portfolio before any liquidation/order validation runs.
fn seed_missing_brokerage_holding_prices(
    algorithm: &mut rlean_algorithm::qc_algorithm::QcAlgorithm,
    holdings: &[rlean_brokerages::BrokerageHolding],
    history_service: &Arc<dyn rlean_algorithm::lifecycle::AlgorithmHistoryService>,
) {
    if algorithm.utc_time == rlean_core::DateTime::EPOCH {
        crate::algorithm_services::advance_algorithm_time(algorithm, rlean_core::DateTime::now());
    }
    for holding in holdings {
        let current_price = algorithm
            .securities
            .get(&holding.symbol)
            .map(|security| security.current_price())
            .unwrap_or_default();
        if current_price > rust_decimal::Decimal::ZERO {
            continue;
        }
        let seeded = history_service
            .last_known_close_price(algorithm, &holding.symbol, rlean_core::Resolution::Daily)
            .and_then(|price| rust_decimal::Decimal::try_from(price).ok())
            .filter(|price| *price > rust_decimal::Decimal::ZERO);
        match seeded {
            Some(price) => {
                algorithm.securities.update_price(&holding.symbol, price);
                algorithm.portfolio.update_prices(&holding.symbol, price);
                tracing::info!(
                    "seeded last-known price for brokerage holding {}: {}",
                    holding.symbol.value,
                    price
                );
            }
            None => tracing::warn!(
                "no brokerage or last-known price available for holding {}; position-reducing orders will wait for live data",
                holding.symbol.value
            ),
        }
    }
}

/// Feature A: register a security + subscription for every symbol referenced by a
/// restored (persisted) active insight, so restored insights become actionable
/// after a restart.
///
/// Securities originally added mid-session (e.g. `add_equity` from an alpha model
/// on a data event) do not exist after a restart because only `initialize` runs.
/// This walks the framework's active insights and, for each symbol not already
/// subscribed, calls the same `add_security_symbol` path universe additions use
/// (security initializer + subscription registration included). The added
/// subscriptions then flow through `subscriptions()` into the initial live
/// subscription set and later syncs, so the live data feed follows automatically.
///
/// It also notifies the framework of the added securities. Whether that causes
/// target creation remains entirely controlled by the PCM's configured
/// rebalance-on-security-changes policy.
///
/// Restored securities start with price 0 — live quotes arrive only seconds
/// after startup, typically *after* the first framework run, and the PCM skips
/// zero-price symbols when building targets. So every active-insight security
/// without a price is seeded synchronously with its last-known close from the
/// history provider (daily resolution; the engine-side FuncSecuritySeeder
/// equivalent). Symbols whose history fetch yields nothing are returned in the
/// pending map; the live loop re-notifies the framework the moment such a
/// security gains a live price. Insight-only policies still remain no-ops.
fn restore_securities_for_active_insights<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    history_service: &Arc<dyn rlean_algorithm::lifecycle::AlgorithmHistoryService>,
) -> HashMap<u64, rlean_core::Symbol> {
    let mut pending_prices: HashMap<u64, rlean_core::Symbol> = HashMap::new();
    let framework = algorithm_manager.framework();
    let insight_symbols: Vec<rlean_core::Symbol> = {
        let fw = match framework.lock() {
            Ok(fw) => fw,
            Err(_) => return pending_prices,
        };
        // Deduplicate by sid while preserving order.
        let mut seen = HashSet::new();
        fw.insights
            .active(rlean_core::DateTime::now())
            .into_iter()
            .map(|insight| insight.symbol)
            .filter(|symbol| seen.insert(symbol.id.sid))
            .collect()
    };
    if insight_symbols.is_empty() {
        return pending_prices;
    }

    let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() else {
        return pending_prices;
    };

    let already_subscribed: HashSet<u64> = algorithm_manager
        .subscriptions()
        .iter()
        .map(|config| config.symbol.id.sid)
        .collect();

    let mut restored: Vec<rlean_core::Symbol> = Vec::new();
    for symbol in insight_symbols {
        let security_type = symbol.security_type();
        if !already_subscribed.contains(&symbol.id.sid) {
            // `add_security_symbol` panics on security types it cannot register
            // (e.g. Future/Index). Guard against those up front — WARN and
            // continue rather than aborting the whole restore (and poisoning the
            // algorithm lock).
            if !is_restorable_security_type(security_type) {
                tracing::warn!(
                    "could not restore security for persisted insight {} ({:?}); unsupported security type — skipping",
                    symbol.value,
                    security_type
                );
                continue;
            }
            // LEAN live subscribes equities (and, in this engine, other live
            // security types) at minute resolution; `add_security_symbol`
            // dispatches on the symbol's own security type for correct
            // model/subscription setup.
            let resolution = rlean_core::Resolution::Minute;
            {
                let mut algorithm = match algorithm_state.lock() {
                    Ok(algorithm) => algorithm,
                    Err(_) => return pending_prices,
                };
                algorithm.add_security_symbol(symbol.clone(), resolution);
            }
            restored.push(symbol.clone());
            tracing::info!(
                "restoring security for persisted insight: {} ({:?} {:?})",
                symbol.value,
                security_type,
                resolution
            );
        }

        // Seed the last-known price so the startup rebalance sees a valid price
        // and can emit targets immediately. Applies to every active-insight
        // security still priced at zero — both freshly restored ones and ones
        // initialize re-added but that have no live quote yet.
        let mut algorithm = match algorithm_state.lock() {
            Ok(algorithm) => algorithm,
            Err(_) => return pending_prices,
        };
        // The live frontier is "now"; `last_known_close_price` derives its
        // as-of/lookback window from the algorithm clock, which is still EPOCH
        // before the first slice.
        if algorithm.utc_time == rlean_core::DateTime::EPOCH {
            crate::algorithm_services::advance_algorithm_time(
                &mut algorithm,
                rlean_core::DateTime::now(),
            );
        }
        let current_price = algorithm
            .securities
            .get(&symbol)
            .map(|security| security.current_price())
            .unwrap_or_default();
        if current_price > rust_decimal::Decimal::ZERO {
            continue;
        }
        let seeded = history_service
            .last_known_close_price(&algorithm, &symbol, rlean_core::Resolution::Daily)
            .and_then(|price| rust_decimal::Decimal::try_from(price).ok())
            .filter(|price| *price > rust_decimal::Decimal::ZERO);
        match seeded {
            Some(price) => {
                algorithm.securities.update_price(&symbol, price);
                algorithm.portfolio.update_prices(&symbol, price);
                tracing::info!(
                    "seeded last-known price for restored insight security {}: {}",
                    symbol.value,
                    price
                );
            }
            None => {
                tracing::warn!(
                    "no last-known price available for restored insight security {}; will re-trigger rebalance when live data arrives",
                    symbol.value
                );
                pending_prices.insert(symbol.id.sid, symbol.clone());
            }
        }
    }

    if !restored.is_empty() {
        crate::framework::notify_framework_securities_changed(&framework, &restored, &[]);
    }
    pending_prices
}

/// Fallback for restored-insight securities whose history seed failed (halted,
/// IPO'd yesterday, provider gap): once such a security gains its first live
/// price, notify the framework again. This only rebalances when the PCM enables
/// security-change rebalancing; checkpoint restoration is not a new insight.
fn renotify_restored_securities_with_prices<B: AlgorithmBridge>(
    algorithm_manager: &AlgorithmManager<B>,
    pending: &mut HashMap<u64, rlean_core::Symbol>,
) {
    if pending.is_empty() {
        return;
    }
    let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() else {
        pending.clear();
        return;
    };
    let priced: Vec<rlean_core::Symbol> = {
        let algorithm = match algorithm_state.lock() {
            Ok(algorithm) => algorithm,
            Err(_) => return,
        };
        pending
            .values()
            .filter(|symbol| {
                algorithm
                    .securities
                    .get(symbol)
                    .map(|security| security.current_price() > rust_decimal::Decimal::ZERO)
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    };
    if priced.is_empty() {
        return;
    }
    for symbol in &priced {
        pending.remove(&symbol.id.sid);
        tracing::info!(
            "restored insight security {} received its first live price; re-triggering framework rebalance",
            symbol.value
        );
    }
    crate::framework::notify_framework_securities_changed(
        &algorithm_manager.framework(),
        &priced,
        &[],
    );
}

/// Security types `QcAlgorithm::add_security_symbol` can register (it panics on
/// anything else). Kept in lockstep with that method's supported arms.
fn is_restorable_security_type(security_type: rlean_core::SecurityType) -> bool {
    matches!(
        security_type,
        rlean_core::SecurityType::Base
            | rlean_core::SecurityType::Equity
            | rlean_core::SecurityType::Forex
            | rlean_core::SecurityType::Crypto
            | rlean_core::SecurityType::CryptoFuture
            | rlean_core::SecurityType::Option
    )
}

/// Feature B: submit market orders to close every synced account holding that no
/// active (restored) framework insight covers.
///
/// The unmanaged set is: each synced holding with a nonzero quantity whose symbol
/// has no active insight. Holdings covered by an active insight are kept (the PCM
/// manages them). For each unmanaged holding a `-quantity` market order is
/// registered in the transaction manager and dispatched through the existing
/// brokerage router submission path, so it reconciles via the normal poll path.
/// Existing brokerage orders that already reduce the holding are included in
/// the target delta. This makes startup idempotent: restarting while a closing
/// order is queued cannot submit the full liquidation again.
///
/// Order ids for these liquidations are drawn from the algorithm's single
/// order-id authority (`QcAlgorithm::next_order_id`) — the exact same counter
/// every framework/algorithm order uses. Historically this path pulled ids from
/// `TransactionManager::next_order_id`, a *parallel* counter that also starts at
/// 1, so liquidation id=1,2,... collided with the framework's id=1,2,.... The
/// router maps brokerage ids back to engine ids, so a colliding pair let a
/// liquidation's fill be applied to a different framework order (different
/// symbol/quantity) — the production incident in issue #33. Using the algorithm
/// counter guarantees liquidation and framework orders never share an id.
///
/// Market-hours aware (issue #86): if a holding's exchange is closed at `now`
/// (e.g. a weekend restart), the liquidation is registered as a `MarketOnOpen`
/// order that the router holds and releases at the next open, instead of a
/// market order that would be submitted into a closed market. Futures and
/// future options are exempt (extended-hours trading), mirroring C# LEAN
/// `QCAlgorithm.Trading.cs` `MarketOrder`, which converts closed-market market
/// orders to MarketOnOpen for every other security type. `now` is a parameter
/// so tests can simulate weekend/weekday times; production passes the wall
/// clock.
fn liquidate_unmanaged_holdings<B: AlgorithmBridge>(
    algorithm_manager: &AlgorithmManager<B>,
    transactions: Option<&Arc<rlean_orders::TransactionManager>>,
    holdings: &[rlean_brokerages::BrokerageHolding],
    router: &mut crate::live::transaction_handler::LiveBrokerageRouter,
    now: rlean_core::DateTime,
) {
    let Some(transactions) = transactions else {
        tracing::warn!(
            "liquidate-unmanaged-holdings requested but no transaction manager is available; skipping"
        );
        return;
    };
    let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() else {
        tracing::warn!(
            "liquidate-unmanaged-holdings requested but no algorithm state is available; skipping"
        );
        return;
    };

    let framework = algorithm_manager.framework();
    let managed_sids: HashSet<u64> = match framework.lock() {
        Ok(fw) => fw
            .insights
            .active(rlean_core::DateTime::now())
            .into_iter()
            .map(|insight| insight.symbol.id.sid)
            .collect(),
        Err(_) => HashSet::new(),
    };

    let mut kept = 0usize;
    let mut liquidated = 0usize;
    for holding in holdings {
        if holding.quantity.is_zero() {
            continue;
        }
        if managed_sids.contains(&holding.symbol.id.sid) {
            kept += 1;
            continue;
        }
        // Asset-agnostic: -quantity closes longs (sell) and shorts (buy-to-cover)
        // alike, and preserves fractional/options/crypto quantities unchanged.
        let desired_quantity = -holding.quantity;
        let already_open_quantity =
            open_closing_quantity(transactions, holding.symbol.id.sid, desired_quantity);
        let quantity = desired_quantity - already_open_quantity;
        if quantity.is_zero() {
            tracing::info!(
                "unmanaged holding {} already has a complete closing order queued; not duplicating it",
                holding.symbol.value
            );
            continue;
        }
        if (quantity.is_sign_positive() && desired_quantity.is_sign_negative())
            || (quantity.is_sign_negative() && desired_quantity.is_sign_positive())
        {
            tracing::error!(
                "unmanaged holding {} has over-covering open orders: holding={} desired_close={} open_close={}; refusing to submit another order",
                holding.symbol.value,
                holding.quantity,
                desired_quantity,
                already_open_quantity,
            );
            continue;
        }
        // Single id authority: allocate from the algorithm's order-id counter, the
        // same sequence framework/algorithm orders draw from, so ids never collide
        // with later framework orders (issue #33).
        let order_id = match algorithm_state.lock() {
            Ok(mut algorithm) => algorithm.next_order_id(),
            Err(_) => {
                tracing::error!(
                    "liquidate-unmanaged-holdings: algorithm lock poisoned; skipping {}",
                    holding.symbol.value
                );
                continue;
            }
        };
        let mut order = rlean_orders::Order::market(
            order_id,
            holding.symbol.clone(),
            quantity,
            now,
            "Liquidate unmanaged holding",
        );
        // Closed exchange (weekend/overnight restart): register as MarketOnOpen
        // so the router holds it until the next open instead of submitting into
        // a closed market. Futures/FOPs trade extended hours and stay market
        // orders, mirroring LEAN.
        let moo_exempt = matches!(
            holding.symbol.security_type(),
            rlean_core::SecurityType::Future | rlean_core::SecurityType::FutureOption
        );
        if !moo_exempt
            && !rlean_core::MarketHoursDatabase::global()
                .exchange_hours(&holding.symbol)
                .is_open_at(now)
        {
            order.order_type = rlean_orders::OrderType::MarketOnOpen;
            tracing::info!(
                "liquidation for {} (order_id={order_id}) created while market closed; \
                 registering as market-on-open to fill at the next open",
                holding.symbol.value,
            );
        }
        transactions.add_or_update_order(order);
        liquidated += 1;
        tracing::info!(
            "liquidating unmanaged holding: {} (order_id={order_id}) quantity={}",
            holding.symbol.value,
            quantity
        );
    }

    tracing::info!("liquidate-unmanaged-holdings: kept={kept} (managed) liquidated={liquidated}");

    if liquidated > 0 {
        // Reuse the router's existing submission path; the New orders just
        // registered flow to the brokerage and reconcile on the poll path.
        // Market-on-open registrations are held by the router until their
        // exchange opens.
        match algorithm_state.lock() {
            Ok(algorithm) => {
                let _ = router.dispatch_pending_at(transactions, &algorithm, now);
            }
            Err(_) => tracing::error!(
                "liquidate-unmanaged-holdings: algorithm lock poisoned before submission"
            ),
        }
    }
}

fn open_closing_quantity(
    transactions: &rlean_orders::TransactionManager,
    symbol_sid: u64,
    desired_quantity: rlean_core::Quantity,
) -> rlean_core::Quantity {
    transactions
        .get_open_orders()
        .into_iter()
        .filter(|order| {
            order.symbol.id.sid == symbol_sid
                && ((order.remaining_quantity().is_sign_positive()
                    && desired_quantity.is_sign_positive())
                    || (order.remaining_quantity().is_sign_negative()
                        && desired_quantity.is_sign_negative()))
        })
        .map(|order| order.remaining_quantity())
        .sum()
}

#[allow(clippy::too_many_arguments)]
fn service_live_brokerage<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    services: &mut dyn AlgorithmServices,
    router: &mut crate::live::transaction_handler::LiveBrokerageRouter,
    transactions: &Arc<rlean_orders::TransactionManager>,
    portfolio: Option<&Arc<rlean_algorithm::portfolio::SecurityPortfolioManager>>,
    live_writer: Option<&crate::live::deployment_writer::LiveDeploymentWriter>,
    all_order_events: &mut Vec<OrderEvent>,
    trade_builder: &mut TradeBuilder,
    completed_trades: &mut Vec<Trade>,
) {
    let previous_order_events = all_order_events.len();
    let previous_trades = completed_trades.len();
    router.drain_events(
        algorithm_manager,
        services,
        transactions,
        portfolio,
        all_order_events,
        trade_builder,
        completed_trades,
    );
    if let Some(writer) = live_writer {
        writer.append_order_events(&all_order_events[previous_order_events..]);
        writer.append_trades(&completed_trades[previous_trades..]);
    }

    let invalid_events = algorithm_manager
        .algorithm()
        .algorithm_state()
        .map(|state| {
            let algorithm = state.lock().expect("algorithm state poisoned");
            router.dispatch_pending(transactions, &algorithm)
        })
        .unwrap_or_default();
    if !invalid_events.is_empty() {
        for event in &invalid_events {
            algorithm_manager.algorithm.on_order_event(event, services);
        }
        if let Some(writer) = live_writer {
            writer.append_order_events(&invalid_events);
        }
        all_order_events.extend(invalid_events);
    }

    // Brokerage events are independent of market-data slices. Match LEAN's
    // transaction/result lifecycle by refreshing restart and CLI artifacts as
    // soon as submitted/filled/invalid state changes, including for sparse
    // Daily strategies that may not receive another slice today.
    if all_order_events.len() != previous_order_events || completed_trades.len() != previous_trades
    {
        if let (Some(writer), Some(portfolio)) = (live_writer, portfolio) {
            writer.record_snapshot_from_transactions(
                rlean_core::DateTime::now(),
                portfolio,
                transactions.as_ref(),
                Some(&algorithm_manager.framework()),
                crate::live::deployment_writer::LiveSnapshotCounts {
                    slices_processed: algorithm_manager.slices_processed() as usize,
                    order_events: all_order_events.len(),
                    trades: completed_trades.len(),
                },
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_live_slice<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    services: &mut dyn AlgorithmServices,
    live_subscriptions: &mut LiveSubscriptionSet,
    benchmark_subscription: Option<&SubscriptionDataConfig>,
    order_processor: &Option<OrderProcessor>,
    mut brokerage_router: Option<&mut crate::live::transaction_handler::LiveBrokerageRouter>,
    transactions: Option<&std::sync::Arc<rlean_orders::TransactionManager>>,
    portfolio: Option<&std::sync::Arc<rlean_algorithm::portfolio::SecurityPortfolioManager>>,
    live_writer: Option<&crate::live::deployment_writer::LiveDeploymentWriter>,
    all_order_events: &mut Vec<OrderEvent>,
    trade_builder: &mut TradeBuilder,
    completed_trades: &mut Vec<Trade>,
    pending_price_restores: &mut HashMap<u64, rlean_core::Symbol>,
    feed_context: &DataFeedContext,
    slice: &rlean_data::Slice,
) -> Result<()> {
    if !slice.has_data {
        return Ok(());
    }
    let slice_arc = Arc::new(slice.clone());

    let new_trading_day = algorithm_manager.handle_new_trading_day(slice, services)?;
    let changes = algorithm_manager.apply_universe_selection(slice, new_trading_day, services);
    if changes.has_changes() {
        sync_live_subscriptions(
            algorithm_manager,
            live_subscriptions,
            benchmark_subscription,
            feed_context,
        )
        .await?;
    }

    algorithm_manager.advance_frontier(slice, services);
    // Restored-insight securities whose startup history seed failed: the frontier
    // advance above applies this slice's prices, so a symbol that just received
    // its first live price re-notifies the framework here — before run_framework
    // below — so the PCM re-rebalances and the restored insight produces targets.
    renotify_restored_securities_with_prices(algorithm_manager, pending_price_restores);
    let option_chains: Vec<(&str, &rlean_data::OptionChain)> = slice
        .option_chains
        .iter()
        .map(|(key, chain)| (key.as_str(), chain.as_ref()))
        .collect();
    let prev_order_events = all_order_events.len();
    let prev_trades = completed_trades.len();
    // Under real brokerage routing, do not run the local fill model: fills come
    // from the brokerage poll and are applied when router events are drained.
    let local_fill_processor = if brokerage_router.is_some() {
        None
    } else {
        order_processor.as_ref()
    };
    algorithm_manager.process_order_events(OrderEventProcessing {
        slice,
        option_chains: &option_chains,
        order_processor: local_fill_processor,
        portfolio,
        services,
        all_order_events,
        trade_builder,
        completed_trades,
    });
    // Drain brokerage-originated status/fill events before on_data so the
    // algorithm observes fills at the top of the time step, matching the local
    // fill ordering.
    if let (Some(router), Some(transactions)) = (brokerage_router.as_mut(), transactions) {
        router.drain_events(
            algorithm_manager,
            services,
            transactions,
            portfolio,
            all_order_events,
            trade_builder,
            completed_trades,
        );
    }
    if let Some(writer) = live_writer {
        writer.append_order_events(&all_order_events[prev_order_events..]);
        writer.append_trades(&completed_trades[prev_trades..]);
    }

    algorithm_manager.deliver_data(
        rlean_algorithm::algorithm::DataDeliveryPayload { slice: slice_arc },
        services,
    );
    algorithm_manager.run_framework(slice, services);
    // Under real brokerage routing, forward any New orders the framework/strategy
    // just created to the brokerage worker for submission.
    if let (Some(router), Some(transactions)) = (brokerage_router.as_mut(), transactions) {
        let invalid_events = algorithm_manager
            .algorithm()
            .algorithm_state()
            .map(|state| {
                let algorithm = state.lock().expect("algorithm state poisoned");
                router.dispatch_pending(transactions, &algorithm)
            })
            .unwrap_or_default();
        if !invalid_events.is_empty() {
            for event in &invalid_events {
                algorithm_manager.algorithm.on_order_event(event, services);
            }
            if let Some(writer) = live_writer {
                writer.append_order_events(&invalid_events);
            }
            all_order_events.extend(invalid_events);
        }
    }
    // Mirror the backtest runner: securities added mid-run (e.g. add_equity from
    // an alpha model or OnData) never surface through universe selection, so the
    // live data subscriptions must be re-synced against the algorithm's current
    // subscription list after strategy code has run.
    sync_live_subscriptions(
        algorithm_manager,
        live_subscriptions,
        benchmark_subscription,
        feed_context,
    )
    .await?;
    algorithm_manager.end_time_step(services);
    let custom_points: usize = slice.custom_data.values().map(Vec::len).sum();
    tracing::debug!(
        "processed live slice time={:?} custom_keys={} custom_points={} order_events={} completed_trades={}",
        slice.time,
        slice.custom_data.len(),
        custom_points,
        all_order_events.len(),
        completed_trades.len()
    );
    if let (Some(writer), Some(processor), Some(portfolio)) =
        (live_writer, order_processor.as_ref(), portfolio)
    {
        writer.record_snapshot(
            slice.time,
            portfolio,
            processor,
            Some(&algorithm_manager.framework()),
            crate::live::deployment_writer::LiveSnapshotCounts {
                slices_processed: algorithm_manager.slices_processed() as usize,
                order_events: all_order_events.len(),
                trades: completed_trades.len(),
            },
        );
    }
    Ok(())
}

async fn sync_live_subscriptions<B: AlgorithmBridge>(
    algorithm_manager: &AlgorithmManager<B>,
    live_subscriptions: &mut LiveSubscriptionSet,
    benchmark_subscription: Option<&SubscriptionDataConfig>,
    _feed_context: &DataFeedContext,
) -> Result<()> {
    let subscriptions = subscriptions_with_option_chains(
        subscriptions_with_benchmark(
            algorithm_manager.subscriptions(),
            benchmark_subscription.cloned(),
        )
        .into_iter()
        .map(Arc::new)
        .collect(),
        &algorithm_manager.option_subscriptions(),
    );
    // Consumers below take owned config slices; materialize once (live has its
    // own id-set short-circuit just below, so this is not the hot path).
    let subscriptions: Vec<SubscriptionDataConfig> = subscriptions
        .iter()
        .map(|config| (**config).clone())
        .collect();
    // Short-circuit the per-slice sync when the subscription set is unchanged
    // (issue #39). The unique-ids are now cached, so building this set is cheap,
    // and both `.sync()` calls below key purely on unique-id — an unchanged id
    // set means neither would add or remove anything, and the corporate-action
    // resolver is already a no-op for symbols it has seen.
    let desired_ids: HashSet<u64> = subscriptions
        .iter()
        .map(SubscriptionDataConfig::unique_id)
        .collect();
    if live_subscriptions.last_synced_ids.as_ref() == Some(&desired_ids) {
        return Ok(());
    }
    live_subscriptions.sync(&subscriptions).await?;
    live_subscriptions.last_synced_ids = Some(desired_ids);
    Ok(())
}

fn should_stop(
    slices_processed: usize,
    max_slices: Option<usize>,
    started: Instant,
    max_runtime: Option<Duration>,
) -> bool {
    max_slices
        .map(|max_slices| slices_processed >= max_slices)
        .unwrap_or(false)
        || max_runtime
            .map(|max_runtime| started.elapsed() >= max_runtime)
            .unwrap_or(false)
}

/// Outcome of polling the live subscription set for the next data item.
enum LivePoll {
    /// A data item is ready. Boxed because `LiveDataItem` is much larger than the
    /// other variants.
    Item(Box<LiveDataItem>),
    /// No item is ready right now, but every subscription stream is healthy.
    Idle,
    /// A subscription stream dropped (channel error or disconnect) — the sidecar
    /// session died and the runner must re-establish it.
    StreamLost,
}

fn poll_live_item(subscriptions: &mut LiveSubscriptionSet, timeout: Duration) -> LivePoll {
    // Non-blocking sweep across every subscription: prefer a ready item, but
    // surface any dropped stream so the runner reconnects instead of trading
    // blind on a dead subscription.
    for subscription in subscriptions.market.values() {
        match subscription.receiver.recv_timeout(Duration::ZERO) {
            Ok(Ok(item)) => return LivePoll::Item(Box::new(item)),
            Ok(Err(_)) => return LivePoll::StreamLost,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return LivePoll::StreamLost,
        }
    }

    // Nothing ready and no drop detected; block briefly on one subscription so the
    // runner loop does not busy-spin. Other subscriptions are picked up by the
    // non-blocking sweep on the next iteration.
    let Some(subscription) = subscriptions.market.values().next() else {
        return LivePoll::Idle;
    };
    match subscription.receiver.recv_timeout(timeout) {
        Ok(Ok(item)) => LivePoll::Item(Box::new(item)),
        Ok(Err(_)) => LivePoll::StreamLost,
        Err(RecvTimeoutError::Timeout) => LivePoll::Idle,
        Err(RecvTimeoutError::Disconnected) => LivePoll::StreamLost,
    }
}

/// The live-data operations the runner needs from the sidecar session. Extracted
/// as a seam so the reconnect supervisor can be exercised with a test double; the
/// production implementation is `DataSidecarClient`.
#[async_trait::async_trait]
pub(crate) trait LiveFeedTransport: Send + Sync {
    /// Generation that owns newly-opened streams.
    fn session_epoch(&self) -> u64;
    /// Re-establish the underlying Flight session after a sidecar drop. Coalesced
    /// across concurrent callers so a restart triggers a single re-establish.
    async fn reconnect_session(&self, failed_epoch: u64) -> Result<()>;
    /// Register a single live subscription, returning its sidecar-side (remote)
    /// id and the batch stream.
    async fn subscribe_live(
        &self,
        config: &SubscriptionDataConfig,
    ) -> Result<(u64, rlean_data_sidecar::DataBatchStream)>;
    /// Remove a live subscription by its sidecar-side (remote) id.
    async fn remove_subscription(&self, remote_id: u64) -> Result<()>;
}

#[async_trait::async_trait]
impl LiveFeedTransport for DataSidecarClient {
    fn session_epoch(&self) -> u64 {
        DataSidecarClient::session_epoch(self)
    }

    async fn reconnect_session(&self, failed_epoch: u64) -> Result<()> {
        self.reconnect_failed_epoch(failed_epoch).await
    }

    async fn subscribe_live(
        &self,
        config: &SubscriptionDataConfig,
    ) -> Result<(u64, rlean_data_sidecar::DataBatchStream)> {
        DataSidecarClient::subscribe_live(self, config).await
    }

    async fn remove_subscription(&self, remote_id: u64) -> Result<()> {
        DataSidecarClient::remove_subscription(self, remote_id).await
    }
}

struct LiveSubscriptionSet {
    transport: Arc<dyn LiveFeedTransport>,
    market: HashMap<u64, LiveDataSubscription>,
    configs: HashMap<u64, SubscriptionDataConfig>,
    remote_ids: HashMap<u64, u64>,
    tasks: HashMap<u64, tokio::task::JoinHandle<()>>,
    session_epoch: u64,
    /// Unique-id set from the last full `sync_live_subscriptions` pass. Used to
    /// short-circuit the per-slice sync when the subscription set is unchanged
    /// (issue #39). `None` until the first sync runs.
    last_synced_ids: Option<HashSet<u64>>,
}

impl LiveSubscriptionSet {
    async fn subscribe_initial(
        transport: Arc<dyn LiveFeedTransport>,
        subscriptions: &[SubscriptionDataConfig],
    ) -> Result<Self> {
        let session_epoch = transport.session_epoch();
        let mut set = Self {
            transport,
            market: HashMap::new(),
            configs: HashMap::new(),
            remote_ids: HashMap::new(),
            tasks: HashMap::new(),
            session_epoch,
            last_synced_ids: None,
        };
        for config in subscriptions {
            set.add(config.clone()).await?;
        }
        Ok(set)
    }

    async fn sync(&mut self, current: &[SubscriptionDataConfig]) -> Result<()> {
        let desired: HashSet<u64> = current
            .iter()
            .map(SubscriptionDataConfig::unique_id)
            .collect();
        let existing: Vec<u64> = self.configs.keys().copied().collect();
        for id in existing {
            if !desired.contains(&id) {
                if let Some(subscription_config) = self.configs.remove(&id) {
                    let _ = subscription_config;
                    if let Some(task) = self.tasks.remove(&id) {
                        task.abort();
                    }
                    if let Some(remote_id) = self.remote_ids.remove(&id) {
                        self.transport.remove_subscription(remote_id).await?;
                    }
                }
                self.market.remove(&id);
            }
        }

        for subscription_config in current {
            if !self.configs.contains_key(&subscription_config.unique_id()) {
                self.add(subscription_config.clone()).await?;
            }
        }
        Ok(())
    }

    async fn add(&mut self, config: SubscriptionDataConfig) -> Result<()> {
        let id = config.unique_id();
        tracing::info!(
            "subscribing live market data for {} ({:?} {:?})",
            config.symbol,
            config.resolution,
            config.tick_type
        );
        let data_type = WireDataType::try_from(SubscriptionSpec::from(&config).data_type)
            .map_err(|value| anyhow::anyhow!("unknown live data type {value}"))?;
        let (remote_id, mut stream) = self.transport.subscribe_live(&config).await?;
        let (sender, receiver) = rlean_data::live_data_channel();
        let live_config = config.clone();
        let symbol = config.symbol.clone();
        let task = tokio::spawn(async move {
            while let Some(batch) = stream.next().await {
                match batch {
                    Ok(batch) => {
                        match decode_batch(data_type, batch, &live_config.symbol)
                            .and_then(|batch| live_items(batch, &live_config))
                        {
                            Ok(items) => {
                                for item in items {
                                    if sender.send(Ok(item)).is_err() {
                                        return;
                                    }
                                }
                            }
                            // A single undecodable batch is a hard data error:
                            // note it and keep the subscription alive rather than
                            // tearing it down over one bad message.
                            Err(error) => {
                                tracing::error!(
                                    symbol = %symbol,
                                    "skipping undecodable live batch: {error}"
                                );
                            }
                        }
                    }
                    // A stream error is the sidecar session dropping (restart,
                    // crash-loop). End the task; dropping `sender` disconnects the
                    // channel, which the runner observes and re-establishes.
                    Err(error) => {
                        tracing::warn!(
                            symbol = %symbol,
                            "live data stream ended; runner will re-establish it: {error}"
                        );
                        return;
                    }
                }
            }
            // Stream ended cleanly (sidecar closed the exchange). Returning drops
            // `sender`, so the runner sees the drop and reconnects.
        });
        let subscription = LiveDataSubscription::new(
            rlean_data::LiveDataSubscriptionConfig::Market(Box::new(config.clone())),
            receiver,
        );
        self.configs.insert(id, config);
        self.remote_ids.insert(id, remote_id);
        self.tasks.insert(id, task);
        self.market.insert(id, subscription);
        Ok(())
    }

    /// Re-establish the sidecar session after it dropped, then re-register every
    /// currently-desired subscription on the fresh session.
    ///
    /// Reconnection uses bounded exponential backoff (1s → 60s cap) and retries a
    /// restarting sidecar forever — the sidecar restarting is a normal event.
    /// Only the subscriptions still in `configs` are re-registered, so anything
    /// the algorithm removed during downtime is not resurrected. Fresh remote ids
    /// are minted for the new session; the old session's ids died with it, so no
    /// subscription is duplicated. The slice synchronizer's frontier is untouched,
    /// so algorithm time never moves backward across a reconnect.
    #[cfg(test)]
    async fn reconnect(&mut self) -> Result<()> {
        self.begin_reconnect().await??;
        self.finish_reconnect().await
    }

    /// Start only the potentially unbounded transport recovery. The runner
    /// keeps this join handle off its data/schedule loop, matching LEAN's
    /// independent real-time handler.
    fn begin_reconnect(&self) -> tokio::task::JoinHandle<Result<()>> {
        let transport = self.transport.clone();
        let failed_epoch = self.session_epoch;
        tokio::spawn(async move {
            let policy = rlean_live::ReconnectPolicy::sidecar_session();
            let mut attempt: u32 = 0;
            loop {
                match transport.reconnect_session(failed_epoch).await {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        if !is_transient_sidecar_error(&error) {
                            return Err(
                                error.context("live sidecar reconnect failed and is not retryable")
                            );
                        }
                        attempt = attempt.saturating_add(1);
                        let delay = policy.delay_for_attempt(attempt - 1);
                        tracing::warn!(
                            attempt,
                            delay_secs = delay.as_secs(),
                            "live sidecar session down; retrying reconnect: {error}"
                        );
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        })
    }

    /// Re-register the desired subscriptions after transport recovery.
    async fn finish_reconnect(&mut self) -> Result<()> {
        // Snapshot the desired set before we start; removals during downtime have
        // already dropped their entries from `configs`.
        let desired: Vec<SubscriptionDataConfig> = self.configs.values().cloned().collect();

        // Abort the drained forwarding tasks and clear the dead session's state.
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
        self.remote_ids.clear();
        self.market.clear();
        self.configs.clear();

        let mut resubscribed = 0usize;
        for config in desired {
            self.add(config).await?;
            resubscribed += 1;
        }
        self.session_epoch = self.transport.session_epoch();
        tracing::warn!(
            resubscribed,
            session_epoch = self.session_epoch,
            "re-established live sidecar subscriptions after reconnect"
        );
        Ok(())
    }

    async fn unsubscribe_all(&mut self) {
        self.configs.clear();
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
        let remote_ids: Vec<_> = self.remote_ids.drain().map(|(_, id)| id).collect();
        for remote_id in remote_ids {
            if let Err(error) = self.transport.remove_subscription(remote_id).await {
                tracing::warn!(
                    "failed to unsubscribe live sidecar subscription {remote_id}: {error}"
                );
            }
        }
        self.market.clear();
    }
}

fn live_items(
    batch: CanonicalDataBatch,
    config: &SubscriptionDataConfig,
) -> anyhow::Result<Vec<LiveDataItem>> {
    Ok(match batch {
        CanonicalDataBatch::TradeBars(rows) => rows
            .into_iter()
            .map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                LiveDataItem::TradeBar(bar)
            })
            .collect(),
        CanonicalDataBatch::QuoteBars(rows) => rows
            .into_iter()
            .map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                LiveDataItem::QuoteBar(bar)
            })
            .collect(),
        CanonicalDataBatch::Ticks(rows) => rows
            .into_iter()
            .map(|mut tick| {
                tick.venue.get_or_insert_with(|| config.venue.clone());
                LiveDataItem::Tick(tick)
            })
            .collect(),
        CanonicalDataBatch::Custom(rows) => {
            let source_type = config
                .custom
                .as_ref()
                .map(|value| value.source_type.clone())
                .unwrap_or_default();
            let ticker = config
                .custom
                .as_ref()
                .map(|value| value.ticker.clone())
                .unwrap_or_else(|| config.symbol.value.to_string());
            rows.into_iter()
                .map(|mut point| {
                    point.venue.get_or_insert_with(|| config.venue.clone());
                    LiveDataItem::CustomData {
                        symbol: config.symbol.clone(),
                        source_type: source_type.clone(),
                        ticker: ticker.clone(),
                        point,
                    }
                })
                .collect()
        }
        CanonicalDataBatch::Universe(mut rows) => {
            for point in &mut rows {
                point.venue.get_or_insert_with(|| config.venue.clone());
            }
            let Some(time) = rows.first().map(|point| point.time) else {
                return Ok(Vec::new());
            };
            let custom = config.custom.as_ref();
            vec![LiveDataItem::UniverseData {
                source_type: custom
                    .map(|value| value.source_type.clone())
                    .unwrap_or_default(),
                ticker: custom
                    .map(|value| value.ticker.clone())
                    .unwrap_or_else(|| config.symbol.value.to_string()),
                resolution: config.resolution,
                time,
                data: rows,
            }]
        }
        CanonicalDataBatch::Fundamentals(rows) => {
            let Some(time) = rows.first().map(|row| row.end_time) else {
                return Ok(Vec::new());
            };
            vec![LiveDataItem::FundamentalUniverseData { time, data: rows }]
        }
        CanonicalDataBatch::OptionUniverse(rows) => {
            let time = rlean_core::DateTime::now();
            crate::option_universe::option_chains_from_rows(config, rows)?
                .into_iter()
                .map(|(_, chain)| LiveDataItem::OptionChainData {
                    time,
                    canonical_permtick: config.symbol.permtick.to_string(),
                    chain: Arc::new(chain),
                })
                .collect()
        }
        CanonicalDataBatch::RiskFreeInterestRates(_) | CanonicalDataBatch::RecordBatch(_) => {
            anyhow::bail!("unsupported canonical live batch type")
        }
    })
}

#[cfg(test)]
mod live_time_pulse_tests {
    use super::*;

    #[test]
    fn sparse_feeds_still_advance_on_wall_clock_seconds() {
        let mut pulses = LiveTimePulse::new(rlean_core::DateTime::from_secs(100));

        assert_eq!(
            pulses.next(rlean_core::DateTime::from_secs(100)),
            None,
            "a pulse is not market data and must not repeat the current frontier"
        );
        assert_eq!(
            pulses.next(rlean_core::DateTime::from_secs(101)),
            Some(rlean_core::DateTime::from_secs(101))
        );
        assert_eq!(
            pulses.next(rlean_core::DateTime::from_secs(105)),
            Some(rlean_core::DateTime::from_secs(105)),
            "a delayed runner adopts wall time rather than replaying fake slices"
        );
    }

    #[test]
    fn wall_clock_regression_never_moves_live_time_backward() {
        let mut pulses = LiveTimePulse::new(rlean_core::DateTime::from_secs(100));
        assert_eq!(pulses.next(rlean_core::DateTime::from_secs(99)), None);
        assert_eq!(
            pulses.next(rlean_core::DateTime::from_secs(101)),
            Some(rlean_core::DateTime::from_secs(101))
        );
    }
}

#[cfg(test)]
mod unmanaged_liquidation_tests {
    use super::*;
    use rlean_algorithm::lifecycle::AlgorithmHistoryService;
    use rlean_core::{Resolution, Symbol};
    use rust_decimal_macros::dec;

    struct FixedLastKnownPrice(f64);

    impl AlgorithmHistoryService for FixedLastKnownPrice {
        fn history(
            &self,
            _algorithm: &rlean_algorithm::qc_algorithm::QcAlgorithm,
            _symbol: &Symbol,
            _periods: usize,
            _resolution: Resolution,
        ) -> rlean_algorithm::lifecycle::HistoryColumns {
            HashMap::new()
        }

        fn last_known_close_price(
            &self,
            _algorithm: &rlean_algorithm::qc_algorithm::QcAlgorithm,
            _symbol: &Symbol,
            _resolution: Resolution,
        ) -> Option<f64> {
            Some(self.0)
        }
    }

    #[test]
    fn queued_closing_orders_reduce_restart_liquidation_quantity() {
        let symbol = rlean_core::Symbol::create_equity("AA", &rlean_core::Market::usa());
        let transactions = rlean_orders::TransactionManager::new();
        let mut closing = rlean_orders::Order::market(
            -1,
            symbol.clone(),
            dec!(-150),
            rlean_core::DateTime::now(),
            "existing brokerage order",
        );
        closing.status = rlean_orders::OrderStatus::Submitted;
        transactions.add_order(closing);
        let mut opposing = rlean_orders::Order::market(
            -2,
            symbol.clone(),
            dec!(3),
            rlean_core::DateTime::now(),
            "opposing order",
        );
        opposing.status = rlean_orders::OrderStatus::Submitted;
        transactions.add_order(opposing);

        assert_eq!(
            open_closing_quantity(&transactions, symbol.id.sid, dec!(-198)),
            dec!(-150)
        );
        assert_eq!(dec!(-198) - dec!(-150), dec!(-48));
    }

    #[test]
    fn brokerage_holdings_are_registered_as_securities_before_liquidation() {
        let symbol = rlean_core::Symbol::create_equity("AA", &rlean_core::Market::usa());
        let mut algorithm = rlean_algorithm::qc_algorithm::QcAlgorithm::new("test", dec!(10000));
        algorithm.set_brokerage_model(
            rlean_algorithm::qc_algorithm::BrokerageName::RobinhoodBrokerage,
            rlean_algorithm::qc_algorithm::AccountType::Cash,
        );
        let holding = rlean_brokerages::BrokerageHolding {
            symbol: symbol.clone(),
            quantity: dec!(198),
            average_price: dec!(46.51),
            market_price: dec!(45.23),
        };

        ensure_brokerage_holding_securities(&mut algorithm, &[holding]);

        assert!(algorithm.securities.contains(&symbol));
        assert_eq!(
            algorithm.securities.get(&symbol).unwrap().current_price(),
            dec!(45.23)
        );
    }

    #[test]
    fn zero_price_brokerage_holding_is_seeded_from_last_known_history() {
        let symbol = rlean_core::Symbol::create_equity("AA", &rlean_core::Market::usa());
        let mut algorithm = rlean_algorithm::qc_algorithm::QcAlgorithm::new("test", dec!(10000));
        let holding = rlean_brokerages::BrokerageHolding {
            symbol: symbol.clone(),
            quantity: dec!(198),
            average_price: dec!(46.51),
            market_price: dec!(0),
        };
        ensure_brokerage_holding_securities(&mut algorithm, std::slice::from_ref(&holding));
        let history_service: Arc<dyn AlgorithmHistoryService> =
            Arc::new(FixedLastKnownPrice(45.23));

        seed_missing_brokerage_holding_prices(&mut algorithm, &[holding], &history_service);

        assert_eq!(
            algorithm.securities.get(&symbol).unwrap().current_price(),
            dec!(45.23)
        );
        assert_eq!(
            algorithm.portfolio.get_holding(&symbol).last_price,
            dec!(45.23)
        );
    }
}

#[cfg(test)]
mod sidecar_reconnect_tests {
    use super::*;
    use rlean_core::{DataNormalizationMode, Market, Resolution, Symbol};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Mutex as StdMutex;
    use tokio_stream::wrappers::ReceiverStream;

    fn equity_config(ticker: &str) -> SubscriptionDataConfig {
        SubscriptionDataConfig::new_equity(
            Symbol::create_equity(ticker, &Market::usa()),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        )
    }

    /// Test double for the sidecar transport. Records every subscribe/remove and
    /// can be told to fail a bounded number of session reconnects (a crash-loop)
    /// before recovering, or to fail permanently (a hard/auth failure).
    struct FakeTransport {
        reconnect_calls: AtomicUsize,
        session_epoch: AtomicU64,
        next_remote_id: AtomicU64,
        subscribe_log: StdMutex<Vec<String>>,
        remove_log: StdMutex<Vec<u64>>,
        reconnects_to_fail: AtomicUsize,
        hard_fail: bool,
    }

    impl FakeTransport {
        fn new() -> Self {
            Self {
                reconnect_calls: AtomicUsize::new(0),
                session_epoch: AtomicU64::new(0),
                next_remote_id: AtomicU64::new(1000),
                subscribe_log: StdMutex::new(Vec::new()),
                remove_log: StdMutex::new(Vec::new()),
                reconnects_to_fail: AtomicUsize::new(0),
                hard_fail: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl LiveFeedTransport for FakeTransport {
        fn session_epoch(&self) -> u64 {
            self.session_epoch.load(AtomicOrdering::SeqCst)
        }

        async fn reconnect_session(&self, failed_epoch: u64) -> Result<()> {
            if self.session_epoch() != failed_epoch {
                return Ok(());
            }
            self.reconnect_calls.fetch_add(1, AtomicOrdering::SeqCst);
            if self.hard_fail {
                return Err(anyhow::anyhow!("invalid Flight authorization metadata"));
            }
            if self.reconnects_to_fail.load(AtomicOrdering::SeqCst) > 0 {
                self.reconnects_to_fail.fetch_sub(1, AtomicOrdering::SeqCst);
                return Err(anyhow::anyhow!("Flight exchange closed"));
            }
            self.session_epoch.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }

        async fn subscribe_live(
            &self,
            config: &SubscriptionDataConfig,
        ) -> Result<(u64, rlean_data_sidecar::DataBatchStream)> {
            self.subscribe_log
                .lock()
                .unwrap()
                .push(config.symbol.value.to_string());
            let remote_id = self.next_remote_id.fetch_add(1, AtomicOrdering::SeqCst);
            // An empty, immediately-ended stream: the runner's forwarding task
            // exits at once, which is all these reconnect assertions need.
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            drop(tx);
            Ok((remote_id, ReceiverStream::new(rx)))
        }

        async fn remove_subscription(&self, remote_id: u64) -> Result<()> {
            self.remove_log.lock().unwrap().push(remote_id);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_resubscribes_all_with_fresh_remote_ids() {
        let transport = Arc::new(FakeTransport::new());
        let configs = vec![equity_config("SPY"), equity_config("QQQ")];
        let mut set = LiveSubscriptionSet::subscribe_initial(transport.clone(), &configs)
            .await
            .unwrap();
        let remote_ids_before: HashSet<u64> = set.remote_ids.values().copied().collect();
        assert_eq!(remote_ids_before.len(), 2);

        set.reconnect().await.unwrap();

        assert_eq!(transport.reconnect_calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(set.configs.len(), 2);
        assert_eq!(set.market.len(), 2);
        assert_eq!(set.remote_ids.len(), 2);
        // Fresh remote ids after reconnect: the old session's ids died with it,
        // so nothing is double-subscribed under a stale id.
        let remote_ids_after: HashSet<u64> = set.remote_ids.values().copied().collect();
        assert!(remote_ids_after.is_disjoint(&remote_ids_before));
        // Two initial subscribes plus two resubscribes.
        assert_eq!(transport.subscribe_log.lock().unwrap().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn subscription_removed_during_downtime_is_not_resurrected() {
        let transport = Arc::new(FakeTransport::new());
        let configs = vec![equity_config("SPY"), equity_config("QQQ")];
        let mut set = LiveSubscriptionSet::subscribe_initial(transport.clone(), &configs)
            .await
            .unwrap();
        // The algorithm dropped QQQ; the desired set is now just SPY.
        set.sync(&[equity_config("SPY")]).await.unwrap();
        assert_eq!(set.configs.len(), 1);

        set.reconnect().await.unwrap();

        assert_eq!(set.configs.len(), 1);
        assert_eq!(set.market.len(), 1);
        let resubscribed: Vec<String> = set
            .configs
            .values()
            .map(|config| config.symbol.value.to_string())
            .collect();
        assert_eq!(resubscribed, vec!["SPY".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_retries_through_a_sidecar_crash_loop() {
        let transport = Arc::new(FakeTransport::new());
        transport
            .reconnects_to_fail
            .store(3, AtomicOrdering::SeqCst);
        let configs = vec![equity_config("SPY")];
        let mut set = LiveSubscriptionSet::subscribe_initial(transport.clone(), &configs)
            .await
            .unwrap();

        set.reconnect().await.unwrap();

        // Three transient failures then success: four attempts total.
        assert_eq!(transport.reconnect_calls.load(AtomicOrdering::SeqCst), 4);
        assert_eq!(set.market.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_surfaces_non_transient_failures() {
        let mut transport = FakeTransport::new();
        transport.hard_fail = true;
        let transport = Arc::new(transport);
        let configs = vec![equity_config("SPY")];
        let mut set = LiveSubscriptionSet::subscribe_initial(transport.clone(), &configs)
            .await
            .unwrap();

        let result = set.reconnect().await;
        assert!(
            result.is_err(),
            "a hard/auth failure must not retry forever"
        );
        assert_eq!(transport.reconnect_calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_stream_owner_adopts_newer_session_generation() {
        let transport = FakeTransport::new();

        transport.reconnect_session(0).await.unwrap();
        assert_eq!(transport.session_epoch(), 1);
        transport.reconnect_session(0).await.unwrap();

        assert_eq!(
            transport.reconnect_calls.load(AtomicOrdering::SeqCst),
            1,
            "a brokerage stream failing after data reconnected must not churn the fresh session"
        );
        assert_eq!(transport.session_epoch(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_retry_does_not_own_the_live_clock() {
        let transport = Arc::new(FakeTransport::new());
        transport
            .reconnects_to_fail
            .store(usize::MAX, AtomicOrdering::SeqCst);
        let set = LiveSubscriptionSet::subscribe_initial(transport, &[equity_config("SPY")])
            .await
            .unwrap();
        let reconnect = set.begin_reconnect();
        tokio::task::yield_now().await;
        assert!(
            !reconnect.is_finished(),
            "the simulated sidecar remains unavailable"
        );

        let mut pulses = LiveTimePulse::new(rlean_core::DateTime::from_secs(100));
        assert_eq!(
            pulses.next(rlean_core::DateTime::from_secs(101)),
            Some(rlean_core::DateTime::from_secs(101)),
            "wall-clock scheduling must continue while sidecar recovery retries"
        );
        reconnect.abort();
    }
}
