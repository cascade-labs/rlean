// Engine-owned Algorithm Framework pipeline.
//
// Owns the alpha -> portfolio-construction -> risk -> execution pipeline and the
// insight scoring / rebalance state machine. Language bindings (Python) only
// register models and implement the `InsightObserver` callback used to expose
// scored insights back to user code.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rlean_alpha::{
    ActiveInsightSnapshot, AlphaAnalytics, AlphaPerformanceTracker, IAlphaModel, Insight,
    InsightCollection, InsightCollectionSnapshot, InsightDirection as AlphaDir, InsightEvent,
    InsightEventKind,
};
use rlean_core::{DateTime, Symbol, TickType};
use rlean_execution::execution_model::ExecutionTargetRef;
use rlean_execution::{
    ExecutionContext, ExecutionOpenOrder, ExecutionOrderType, IExecutionModel,
    ImmediateExecutionModel, OrderRequest, SecurityData,
};
use rlean_portfolio_construction::portfolio_construction_model::{
    InsightForPcmRef, RebalanceCadence, RebalancePolicy,
};
use rlean_portfolio_construction::{
    EqualWeightingPortfolioConstructionModel, IPortfolioConstructionModel,
    InsightDirection as PcmDir,
};
use rlean_risk::risk_management::{
    HoldingSnapshot, NullRiskManagement, PortfolioTarget as RiskTarget, RiskContext,
    RiskManagementModel,
};
use rust_decimal::Decimal;

/// Callback used by the framework to publish the current scored insight set back
/// to a language binding (e.g. setting `algorithm.Insights` on a Python object).
///
/// The engine owns all pipeline logic; this is the only seam a language binding
/// hooks into for insight exposure.
pub trait InsightObserver: Send + Sync {
    fn closed_snapshot_cache(&self) -> (Option<usize>, Option<Arc<[Insight]>>) {
        (None, None)
    }

    fn on_insights(&self, snapshot: ActiveInsightSnapshot, utc_now: DateTime);
}

/// Holds the registered Algorithm Framework models and the engine-owned
/// pipeline state machine (insight scoring, rebalance cadence, analytics).
pub struct FrameworkState {
    pub alpha_models: Vec<Box<dyn IAlphaModel>>,
    pub pcm: Box<dyn IPortfolioConstructionModel>,
    pub exec_model: Box<dyn IExecutionModel>,
    pub risk_model: Box<dyn RiskManagementModel>,
    pub insights: InsightCollection,
    pub alpha_tracker: AlphaPerformanceTracker,
    pending_flat_targets: Vec<Insight>,
    pending_insight_events: Vec<InsightEvent>,
    pending_security_changes: bool,
    next_rebalance_time: Option<DateTime>,
    /// Optional language-binding observer used to expose scored insights.
    observer: Option<Arc<dyn InsightObserver>>,
}

impl FrameworkState {
    pub fn new() -> Self {
        Self {
            alpha_models: Vec::new(),
            pcm: Box::new(EqualWeightingPortfolioConstructionModel::new()),
            exec_model: Box::new(ImmediateExecutionModel::new()),
            risk_model: Box::new(NullRiskManagement),
            insights: InsightCollection::new(),
            alpha_tracker: AlphaPerformanceTracker::new(),
            pending_flat_targets: Vec::new(),
            pending_insight_events: Vec::new(),
            pending_security_changes: false,
            next_rebalance_time: None,
            observer: None,
        }
    }

    /// Install the language-binding insight observer.
    pub fn set_observer(&mut self, observer: Arc<dyn InsightObserver>) {
        self.observer = Some(observer);
    }

    pub fn has_observer(&self) -> bool {
        self.observer.is_some()
    }

    /// True when at least one alpha model has been registered.
    pub fn is_active(&self) -> bool {
        !self.alpha_models.is_empty()
    }

    fn expose_insights(&self, utc_now: DateTime) {
        let Some(observer) = &self.observer else {
            return;
        };
        let (closed_version, closed) = observer.closed_snapshot_cache();
        observer.on_insights(
            self.insights
                .active_snapshot_reusing_closed(closed_version, closed),
            utc_now,
        );
    }

