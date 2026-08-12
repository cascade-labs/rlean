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
use anyhow::{Context, Result};
use crossbeam_channel::RecvTimeoutError;
use futures::StreamExt;
use rlean_algorithm::lifecycle::{AlgorithmBridge, AlgorithmServices};
use rlean_core::MarketHoursDatabase;
use rlean_data::{LiveDataItem, LiveDataSubscription, SubscriptionDataConfig};
use rlean_data_providers::{HistoricalData, LiveDataEvent, LiveDataProvider, LiveSubscription};
use rlean_live::LiveSliceAssembler;
use rlean_orders::{fill_model::ImmediateFillModel, order_processor::OrderProcessor, OrderEvent};
use rlean_statistics::{Trade, TradeBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Allow one five-minute custom-data publication cycle plus provider and
/// scheduler delay. Older rows remain available through History, but must not
/// enter the live event stream.
const LIVE_CUSTOM_DATA_MAX_AGE_NS: i64 = 10 * 60 * 1_000_000_000;

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
    let runtime_context = crate::AlgorithmRuntimeContext::new(
        config.historical_provider.clone(),
        config.parameters.clone(),
    );
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

    // Execution is independent from the live-data feed. Without a configured
    // execution brokerage, fills remain local paper fills.
    let real_routing = config.brokerage.is_some();
    let mut snapshot = crate::live::snapshots::LiveDeploymentSnapshot::new(
        config
            .deploy_id
            .clone()
            .unwrap_or_else(|| "live".to_string()),
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
            &config.historical_provider,
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

    if let Some(restore) = config.restore.as_ref() {
        if let Some(insights) = restore.insights.as_ref() {
            if let Some((active, closed)) = crate::live::catalog_state::restore_live_insights(
                &algorithm_manager.framework(),
                insights,
            ) {
                tracing::info!(
                    "Restored live framework insights from Verglas catalog: active={active} closed={closed}"
                );
            }
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
    let feed_context = DataFeedContext::new(config.historical_provider.clone());
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
    }
    algorithm_manager.warmup_finished(&mut services);
    // Match C# LEAN's live real-time handler: scheduled events that elapsed
    // while the deployment was offline or warming up are skipped, not replayed.
    algorithm_manager.prime_scheduled_events(rlean_core::DateTime::now());

    let mut live_subscriptions =
        LiveSubscriptionSet::subscribe_initial(config.live_data_provider.clone(), &subscriptions)
            .await?;

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
            let mut brokerage = brokerage;
            match crate::live::transaction_handler::startup_account_sync(
                &mut brokerage,
                portfolio.as_ref(),
                transactions.as_ref(),
            ) {
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
                        crate::live::catalog_state::set_live_starting_portfolio_value_from_synced_account(portfolio);
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
            let router = crate::live::transaction_handler::LiveBrokerageRouter::spawn(brokerage);
            // Match C# LEAN's BrokerageSetupHandler.LoadExistingHoldingsAndOrders:
            // brokerage holdings are authoritative startup state. Loading them
            // must not itself create liquidation orders. The framework can change
            // positions only after its restored or newly generated insights create
            // portfolio targets through the normal execution path.
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
    let stream_updates = config.stream_updates.clone();
    let deploy_started_at = config.deploy_started_at;
    let deploy_id = config.deploy_id.clone();
    let mut catalog_progress_date: Option<chrono::NaiveDate> = None;
    let restored = if config.paper_trading {
        config.restore.clone()
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
    let mut catalog_order_events = all_order_events.len();
    let mut catalog_trades = completed_trades.len();
    if let (Some(restore), Some(algorithm_state)) = (
        restored.as_ref(),
        algorithm_manager.algorithm().algorithm_state(),
    ) {
        crate::live::catalog_state::apply_initial_brokerage_account_state(
            &algorithm_state,
            &restore.account_state,
        );
        if let Some(portfolio) = portfolio.as_ref() {
            let starting_value =
                crate::live::catalog_state::set_live_starting_portfolio_value_from_synced_account(
                    portfolio,
                );
            tracing::info!(
                "Restored paper account from Verglas catalog: cash={} holdings={} open_orders={} order_events={} trades={} starting_value={starting_value}",
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
    if let (Some(sender), Some(transactions), Some(portfolio)) = (
        stream_updates.as_ref(),
        transactions.as_ref(),
        portfolio.as_ref(),
    ) {
        emit_live_catalog_update(
            sender,
            rlean_core::DateTime::now(),
            portfolio,
            transactions,
            Some(&algorithm_manager.framework()),
            deploy_id.as_deref(),
            deploy_started_at,
            &all_order_events,
            &completed_trades,
            &mut catalog_order_events,
            &mut catalog_trades,
            &mut catalog_progress_date,
            true,
        )
        .await?;
    }
    let run_started = Instant::now();
    let mut assembler = LiveSliceAssembler::new();
    assembler.set_subscriptions(live_subscriptions.configs.values());
    let mut time_pulse = LiveTimePulse::new(rlean_core::DateTime::now());

    'live: loop {
        if should_stop(
            algorithm_manager.slices_processed() as usize,
            config.max_slices,
            run_started,
            config.max_runtime,
        ) {
            break;
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
                    stream_updates.as_ref(),
                    deploy_id.as_deref(),
                    deploy_started_at,
                    &mut catalog_order_events,
                    &mut catalog_trades,
                    &mut catalog_progress_date,
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                )
                .await?;
            }
        }

        let poll = poll_live_item(&mut live_subscriptions, Duration::from_millis(250));
        match poll {
            LivePoll::Item(item) => {
                assembler.enqueue(*item);
                loop {
                    match poll_live_item(&mut live_subscriptions, Duration::ZERO) {
                        LivePoll::Item(item) => assembler.enqueue(*item),
                        LivePoll::Idle => break,
                        LivePoll::StreamLost => {
                            anyhow::bail!("live data provider event stream ended unexpectedly")
                        }
                    }
                }
            }
            LivePoll::Idle => {}
            LivePoll::StreamLost => {
                anyhow::bail!("live data provider event stream ended unexpectedly")
            }
        }
        // Universe and strategy code can add/remove subscriptions while live.
        // Keep the assembler's LEAN-style fill-forward enumerators aligned with
        // the exact active provider subscription set.
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
                    stream_updates.as_ref(),
                    deploy_id.as_deref(),
                    deploy_started_at,
                    &mut catalog_order_events,
                    &mut catalog_trades,
                    &mut catalog_progress_date,
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                )
                .await?;
            }
        }

        for mut slice in ready_slices {
            algorithm_manager.include_active_option_chains(&mut slice);
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
                stream_updates.as_ref(),
                deploy_id.as_deref(),
                deploy_started_at,
                &mut catalog_order_events,
                &mut catalog_trades,
                &mut catalog_progress_date,
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
            algorithm_manager.include_active_option_chains(&mut slice);
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
                    stream_updates.as_ref(),
                    deploy_id.as_deref(),
                    deploy_started_at,
                    &mut catalog_order_events,
                    &mut catalog_trades,
                    &mut catalog_progress_date,
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
    algorithm_manager.finish(&mut services);
    live_subscriptions.unsubscribe_all().await;
    config.live_data_provider.disconnect().await?;
    snapshot.slices_processed = algorithm_manager.slices_processed() as usize;
    snapshot.final_value = algorithm_manager
        .portfolio_value()
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    snapshot.recent_order_events = all_order_events.clone();

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
/// security and portfolio before order validation runs.
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

#[allow(clippy::too_many_arguments)]
async fn emit_live_catalog_update(
    sender: &tokio::sync::mpsc::Sender<crate::BacktestStreamUpdate>,
    time: rlean_core::DateTime,
    portfolio: &rlean_algorithm::portfolio::SecurityPortfolioManager,
    transactions: &rlean_orders::TransactionManager,
    framework: Option<&std::sync::Arc<std::sync::Mutex<crate::framework::FrameworkState>>>,
    deploy_id: Option<&str>,
    deploy_started_at: chrono::DateTime<chrono::Utc>,
    all_order_events: &[OrderEvent],
    completed_trades: &[Trade],
    catalog_order_events: &mut usize,
    catalog_trades: &mut usize,
    catalog_progress_date: &mut Option<chrono::NaiveDate>,
    force_checkpoint: bool,
) -> Result<()> {
    let current_date = time.date_utc();
    let start_date = deploy_started_at.date_naive();
    let trading_days = (current_date - start_date).num_days().max(0);
    let starting_cash = portfolio
        .starting_cash()
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    let portfolio_value = portfolio
        .total_portfolio_value()
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    let progress = crate::BacktestProgress {
        current_date,
        start_date,
        end_date: current_date,
        trading_days,
        starting_cash,
        portfolio_value,
    };
    let is_new_day = *catalog_progress_date != Some(current_date);
    let has_events = all_order_events.len() > *catalog_order_events;
    let has_trades = completed_trades.len() > *catalog_trades;
    if !(force_checkpoint || has_events || has_trades || is_new_day) {
        return Ok(());
    }

    let (checkpoint, insight_events) = crate::live::catalog_state::build_live_checkpoint(
        time,
        portfolio,
        transactions,
        framework,
        deploy_id,
        deploy_started_at,
    );
    let checkpoint_json =
        Some(serde_json::to_string(&checkpoint).context("serialize durable live checkpoint")?);
    sender
        .send(crate::BacktestStreamUpdate {
            progress,
            record_daily_progress: is_new_day,
            order_events: all_order_events[*catalog_order_events..].to_vec(),
            trades: completed_trades[*catalog_trades..].to_vec(),
            insight_events,
            checkpoint_json,
        })
        .await
        .map_err(|_| anyhow::anyhow!("live run-catalog stream closed"))?;
    *catalog_order_events = all_order_events.len();
    *catalog_trades = completed_trades.len();
    *catalog_progress_date = Some(current_date);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn service_live_brokerage<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    services: &mut dyn AlgorithmServices,
    router: &mut crate::live::transaction_handler::LiveBrokerageRouter,
    transactions: &Arc<rlean_orders::TransactionManager>,
    portfolio: Option<&Arc<rlean_algorithm::portfolio::SecurityPortfolioManager>>,
    stream_updates: Option<&tokio::sync::mpsc::Sender<crate::BacktestStreamUpdate>>,
    deploy_id: Option<&str>,
    deploy_started_at: chrono::DateTime<chrono::Utc>,
    catalog_order_events: &mut usize,
    catalog_trades: &mut usize,
    catalog_progress_date: &mut Option<chrono::NaiveDate>,
    all_order_events: &mut Vec<OrderEvent>,
    trade_builder: &mut TradeBuilder,
    completed_trades: &mut Vec<Trade>,
) -> Result<()> {
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
        all_order_events.extend(invalid_events);
    }

    // Brokerage events are independent of market-data slices. Stream catalog
    // updates as soon as submitted/filled/invalid state changes.
    let insight_state_changed = algorithm_manager
        .framework()
        .lock()
        .map(|framework| framework.has_pending_insight_events())
        .unwrap_or(true);
    if insight_state_changed
        || all_order_events.len() != previous_order_events
        || completed_trades.len() != previous_trades
    {
        if let (Some(sender), Some(portfolio)) = (stream_updates, portfolio) {
            emit_live_catalog_update(
                sender,
                rlean_core::DateTime::now(),
                portfolio,
                transactions.as_ref(),
                Some(&algorithm_manager.framework()),
                deploy_id,
                deploy_started_at,
                all_order_events,
                completed_trades,
                catalog_order_events,
                catalog_trades,
                catalog_progress_date,
                true,
            )
            .await?;
        }
    }
    Ok(())
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
    stream_updates: Option<&tokio::sync::mpsc::Sender<crate::BacktestStreamUpdate>>,
    deploy_id: Option<&str>,
    deploy_started_at: chrono::DateTime<chrono::Utc>,
    catalog_order_events: &mut usize,
    catalog_trades: &mut usize,
    catalog_progress_date: &mut Option<chrono::NaiveDate>,
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
    if let (Some(sender), Some(transactions), Some(portfolio)) =
        (stream_updates, transactions, portfolio)
    {
        // An alpha refresh can change the active insight expiry/weight without
        // producing an order. Persist that framework state immediately; using
        // only order/trade events leaves a stale checkpoint that can make a
        // real brokerage holding look unmanaged after restart.
        let insight_state_changed = algorithm_manager
            .framework()
            .lock()
            .map(|framework| framework.has_pending_insight_events())
            .unwrap_or(true);
        let force_checkpoint = insight_state_changed
            || all_order_events.len() != prev_order_events
            || completed_trades.len() != prev_trades;
        emit_live_catalog_update(
            sender,
            slice.time,
            portfolio,
            transactions.as_ref(),
            Some(&algorithm_manager.framework()),
            deploy_id,
            deploy_started_at,
            all_order_events,
            completed_trades,
            catalog_order_events,
            catalog_trades,
            catalog_progress_date,
            force_checkpoint,
        )
        .await?;
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
    /// A subscription stream dropped because its channel or provider disconnected.
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

#[derive(Clone)]
struct ProviderSubscriptionSender {
    config: SubscriptionDataConfig,
    sender: crossbeam_channel::Sender<rlean_core::Result<LiveDataItem>>,
}

struct LiveSubscriptionSet {
    provider: Arc<dyn LiveDataProvider>,
    market: HashMap<u64, LiveDataSubscription>,
    configs: HashMap<u64, SubscriptionDataConfig>,
    senders: Arc<std::sync::Mutex<HashMap<u64, ProviderSubscriptionSender>>>,
    dispatcher: tokio::task::JoinHandle<()>,
    last_synced_ids: Option<HashSet<u64>>,
}

impl LiveSubscriptionSet {
    async fn subscribe_initial(
        provider: Arc<dyn LiveDataProvider>,
        subscriptions: &[SubscriptionDataConfig],
    ) -> Result<Self> {
        let mut events = provider.events().await?;
        provider.connect().await?;
        let senders = Arc::new(std::sync::Mutex::new(HashMap::<
            u64,
            ProviderSubscriptionSender,
        >::new()));
        let dispatch_senders = senders.clone();
        let dispatcher = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                match event {
                    Ok(LiveDataEvent::Data {
                        subscription_id,
                        data,
                    }) => {
                        let entry = dispatch_senders
                            .lock()
                            .ok()
                            .and_then(|senders| senders.get(&subscription_id).cloned());
                        let send_result = entry.map(|entry| {
                            native_live_items(data, &entry.config, rlean_core::DateTime::now()).map(
                                |items| {
                                    for item in items {
                                        if entry.sender.send(Ok(item)).is_err() {
                                            break;
                                        }
                                    }
                                },
                            )
                        });
                        if let Some(Err(error)) = send_result {
                            tracing::error!(subscription_id, %error, "invalid live provider data");
                        }
                    }
                    Ok(LiveDataEvent::Reconnected) => {
                        tracing::info!("live data provider reconnected");
                    }
                    Ok(LiveDataEvent::Disconnected { reason }) => {
                        tracing::warn!(%reason, "live data provider disconnected; provider is reconnecting");
                    }
                    Err(error) => {
                        tracing::error!(%error, "live data provider event error");
                    }
                }
            }
            if let Ok(senders) = dispatch_senders.lock() {
                for entry in senders.values() {
                    let _ = entry.sender.send(Err(rlean_core::LeanError::DataError(
                        "live data provider event stream ended".to_string(),
                    )));
                }
            }
        });
        let mut set = Self {
            provider,
            market: HashMap::new(),
            configs: HashMap::new(),
            senders,
            dispatcher,
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
                self.provider.unsubscribe(id).await?;
                self.configs.remove(&id);
                self.market.remove(&id);
                if let Ok(mut senders) = self.senders.lock() {
                    senders.remove(&id);
                }
            }
        }
        for config in current {
            if !self.configs.contains_key(&config.unique_id()) {
                self.add(config.clone()).await?;
            }
        }
        Ok(())
    }

    async fn add(&mut self, config: SubscriptionDataConfig) -> Result<()> {
        let id = config.unique_id();
        let (sender, receiver) = rlean_data::live_data_channel();
        self.provider
            .subscribe(LiveSubscription {
                id,
                configuration: config.clone(),
            })
            .await?;
        if let Ok(mut senders) = self.senders.lock() {
            senders.insert(
                id,
                ProviderSubscriptionSender {
                    config: config.clone(),
                    sender,
                },
            );
        }
        self.market.insert(
            id,
            LiveDataSubscription::new(
                rlean_data::LiveDataSubscriptionConfig::Market(Box::new(config.clone())),
                receiver,
            ),
        );
        self.configs.insert(id, config);
        Ok(())
    }

    async fn unsubscribe_all(&mut self) {
        let ids: Vec<u64> = self.configs.keys().copied().collect();
        for id in ids {
            if let Err(error) = self.provider.unsubscribe(id).await {
                tracing::warn!(subscription_id = id, %error, "failed to unsubscribe live provider");
            }
        }
        self.configs.clear();
        self.market.clear();
        if let Ok(mut senders) = self.senders.lock() {
            senders.clear();
        }
        self.dispatcher.abort();
    }
}

fn native_live_items(
    batch: HistoricalData,
    config: &SubscriptionDataConfig,
    now: rlean_core::DateTime,
) -> anyhow::Result<Vec<LiveDataItem>> {
    Ok(match batch {
        HistoricalData::TradeBars(rows) => rows
            .into_iter()
            .map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                LiveDataItem::TradeBar(bar)
            })
            .collect(),
        HistoricalData::QuoteBars(rows) => rows
            .into_iter()
            .map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                LiveDataItem::QuoteBar(bar)
            })
            .collect(),
        HistoricalData::Ticks(rows) => rows
            .into_iter()
            .map(|mut tick| {
                tick.venue.get_or_insert_with(|| config.venue.clone());
                LiveDataItem::Tick(tick)
            })
            .collect(),
        HistoricalData::CustomPoints(rows) => {
            let custom = config
                .custom
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("custom live data has no subscription metadata"))?;
            // <DIV> C# LEAN's LiveCustomDataSubscriptionEnumeratorFactory
            // disables maximum-age filtering because some daily custom sources
            // publish after weekends. rlean custom points share a durable table
            // with historical backfills, so live delivery uses a bounded grace
            // period to prevent an archive append from replaying through OnData.
            let cutoff = now - rlean_core::TimeSpan::from_nanos(LIVE_CUSTOM_DATA_MAX_AGE_NS);
            let mut stale = 0usize;
            let items = rows
                .into_iter()
                .filter(|point| {
                    let fresh = point.end_time >= cutoff;
                    stale += usize::from(!fresh);
                    fresh
                })
                .map(|point| LiveDataItem::CustomData {
                    symbol: config.symbol.clone(),
                    source_type: custom.source_type.clone(),
                    ticker: custom.ticker.clone(),
                    point,
                })
                .collect();
            if stale > 0 {
                tracing::warn!(
                    provider = %custom.source_type,
                    feed = %custom.ticker,
                    stale_rows = stale,
                    cutoff_ns = cutoff.0,
                    "discarded stale live custom-data events"
                );
            }
            items
        }
        HistoricalData::OptionUniverse(rows) => {
            crate::option_universe::option_chains_from_rows(config, rows)?
                .into_iter()
                .filter_map(|(date, chain)| {
                    Some(LiveDataItem::OptionChainData {
                        time: rlean_core::NanosecondTimestamp(
                            date.succ_opt()?
                                .and_hms_opt(0, 0, 0)?
                                .and_utc()
                                .timestamp_nanos_opt()?,
                        ),
                        canonical_permtick: config
                            .option_chain
                            .as_ref()?
                            .canonical_permtick
                            .clone(),
                        chain: std::sync::Arc::new(chain),
                    })
                })
                .collect()
        }
        HistoricalData::FundamentalUniverse(rows) => {
            let mut by_frontier = std::collections::BTreeMap::new();
            for row in rows {
                let time = rlean_core::NanosecondTimestamp(
                    row.time.and_utc().timestamp_nanos_opt().unwrap_or_default(),
                );
                let frontier = rlean_core::NanosecondTimestamp(
                    row.end_time
                        .and_utc()
                        .timestamp_nanos_opt()
                        .unwrap_or_default(),
                );
                let mut point = rlean_data::FundamentalData::new(
                    rlean_core::Symbol::create_equity(
                        &row.symbol_value,
                        &rlean_core::Market::new(&row.market),
                    ),
                    time,
                );
                point.end_time = frontier;
                point.volume = Some(row.volume);
                point.dollar_volume = Some(row.dollar_volume);
                point.market_cap = Some(row.market_cap);
                by_frontier
                    .entry(frontier)
                    .or_insert_with(Vec::new)
                    .push(point);
            }
            by_frontier
                .into_iter()
                .map(|(time, data)| LiveDataItem::FundamentalUniverseData { time, data })
                .collect()
        }
        HistoricalData::FutureUniverse(_) => {
            anyhow::bail!("future-universe live delivery is not supported")
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
mod live_custom_data_age_tests {
    use super::*;
    use rlean_core::{Market, Resolution, Symbol, TimeSpan};
    use rlean_data::{CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata};
    use rlean_data_tables::CustomDataPoint;
    use rust_decimal_macros::dec;

    fn custom_config() -> SubscriptionDataConfig {
        let query = CustomDataQuery::default();
        let metadata = CustomSubscriptionMetadata {
            source_type: "unusual_whales".to_string(),
            ticker: "market_tide".to_string(),
            config: CustomDataConfig {
                ticker: "market_tide".to_string(),
                source_type: "unusual_whales".to_string(),
                resolution: Resolution::Minute,
                properties: HashMap::new(),
                query: query.clone(),
            },
            dynamic_query: query,
        };
        SubscriptionDataConfig::new_custom(
            Symbol::create_base("unusual_whales", "market_tide", &Market::usa()),
            Resolution::Minute,
            metadata,
        )
    }

    #[test]
    fn live_custom_data_keeps_delay_grace_and_rejects_archive_rows() {
        let now = rlean_core::DateTime::from_secs(2_000_000);
        let exactly_at_cutoff = now - TimeSpan::from_mins(10);
        let stale = exactly_at_cutoff - TimeSpan::from_nanos(1);
        let recent = now - TimeSpan::from_mins(5);
        let rows = vec![
            CustomDataPoint::empty(stale, stale, dec!(1)),
            CustomDataPoint::empty(exactly_at_cutoff, exactly_at_cutoff, dec!(2)),
            CustomDataPoint::empty(recent, recent, dec!(3)),
        ];

        let items =
            native_live_items(HistoricalData::CustomPoints(rows), &custom_config(), now).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].end_time(), exactly_at_cutoff);
        assert_eq!(items[1].end_time(), recent);
    }
}

#[cfg(test)]
mod brokerage_holding_restore_tests {
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
    fn brokerage_holdings_are_registered_as_securities_during_startup_sync() {
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
