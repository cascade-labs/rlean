use crate::{
    algorithm_manager::{AlgorithmManager, OrderEventProcessing},
    data_feed::DataFeedContext,
    runner::backtest::{
        benchmark_subscription_for_symbol, subscriptions_with_benchmark,
        subscriptions_with_option_chains,
    },
    LiveRunConfig, LiveRunResult,
};
use anyhow::Result;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lean_algorithm::lifecycle::{AlgorithmBridge, AlgorithmServices};
use lean_core::{LeanError, MarketHoursDatabase};
use lean_data::{
    live_data_channel, CustomDataPoint, LiveDataItem, LiveDataSubscription, SubscriptionDataConfig,
};
use lean_data_providers::ICustomDataSource;
use lean_live::LiveSliceAssembler;
use lean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
    OrderEvent,
};
use lean_statistics::{Trade, TradeBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// Max times a single symbol's Invalid insight-backed order re-arms the
/// framework rebalance before the runner gives up (until a natural trigger:
/// insight change, expiry, or day boundary). Bounds retry loops against
/// permanently-rejecting symbols (e.g. restricted tickers).
const MAX_INSIGHT_RETRIES: u32 = 3;

/// Engine-owned live runner entry point.
///
/// All strategy languages enter through `lean_algorithm::lifecycle::AlgorithmBridge`; language
/// crates do not provide runner futures or alternate loops.
pub async fn run_live<B>(bridge: B, config: LiveRunConfig) -> Result<LiveRunResult>
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
        crate::EngineAlgorithmServices::new(lean_core::DateTime::now(), runtime_context.clone());
    let mut algorithm_manager = AlgorithmManager::new(bridge, runtime_context);
    let market_hours_database = MarketHoursDatabase::global();
    algorithm_manager.set_market_hours_database(market_hours_database);

    // Mode selection: when a real brokerage is attached that does not use local
    // paper fills, orders are routed to and reconciled with the brokerage. When
    // the brokerage is None or `uses_local_paper_fills()`, the local paper-fill
    // path is used unchanged.
    let real_routing = config
        .brokerage
        .as_ref()
        .map(|brokerage| !brokerage.uses_local_paper_fills())
        .unwrap_or(false);

    let brokerage_sync =
        crate::live::brokerage::sync_brokerage_state(config.brokerage.as_mut()).await?;
    let mut snapshot = crate::live::snapshots::LiveDeploymentSnapshot::new(
        config
            .output_dir
            .as_ref()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("live")
            .to_string(),
    );
    snapshot.recent_order_events = brokerage_sync.order_events.clone();

    algorithm_manager.initialize(&mut services)?;

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

    // Fix 3: bounded retry of failed insight-backed targets. When an order tied
    // to a framework portfolio target comes back Invalid, re-arm the rebalance so
    // the PCM recomputes and resubmits on the next slice (e.g. after trims free
    // cash). Bounded per symbol so permanently-restricted symbols cannot cause a
    // rebalance storm; exhausted symbols WARN and wait for the next natural
    // trigger.
    let mut insight_retry_budget: HashMap<u64, u32> = HashMap::new();

    let benchmark_subscription = benchmark_subscription_for_symbol(
        &algorithm_manager.benchmark_symbol(),
        algorithm_manager.subscriptions(),
    );
    let subscriptions = subscriptions_with_benchmark(
        algorithm_manager.subscriptions(),
        benchmark_subscription.clone(),
    );
    let subscriptions =
        subscriptions_with_option_chains(subscriptions, &algorithm_manager.option_subscriptions());
    algorithm_manager.prepare_data_delivery(&subscriptions)?;
    algorithm_manager.warmup_finished(&mut services);

    // Resolve factor/map files for the initial live subscriptions the same way
    // the backtest feed does, independent of any bar data (issue #30). Live price
    // seeding pulls daily history through the same provider, so a live deployment
    // must fetch + persist corporate actions for every subscribed equity too — it
    // does not build subscriptions through DataManager/SubscriptionStream, so the
    // resolution that path performs would otherwise never run here. The
    // DataFeedContext holds the per-run resolution cache and persists eagerly.
    let feed_context = DataFeedContext::new(config.data_store.clone())
        .with_history_provider(config.history_provider.clone());
    resolve_live_corporate_actions(&feed_context, &subscriptions).await;

    let mut live_subscriptions = LiveSubscriptionSet::subscribe_initial(
        &mut config.live_data_queue,
        &subscriptions,
        &config.custom_data_sources,
        config
            .brokerage_model
            .as_ref()
            .map(|model| format!("{model:?}")),
        config.paper_trading,
        &config.parameters,
    )?;

    let transactions = algorithm_manager.transactions();
    let portfolio = algorithm_manager.portfolio();
    // The order processor drives local paper fills and also exposes the
    // transaction manager to the deployment-writer snapshot. It is always built
    // (so snapshots keep working), but under real brokerage routing it is *not*
    // passed to `process_order_events`, so it never generates local fills.
    let order_processor = transactions.as_ref().map(|tm| {
        OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            tm.clone(),
        )
    });

    // Real brokerage routing: connect + sync the account, then hand the
    // brokerage to a dedicated worker thread that submits orders and polls for
    // fills/status changes. Under paper/local mode this stays `None` and the
    // local paper-fill path is used unchanged.
    let mut brokerage_router = None;
    if real_routing {
        if let Some(mut brokerage) = config.brokerage.take() {
            let sync = match crate::live::transaction_handler::startup_account_sync(
                &mut brokerage,
                portfolio.as_ref(),
                transactions.as_ref(),
            ) {
                Ok(sync) => {
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
                crate::live::transaction_handler::LiveBrokerageRouter::spawn(brokerage);
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
            );
            brokerage_router = Some(router);
        }
    }
    let live_writer = config
        .output_dir
        .as_ref()
        .map(|dir| crate::live::deployment_writer::LiveDeploymentWriter::new(dir.clone()));
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
        None => brokerage_sync.order_events,
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
                lean_core::DateTime::now(),
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
            lean_core::DateTime::now(),
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
    let mut custom_subscriptions =
        LiveCustomSubscriptionSet::subscribe_initial(&subscriptions, &config.custom_data_sources);

    'live: loop {
        if should_stop(
            algorithm_manager.slices_processed() as usize,
            config.max_slices,
            run_started,
            config.max_runtime,
        ) {
            break;
        }

        let Some(item) = next_live_item(
            &mut live_subscriptions,
            &custom_subscriptions,
            Duration::from_millis(250),
        )?
        else {
            continue;
        };

        for slice in assembler.push(item) {
            process_live_slice(
                &mut algorithm_manager,
                &mut services,
                &mut config,
                &mut live_subscriptions,
                &mut custom_subscriptions,
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
                &mut insight_retry_budget,
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
        if let Some(slice) = assembler.flush() {
            if !should_stop(
                algorithm_manager.slices_processed() as usize,
                config.max_slices,
                run_started,
                config.max_runtime,
            ) {
                process_live_slice(
                    &mut algorithm_manager,
                    &mut services,
                    &mut config,
                    &mut live_subscriptions,
                    &mut custom_subscriptions,
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
                    &mut insight_retry_budget,
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
    // Flush any corporate-action rows still buffered (eager persistence flushes
    // as symbols resolve, so this is a final safety net for the tail).
    if let Err(err) = feed_context.flush_corporate_action_cache_writes().await {
        tracing::warn!("failed to flush corporate-action cache on live shutdown: {err}");
    }
    live_subscriptions.unsubscribe_all(&mut config.live_data_queue);
    custom_subscriptions.unsubscribe_all();
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
/// It also notifies the framework of the added securities so the PCM's
/// rebalance-on-security-changes policy fires and the alpha/PCM/execution models
/// observe the restored universe.
///
/// Restored securities start with price 0 — live quotes arrive only seconds
/// after startup, typically *after* the first framework run, and the PCM skips
/// zero-price symbols when building targets. So every active-insight security
/// without a price is seeded synchronously with its last-known close from the
/// history provider (daily resolution; the engine-side FuncSecuritySeeder
/// equivalent). Symbols whose history fetch yields nothing are returned in the
/// pending map; the live loop re-notifies the framework the moment such a
/// security gains a live price, so the PCM re-rebalances then.
fn restore_securities_for_active_insights<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    history_service: &Arc<dyn lean_algorithm::lifecycle::AlgorithmHistoryService>,
) -> HashMap<u64, lean_core::Symbol> {
    let mut pending_prices: HashMap<u64, lean_core::Symbol> = HashMap::new();
    let framework = algorithm_manager.framework();
    let insight_symbols: Vec<lean_core::Symbol> = {
        let fw = match framework.lock() {
            Ok(fw) => fw,
            Err(_) => return pending_prices,
        };
        // Deduplicate by sid while preserving order.
        let mut seen = HashSet::new();
        fw.insights
            .active(lean_core::DateTime::now())
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

    let mut restored: Vec<lean_core::Symbol> = Vec::new();
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
            let resolution = lean_core::Resolution::Minute;
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
        if algorithm.utc_time == lean_core::DateTime::EPOCH {
            crate::algorithm_services::advance_algorithm_time(
                &mut algorithm,
                lean_core::DateTime::now(),
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
            .last_known_close_price(&algorithm, &symbol, lean_core::Resolution::Daily)
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
/// price, notify the framework again so the PCM's rebalance-on-security-changes
/// policy re-fires and the restored insight finally produces a target.
fn renotify_restored_securities_with_prices<B: AlgorithmBridge>(
    algorithm_manager: &AlgorithmManager<B>,
    pending: &mut HashMap<u64, lean_core::Symbol>,
) {
    if pending.is_empty() {
        return;
    }
    let Some(algorithm_state) = algorithm_manager.algorithm().algorithm_state() else {
        pending.clear();
        return;
    };
    let priced: Vec<lean_core::Symbol> = {
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

/// Fix 3: re-arm the framework rebalance for every Invalid order event in
/// `new_events` whose symbol has an active (insight-backed) target and still has
/// retry budget left.
///
/// This is the latency fix for the production incident: a rejected trim/entry no
/// longer waits for the next insight change/expiry/day boundary — the next slice
/// recomputes targets against corrected state (e.g. cash freed by a retried trim)
/// and resubmits within seconds.
///
/// Bounded per symbol (`MAX_INSIGHT_RETRIES`): once a symbol exhausts its budget
/// it WARNs and no longer re-triggers, so a permanently-rejecting symbol (e.g. a
/// restricted ticker) cannot cause a rebalance storm. Liquidation orders are not
/// insight-backed (no active insight), so `rearm_framework_rebalance_for_symbol`
/// returns false for them and they never re-trigger.
fn rearm_rebalance_for_invalid_insight_orders<B: AlgorithmBridge>(
    algorithm_manager: &AlgorithmManager<B>,
    new_events: &[OrderEvent],
    insight_retry_budget: &mut HashMap<u64, u32>,
) {
    let framework = algorithm_manager.framework();
    let mut seen_this_slice: HashSet<u64> = HashSet::new();
    for event in new_events {
        if event.status != lean_orders::OrderStatus::Invalid {
            continue;
        }
        let sid = event.symbol.id.sid;
        // Only act once per symbol per slice even if several legs were rejected.
        if !seen_this_slice.insert(sid) {
            continue;
        }
        let attempts = insight_retry_budget.entry(sid).or_insert(0);
        if *attempts >= MAX_INSIGHT_RETRIES {
            tracing::warn!(
                "Invalid order for {} exhausted its {MAX_INSIGHT_RETRIES}-attempt retry budget; not re-arming rebalance until the next natural trigger",
                event.symbol.value
            );
            continue;
        }
        let now = lean_core::DateTime::now();
        if crate::framework::rearm_framework_rebalance_for_symbol(&framework, &event.symbol, now) {
            *attempts += 1;
            tracing::info!(
                "Invalid insight-backed order for {}; re-arming framework rebalance (attempt {}/{MAX_INSIGHT_RETRIES})",
                event.symbol.value,
                *attempts
            );
        } else {
            // Not insight-backed (e.g. a liquidation order): leave the rebalance
            // trigger alone. Roll back the speculative counter insert so a later
            // insight-backed rejection for the same symbol keeps its full budget.
            if *attempts == 0 {
                insight_retry_budget.remove(&sid);
            }
        }
    }
}

/// Security types `QcAlgorithm::add_security_symbol` can register (it panics on
/// anything else). Kept in lockstep with that method's supported arms.
fn is_restorable_security_type(security_type: lean_core::SecurityType) -> bool {
    matches!(
        security_type,
        lean_core::SecurityType::Base
            | lean_core::SecurityType::Equity
            | lean_core::SecurityType::Forex
            | lean_core::SecurityType::Crypto
            | lean_core::SecurityType::CryptoFuture
            | lean_core::SecurityType::Option
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
fn liquidate_unmanaged_holdings<B: AlgorithmBridge>(
    algorithm_manager: &AlgorithmManager<B>,
    transactions: Option<&Arc<lean_orders::TransactionManager>>,
    holdings: &[lean_brokerages::BrokerageHolding],
    router: &mut crate::live::transaction_handler::LiveBrokerageRouter,
) {
    let Some(transactions) = transactions else {
        tracing::warn!(
            "liquidate-unmanaged-holdings requested but no transaction manager is available; skipping"
        );
        return;
    };

    let framework = algorithm_manager.framework();
    let managed_sids: HashSet<u64> = match framework.lock() {
        Ok(fw) => fw
            .insights
            .active(lean_core::DateTime::now())
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
        let quantity = -holding.quantity;
        let order_id = transactions.next_order_id();
        let order = lean_orders::Order::market(
            order_id,
            holding.symbol.clone(),
            quantity,
            lean_core::DateTime::now(),
            "Liquidate unmanaged holding",
        );
        transactions.add_or_update_order(order);
        liquidated += 1;
        tracing::info!(
            "liquidating unmanaged holding: {} quantity={}",
            holding.symbol.value,
            quantity
        );
    }

    tracing::info!("liquidate-unmanaged-holdings: kept={kept} (managed) liquidated={liquidated}");

    if liquidated > 0 {
        // Reuse the router's existing submission path; the New orders just
        // registered flow to the brokerage and reconcile on the poll path.
        router.dispatch_pending(transactions);
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_live_slice<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    services: &mut dyn AlgorithmServices,
    config: &mut LiveRunConfig,
    live_subscriptions: &mut LiveSubscriptionSet,
    custom_subscriptions: &mut LiveCustomSubscriptionSet,
    benchmark_subscription: Option<&SubscriptionDataConfig>,
    order_processor: &Option<OrderProcessor>,
    mut brokerage_router: Option<&mut crate::live::transaction_handler::LiveBrokerageRouter>,
    transactions: Option<&std::sync::Arc<lean_orders::TransactionManager>>,
    portfolio: Option<&std::sync::Arc<lean_algorithm::portfolio::SecurityPortfolioManager>>,
    live_writer: Option<&crate::live::deployment_writer::LiveDeploymentWriter>,
    all_order_events: &mut Vec<OrderEvent>,
    trade_builder: &mut TradeBuilder,
    completed_trades: &mut Vec<Trade>,
    pending_price_restores: &mut HashMap<u64, lean_core::Symbol>,
    insight_retry_budget: &mut HashMap<u64, u32>,
    feed_context: &DataFeedContext,
    slice: &lean_data::Slice,
) -> Result<()> {
    if !slice.has_data {
        return Ok(());
    }
    let slice_arc = Arc::new(slice.clone());

    let new_trading_day = algorithm_manager.handle_new_trading_day(slice, services);
    let changes = algorithm_manager.apply_universe_selection(slice, new_trading_day, services);
    if changes.has_changes() {
        sync_live_subscriptions(
            algorithm_manager,
            config,
            live_subscriptions,
            custom_subscriptions,
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
    let option_chains: Vec<(&str, &lean_data::OptionChain)> = slice
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
    // Fix 3: Invalid order events for insight-backed symbols re-arm the framework
    // rebalance so run_framework below recomputes and resubmits this slice. Only
    // examine events produced this slice, and only under real brokerage routing.
    if brokerage_router.is_some() {
        rearm_rebalance_for_invalid_insight_orders(
            algorithm_manager,
            &all_order_events[prev_order_events..],
            insight_retry_budget,
        );
    }
    if let Some(writer) = live_writer {
        writer.append_order_events(&all_order_events[prev_order_events..]);
        writer.append_trades(&completed_trades[prev_trades..]);
    }

    algorithm_manager.deliver_data(
        lean_algorithm::algorithm::DataDeliveryPayload { slice: slice_arc },
        services,
    );
    algorithm_manager.run_framework(slice, services);
    // Under real brokerage routing, forward any New orders the framework/strategy
    // just created to the brokerage worker for submission.
    if let (Some(router), Some(transactions)) = (brokerage_router.as_mut(), transactions) {
        router.dispatch_pending(transactions);
    }
    // Mirror the backtest runner: securities added mid-run (e.g. add_equity from
    // an alpha model or OnData) never surface through universe selection, so the
    // live data subscriptions must be re-synced against the algorithm's current
    // subscription list after strategy code has run.
    sync_live_subscriptions(
        algorithm_manager,
        config,
        live_subscriptions,
        custom_subscriptions,
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
    config: &mut LiveRunConfig,
    live_subscriptions: &mut LiveSubscriptionSet,
    custom_subscriptions: &mut LiveCustomSubscriptionSet,
    benchmark_subscription: Option<&SubscriptionDataConfig>,
    feed_context: &DataFeedContext,
) -> Result<()> {
    let subscriptions = subscriptions_with_option_chains(
        subscriptions_with_benchmark(
            algorithm_manager.subscriptions(),
            benchmark_subscription.cloned(),
        ),
        &algorithm_manager.option_subscriptions(),
    );
    // Resolve factor/map files for any equity added mid-session (e.g. add_equity
    // from an alpha model). The per-run cache makes this a no-op for symbols
    // already resolved, so it is cheap to call on every sync.
    resolve_live_corporate_actions(feed_context, &subscriptions).await;
    live_subscriptions.sync(config, &subscriptions)?;
    custom_subscriptions.sync(&subscriptions, &config.custom_data_sources);
    Ok(())
}

/// Resolve + persist factor/map files for the given live subscription configs,
/// mirroring the backtest feed. Bar-independent; the per-run resolution cache in
/// `DataFeedContext` dedups repeat symbols and eager persistence keeps fetched
/// rows even if the deployment is later stopped (issue #30). Equity subscriptions
/// only; non-equities are skipped by `resolve_corporate_actions` itself.
async fn resolve_live_corporate_actions(
    feed_context: &DataFeedContext,
    subscriptions: &[SubscriptionDataConfig],
) {
    let mut seen = HashSet::new();
    for config in subscriptions {
        if !seen.insert(config.symbol.clone()) {
            continue;
        }
        feed_context.resolve_corporate_actions(&config.symbol).await;
    }
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

fn next_live_item(
    subscriptions: &mut LiveSubscriptionSet,
    custom_subscriptions: &LiveCustomSubscriptionSet,
    timeout: Duration,
) -> Result<Option<LiveDataItem>> {
    match custom_subscriptions.receiver.try_recv() {
        Ok(item) => return Ok(Some(item?)),
        Err(crossbeam_channel::TryRecvError::Empty) => {}
        Err(crossbeam_channel::TryRecvError::Disconnected) => {}
    }
    for subscription in subscriptions.market.values() {
        match subscription.receiver.recv_timeout(Duration::ZERO) {
            Ok(item) => return Ok(Some(item?)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {}
        }
    }

    if subscriptions.market.is_empty() {
        return match custom_subscriptions.receiver.recv_timeout(timeout) {
            Ok(item) => Ok(Some(item?)),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => Ok(None),
        };
    }

    for subscription in subscriptions.market.values() {
        match subscription.receiver.recv_timeout(timeout) {
            Ok(item) => return Ok(Some(item?)),
            Err(RecvTimeoutError::Timeout) => {
                return match custom_subscriptions.receiver.try_recv() {
                    Ok(item) => Ok(Some(item?)),
                    Err(crossbeam_channel::TryRecvError::Empty)
                    | Err(crossbeam_channel::TryRecvError::Disconnected) => Ok(None),
                };
            }
            Err(RecvTimeoutError::Disconnected) => {}
        }
    }
    Ok(None)
}

struct LiveSubscriptionSet {
    market: HashMap<u64, LiveDataSubscription>,
    configs: HashMap<u64, SubscriptionDataConfig>,
}

impl LiveSubscriptionSet {
    fn subscribe_initial(
        queue: &mut lean_live::DataQueueHandlerManager,
        subscriptions: &[SubscriptionDataConfig],
        custom_data_sources: &[Arc<dyn ICustomDataSource>],
        brokerage: Option<String>,
        paper_trading: bool,
        parameters: &HashMap<String, String>,
    ) -> Result<Self> {
        let job = lean_data::LiveNodePacket {
            brokerage: brokerage.unwrap_or_else(|| "Default".to_string()),
            data_queue_handlers: Vec::new(),
            brokerage_data: HashMap::new(),
            parameters: parameters.clone(),
            paper_trading,
        };
        queue.set_job(&job)?;

        let mut set = Self {
            market: HashMap::new(),
            configs: HashMap::new(),
        };
        for config in subscriptions {
            if is_custom_subscription(config, custom_data_sources) {
                tracing::info!(
                    "routing live custom data subscription through custom data source {}",
                    config.symbol
                );
                continue;
            }
            set.add(queue, config.clone())?;
        }
        Ok(set)
    }

    fn sync(
        &mut self,
        config: &mut LiveRunConfig,
        current: &[SubscriptionDataConfig],
    ) -> Result<()> {
        let desired: HashSet<u64> = current
            .iter()
            .map(SubscriptionDataConfig::unique_id)
            .collect();
        let existing: Vec<u64> = self.configs.keys().copied().collect();
        for id in existing {
            if !desired.contains(&id) {
                if let Some(subscription_config) = self.configs.remove(&id) {
                    config.live_data_queue.unsubscribe(&subscription_config)?;
                }
                self.market.remove(&id);
            }
        }

        for subscription_config in current {
            if !self.configs.contains_key(&subscription_config.unique_id()) {
                // Custom configs never enter self.configs, so every sync() call
                // sees them as new; log at debug to avoid per-slice spam now
                // that sync runs on every slice.
                if is_custom_subscription(subscription_config, &config.custom_data_sources) {
                    tracing::debug!(
                        "routing live custom data subscription through custom data source {}",
                        subscription_config.symbol
                    );
                    continue;
                }
                self.add(&mut config.live_data_queue, subscription_config.clone())?;
            }
        }
        Ok(())
    }

    fn add(
        &mut self,
        queue: &mut lean_live::DataQueueHandlerManager,
        config: SubscriptionDataConfig,
    ) -> Result<()> {
        let id = config.unique_id();
        tracing::info!(
            "subscribing live market data for {} ({:?} {:?})",
            config.symbol,
            config.resolution,
            config.tick_type
        );
        let subscription = queue.subscribe(&config)?;
        self.configs.insert(id, config);
        self.market.insert(id, subscription);
        Ok(())
    }

    fn unsubscribe_all(&mut self, queue: &mut lean_live::DataQueueHandlerManager) {
        let configs: Vec<_> = self.configs.drain().map(|(_, config)| config).collect();
        for config in configs {
            if let Err(error) = queue.unsubscribe(&config) {
                tracing::warn!(
                    "failed to unsubscribe live data for {}: {error}",
                    config.symbol
                );
            }
        }
        self.market.clear();
    }
}

struct LiveCustomSubscriptionSet {
    receiver: Receiver<lean_core::Result<LiveDataItem>>,
    sender: Sender<lean_core::Result<LiveDataItem>>,
    stop: Arc<AtomicBool>,
    workers: HashMap<u64, std::thread::JoinHandle<()>>,
    configs: HashMap<u64, SubscriptionDataConfig>,
}

impl LiveCustomSubscriptionSet {
    fn subscribe_initial(
        subscriptions: &[SubscriptionDataConfig],
        custom_data_sources: &[Arc<dyn ICustomDataSource>],
    ) -> Self {
        let (sender, receiver) = live_data_channel();
        let mut set = Self {
            receiver,
            sender,
            stop: Arc::new(AtomicBool::new(false)),
            workers: HashMap::new(),
            configs: HashMap::new(),
        };
        set.sync(subscriptions, custom_data_sources);
        set
    }

    fn sync(
        &mut self,
        subscriptions: &[SubscriptionDataConfig],
        custom_data_sources: &[Arc<dyn ICustomDataSource>],
    ) {
        let desired = subscriptions
            .iter()
            .filter(|config| is_custom_subscription(config, custom_data_sources))
            .map(SubscriptionDataConfig::unique_id)
            .collect::<HashSet<_>>();
        let existing = self.configs.keys().copied().collect::<Vec<_>>();
        for id in existing {
            if !desired.contains(&id) {
                self.configs.remove(&id);
                // The worker observes the shared stop flag only on global shutdown.
                // Dynamic custom unsubscription is rare; stale workers are harmless
                // because they no longer feed subscribed symbols once their sender
                // is dropped at process end.
            }
        }

        for config in subscriptions {
            let id = config.unique_id();
            if self.configs.contains_key(&id) {
                continue;
            }
            let Some((source, custom)) = custom_source_for_config(config, custom_data_sources)
            else {
                continue;
            };
            tracing::info!(
                "starting live custom data poller for {}:{}",
                custom.source_type,
                custom.ticker
            );
            let handle = spawn_live_custom_poller(
                config.clone(),
                source,
                self.sender.clone(),
                self.stop.clone(),
            );
            self.configs.insert(id, config.clone());
            self.workers.insert(id, handle);
        }
    }

    fn unsubscribe_all(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for (_, worker) in self.workers.drain() {
            let _ = worker.join();
        }
        self.configs.clear();
    }
}

fn is_custom_subscription(
    config: &SubscriptionDataConfig,
    custom_data_sources: &[Arc<dyn ICustomDataSource>],
) -> bool {
    custom_source_for_config(config, custom_data_sources).is_some()
}

fn custom_source_for_config(
    config: &SubscriptionDataConfig,
    custom_data_sources: &[Arc<dyn ICustomDataSource>],
) -> Option<(
    Arc<dyn ICustomDataSource>,
    lean_data::CustomSubscriptionMetadata,
)> {
    let custom = config.custom.as_ref()?;
    let source = custom_data_sources
        .iter()
        .find(|source| source.name().eq_ignore_ascii_case(&custom.source_type))?
        .clone();
    Some((source, custom.clone()))
}

fn spawn_live_custom_poller(
    config: SubscriptionDataConfig,
    source: Arc<dyn ICustomDataSource>,
    sender: Sender<lean_core::Result<LiveDataItem>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("live-custom-{}", config.symbol.value))
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(Err(LeanError::DataError(format!(
                        "failed to start live custom data poller: {error}"
                    ))));
                    return;
                }
            };
            runtime.block_on(run_live_custom_poller(config, source, sender, stop));
        })
        .expect("failed to spawn live custom data poller")
}

async fn run_live_custom_poller(
    config: SubscriptionDataConfig,
    source: Arc<dyn ICustomDataSource>,
    sender: Sender<lean_core::Result<LiveDataItem>>,
    stop: Arc<AtomicBool>,
) {
    let Some(custom) = config.custom.clone() else {
        return;
    };
    // Match the historical reader: filter live points by the merged query so the
    // engine-side `CustomDataQuery::symbols` (and string/numeric) filters apply
    // identically live and in backtests. Providers populate `point.symbol`
    // directly on the live path (the engine copies `symbol_column` on the
    // parquet history path); a point that never got a symbol is dropped when a
    // symbol filter is active, mirroring backtest behavior.
    let query = custom.config.query.merge(&custom.dynamic_query);
    let mut seen = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        let utc_now = lean_core::DateTime::now();
        let mut source_available = false;
        if let Some(points) = source.get_live_points(
            &custom.ticker,
            utc_now,
            &custom.config,
            &custom.dynamic_query,
        ) {
            source_available = true;
            tracing::info!(
                "live custom data {}:{} fetched {} point(s)",
                custom.source_type,
                custom.ticker,
                points.len()
            );
            for point in points {
                if !query.matches_point(&point) {
                    continue;
                }
                let key = custom_point_key(&point);
                if !seen.insert(key) {
                    continue;
                }
                if sender
                    .send(Ok(LiveDataItem::CustomData {
                        symbol: config.symbol.clone(),
                        source_type: custom.source_type.clone(),
                        ticker: custom.ticker.clone(),
                        point,
                    }))
                    .is_err()
                {
                    return;
                }
            }
        } else {
            tracing::debug!(
                "live custom data {}:{} no source available at {:?}",
                custom.source_type,
                custom.ticker,
                utc_now
            );
        }
        let delay = source.live_poll_delay(
            &custom.ticker,
            utc_now,
            source_available,
            &custom.config,
            &custom.dynamic_query,
        );
        sleep_until_stop_or_delay(delay, &stop).await;
    }
}

async fn sleep_until_stop_or_delay(delay: Duration, stop: &AtomicBool) {
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) && started.elapsed() < delay {
        let remaining = delay.saturating_sub(started.elapsed());
        tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
    }
}

fn custom_point_key(point: &CustomDataPoint) -> String {
    let time = point
        .end_time
        .map(|time| time.0.to_string())
        .unwrap_or_else(|| point.time.to_string());
    let fields = serde_json::to_string(&point.fields).unwrap_or_default();
    format!("{time}:{fields}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_algorithm::{
        algorithm::{AlgorithmStatus, DataDeliveryPayload, SecurityChanges},
        lifecycle::{AlgorithmServices, OptionSubscription, UniverseSelection},
    };
    use lean_core::{
        DataNormalizationMode, DateTime, LeanError, Market, Resolution, Result, Symbol, TimeSpan,
    };
    use lean_data::{
        live_data_channel, DataQueueHandler, LiveDataItem, LiveDataSubscriptionConfig, TradeBar,
        TradeBarData,
    };
    use lean_orders::{Order, OrderEvent, TransactionManager};
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<String>>>);

    impl EventLog {
        fn push(&self, event: impl Into<String>) {
            self.0.lock().unwrap().push(event.into());
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        fn position(&self, event: &str) -> usize {
            self.entries()
                .iter()
                .position(|entry| entry == event)
                .unwrap_or_else(|| panic!("event {event} not found in {:?}", self.entries()))
        }

        fn count(&self, event: &str) -> usize {
            self.entries()
                .iter()
                .filter(|entry| entry.as_str() == event)
                .count()
        }
    }

    struct RecordingLiveHandler {
        name: &'static str,
        accepts: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    struct ScriptedLiveHandler {
        log: EventLog,
        items: Vec<LiveDataItem>,
    }

    impl DataQueueHandler for ScriptedLiveHandler {
        fn set_job(&mut self, _job: &lean_data::LiveNodePacket) -> Result<()> {
            self.log.push("queue:set_job");
            Ok(())
        }

        fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription> {
            self.log
                .push(format!("queue:subscribe:{}", config.symbol.value));
            let (sender, receiver) = live_data_channel();
            for item in self.items.clone() {
                sender.send(Ok(item)).unwrap();
            }
            Ok(LiveDataSubscription::new(
                LiveDataSubscriptionConfig::Market(Box::new(config.clone())),
                receiver,
            ))
        }

        fn unsubscribe(&mut self, config: &SubscriptionDataConfig) -> Result<()> {
            self.log
                .push(format!("queue:unsubscribe:{}", config.symbol.value));
            Ok(())
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    #[derive(Clone)]
    struct RecordingBridge {
        log: EventLog,
        subscriptions: Arc<Mutex<Vec<SubscriptionDataConfig>>>,
        universe_changes: Arc<Mutex<Vec<UniverseSelection>>>,
        terminal_status_after_data: Arc<Mutex<Option<AlgorithmStatus>>>,
        runtime_error_after_data: Arc<Mutex<Option<String>>>,
        terminal_status: Arc<Mutex<Option<AlgorithmStatus>>>,
        runtime_error: Arc<Mutex<Option<String>>>,
        transactions: Option<Arc<TransactionManager>>,
        portfolio: Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>>,
    }

    impl RecordingBridge {
        fn new(log: EventLog, symbol: Symbol) -> Self {
            Self {
                log,
                subscriptions: Arc::new(Mutex::new(vec![SubscriptionDataConfig::new_equity(
                    symbol,
                    Resolution::Minute,
                    DataNormalizationMode::Raw,
                )])),
                universe_changes: Arc::new(Mutex::new(Vec::new())),
                terminal_status_after_data: Arc::new(Mutex::new(None)),
                runtime_error_after_data: Arc::new(Mutex::new(None)),
                terminal_status: Arc::new(Mutex::new(None)),
                runtime_error: Arc::new(Mutex::new(None)),
                transactions: None,
                portfolio: None,
            }
        }

        fn with_universe_change(self, selection: UniverseSelection) -> Self {
            self.universe_changes.lock().unwrap().push(selection);
            self
        }

        fn with_terminal_status_after_data(self, status: AlgorithmStatus) -> Self {
            *self.terminal_status_after_data.lock().unwrap() = Some(status);
            self
        }

        fn with_runtime_error_after_data(self, message: &str) -> Self {
            *self.runtime_error_after_data.lock().unwrap() = Some(message.to_string());
            self
        }

        fn with_order_state(mut self, symbol: Symbol) -> Self {
            let transactions = Arc::new(TransactionManager::new());
            transactions.add_order(Order::market(1, symbol, dec!(10), DateTime::EPOCH, ""));
            self.transactions = Some(transactions);
            self.portfolio = Some(Arc::new(
                lean_algorithm::portfolio::SecurityPortfolioManager::new_live(dec!(100000)),
            ));
            self
        }
    }

    impl lean_algorithm::lifecycle::AlgorithmStateAccess for RecordingBridge {}

    impl lean_algorithm::lifecycle::LifecycleBridge for RecordingBridge {
        fn initialize(&mut self, _services: &mut dyn AlgorithmServices) -> anyhow::Result<()> {
            self.log.push("bridge:initialize");
            Ok(())
        }

        fn on_data(
            &mut self,
            _payload: DataDeliveryPayload,
            _services: &mut dyn AlgorithmServices,
        ) {
            self.log.push("bridge:on_data");
            if let Some(status) = *self.terminal_status_after_data.lock().unwrap() {
                *self.terminal_status.lock().unwrap() = Some(status);
            }
            if let Some(message) = self.runtime_error_after_data.lock().unwrap().clone() {
                *self.runtime_error.lock().unwrap() = Some(message);
            }
        }

        fn on_order_event(&mut self, _event: &OrderEvent, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_order_event");
        }

        fn on_otm_expiry(
            &mut self,
            _contract: lean_options::OptionContract,
            _quantity: rust_decimal::Decimal,
            _underlying_price: rust_decimal::Decimal,
            _entry_premium: rust_decimal::Decimal,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_assignment_order_event(
            &mut self,
            _contract: lean_options::OptionContract,
            _quantity: rust_decimal::Decimal,
            _is_assignment: bool,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_end_of_day(
            &mut self,
            _symbol: Option<Symbol>,
            _services: &mut dyn AlgorithmServices,
        ) {
            self.log.push("bridge:on_end_of_day");
        }

        fn on_warmup_finished(&mut self, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_warmup_finished");
        }

        fn on_end_of_algorithm(&mut self, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_end_of_algorithm");
        }

        fn on_margin_call(&mut self, _requests: &[Order], _services: &mut dyn AlgorithmServices) {}

        fn on_margin_call_warning(&mut self, _services: &mut dyn AlgorithmServices) {}

        fn on_securities_changed(
            &mut self,
            changes: &SecurityChanges,
            _services: &mut dyn AlgorithmServices,
        ) {
            self.log.push(format!(
                "bridge:on_securities_changed:{}:{}",
                changes.added.len(),
                changes.removed.len()
            ));
        }

        fn on_splits(
            &mut self,
            _splits: &HashMap<u64, lean_data::Split>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_dividends(
            &mut self,
            _dividends: &HashMap<u64, lean_data::Dividend>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_delistings(
            &mut self,
            _delistings: &HashMap<u64, lean_data::Delisting>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_symbol_changed_events(
            &mut self,
            _events: &HashMap<u64, lean_data::SymbolChangedEvent>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn select_universe_changes(
            &mut self,
            _utc_ns: i64,
            _resolution: Resolution,
            _services: &mut dyn AlgorithmServices,
        ) -> Vec<UniverseSelection> {
            self.log.push("bridge:select_universe_changes");
            let selections: Vec<_> = self.universe_changes.lock().unwrap().drain(..).collect();
            if !selections.is_empty() {
                let mut subscriptions = self.subscriptions.lock().unwrap();
                for selection in &selections {
                    subscriptions.retain(|config| {
                        !selection
                            .changes
                            .removed
                            .iter()
                            .any(|symbol| symbol == &config.symbol)
                    });
                    for symbol in &selection.changes.added {
                        subscriptions.push(SubscriptionDataConfig::new_equity(
                            symbol.clone(),
                            selection.resolution,
                            DataNormalizationMode::Raw,
                        ));
                    }
                }
            }
            selections
        }

        fn select_custom_universe_changes(
            &mut self,
            _utc_ns: i64,
            _resolution: Resolution,
            _custom_data: &HashMap<String, Vec<lean_data::CustomDataPoint>>,
            _services: &mut dyn AlgorithmServices,
        ) -> Vec<UniverseSelection> {
            Vec::new()
        }

        fn on_end_of_time_step(&mut self, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_end_of_time_step");
        }

        fn on_brokerage_message(&mut self, _message: &str, _services: &mut dyn AlgorithmServices) {}

        fn on_brokerage_disconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

        fn on_brokerage_reconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

        fn terminal_status(&self) -> Option<AlgorithmStatus> {
            *self.terminal_status.lock().unwrap()
        }

        fn runtime_error(&self) -> Option<String> {
            self.runtime_error.lock().unwrap().clone()
        }

        fn name(&self) -> &str {
            "RecordingBridge"
        }

        fn start_date(&self) -> DateTime {
            DateTime::EPOCH
        }

        fn end_date(&self) -> DateTime {
            DateTime::from(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap())
        }

        fn portfolio_value(&self) -> lean_core::Price {
            dec!(100000)
        }

        fn starting_cash(&self) -> lean_core::Price {
            dec!(100000)
        }

        fn subscriptions(&self) -> Vec<SubscriptionDataConfig> {
            self.subscriptions.lock().unwrap().clone()
        }

        fn prepare_data_delivery(
            &mut self,
            _subscriptions: &[SubscriptionDataConfig],
        ) -> anyhow::Result<()> {
            self.log.push("bridge:prepare_data_delivery");
            Ok(())
        }

        fn option_subscriptions(&self) -> Vec<OptionSubscription> {
            Vec::new()
        }

        fn portfolio(&self) -> Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>> {
            self.portfolio.clone()
        }

        fn transactions(&self) -> Option<Arc<TransactionManager>> {
            self.transactions.clone()
        }

        fn order_fee(&self, _order: &Order, _fill_price: lean_core::Price) -> lean_core::Price {
            dec!(0)
        }

        fn contract_multiplier_for_symbol(&self, _symbol: &Symbol) -> lean_core::Price {
            dec!(1)
        }

        fn validate_order_buying_power(
            &self,
            _order: &Order,
            _fill_price: lean_core::Price,
            _fee: lean_core::Price,
        ) -> std::result::Result<(), String> {
            Ok(())
        }

        fn has_universes(&self) -> bool {
            !self.universe_changes.lock().unwrap().is_empty()
        }

        fn universe_resolution(&self) -> Option<Resolution> {
            Some(Resolution::Minute)
        }
    }

    impl DataQueueHandler for RecordingLiveHandler {
        fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription> {
            self.calls.lock().unwrap().push(self.name);
            if !self.accepts {
                return Err(LeanError::Unsupported(format!(
                    "{} does not support {}",
                    self.name, config.symbol
                )));
            }

            let (sender, receiver) = live_data_channel();
            sender
                .send(Ok(LiveDataItem::Heartbeat(lean_core::DateTime::EPOCH)))
                .unwrap();
            Ok(LiveDataSubscription::new(
                LiveDataSubscriptionConfig::Market(Box::new(config.clone())),
                receiver,
            ))
        }

        fn unsubscribe(&mut self, _config: &SubscriptionDataConfig) -> Result<()> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            self.name
        }
    }

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    fn spy() -> Symbol {
        Symbol::create_equity("SPY", &Market::usa())
    }

    fn qqq() -> Symbol {
        Symbol::create_equity("QQQ", &Market::usa())
    }

    fn trade_bar_item(symbol: Symbol, date: NaiveDate, minute: u32) -> LiveDataItem {
        LiveDataItem::TradeBar(TradeBar::new(
            symbol,
            dt(date, 9, 30 + minute),
            TimeSpan::from_mins(1),
            TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
        ))
    }

    fn emit_ready(items: Vec<LiveDataItem>) -> Vec<LiveDataItem> {
        let next_time = items
            .iter()
            .map(LiveDataItem::end_time)
            .max()
            .unwrap_or(DateTime::EPOCH)
            + TimeSpan::from_mins(1);
        let mut out = items;
        out.push(LiveDataItem::Heartbeat(next_time));
        out
    }

    async fn live_config(log: EventLog, items: Vec<LiveDataItem>) -> LiveRunConfig {
        let tmp = tempfile::tempdir().unwrap().keep();
        let store = Arc::new(
            lean_storage::IcebergStore::connect_local(&tmp)
                .await
                .unwrap(),
        );
        LiveRunConfig {
            data_root: tmp,
            data_store: store,
            history_provider: None,
            parameters: HashMap::new(),
            custom_data_sources: Vec::new(),
            live_data_queue: lean_live::DataQueueHandlerManager::new(vec![Box::new(
                ScriptedLiveHandler { log, items },
            )]),
            brokerage: None,
            brokerage_model: None,
            paper_trading: true,
            max_slices: Some(1),
            max_runtime: Some(Duration::from_millis(50)),
            output_dir: None,
        }
    }

    #[test]
    fn live_subscription_set_uses_stacked_queue_provider_order() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut queue = lean_live::DataQueueHandlerManager::new(vec![
            Box::new(RecordingLiveHandler {
                name: "tradier",
                accepts: false,
                calls: calls.clone(),
            }),
            Box::new(RecordingLiveHandler {
                name: "thetadata",
                accepts: true,
                calls: calls.clone(),
            }),
        ]);
        let config = SubscriptionDataConfig::new_crypto(
            Symbol::create_crypto("BTCUSDT", &Market::binance()),
            Resolution::Minute,
        );

        let set = LiveSubscriptionSet::subscribe_initial(
            &mut queue,
            &[config],
            &[],
            Some("paper".to_string()),
            true,
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(set.market.len(), 1);
        assert_eq!(calls.lock().unwrap().as_slice(), &["tradier", "thetadata"]);
    }

    #[tokio::test]
    async fn live_run_invokes_initialize_prepare_warmup_before_subscribing() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert!(log.position("bridge:initialize") < log.position("bridge:prepare_data_delivery"));
        assert!(
            log.position("bridge:prepare_data_delivery")
                < log.position("bridge:on_warmup_finished")
        );
        assert!(log.position("bridge:on_warmup_finished") < log.position("queue:set_job"));
        assert!(log.position("queue:set_job") < log.position("queue:subscribe:SPY"));
    }

    #[tokio::test]
    async fn live_run_delivers_on_data_then_framework_then_end_time_step_per_slice() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_time_step"));
    }

    #[tokio::test]
    async fn live_run_calls_warmup_finished_once_before_first_data() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("bridge:on_warmup_finished"), 1);
        assert!(log.position("bridge:on_warmup_finished") < log.position("bridge:on_data"));
    }

    #[tokio::test]
    async fn live_run_calls_end_of_day_on_day_transition_and_final_finish() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let first_day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let second_day = NaiveDate::from_ymd_opt(2024, 1, 18).unwrap();
        let mut config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), first_day, 0),
                trade_bar_item(symbol, second_day, 0),
            ]),
        )
        .await;
        config.max_slices = Some(2);

        run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("bridge:on_end_of_day"), 2);
        assert!(log.position("bridge:on_end_of_day") < log.position("bridge:on_end_of_algorithm"));
    }

    #[tokio::test]
    async fn live_run_calls_on_end_of_algorithm_even_when_stopped_by_max_slices() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), day, 0),
                trade_bar_item(symbol, day, 1),
            ]),
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_algorithm"));
        assert!(log.position("bridge:on_end_of_algorithm") < log.position("queue:unsubscribe:SPY"));
    }

    #[tokio::test]
    async fn live_run_unsubscribes_all_after_finish() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("queue:unsubscribe:SPY"), 1);
        assert!(log.position("bridge:on_end_of_algorithm") < log.position("queue:unsubscribe:SPY"));
    }

    #[tokio::test]
    async fn live_run_syncs_dynamic_subscriptions_after_universe_changes() {
        let log = EventLog::default();
        let symbol = spy();
        let added = qqq();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone()).with_universe_change(
            UniverseSelection {
                changes: SecurityChanges {
                    added: vec![added.clone()],
                    removed: Vec::new(),
                },
                resolution: Resolution::Minute,
            },
        );
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol.clone(), day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert!(
            log.position("bridge:on_securities_changed:1:0") < log.position("queue:subscribe:QQQ")
        );
        assert_eq!(log.count("queue:subscribe:QQQ"), 1);
        assert_eq!(log.count("queue:unsubscribe:QQQ"), 1);
    }

    #[tokio::test]
    async fn live_run_processes_order_events_before_on_data() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge =
            RecordingBridge::new(log.clone(), symbol.clone()).with_order_state(symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("bridge:on_order_event"), 1);
        assert!(log.position("bridge:on_order_event") < log.position("bridge:on_data"));
        assert_eq!(result.order_events.len(), 1);
    }

    #[tokio::test]
    async fn live_run_ignores_empty_slices_without_callbacks() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol);
        let config = live_config(
            log.clone(),
            vec![
                LiveDataItem::Heartbeat(DateTime::EPOCH),
                LiveDataItem::Heartbeat(DateTime::EPOCH + TimeSpan::from_mins(1)),
            ],
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 0);
        assert_eq!(log.count("bridge:on_data"), 0);
        assert_eq!(log.count("bridge:on_framework_data"), 0);
        assert_eq!(log.count("bridge:on_end_of_time_step"), 0);
    }

    #[tokio::test]
    async fn live_run_stops_on_algorithm_terminal_status() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone())
            .with_terminal_status_after_data(AlgorithmStatus::Stopped);
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let mut config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), day, 0),
                trade_bar_item(symbol, day, 1),
            ]),
        )
        .await;
        config.max_slices = Some(10);

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert_eq!(log.count("bridge:on_data"), 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_algorithm"));
    }

    #[tokio::test]
    async fn live_run_stops_on_runtime_error_handler_signal() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone())
            .with_runtime_error_after_data("fatal result handler error");
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let mut config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), day, 0),
                trade_bar_item(symbol, day, 1),
            ]),
        )
        .await;
        config.max_slices = Some(10);

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert_eq!(log.count("bridge:on_data"), 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_algorithm"));
    }

    // ─── Live brokerage routing tests ───────────────────────────────────────

    use crate::live::transaction_handler::{test_support::MockBrokerage, LiveBrokerageRouter};
    use lean_brokerages::Brokerage;
    use lean_orders::OrderStatus;

    fn manager_with_order_state(
        symbol: Symbol,
    ) -> (
        AlgorithmManager<RecordingBridge>,
        crate::EngineAlgorithmServices,
        Arc<TransactionManager>,
        Arc<lean_algorithm::portfolio::SecurityPortfolioManager>,
    ) {
        let log = EventLog::default();
        let bridge = RecordingBridge::new(log, symbol.clone()).with_order_state(symbol);
        let transactions = bridge.transactions.clone().unwrap();
        let portfolio = bridge.portfolio.clone().unwrap();
        let context = crate::AlgorithmRuntimeContext::with_history_service(
            std::sync::Arc::new(NoopHistory),
            HashMap::new(),
        );
        let services = crate::EngineAlgorithmServices::new(DateTime::now(), context.clone());
        let manager = AlgorithmManager::new(bridge, context);
        (manager, services, transactions, portfolio)
    }

    struct NoopHistory;
    impl lean_algorithm::lifecycle::AlgorithmHistoryService for NoopHistory {
        fn history(
            &self,
            _algorithm: &lean_algorithm::qc_algorithm::QcAlgorithm,
            _symbol: &Symbol,
            _periods: usize,
            _resolution: Resolution,
        ) -> lean_algorithm::lifecycle::HistoryColumns {
            HashMap::new()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn drain_until<F: Fn() -> bool>(
        router: &mut LiveBrokerageRouter,
        manager: &mut AlgorithmManager<RecordingBridge>,
        services: &mut crate::EngineAlgorithmServices,
        transactions: &Arc<TransactionManager>,
        portfolio: &Arc<lean_algorithm::portfolio::SecurityPortfolioManager>,
        events: &mut Vec<OrderEvent>,
        trade_builder: &mut TradeBuilder,
        completed_trades: &mut Vec<Trade>,
        done: F,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            router.drain_events(
                manager,
                services,
                transactions,
                Some(portfolio),
                events,
                trade_builder,
                completed_trades,
            );
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn router_submits_new_order_and_records_brokerage_id() {
        let symbol = spy();
        let (mut manager, mut services, transactions, portfolio) = manager_with_order_state(symbol);
        let mut router = LiveBrokerageRouter::spawn(Box::new(MockBrokerage::new()));
        let mut events = Vec::new();
        let mut trade_builder = TradeBuilder::new();
        let mut trades = Vec::new();

        router.dispatch_pending(&transactions);
        drain_until(
            &mut router,
            &mut manager,
            &mut services,
            &transactions,
            &portfolio,
            &mut events,
            &mut trade_builder,
            &mut trades,
            || {
                transactions
                    .get_order(1)
                    .map(|order| order.status == OrderStatus::Submitted)
                    .unwrap_or(false)
            },
        );

        let order = transactions.get_order(1).unwrap();
        assert_eq!(order.status, OrderStatus::Submitted);
        assert!(!order.brokerage_id.is_empty(), "brokerage id recorded");
        assert!(events
            .iter()
            .any(|event| event.status == OrderStatus::Submitted));
        router.shutdown();
    }

    #[test]
    fn router_poll_detected_fill_updates_portfolio_and_algorithm() {
        let symbol = spy();
        let (mut manager, mut services, transactions, portfolio) =
            manager_with_order_state(symbol.clone());
        let mock = MockBrokerage::new();
        let account_orders = mock.account_orders.clone();
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));
        let mut events = Vec::new();
        let mut trade_builder = TradeBuilder::new();
        let mut trades = Vec::new();

        router.dispatch_pending(&transactions);
        // Wait for Submitted, then flip account order to Filled.
        drain_until(
            &mut router,
            &mut manager,
            &mut services,
            &transactions,
            &portfolio,
            &mut events,
            &mut trade_builder,
            &mut trades,
            || {
                transactions
                    .get_order(1)
                    .map(|order| order.status == OrderStatus::Submitted)
                    .unwrap_or(false)
            },
        );
        {
            let mut orders = account_orders.lock();
            for order in orders.iter_mut() {
                order.status = OrderStatus::Filled;
                order.filled_quantity = dec!(10);
                order.average_fill_price = dec!(400);
            }
        }
        drain_until(
            &mut router,
            &mut manager,
            &mut services,
            &transactions,
            &portfolio,
            &mut events,
            &mut trade_builder,
            &mut trades,
            || {
                transactions
                    .get_order(1)
                    .map(|order| order.status == OrderStatus::Filled)
                    .unwrap_or(false)
            },
        );

        let order = transactions.get_order(1).unwrap();
        assert_eq!(order.status, OrderStatus::Filled);
        // Portfolio holding was updated by the fill.
        let holding = portfolio.get_holding(&symbol);
        assert_eq!(holding.quantity, dec!(10));
        // Algorithm bridge observed the fill.
        assert!(manager.algorithm().log.count("bridge:on_order_event") >= 1);
        assert!(events
            .iter()
            .any(|event| event.status == OrderStatus::Filled && event.fill_quantity == dec!(10)));
        router.shutdown();
    }

    #[test]
    fn router_submission_failure_marks_order_invalid() {
        let symbol = spy();
        let (mut manager, mut services, transactions, portfolio) = manager_with_order_state(symbol);
        let mock = MockBrokerage {
            submit_should_fail: true,
            ..MockBrokerage::new()
        };
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));
        let mut events = Vec::new();
        let mut trade_builder = TradeBuilder::new();
        let mut trades = Vec::new();

        router.dispatch_pending(&transactions);
        drain_until(
            &mut router,
            &mut manager,
            &mut services,
            &transactions,
            &portfolio,
            &mut events,
            &mut trade_builder,
            &mut trades,
            || {
                transactions
                    .get_order(1)
                    .map(|order| order.status == OrderStatus::Invalid)
                    .unwrap_or(false)
            },
        );

        let order = transactions.get_order(1).unwrap();
        assert_eq!(order.status, OrderStatus::Invalid);
        assert!(events
            .iter()
            .any(|event| event.status == OrderStatus::Invalid));
        router.shutdown();
    }

    #[tokio::test]
    async fn live_run_uses_local_paper_path_when_uses_local_paper_fills() {
        // A brokerage that opts into local paper fills must not activate real
        // routing: the local ImmediateFillModel fills the order, exactly as when
        // no brokerage is attached.
        let log = EventLog::default();
        let symbol = spy();
        let bridge =
            RecordingBridge::new(log.clone(), symbol.clone()).with_order_state(symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let mut config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;
        config.brokerage = Some(Box::new(PaperFillMock));
        config.paper_trading = false;

        let result = run_live(bridge, config).await.unwrap();

        // The New market order was filled locally (one fill order event).
        assert_eq!(log.count("bridge:on_order_event"), 1);
        assert_eq!(result.order_events.len(), 1);
        assert_eq!(result.order_events[0].status, OrderStatus::Filled);
    }

    struct PaperFillMock;
    impl Brokerage for PaperFillMock {
        fn name(&self) -> &str {
            "PaperFillMock"
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn uses_local_paper_fills(&self) -> bool {
            true
        }
        fn connect(&mut self) -> lean_core::Result<()> {
            Ok(())
        }
        fn disconnect(&mut self) {}
        fn place_order(&mut self, _order: lean_orders::Order) -> lean_core::Result<bool> {
            Ok(true)
        }
        fn update_order(&mut self, _order: &lean_orders::Order) -> lean_core::Result<bool> {
            Ok(true)
        }
        fn cancel_order(&mut self, _order: &lean_orders::Order) -> lean_core::Result<bool> {
            Ok(true)
        }
        fn get_open_orders(&self) -> Vec<lean_orders::Order> {
            Vec::new()
        }
        fn get_cash_balance(&self) -> Vec<(String, lean_core::Price)> {
            vec![("USD".to_string(), dec!(100000))]
        }
        fn get_account_holdings(&self) -> HashMap<Symbol, lean_core::Quantity> {
            HashMap::new()
        }
    }

    // ─── Feature A/B: restored-insight security restore + unmanaged liquidation ──

    use lean_algorithm::qc_algorithm::QcAlgorithm;
    use lean_alpha::{Insight, InsightDirection, NullAlphaModel};
    use lean_brokerages::BrokerageHolding;
    use lean_core::TimeSpan as CoreTimeSpan;

    /// Minimal bridge backed by a real `QcAlgorithm`, so Feature A can register
    /// securities through the same `add_security_symbol` path universe additions
    /// use and tests can observe the resulting securities/subscriptions.
    #[derive(Clone)]
    struct FrameworkBridge {
        state: Arc<Mutex<QcAlgorithm>>,
    }

    impl FrameworkBridge {
        fn new(state: Arc<Mutex<QcAlgorithm>>) -> Self {
            Self { state }
        }
    }

    impl lean_algorithm::lifecycle::AlgorithmStateAccess for FrameworkBridge {
        fn algorithm_state(&self) -> Option<Arc<Mutex<QcAlgorithm>>> {
            Some(self.state.clone())
        }
    }

    impl lean_algorithm::lifecycle::LifecycleBridge for FrameworkBridge {
        fn initialize(&mut self, _services: &mut dyn AlgorithmServices) -> anyhow::Result<()> {
            Ok(())
        }
        fn on_data(
            &mut self,
            _payload: DataDeliveryPayload,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_order_event(&mut self, _event: &OrderEvent, _services: &mut dyn AlgorithmServices) {}
        fn on_otm_expiry(
            &mut self,
            _contract: lean_options::OptionContract,
            _quantity: rust_decimal::Decimal,
            _underlying_price: rust_decimal::Decimal,
            _entry_premium: rust_decimal::Decimal,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_assignment_order_event(
            &mut self,
            _contract: lean_options::OptionContract,
            _quantity: rust_decimal::Decimal,
            _is_assignment: bool,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_end_of_day(
            &mut self,
            _symbol: Option<Symbol>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_warmup_finished(&mut self, _services: &mut dyn AlgorithmServices) {}
        fn on_end_of_algorithm(&mut self, _services: &mut dyn AlgorithmServices) {}
        fn on_margin_call(&mut self, _requests: &[Order], _services: &mut dyn AlgorithmServices) {}
        fn on_margin_call_warning(&mut self, _services: &mut dyn AlgorithmServices) {}
        fn on_securities_changed(
            &mut self,
            _changes: &SecurityChanges,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_splits(
            &mut self,
            _splits: &HashMap<u64, lean_data::Split>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_dividends(
            &mut self,
            _dividends: &HashMap<u64, lean_data::Dividend>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_delistings(
            &mut self,
            _delistings: &HashMap<u64, lean_data::Delisting>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn on_symbol_changed_events(
            &mut self,
            _events: &HashMap<u64, lean_data::SymbolChangedEvent>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }
        fn select_universe_changes(
            &mut self,
            _utc_ns: i64,
            _resolution: Resolution,
            _services: &mut dyn AlgorithmServices,
        ) -> Vec<UniverseSelection> {
            Vec::new()
        }
        fn select_custom_universe_changes(
            &mut self,
            _utc_ns: i64,
            _resolution: Resolution,
            _custom_data: &HashMap<String, Vec<lean_data::CustomDataPoint>>,
            _services: &mut dyn AlgorithmServices,
        ) -> Vec<UniverseSelection> {
            Vec::new()
        }
        fn on_end_of_time_step(&mut self, _services: &mut dyn AlgorithmServices) {}
        fn on_brokerage_message(&mut self, _message: &str, _services: &mut dyn AlgorithmServices) {}
        fn on_brokerage_disconnect(&mut self, _services: &mut dyn AlgorithmServices) {}
        fn on_brokerage_reconnect(&mut self, _services: &mut dyn AlgorithmServices) {}
        fn terminal_status(&self) -> Option<AlgorithmStatus> {
            None
        }
        fn runtime_error(&self) -> Option<String> {
            None
        }
        fn name(&self) -> &str {
            "FrameworkBridge"
        }
        fn start_date(&self) -> DateTime {
            DateTime::EPOCH
        }
        fn end_date(&self) -> DateTime {
            DateTime::from(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap())
        }
        fn portfolio_value(&self) -> lean_core::Price {
            self.state.lock().unwrap().portfolio.total_portfolio_value()
        }
        fn starting_cash(&self) -> lean_core::Price {
            dec!(100000)
        }
        fn subscriptions(&self) -> Vec<SubscriptionDataConfig> {
            self.state
                .lock()
                .unwrap()
                .subscription_manager
                .get_all()
                .into_iter()
                .map(|config| (*config).clone())
                .collect()
        }
        fn prepare_data_delivery(
            &mut self,
            _subscriptions: &[SubscriptionDataConfig],
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn option_subscriptions(&self) -> Vec<OptionSubscription> {
            Vec::new()
        }
        fn portfolio(&self) -> Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>> {
            Some(self.state.lock().unwrap().portfolio.clone())
        }
        fn transactions(&self) -> Option<Arc<TransactionManager>> {
            Some(self.state.lock().unwrap().transactions.clone())
        }
        fn order_fee(&self, _order: &Order, _fill_price: lean_core::Price) -> lean_core::Price {
            dec!(0)
        }
        fn contract_multiplier_for_symbol(&self, _symbol: &Symbol) -> lean_core::Price {
            dec!(1)
        }
        fn validate_order_buying_power(
            &self,
            _order: &Order,
            _fill_price: lean_core::Price,
            _fee: lean_core::Price,
        ) -> std::result::Result<(), String> {
            Ok(())
        }
        fn has_universes(&self) -> bool {
            false
        }
        fn universe_resolution(&self) -> Option<Resolution> {
            None
        }
    }

    fn framework_manager(
        state: Arc<Mutex<QcAlgorithm>>,
    ) -> (
        AlgorithmManager<FrameworkBridge>,
        crate::AlgorithmRuntimeContext,
    ) {
        framework_manager_with_history(state, std::sync::Arc::new(NoopHistory))
    }

    fn framework_manager_with_history(
        state: Arc<Mutex<QcAlgorithm>>,
        history: Arc<dyn lean_algorithm::lifecycle::AlgorithmHistoryService>,
    ) -> (
        AlgorithmManager<FrameworkBridge>,
        crate::AlgorithmRuntimeContext,
    ) {
        let context = crate::AlgorithmRuntimeContext::with_history_service(history, HashMap::new());
        let manager = AlgorithmManager::new(FrameworkBridge::new(state), context.clone());
        (manager, context)
    }

    /// History service that answers every last-known-price request with a fixed
    /// daily close, mimicking a healthy history provider at live startup.
    struct SeededHistory(f64);

    impl lean_algorithm::lifecycle::AlgorithmHistoryService for SeededHistory {
        fn history(
            &self,
            _algorithm: &QcAlgorithm,
            _symbol: &Symbol,
            _periods: usize,
            _resolution: Resolution,
        ) -> lean_algorithm::lifecycle::HistoryColumns {
            HashMap::new()
        }

        fn last_known_close_price(
            &self,
            _algorithm: &QcAlgorithm,
            _symbol: &Symbol,
            _resolution: Resolution,
        ) -> Option<f64> {
            Some(self.0)
        }
    }

    fn active_up_insight(symbol: Symbol) -> Insight {
        Insight::new(
            symbol,
            InsightDirection::Up,
            CoreTimeSpan::from_days(30),
            None,
            None,
            "restored-model",
        )
    }

    /// Feature A (registration): a restored active insight for a symbol not added
    /// by initialize gets a security + subscription registered in the algorithm.
    #[test]
    fn restored_insight_registers_security_and_subscription() {
        let ypf = Symbol::create_equity("YPF", &Market::usa());
        let state = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let (mut manager, context) = framework_manager(state.clone());

        // Seed a restored active insight into the runtime-context framework, as
        // `restore_live_insights` would after reading insights.json.
        {
            let framework = context.framework();
            let mut fw = framework.lock().unwrap();
            fw.alpha_models.push(Box::new(NullAlphaModel));
            fw.insights.add(active_up_insight(ypf.clone()));
        }
        // Precondition: initialize did not add YPF.
        assert!(!state.lock().unwrap().securities.contains(&ypf));

        let history = context.history_service();
        let pending = restore_securities_for_active_insights(&mut manager, &history);

        let algorithm = state.lock().unwrap();
        assert!(
            algorithm.securities.contains(&ypf),
            "restored insight security should be registered"
        );
        assert!(
            algorithm
                .subscription_manager
                .get_all()
                .iter()
                .any(|config| config.symbol == ypf),
            "restored insight security should have a live subscription"
        );
        // NoopHistory yields no last-known price, so YPF stays pending for the
        // live-loop price-arrival fallback.
        assert!(
            pending.contains_key(&ypf.id.sid),
            "unseeded security should be tracked for a price-arrival re-notify"
        );
    }

    /// Feature A (acceptance): after restore + first framework run, the PCM
    /// recomputes a target for the restored insight symbol the portfolio does not
    /// hold, and a buy order (quantity > 0) is submitted to the brokerage.
    ///
    /// The price is NOT hand-seeded: it arrives only through the normal restore
    /// path (last-known close fetched from the history provider during
    /// `restore_securities_for_active_insights`), mirroring live startup where
    /// quotes have not arrived yet at the first framework run.
    #[test]
    fn restored_insight_produces_buy_order_submitted_to_brokerage() {
        let ypf = Symbol::create_equity("YPF", &Market::usa());
        let state = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let (mut manager, context) =
            framework_manager_with_history(state.clone(), Arc::new(SeededHistory(20.0)));

        {
            let framework = context.framework();
            let mut fw = framework.lock().unwrap();
            fw.alpha_models.push(Box::new(NullAlphaModel));
            // Use the real restore path so `restored_insight_change` is set and
            // the PCM rebalances on the first framework run, exactly as
            // `restore_live_insights` does at startup.
            fw.restore_insights(
                lean_alpha::InsightCollectionSnapshot {
                    active: vec![active_up_insight(ypf.clone())],
                    closed: Vec::new(),
                    total_count: 1,
                },
                DateTime::now(),
            );
        }

        // Feature A registers the security and seeds its last-known price from
        // the history provider. The account sync holds no YPF, so the whole
        // target is a fresh buy.
        let history = context.history_service();
        let pending = restore_securities_for_active_insights(&mut manager, &history);
        assert!(
            pending.is_empty(),
            "history seed succeeded, nothing should be pending"
        );
        assert_eq!(
            state
                .lock()
                .unwrap()
                .securities
                .get(&ypf)
                .map(|security| security.current_price()),
            Some(dec!(20)),
            "restore should seed the last-known close from history"
        );

        // Run the framework pipeline for a slice at the insight's frontier and
        // submit the resulting execution requests, exactly as the live loop does.
        let slice = lean_data::Slice::new(DateTime::now());
        let order_requests = crate::run_framework_pipeline(&context.framework(), &state, &slice);
        assert!(
            !order_requests.is_empty(),
            "PCM/execution should produce an order request for the restored insight"
        );
        crate::algorithm_services::submit_execution_order_requests(&state, order_requests);

        let transactions = state.lock().unwrap().transactions.clone();
        let ypf_orders: Vec<_> = transactions
            .get_all_orders()
            .into_iter()
            .filter(|order| order.symbol == ypf)
            .collect();
        assert_eq!(ypf_orders.len(), 1, "one order for the restored insight");
        assert!(
            ypf_orders[0].quantity > dec!(0),
            "restored Up insight yields a buy order, got {}",
            ypf_orders[0].quantity
        );

        // Submit through the real router path to the mock brokerage.
        let mock = MockBrokerage::new();
        let submitted = mock.submitted.clone();
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));
        router.dispatch_pending(&transactions);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && submitted.lock().is_empty() {
            std::thread::sleep(Duration::from_millis(25));
        }
        let submitted_orders = submitted.lock().clone();
        assert_eq!(
            submitted_orders.len(),
            1,
            "exactly one order submitted to the brokerage"
        );
        assert_eq!(submitted_orders[0].symbol, ypf);
        assert!(submitted_orders[0].quantity > dec!(0));
        router.shutdown();
    }

    /// Fallback path: the history seed fails (NoopHistory), so the restored
    /// insight cannot produce a target on the first framework run. When the
    /// security gains its first live price (as `advance_frontier` applies from a
    /// slice), the re-notify fires, the PCM re-rebalances, and the buy order is
    /// finally produced.
    #[test]
    fn restored_insight_without_history_seed_buys_once_live_price_arrives() {
        let ypf = Symbol::create_equity("YPF", &Market::usa());
        let state = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let (mut manager, context) = framework_manager(state.clone());

        {
            let framework = context.framework();
            let mut fw = framework.lock().unwrap();
            fw.alpha_models.push(Box::new(NullAlphaModel));
            fw.restore_insights(
                lean_alpha::InsightCollectionSnapshot {
                    active: vec![active_up_insight(ypf.clone())],
                    closed: Vec::new(),
                    total_count: 1,
                },
                DateTime::now(),
            );
        }

        let history = context.history_service();
        let mut pending = restore_securities_for_active_insights(&mut manager, &history);
        assert!(
            pending.contains_key(&ypf.id.sid),
            "history seed failed, so YPF must be pending"
        );

        // First framework run: YPF has no price, so the PCM cannot size a target
        // and no YPF order is created.
        let slice = lean_data::Slice::new(DateTime::now());
        let requests = crate::run_framework_pipeline(&context.framework(), &state, &slice);
        crate::algorithm_services::submit_execution_order_requests(&state, requests);
        let transactions = state.lock().unwrap().transactions.clone();
        assert!(
            !transactions
                .get_all_orders()
                .iter()
                .any(|order| order.symbol == ypf),
            "no order should exist while YPF is unpriced"
        );

        // Price still absent: the re-notify is a no-op and YPF stays pending.
        renotify_restored_securities_with_prices(&manager, &mut pending);
        assert!(pending.contains_key(&ypf.id.sid));

        // First live price arrives, exactly as apply_slice_security_prices would
        // apply it during advance_frontier.
        {
            let algorithm = state.lock().unwrap();
            algorithm.securities.update_price(&ypf, dec!(20));
            algorithm.portfolio.update_prices(&ypf, dec!(20));
        }
        renotify_restored_securities_with_prices(&manager, &mut pending);
        assert!(
            pending.is_empty(),
            "priced security should leave the pending set"
        );

        // Next framework run now produces the buy for the restored insight.
        let slice = lean_data::Slice::new(DateTime::now());
        let requests = crate::run_framework_pipeline(&context.framework(), &state, &slice);
        assert!(
            !requests.is_empty(),
            "re-notified security should produce an order request"
        );
        crate::algorithm_services::submit_execution_order_requests(&state, requests);
        let ypf_orders: Vec<_> = transactions
            .get_all_orders()
            .into_iter()
            .filter(|order| order.symbol == ypf)
            .collect();
        assert_eq!(ypf_orders.len(), 1, "one buy order after the price arrived");
        assert!(ypf_orders[0].quantity > dec!(0));
    }

    fn holding(symbol: Symbol, quantity: rust_decimal::Decimal) -> BrokerageHolding {
        BrokerageHolding {
            symbol,
            quantity,
            average_price: dec!(100),
            market_price: dec!(100),
        }
    }

    /// Feature B: two synced holdings — A (no insight) and B (active insight).
    /// Exactly one liquidation order for A with quantity -qty(A) is submitted;
    /// B is untouched.
    #[test]
    fn liquidates_only_unmanaged_holdings() {
        let a = Symbol::create_equity("AAA", &Market::usa());
        let b = Symbol::create_equity("BBB", &Market::usa());
        let state = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let (manager, context) = framework_manager(state.clone());

        // B has an active insight (managed); A does not (unmanaged).
        {
            let framework = context.framework();
            let mut fw = framework.lock().unwrap();
            fw.insights.add(active_up_insight(b.clone()));
        }
        let transactions = state.lock().unwrap().transactions.clone();
        let holdings = vec![holding(a.clone(), dec!(7)), holding(b.clone(), dec!(3))];

        let mock = MockBrokerage::new();
        let submitted = mock.submitted.clone();
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));

        liquidate_unmanaged_holdings(&manager, Some(&transactions), &holdings, &mut router);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && submitted.lock().is_empty() {
            std::thread::sleep(Duration::from_millis(25));
        }
        let submitted_orders = submitted.lock().clone();
        assert_eq!(submitted_orders.len(), 1, "only A is liquidated");
        assert_eq!(submitted_orders[0].symbol, a);
        assert_eq!(
            submitted_orders[0].quantity,
            dec!(-7),
            "sell the full A position"
        );
        router.shutdown();
    }

    /// Feature B is gated on real brokerage routing. Under local paper mode (a
    /// brokerage that opts into local paper fills, or no brokerage at all), the
    /// runner never enters the routing block, so unmanaged holdings are never
    /// liquidated. This asserts that gate: mirroring the runner, when real
    /// routing is off the liquidation path is not invoked and no orders appear.
    #[test]
    fn no_liquidation_in_local_paper_mode() {
        // real_routing mirrors the runner: only true for a real brokerage that
        // does not use local paper fills.
        let real_routing = |brokerage: Option<&dyn Brokerage>| {
            brokerage
                .map(|brokerage| !brokerage.uses_local_paper_fills())
                .unwrap_or(false)
        };
        // No brokerage → local/paper → no routing.
        assert!(!real_routing(None));
        // Paper-fill brokerage → local paper mode → no routing.
        let paper = PaperFillMock;
        assert!(!real_routing(Some(&paper)));

        let a = Symbol::create_equity("AAA", &Market::usa());
        let state = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let (manager, _context) = framework_manager(state.clone());
        let transactions = state.lock().unwrap().transactions.clone();
        let holdings = vec![holding(a, dec!(7))];

        let mut router = LiveBrokerageRouter::spawn(Box::new(MockBrokerage::new()));
        // Mirror the runner's `if real_routing { ... liquidate ... }` gate.
        if real_routing(Some(&paper)) {
            liquidate_unmanaged_holdings(&manager, Some(&transactions), &holdings, &mut router);
        }
        router.shutdown();

        assert!(
            transactions.get_all_orders().is_empty(),
            "local paper mode leaves no liquidation orders"
        );
    }

    // ─── Fix 3: bounded retry of failed insight-backed targets ───────────────

    /// An Invalid order for a symbol with an active insight re-arms the framework
    /// rebalance and consumes retry budget; a subsequent framework run then
    /// recomputes and resubmits the target. After MAX_INSIGHT_RETRIES rejections
    /// the symbol stops re-arming (bounded), guarding against reject storms.
    #[test]
    fn invalid_insight_order_rearms_rebalance_and_is_bounded() {
        let ypf = Symbol::create_equity("YPF", &Market::usa());
        let state = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let (manager, context) =
            framework_manager_with_history(state.clone(), Arc::new(SeededHistory(20.0)));

        {
            let framework = context.framework();
            let mut fw = framework.lock().unwrap();
            fw.alpha_models.push(Box::new(NullAlphaModel));
            fw.restore_insights(
                lean_alpha::InsightCollectionSnapshot {
                    active: vec![active_up_insight(ypf.clone())],
                    closed: Vec::new(),
                    total_count: 1,
                },
                DateTime::now(),
            );
        }
        // Register + seed the price so the PCM can size a target.
        {
            let mut algorithm = state.lock().unwrap();
            algorithm.add_security_symbol(ypf.clone(), Resolution::Minute);
            algorithm.securities.update_price(&ypf, dec!(20));
            algorithm.portfolio.update_prices(&ypf, dec!(20));
        }

        // Consume the initial restore-driven rebalance so the framework is quiet.
        let slice = lean_data::Slice::new(DateTime::now());
        let _ = crate::run_framework_pipeline(&context.framework(), &state, &slice);
        // A follow-up run with no trigger produces nothing.
        let slice = lean_data::Slice::new(DateTime::now());
        assert!(
            crate::run_framework_pipeline(&context.framework(), &state, &slice).is_empty(),
            "quiet framework should not rebalance without a trigger"
        );

        // An Invalid order for YPF re-arms the rebalance; the next run resubmits.
        let mut budget: HashMap<u64, u32> = HashMap::new();
        let invalid =
            OrderEvent::invalid(1, ypf.clone(), DateTime::now(), "Brokerage rejected: cash");
        rearm_rebalance_for_invalid_insight_orders(
            &manager,
            std::slice::from_ref(&invalid),
            &mut budget,
        );
        assert_eq!(budget.get(&ypf.id.sid), Some(&1), "one retry consumed");

        let slice = lean_data::Slice::new(DateTime::now());
        let requests = crate::run_framework_pipeline(&context.framework(), &state, &slice);
        assert!(
            !requests.is_empty(),
            "re-armed rebalance should recompute the YPF target"
        );

        // Exhaust the budget: after MAX_INSIGHT_RETRIES rejections, no more retries.
        while budget.get(&ypf.id.sid).copied().unwrap_or(0) < MAX_INSIGHT_RETRIES {
            rearm_rebalance_for_invalid_insight_orders(
                &manager,
                std::slice::from_ref(&invalid),
                &mut budget,
            );
        }
        assert_eq!(budget.get(&ypf.id.sid), Some(&MAX_INSIGHT_RETRIES));
        // Consume any pending rebalance, then a further Invalid must NOT re-arm.
        let slice = lean_data::Slice::new(DateTime::now());
        let _ = crate::run_framework_pipeline(&context.framework(), &state, &slice);
        rearm_rebalance_for_invalid_insight_orders(&manager, &[invalid], &mut budget);
        assert_eq!(
            budget.get(&ypf.id.sid),
            Some(&MAX_INSIGHT_RETRIES),
            "budget stays capped"
        );
        let slice = lean_data::Slice::new(DateTime::now());
        assert!(
            crate::run_framework_pipeline(&context.framework(), &state, &slice).is_empty(),
            "exhausted symbol must not re-trigger the rebalance"
        );
    }

    /// A non-insight (liquidation) Invalid order does not re-arm the rebalance:
    /// the symbol has no active insight, so no retry budget is consumed and the
    /// framework stays quiet.
    #[test]
    fn invalid_liquidation_order_does_not_rearm_rebalance() {
        let aaa = Symbol::create_equity("AAA", &Market::usa());
        let state = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let (manager, context) = framework_manager(state.clone());
        {
            let framework = context.framework();
            let mut fw = framework.lock().unwrap();
            fw.alpha_models.push(Box::new(NullAlphaModel));
        }

        let mut budget: HashMap<u64, u32> = HashMap::new();
        let invalid = OrderEvent::invalid(
            9,
            aaa.clone(),
            DateTime::now(),
            "Brokerage rejected: restricted",
        );
        rearm_rebalance_for_invalid_insight_orders(&manager, &[invalid], &mut budget);

        assert!(
            !budget.contains_key(&aaa.id.sid),
            "non-insight symbol consumes no retry budget"
        );
        let slice = lean_data::Slice::new(DateTime::now());
        assert!(
            crate::run_framework_pipeline(&context.framework(), &state, &slice).is_empty(),
            "liquidation rejection must not trigger a rebalance"
        );
    }
}