    fn collect_alpha_insights(
        &mut self,
        slice: &rlean_data::Slice,
        securities: &[Symbol],
    ) -> Vec<Insight> {
        self.alpha_models
            .iter_mut()
            .flat_map(|m| m.update(slice, securities))
            .map(|i| i.with_generated_time_utc(slice.time))
            .collect()
    }

    fn run_pipeline_from_alpha(
        &mut self,
        slice: &rlean_data::Slice,
        alpha_insights: Vec<Insight>,
        portfolio_value: Decimal,
        prices: &HashMap<u64, Decimal>,
        risk_context: &RiskContext,
        execution_context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        // Always update PCM price history so warm-up-requiring models (e.g.
        // Black-Litterman) accumulate data even when alpha is silent.
        self.pcm.update_security_prices(prices);

        // Score existing active insights against the latest prices before any of
        // them close this bar (mirrors C# Lean insight scoring).
        self.insights.score_active(prices, slice.time);

        let expired_insights = self.insights.remove_expired(slice.time);
        let expired_insight_count = expired_insights.len();
        self.push_insight_events(InsightEventKind::Expired, slice.time, &expired_insights);

        // Record the now-closed insights (with their realised forward returns)
        // for end-of-backtest IC / correlation / ranking analytics.
        self.alpha_tracker.record_expired(&expired_insights);

        let mut flat_closed: Vec<Insight> = Vec::new();
        for insight in &alpha_insights {
            if insight.direction == AlphaDir::Flat {
                flat_closed.extend(self.insights.close_source_symbol(
                    &insight.symbol,
                    insight.source_model.as_ref(),
                    slice.time,
                ));
                self.pending_flat_targets.push(insight.clone());
            } else {
                let mut ins = insight.clone();
                if ins.reference_value.is_none() {
                    ins.reference_value = prices.get(&ins.symbol.id.sid).copied();
                }
                self.insights.add(ins);
                if let Some(added) = self.insights.latest_for_symbol(&insight.symbol).cloned() {
                    self.push_insight_event(InsightEventKind::Emitted, slice.time, added);
                }
            }
        }
        if !flat_closed.is_empty() {
            self.alpha_tracker.record_expired(&flat_closed);
            self.push_insight_events(InsightEventKind::FlatClosed, slice.time, &flat_closed);
        }

        let pending_flat_count = self.pending_flat_targets.len();
        let active_target_count = if self.pcm.use_all_active_insights() {
            self.insights.active_count(slice.time)
        } else {
            self.insights.latest_active_symbol_count(slice.time)
        };
        let expired_flat_count = expired_insights
            .iter()
            .filter(|insight| !self.insights.has_active(&insight.symbol, slice.time))
            .count();
        if active_target_count == 0 && expired_flat_count == 0 && pending_flat_count == 0 {
            tracing::debug!(
                time = %slice.time,
                alpha_insights = alpha_insights.len(),
                active_target_count,
                "FWDBG no active targets; skipping rebalance"
            );
            let orders = self.exec_model.execute_with_context(&[], execution_context);
            self.expose_insights(slice.time);
            return orders;
        }

        let insight_changes =
            !alpha_insights.is_empty() || expired_insight_count != 0 || pending_flat_count != 0;
        if !self.is_rebalance_due(slice.time, insight_changes) {
            self.expose_insights(slice.time);
            return Vec::new();
        }

        let mut target_insights = if self.pcm.use_all_active_insights() {
            self.insights.active(slice.time)
        } else {
            self.insights.latest_active_per_symbol(slice.time)
        };

        for insight in expired_insights {
            if !self.insights.has_active(&insight.symbol, slice.time) {
                let mut flat = insight.clone();
                flat.direction = AlphaDir::Flat;
                flat.generated_time_utc = slice.time;
                flat.close_time_utc = slice.time;
                target_insights.push(flat);
            }
        }
        target_insights.append(&mut self.pending_flat_targets);

        self.expose_insights(slice.time);

        let pcm_insights: Vec<InsightForPcmRef<'_>> = target_insights
            .iter()
            .map(|i| InsightForPcmRef {
                symbol: &i.symbol,
                direction: match i.direction {
                    AlphaDir::Up => PcmDir::Up,
                    AlphaDir::Down => PcmDir::Down,
                    AlphaDir::Flat => PcmDir::Flat,
                },
                magnitude: i.magnitude,
                confidence: i.confidence,
                source_model: i.source_model.as_ref(),
            })
            .collect();

        let pcm_targets = self
            .pcm
            .create_targets_from_refs(&pcm_insights, portfolio_value, prices);

        tracing::debug!(
            time = %slice.time,
            alpha_insights = pcm_insights.len(),
            target_insights = target_insights.len(),
            pcm_targets = pcm_targets.len(),
            portfolio_value = %portfolio_value,
            "FWDBG rebalance produced pcm targets"
        );

        let target_tags: HashMap<u64, String> = pcm_targets
            .iter()
            .map(|t| (t.symbol.id.sid, t.tag.clone()))
            .collect();
        let risk_targets: Vec<RiskTarget> = pcm_targets
            .iter()
            .map(|t| RiskTarget::new(t.symbol.clone(), t.quantity))
            .collect();
        let adjusted = self
            .risk_model
            .manage_risk_with_context(&risk_targets, risk_context);
        let canceled_insights = self.risk_model.canceled_insights();
        if !canceled_insights.is_empty() {
            let closed = self.insights.clear_symbols(&canceled_insights);
            self.alpha_tracker.record_expired(&closed);
            self.push_insight_events(InsightEventKind::RiskCancelled, slice.time, &closed);
        }
        let adjusted_by_symbol: HashMap<u64, RiskTarget> =
            risk_targets
                .into_iter()
                .fold(HashMap::new(), |mut targets, target| {
                    targets.insert(target.symbol.id.sid, target);
                    targets
                });
        let adjusted_by_symbol =
            adjusted
                .into_iter()
                .fold(adjusted_by_symbol, |mut targets, target| {
                    targets.insert(target.symbol.id.sid, target);
                    targets
                });

        let exec_targets: Vec<ExecutionTargetRef<'_>> = adjusted_by_symbol
            .iter()
            .map(|t| ExecutionTargetRef {
                symbol: &t.1.symbol,
                quantity: t.1.quantity,
                tag: target_tags
                    .get(&t.1.symbol.id.sid)
                    .map(String::as_str)
                    .unwrap_or_default(),
            })
            .collect();

        let orders = self
            .exec_model
            .execute_refs_with_context(&exec_targets, execution_context);
        tracing::debug!(
            time = %slice.time,
            exec_targets = exec_targets.len(),
            orders = orders.len(),
            "FWDBG execution produced orders"
        );
        orders
    }

