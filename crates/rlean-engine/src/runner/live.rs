use crate::{
    algorithm_manager::{AlgorithmManager, OrderEventProcessing},
    data_feed::DataFeedContext,
    runner::backtest::{
        apply_risk_free_rate_to_option_chains, benchmark_subscription_for_symbol,
        subscriptions_with_benchmark, subscriptions_with_option_chains,
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
use rlean_live::LiveSliceAssembler;
use rlean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
    OrderEvent,
};
use rlean_statistics::{Trade, TradeBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    algorithm_manager.set_market_hours_database(market_hours_database);
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
    algorithm_manager.warmup_finished(&mut services);

    // The sidecar owns every live subscription and pushes canonical batches on
    // the persistent exchange; the engine only assembles them into slices.
    let feed_context = DataFeedContext::new(config.data_sidecar.clone());
    let sidecar = config.data_sidecar.clone();
    let mut live_subscriptions =
        LiveSubscriptionSet::subscribe_initial(sidecar.as_ref(), &subscriptions).await?;

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
                    }
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

    'live: loop {
        if should_stop(
            algorithm_manager.slices_processed() as usize,
            config.max_slices,
            run_started,
            config.max_runtime,
        ) {
            break;
        }

        let ready_slices =
            match next_live_item(&mut live_subscriptions, Duration::from_millis(250))? {
                Some(item) => assembler.push(item),
                None => assembler
                    .flush_ready(rlean_core::DateTime::now())
                    .into_iter()
                    .collect(),
            };

        for mut slice in ready_slices {
            apply_risk_free_rate_to_option_chains(
                &mut slice,
                risk_free_interest_rate_model.as_ref(),
            );
            process_live_slice(
                &mut algorithm_manager,
                &mut services,
                &mut config,
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
        if let Some(mut slice) = assembler.flush() {
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
                    &mut config,
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
    algorithm_manager.finish(&mut services);
    live_subscriptions.unsubscribe_all(sidecar.as_ref()).await;
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
async fn process_live_slice<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    services: &mut dyn AlgorithmServices,
    config: &mut LiveRunConfig,
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

    let new_trading_day = algorithm_manager.handle_new_trading_day(slice, services);
    let changes = algorithm_manager.apply_universe_selection(slice, new_trading_day, services);
    if changes.has_changes() {
        sync_live_subscriptions(
            algorithm_manager,
            config,
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
        config,
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
    config: &mut LiveRunConfig,
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
    let sidecar = config.data_sidecar.clone();
    live_subscriptions
        .sync(sidecar.as_ref(), &subscriptions)
        .await?;
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

fn next_live_item(
    subscriptions: &mut LiveSubscriptionSet,
    timeout: Duration,
) -> Result<Option<LiveDataItem>> {
    for subscription in subscriptions.market.values() {
        match subscription.receiver.recv_timeout(Duration::ZERO) {
            Ok(item) => return Ok(Some(item?)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {}
        }
    }

    if subscriptions.market.is_empty() {
        return Ok(None);
    }

    for subscription in subscriptions.market.values() {
        match subscription.receiver.recv_timeout(timeout) {
            Ok(item) => return Ok(Some(item?)),
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {}
        }
    }
    Ok(None)
}

struct LiveSubscriptionSet {
    market: HashMap<u64, LiveDataSubscription>,
    configs: HashMap<u64, SubscriptionDataConfig>,
    remote_ids: HashMap<u64, u64>,
    tasks: HashMap<u64, tokio::task::JoinHandle<()>>,
    /// Unique-id set from the last full `sync_live_subscriptions` pass. Used to
    /// short-circuit the per-slice sync when the subscription set is unchanged
    /// (issue #39). `None` until the first sync runs.
    last_synced_ids: Option<HashSet<u64>>,
}

impl LiveSubscriptionSet {
    async fn subscribe_initial(
        sidecar: &DataSidecarClient,
        subscriptions: &[SubscriptionDataConfig],
    ) -> Result<Self> {
        let mut set = Self {
            market: HashMap::new(),
            configs: HashMap::new(),
            remote_ids: HashMap::new(),
            tasks: HashMap::new(),
            last_synced_ids: None,
        };
        for config in subscriptions {
            set.add(sidecar, config.clone()).await?;
        }
        Ok(set)
    }

    async fn sync(
        &mut self,
        sidecar: &DataSidecarClient,
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
                    let _ = subscription_config;
                    if let Some(task) = self.tasks.remove(&id) {
                        task.abort();
                    }
                    if let Some(remote_id) = self.remote_ids.remove(&id) {
                        sidecar.remove_subscription(remote_id).await?;
                    }
                }
                self.market.remove(&id);
            }
        }

        for subscription_config in current {
            if !self.configs.contains_key(&subscription_config.unique_id()) {
                self.add(sidecar, subscription_config.clone()).await?;
            }
        }
        Ok(())
    }

    async fn add(
        &mut self,
        sidecar: &DataSidecarClient,
        config: SubscriptionDataConfig,
    ) -> Result<()> {
        let id = config.unique_id();
        tracing::info!(
            "subscribing live market data for {} ({:?} {:?})",
            config.symbol,
            config.resolution,
            config.tick_type
        );
        let data_type = WireDataType::try_from(SubscriptionSpec::from(&config).data_type)
            .map_err(|value| anyhow::anyhow!("unknown live data type {value}"))?;
        let (remote_id, mut stream) = sidecar.subscribe_live(&config).await?;
        let (sender, receiver) = rlean_data::live_data_channel();
        let live_config = config.clone();
        let task = tokio::spawn(async move {
            while let Some(batch) = stream.next().await {
                let result = batch
                    .and_then(|batch| decode_batch(data_type, batch, &live_config.symbol))
                    .and_then(|batch| live_items(batch, &live_config));
                match result {
                    Ok(items) => {
                        for item in items {
                            if sender.send(Ok(item)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ =
                            sender.send(Err(rlean_core::LeanError::DataError(error.to_string())));
                        return;
                    }
                }
            }
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

    async fn unsubscribe_all(&mut self, sidecar: &DataSidecarClient) {
        self.configs.clear();
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
        let remote_ids: Vec<_> = self.remote_ids.drain().map(|(_, id)| id).collect();
        for remote_id in remote_ids {
            if let Err(error) = sidecar.remove_subscription(remote_id).await {
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
        CanonicalDataBatch::RiskFreeInterestRates(_) | CanonicalDataBatch::RecordBatch(_) => {
            anyhow::bail!("unsupported canonical live batch type")
        }
    })
}

#[cfg(test)]
mod unmanaged_liquidation_tests {
    use super::*;
    use rust_decimal_macros::dec;

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
}