    /// Run the full alpha → PCM → risk → execution pipeline.
    pub fn run_pipeline(
        &mut self,
        slice: &rlean_data::Slice,
        securities: &[Symbol],
        portfolio_value: Decimal,
        prices: &HashMap<u64, Decimal>,
        risk_context: &RiskContext,
        execution_context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        let alpha_insights = self.collect_alpha_insights(slice, securities);
        self.run_pipeline_from_alpha(
            slice,
            alpha_insights,
            portfolio_value,
            prices,
            risk_context,
            execution_context,
        )
    }

    pub fn compute_alpha_analytics(&self) -> AlphaAnalytics {
        self.alpha_tracker.compute()
    }

    pub fn insight_snapshot(&self) -> InsightCollectionSnapshot {
        self.insights.snapshot()
    }

    pub fn take_insight_events(&mut self) -> Vec<InsightEvent> {
        std::mem::take(&mut self.pending_insight_events)
    }

    pub fn restore_insights(&mut self, snapshot: InsightCollectionSnapshot, utc_now: DateTime) {
        self.insights = InsightCollection::from_snapshot(snapshot);
        let closed = self.insights.closed_insights();
        self.alpha_tracker = AlphaPerformanceTracker::from_closed_insights(&closed);

        let restored_active = self.insights.active_snapshot();
        self.push_insight_events(
            InsightEventKind::Restored,
            utc_now,
            restored_active.active.as_ref(),
        );
        if let Some(observer) = &self.observer {
            observer.on_insights(restored_active, utc_now);
        }
    }

    pub fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        if !added.is_empty() || !removed.is_empty() {
            self.pending_security_changes = true;
        }
        for model in &mut self.alpha_models {
            model.on_securities_changed(added, removed);
        }
        self.pcm.on_securities_changed(added, removed);
        self.exec_model.on_securities_changed(added, removed);
        let removed_insights = self.insights.clear_symbols(removed);
        self.alpha_tracker.record_expired(&removed_insights);
        self.push_insight_events(
            InsightEventKind::UniverseRemoved,
            DateTime::now(),
            &removed_insights,
        );
        for mut insight in removed_insights {
            insight.direction = AlphaDir::Flat;
            self.pending_flat_targets.push(insight);
        }
    }

    fn push_insight_event(
        &mut self,
        kind: InsightEventKind,
        event_time_utc: DateTime,
        insight: Insight,
    ) {
        self.pending_insight_events
            .push(InsightEvent::new(kind, event_time_utc, insight));
    }

    fn push_insight_events(
        &mut self,
        kind: InsightEventKind,
        event_time_utc: DateTime,
        insights: &[Insight],
    ) {
        for insight in insights {
            self.push_insight_event(kind, event_time_utc, insight.clone());
        }
    }

    fn is_rebalance_due(&mut self, now: DateTime, insight_changes: bool) -> bool {
        let policy = self.pcm.rebalance_policy();

        if self.next_rebalance_time.is_none() {
            self.next_rebalance_time = next_rebalance_time(&policy, now);
            if matches!(policy.cadence(), RebalanceCadence::EverySlice) {
                self.refresh_rebalance(now, &policy);
                return true;
            }
        }

        if policy.rebalance_on_security_changes() && self.pending_security_changes {
            self.refresh_rebalance(now, &policy);
            return true;
        }

        if policy.rebalance_on_insight_changes() && insight_changes {
            self.refresh_rebalance(now, &policy);
            return true;
        }

        if self
            .next_rebalance_time
            .is_some_and(|rebalance_time| rebalance_time <= now)
        {
            self.refresh_rebalance(now, &policy);
            return true;
        }

        false
    }

    fn refresh_rebalance(&mut self, now: DateTime, policy: &RebalancePolicy) {
        self.next_rebalance_time = next_rebalance_time(policy, now);
        self.pending_security_changes = false;
    }
}

fn next_rebalance_time(policy: &RebalancePolicy, now: DateTime) -> Option<DateTime> {
    match policy.cadence() {
        RebalanceCadence::EverySlice => None,
        RebalanceCadence::Period(period) => Some(now + *period),
        RebalanceCadence::NextTime(next_time) => next_time(now),
    }
}

impl Default for FrameworkState {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the framework pipeline against the current algorithm state.
/// Called from the runner after on_data (no GIL held).
pub fn run_framework_pipeline(
    framework: &Arc<Mutex<FrameworkState>>,
    algorithm: &Arc<Mutex<rlean_algorithm::qc_algorithm::QcAlgorithm>>,
    slice: &rlean_data::Slice,
) -> Vec<OrderRequest> {
    {
        let fw = framework.lock().unwrap();
        if !fw.is_active() {
            return Vec::new();
        }
    }

    // The alpha's Update() reads only the security symbol list, so give it a cheap
    // symbols-only snapshot instead of a full FrameworkInputs build (holdings,
    // orders, prices, portfolio value). Behaviourally identical: collect_alpha_insights
    // touches nothing else on the inputs.
    let alpha_securities: Vec<Symbol> = {
        let alg = algorithm.lock().unwrap();
        alg.securities.all().map(|s| s.symbol.clone()).collect()
    };
    let alpha_insights = {
        let mut fw = framework.lock().unwrap();
        fw.collect_alpha_insights(slice, &alpha_securities)
    };

    // Alpha models can add securities and seed prices during Update(); build the
    // full inputs once here so PCM/risk/execution consume the post-alpha algorithm
    // state. This single post-alpha build already reflects any securities the alpha
    // added, so no pre-alpha full build is needed.
    let inputs = build_framework_inputs(algorithm, slice);
    let mut fw = framework.lock().unwrap();
    let execution_context = ExecutionContext::new(
        slice.time,
        &inputs.security_data,
        &inputs.open_order_data,
        inputs.portfolio_value,
    )
    .with_minimum_order_margin_portfolio_percentage(
        inputs.minimum_order_margin_portfolio_percentage,
    );
    fw.run_pipeline_from_alpha(
        slice,
        alpha_insights,
        inputs.portfolio_value_less_free_buffer,
        &inputs.prices,
        &inputs.risk_context,
        &execution_context,
    )
}

pub fn notify_framework_securities_changed(
    framework: &Arc<Mutex<FrameworkState>>,
    added: &[Symbol],
    removed: &[Symbol],
) {
    let mut fw = framework.lock().unwrap();
    fw.on_securities_changed(added, removed);
}

struct FrameworkInputs {
    prices: HashMap<u64, Decimal>,
    portfolio_value: Decimal,
    portfolio_value_less_free_buffer: Decimal,
    minimum_order_margin_portfolio_percentage: Decimal,
    risk_context: RiskContext,
    security_data: HashMap<u64, SecurityData>,
    open_order_data: Vec<ExecutionOpenOrder>,
}

fn build_framework_inputs(
    algorithm: &Arc<Mutex<rlean_algorithm::qc_algorithm::QcAlgorithm>>,
    slice: &rlean_data::Slice,
) -> FrameworkInputs {
    let alg = algorithm.lock().unwrap();
    // Single holdings map keyed by sid. `current_qty` and the risk-model snapshot
    // both derive from this, so there is no need for a second parallel quantity map.
    let portfolio_holdings = alg.portfolio.all_holdings();
    let mut holding_data: HashMap<u64, rlean_algorithm::portfolio::SecurityHolding> =
        HashMap::with_capacity(portfolio_holdings.len());
    for holding in portfolio_holdings {
        holding_data.insert(holding.symbol.id.sid, holding);
    }

    let open_orders_snapshot = alg.transactions.get_open_orders();
    let mut open_orders: HashMap<u64, Decimal> = HashMap::with_capacity(open_orders_snapshot.len());
    let mut open_order_data = Vec::with_capacity(open_orders_snapshot.len());
    for order in open_orders_snapshot {
        let order_type = match order.order_type {
            rlean_orders::OrderType::Market => ExecutionOrderType::Market,
            rlean_orders::OrderType::Limit => ExecutionOrderType::Limit,
            rlean_orders::OrderType::MarketOnOpen => ExecutionOrderType::MarketOnOpen,
            rlean_orders::OrderType::MarketOnClose => ExecutionOrderType::MarketOnClose,
            _ => ExecutionOrderType::Market,
        };
        let remaining_quantity = order.remaining_quantity();
        *open_orders
            .entry(order.symbol.id.sid)
            .or_insert(Decimal::ZERO) += remaining_quantity;
        open_order_data.push(ExecutionOpenOrder {
            id: order.id,
            symbol: order.symbol.clone(),
            quantity: order.quantity,
            filled_quantity: order.filled_quantity,
            remaining_quantity,
            order_type,
            limit_price: order.limit_price,
            post_only: order.properties.post_only,
            tag: order.tag.clone(),
            created_time: order.created_time,
            last_update_time: order.last_update_time,
        });
    }
    let portfolio_value = alg.portfolio_value();
    let portfolio_value_less_free_buffer = alg.portfolio_value_less_free_buffer();
    let mut security_data: HashMap<u64, SecurityData> =
        HashMap::with_capacity(alg.securities.count());
    for security in alg.securities.all() {
        let symbol = security.symbol.clone();
        let sid = symbol.id.sid;
        let base_price = security.current_price();
        let current_qty = holding_data
            .get(&sid)
            .map(|holding| holding.quantity)
            .unwrap_or(Decimal::ZERO);
        let open_order_qty = open_orders.get(&sid).copied().unwrap_or(Decimal::ZERO);
        // Immutable per-security decimals converted once at security construction.
        let lot_size = security.lot_size_decimal();
        let minimum_price_variation = security.minimum_price_variation_decimal();
        let data = execution_security_data_from_slice(
            slice,
            symbol,
            base_price,
            current_qty,
            open_order_qty,
            lot_size,
            minimum_price_variation,
        );
        security_data.insert(sid, data);
    }
    let mut prices: HashMap<u64, Decimal> = HashMap::with_capacity(security_data.len());
    for (sid, data) in &security_data {
        prices.insert(*sid, data.price);
    }
    let mut risk_holdings = Vec::with_capacity(holding_data.len());
    for holding in holding_data
        .values()
        .filter(|holding| !holding.quantity.is_zero())
    {
        let last_price = security_data
            .get(&holding.symbol.id.sid)
            .map(|data| data.price)
            .filter(|price| *price > Decimal::ZERO)
            .unwrap_or(holding.last_price);
        risk_holdings.push(HoldingSnapshot {
            symbol: holding.symbol.clone(),
            quantity: holding.quantity,
            average_price: holding.average_price,
            last_price,
            unrealized_pnl: holding.unrealized_pnl,
        });
    }
    let risk_context = RiskContext {
        total_portfolio_value: portfolio_value,
        holdings: risk_holdings,
    };

    FrameworkInputs {
        prices,
        portfolio_value,
        portfolio_value_less_free_buffer,
        minimum_order_margin_portfolio_percentage: alg.minimum_order_margin_portfolio_percentage,
        risk_context,
        security_data,
        open_order_data,
    }
}

#[allow(clippy::too_many_arguments)]
fn execution_security_data_from_slice(
    slice: &rlean_data::Slice,
    symbol: Symbol,
    base_price: Decimal,
    current_quantity: Decimal,
    open_order_quantity: Decimal,
    lot_size: Decimal,
    minimum_price_variation: Decimal,
) -> SecurityData {
    let sid = symbol.id.sid;
    let mut price = base_price;
    let mut bid = None;
    let mut ask = None;
    let mut volume = None;
    let mut vwap_price = None;
    let mut end_time = None;

    if let Some(bar) = slice.bars.get(&sid) {
        if bar.close > Decimal::ZERO {
            price = bar.close;
            bid = Some(bar.close);
            ask = Some(bar.close);
        }
        volume = Some(bar.volume);
        vwap_price = Some((bar.high + bar.low + bar.close) / Decimal::from(3u32));
        end_time = Some(bar.end_time);
    }

    if let Some(quote_bar) = slice.quote_bars.get(&sid) {
        let quote_price = quote_bar.mid_close();
        if quote_price > Decimal::ZERO {
            price = quote_price;
        }
        if let Some(bid_bar) = &quote_bar.bid {
            if bid_bar.close > Decimal::ZERO {
                bid = Some(bid_bar.close);
            }
        }
        if let Some(ask_bar) = &quote_bar.ask {
            if ask_bar.close > Decimal::ZERO {
                ask = Some(ask_bar.close);
            }
        }
        end_time = Some(quote_bar.end_time);
    }

    if let Some(ticks) = slice.ticks.get(&sid) {
        let mut tick_volume = Decimal::ZERO;
        let mut tick_value = Decimal::ZERO;
        for tick in ticks {
            match tick.tick_type {
                TickType::Trade => {
                    if tick.value > Decimal::ZERO {
                        price = tick.value;
                    }
                    if tick.quantity > Decimal::ZERO && tick.value > Decimal::ZERO {
                        tick_volume += tick.quantity;
                        tick_value += tick.value * tick.quantity;
                    }
                    end_time = Some(tick.time);
                }
                TickType::Quote => {
                    if tick.bid_price > Decimal::ZERO {
                        bid = Some(tick.bid_price);
                    }
                    if tick.ask_price > Decimal::ZERO {
                        ask = Some(tick.ask_price);
                    }
                    if tick.value > Decimal::ZERO {
                        price = tick.value;
                    }
                    end_time = Some(tick.time);
                }
                TickType::OpenInterest => {}
            }
        }
        if tick_volume > Decimal::ZERO {
            volume = Some(tick_volume);
            vwap_price = Some(tick_value / tick_volume);
        }
    }

    if let Some(book) = slice.order_books.get(&sid) {
        if let Some(best_bid) = book.best_bid() {
            bid = Some(best_bid.price);
        }
        if let Some(best_ask) = book.best_ask() {
            ask = Some(best_ask.price);
        }
        let book_price = book.mid_price();
        if book_price > Decimal::ZERO {
            price = book_price;
        }
        end_time = Some(book.time);
    }

    SecurityData {
        symbol,
        price,
        bid,
        ask,
        volume,
        vwap_price,
        average_volume: None,
        daily_std_dev: None,
        end_time,
        lot_size,
        minimum_price_variation,
        current_quantity,
        open_order_quantity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use rlean_core::TimeSpan;

    fn dt(year: i32, month: u32, day: u32) -> DateTime {
        let date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()))
    }

    #[test]
    fn period_policy_sets_next_rebalance_time() {
        let now = dt(2026, 1, 1);
        let policy = RebalancePolicy::period(TimeSpan::ONE_DAY);

        assert_eq!(
            next_rebalance_time(&policy, now),
            Some(now + TimeSpan::ONE_DAY)
        );
    }

    #[test]
    fn every_slice_policy_has_no_scheduled_next_time() {
        let policy = RebalancePolicy::every_slice();

        assert_eq!(next_rebalance_time(&policy, dt(2026, 1, 1)), None);
    }

    #[test]
    fn next_time_policy_uses_callback_result() {
        let policy = RebalancePolicy::next_time(|now| Some(now + TimeSpan::ONE_HOUR));
        let now = dt(2026, 1, 1);

        assert_eq!(
            next_rebalance_time(&policy, now),
            Some(now + TimeSpan::ONE_HOUR)
        );
    }

    #[test]
    fn insight_only_policy_ignores_time_and_security_changes() {
        let mut framework = FrameworkState::new();
        framework.pcm = Box::new(
            EqualWeightingPortfolioConstructionModel::with_bias_max_weight_and_rebalance_policy(
                rlean_portfolio_construction::PortfolioBias::LongShort,
                None,
                RebalancePolicy::insight_changes_only(),
            ),
        );
        let now = dt(2026, 1, 1);

        assert!(!framework.is_rebalance_due(now, false));
        framework.pending_security_changes = true;
        assert!(!framework.is_rebalance_due(now + TimeSpan::ONE_DAY, false));
        assert!(framework.is_rebalance_due(now + TimeSpan::ONE_DAY, true));
    }

    #[test]
    fn insight_only_policy_does_not_treat_checkpoint_restore_as_a_new_insight() {
        let mut framework = FrameworkState::new();
        framework.pcm = Box::new(
            EqualWeightingPortfolioConstructionModel::with_bias_max_weight_and_rebalance_policy(
                rlean_portfolio_construction::PortfolioBias::LongShort,
                None,
                RebalancePolicy::insight_changes_only(),
            ),
        );
        let now = dt(2026, 1, 1);
        let insight = Insight::up(
            Symbol::create_equity("SPY", &rlean_core::Market::usa()),
            TimeSpan::ONE_DAY,
        )
        .with_generated_time_utc(now);

        framework.restore_insights(
            InsightCollectionSnapshot {
                active: vec![insight],
                closed: Vec::new(),
                total_count: 1,
            },
            now,
        );

        assert!(!framework.is_rebalance_due(now, false));
    }
}
