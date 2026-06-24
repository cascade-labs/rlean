/// Algorithm Framework: wires add_alpha / set_portfolio_construction /
/// set_execution / set_risk_management together and runs the pipeline
/// after every on_data call.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::py_types::PyResolution;
use lean_alpha::{
    AlphaAnalytics, AlphaPerformanceTracker, IAlphaModel, InsightCollection,
    InsightCollectionSnapshot, InsightDirection as AlphaDir, InsightEvent, InsightEventKind,
};
use lean_core::{DateTime, Resolution, Symbol, TickType, TimeSpan};
use lean_execution::{
    AdaptiveMakerTakerExecutionModel, AggressivePostOnlyExecutionModel, ExecutionContext,
    ExecutionOpenOrder, ExecutionOrderType, ExecutionTarget, IExecutionModel,
    ImmediateExecutionModel, MakerThenTakerExecutionModel, OrderRequest, SecurityData,
};
use lean_portfolio_construction::{
    AccumulativeInsightPortfolioConstructionModel,
    BlackLittermanOptimizationPortfolioConstructionModel,
    ConfidenceWeightingPortfolioConstructionModel, EqualWeightingPortfolioConstructionModel,
    IPortfolioConstructionModel, InsightDirection as PcmDir, InsightForPcm,
    MeanReversionPortfolioConstructionModel, PortfolioBias, RiskParityPortfolioConstructionModel,
};
use lean_risk::risk_management::{
    HoldingSnapshot, NullRiskManagement, PortfolioTarget as RiskTarget, RiskContext,
    RiskManagementModel,
};
use lean_risk::{
    MaximumDrawdownPercentPortfolio, MaximumSectorExposureRiskManagementModel,
    MaximumUnrealizedProfitPercentPerSecurity,
};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

// ─── Framework State ──────────────────────────────────────────────────────────

/// Holds the registered Algorithm Framework models.
/// Shared via Arc<Mutex<>> between PyQcAlgorithm and PyAlgorithmAdapter.
pub struct FrameworkState {
    pub alpha_models: Vec<Box<dyn IAlphaModel>>,
    /// Portfolio construction model — defaults to EqualWeighting.
    pub pcm: Box<dyn IPortfolioConstructionModel>,
    /// Execution model — defaults to Immediate.
    pub exec_model: Box<dyn IExecutionModel>,
    /// Risk management model — defaults to Null (pass-through).
    pub risk_model: Box<dyn RiskManagementModel>,
    /// LEAN-style active insight collection owned by the framework pipeline.
    pub insights: InsightCollection,
    /// Accumulates closed-insight observations for IC / correlation analytics.
    pub alpha_tracker: AlphaPerformanceTracker,
    pending_flat_targets: Vec<lean_alpha::Insight>,
    pending_insight_events: Vec<InsightEvent>,
    pending_security_changes: bool,
    restored_insight_change: bool,
    next_rebalance_time: Option<DateTime>,
    /// Python QCAlgorithm instance; used to expose `algorithm.Insights` with scores.
    pub alg_py: Option<Py<PyAny>>,
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
            restored_insight_change: false,
            next_rebalance_time: None,
            alg_py: None,
        }
    }

    /// True when at least one alpha model has been registered.
    pub fn is_active(&self) -> bool {
        !self.alpha_models.is_empty()
    }

    fn collect_alpha_insights(
        &mut self,
        slice: &lean_data::Slice,
        securities: &[Symbol],
    ) -> Vec<lean_alpha::Insight> {
        self.expose_insights_to_python(slice.time);
        self.alpha_models
            .iter_mut()
            .flat_map(|m| m.update(slice, securities))
            .map(|i| i.with_generated_time_utc(slice.time))
            .collect()
    }

    fn run_pipeline_from_alpha(
        &mut self,
        slice: &lean_data::Slice,
        alpha_insights: Vec<lean_alpha::Insight>,
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
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

        let mut flat_closed: Vec<lean_alpha::Insight> = Vec::new();
        for insight in &alpha_insights {
            if insight.direction == AlphaDir::Flat {
                // Close ONLY the emitting model's insight on this symbol (scored + recorded),
                // not every model's — a Flat from one alpha must not silently destroy another
                // alpha's live insight before it can be scored, which would make per-alpha IC
                // depend on the rest of the book. See InsightCollection::close_source_symbol.
                flat_closed.extend(self.insights.close_source_symbol(
                    &insight.symbol,
                    &insight.source_model,
                    slice.time,
                ));
                self.pending_flat_targets.push(insight.clone());
            } else {
                // Capture the reference price at emission so the insight can be scored.
                let mut ins = insight.clone();
                if ins.reference_value.is_none() {
                    ins.reference_value = prices.get(&ins.symbol.value).copied();
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

        self.expose_insights_to_python(slice.time);

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

        let pending_flat_count = self.pending_flat_targets.len();
        target_insights.append(&mut self.pending_flat_targets);

        if target_insights.is_empty() {
            return self.exec_model.execute_with_context(&[], execution_context);
        }

        let restored_insight_change = self.restored_insight_change;
        self.restored_insight_change = false;
        let insight_changes = !alpha_insights.is_empty()
            || expired_insight_count != 0
            || pending_flat_count != 0
            || restored_insight_change;
        if !self.is_rebalance_due(slice.time, insight_changes) {
            return Vec::new();
        }

        // 2. Convert active alpha Insights → InsightForPcm.
        let pcm_insights: Vec<InsightForPcm> = target_insights
            .iter()
            .map(|i| InsightForPcm {
                symbol: i.symbol.clone(),
                direction: match i.direction {
                    AlphaDir::Up => PcmDir::Up,
                    AlphaDir::Down => PcmDir::Down,
                    AlphaDir::Flat => PcmDir::Flat,
                },
                magnitude: i.magnitude,
                confidence: i.confidence,
                source_model: i.source_model.clone(),
            })
            .collect();

        // 3. Portfolio construction: compute target quantities.
        let pcm_targets = self
            .pcm
            .create_targets(&pcm_insights, portfolio_value, prices);

        // 4. Risk management: filter / adjust targets.
        let risk_targets: Vec<RiskTarget> = pcm_targets
            .iter()
            .map(|t| RiskTarget::new(t.symbol.clone(), t.quantity))
            .collect();
        let adjusted = self
            .risk_model
            .manage_risk_with_context(&risk_targets, risk_context);
        let canceled_insights = self.risk_model.canceled_insights();
        if !canceled_insights.is_empty() {
            // Risk-cancelled insights are closed out — record their realized return for analytics.
            let closed = self.insights.clear_symbols(&canceled_insights);
            self.alpha_tracker.record_expired(&closed);
            self.push_insight_events(InsightEventKind::RiskCancelled, slice.time, &closed);
            self.expose_insights_to_python(slice.time);
        }
        let adjusted_by_symbol: HashMap<String, RiskTarget> =
            risk_targets
                .into_iter()
                .fold(HashMap::new(), |mut targets, target| {
                    targets.insert(target.symbol.value.clone(), target);
                    targets
                });
        let adjusted_by_symbol =
            adjusted
                .into_iter()
                .fold(adjusted_by_symbol, |mut targets, target| {
                    targets.insert(target.symbol.value.clone(), target);
                    targets
                });

        // 5. Execution: convert targets to concrete order requests.
        let exec_targets: Vec<ExecutionTarget> = adjusted_by_symbol
            .iter()
            .map(|t| ExecutionTarget {
                symbol: t.1.symbol.clone(),
                quantity: t.1.quantity,
            })
            .collect();

        self.exec_model
            .execute_with_context(&exec_targets, execution_context)
    }

    /// Run the full alpha → PCM → risk → execution pipeline.
    ///
    /// Returns a list of order requests that the runner should submit.
    pub fn run_pipeline(
        &mut self,
        slice: &lean_data::Slice,
        securities: &[Symbol],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
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

    /// Reduce the accumulated alpha observations into the analytics bundle
    /// (rolling IC series, signal-correlation matrix, keep/exclude ranking).
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

        let restored_active = self.insights.snapshot().active;
        self.push_insight_events(InsightEventKind::Restored, utc_now, &restored_active);
        self.restored_insight_change = !restored_active.is_empty();
        self.expose_insights_to_python(utc_now);
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
        // A removed security's insights are closed out (C# Lean Insights.Expire on universe
        // removal). Record their realized return for analytics before flattening — clear_symbols
        // finalizes + retains them, so they are not dropped from per-alpha IC.
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
        insight: lean_alpha::Insight,
    ) {
        self.pending_insight_events
            .push(InsightEvent::new(kind, event_time_utc, insight));
    }

    fn push_insight_events(
        &mut self,
        kind: InsightEventKind,
        event_time_utc: DateTime,
        insights: &[lean_alpha::Insight],
    ) {
        for insight in insights {
            self.push_insight_event(kind, event_time_utc, insight.clone());
        }
    }

    fn expose_insights_to_python(&self, utc_now: DateTime) {
        let Some(alg) = &self.alg_py else {
            return;
        };
        let snapshot = self.insights.get_insights();
        Python::attach(|py| {
            let mgr = PyInsightManager {
                insights: snapshot,
                utc_now,
            };
            if let Ok(obj) = Py::new(py, mgr) {
                let _ = alg.bind(py).setattr("Insights", obj);
            }
        });
    }

    fn is_rebalance_due(&mut self, now: DateTime, insight_changes: bool) -> bool {
        let Some(period) = self.pcm.rebalance_period() else {
            return true;
        };

        if self.next_rebalance_time.is_none() {
            self.next_rebalance_time = Some(now + period);
        }

        if self.pcm.rebalance_on_security_changes() && self.pending_security_changes {
            self.refresh_rebalance(now);
            return true;
        }

        if self.pcm.rebalance_on_insight_changes() && insight_changes {
            self.refresh_rebalance(now);
            return true;
        }

        if self
            .next_rebalance_time
            .is_some_and(|rebalance_time| rebalance_time <= now)
        {
            self.refresh_rebalance(now);
            return true;
        }

        false
    }

    fn refresh_rebalance(&mut self, now: DateTime) {
        self.next_rebalance_time = self.pcm.rebalance_period().map(|period| now + period);
        self.pending_security_changes = false;
    }
}

impl Default for FrameworkState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Python-side helper ───────────────────────────────────────────────────────

/// Run the framework pipeline and return order requests.
/// Called from the runner after on_data (no GIL held).
pub fn run_framework_pipeline(
    framework: &Arc<Mutex<FrameworkState>>,
    alg_inner: &Arc<Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>>,
    slice: &lean_data::Slice,
) -> Vec<OrderRequest> {
    {
        let fw = framework.lock().unwrap();
        if !fw.is_active() {
            return Vec::new();
        }
    }

    let initial_inputs = build_framework_inputs(alg_inner, slice);
    let alpha_insights = {
        let mut fw = framework.lock().unwrap();
        fw.collect_alpha_insights(slice, &initial_inputs.securities)
    };

    // Python alpha models can add securities and seed prices during Update().
    // Mirror C# LEAN by letting PCM/risk/execution consume the post-alpha
    // algorithm state instead of the stale pre-alpha subscription snapshot.
    let inputs = build_framework_inputs(alg_inner, slice);
    let mut fw = framework.lock().unwrap();
    let execution_context = ExecutionContext::new(
        slice.time,
        &inputs.security_data,
        &inputs.open_order_data,
        inputs.portfolio_value,
    );
    fw.run_pipeline_from_alpha(
        slice,
        alpha_insights,
        inputs.portfolio_value,
        &inputs.prices,
        &inputs.risk_context,
        &execution_context,
    )
}

struct FrameworkInputs {
    securities: Vec<Symbol>,
    prices: HashMap<String, Decimal>,
    portfolio_value: Decimal,
    risk_context: RiskContext,
    security_data: HashMap<String, SecurityData>,
    open_order_data: Vec<ExecutionOpenOrder>,
}

fn build_framework_inputs(
    alg_inner: &Arc<Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>>,
    slice: &lean_data::Slice,
) -> FrameworkInputs {
    let alg = alg_inner.lock().unwrap();
    let securities: Vec<Symbol> = alg.securities.all().map(|s| s.symbol.clone()).collect();
    let portfolio_holdings = alg.portfolio.all_holdings();
    let mut holdings: HashMap<String, Decimal> = HashMap::with_capacity(portfolio_holdings.len());
    for holding in &portfolio_holdings {
        holdings.insert(holding.symbol.value.clone(), holding.quantity);
    }
    let mut holding_data: HashMap<String, lean_algorithm::portfolio::SecurityHolding> =
        HashMap::with_capacity(portfolio_holdings.len());
    for holding in portfolio_holdings {
        holding_data.insert(holding.symbol.value.clone(), holding);
    }

    let open_orders_snapshot = alg.transactions.get_open_orders();
    let mut open_orders: HashMap<String, Decimal> =
        HashMap::with_capacity(open_orders_snapshot.len());
    let mut open_order_data = Vec::with_capacity(open_orders_snapshot.len());
    for order in open_orders_snapshot {
        let order_type = match order.order_type {
            lean_orders::OrderType::Market => ExecutionOrderType::Market,
            lean_orders::OrderType::Limit => ExecutionOrderType::Limit,
            lean_orders::OrderType::MarketOnOpen => ExecutionOrderType::MarketOnOpen,
            lean_orders::OrderType::MarketOnClose => ExecutionOrderType::MarketOnClose,
            _ => ExecutionOrderType::Market,
        };
        let remaining_quantity = order.remaining_quantity();
        *open_orders
            .entry(order.symbol.value.clone())
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
    let portfolio_value = alg.portfolio.total_portfolio_value();
    let mut security_data: HashMap<String, SecurityData> = HashMap::with_capacity(securities.len());
    for security in alg.securities.all() {
        let symbol = security.symbol.clone();
        let key = symbol.value.clone();
        let base_price = security.current_price();
        let current_qty = holdings.get(&key).copied().unwrap_or(Decimal::ZERO);
        let open_order_qty = open_orders.get(&key).copied().unwrap_or(Decimal::ZERO);
        let lot_size = Decimal::from_f64(security.symbol_properties.lot_size)
            .filter(|lot| *lot > Decimal::ZERO)
            .unwrap_or(Decimal::ONE);
        let minimum_price_variation =
            Decimal::from_f64(security.symbol_properties.minimum_price_variation)
                .filter(|tick| *tick > Decimal::ZERO)
                .unwrap_or(Decimal::new(1, 2));
        let data = execution_security_data_from_slice(
            slice,
            symbol,
            base_price,
            current_qty,
            open_order_qty,
            lot_size,
            minimum_price_variation,
        );
        security_data.insert(key, data);
    }
    let mut prices: HashMap<String, Decimal> = HashMap::with_capacity(security_data.len());
    for (key, data) in &security_data {
        prices.insert(key.clone(), data.price);
    }
    let mut risk_holdings = Vec::with_capacity(holding_data.len());
    for holding in holding_data
        .values()
        .filter(|holding| !holding.quantity.is_zero())
    {
        let key = holding.symbol.value.clone();
        let last_price = security_data
            .get(&key)
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
        securities,
        prices,
        portfolio_value,
        risk_context,
        security_data,
        open_order_data,
    }
}

fn execution_security_data_from_slice(
    slice: &lean_data::Slice,
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

    if let Some(context) = slice.perpetual_contexts.get(&sid) {
        if context.mark_px > Decimal::ZERO {
            price = context.mark_px;
        }
        end_time = Some(context.time);
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

pub fn notify_framework_securities_changed(
    framework: &Arc<Mutex<FrameworkState>>,
    added: &[Symbol],
    removed: &[Symbol],
) {
    let mut fw = framework.lock().unwrap();
    fw.on_securities_changed(added, removed);
}

// ─── Python Alpha Model Wrappers ──────────────────────────────────────────────

#[pyclass(name = "ConstantAlphaModel")]
pub struct PyConstantAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyConstantAlphaModel {
    /// direction: "up" | "down" | "flat"; period_days: insight validity in days.
    #[new]
    #[pyo3(signature = (direction, period_days, magnitude=None))]
    pub fn new(direction: &str, period_days: i64, magnitude: Option<f64>) -> Self {
        use lean_alpha::ConstantAlphaModel;
        let dir = match direction.to_lowercase().as_str() {
            "up" => AlphaDir::Up,
            "down" => AlphaDir::Down,
            _ => AlphaDir::Flat,
        };
        let period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        let mag = magnitude.and_then(Decimal::from_f64);
        Self {
            model: Some(Box::new(ConstantAlphaModel {
                direction: dir,
                period,
                magnitude: mag,
            })),
        }
    }
}

#[pyclass(name = "EmaCrossAlphaModel")]
pub struct PyEmaCrossAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyEmaCrossAlphaModel {
    #[new]
    #[pyo3(signature = (fast_period=50, slow_period=200, period_days=1))]
    pub fn new(fast_period: usize, slow_period: usize, period_days: i64) -> Self {
        use lean_alpha::EmaCrossAlphaModel;
        let period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(EmaCrossAlphaModel::new(
                fast_period,
                slow_period,
                period,
            ))),
        }
    }
}

#[pyclass(name = "HistoricalReturnsAlphaModel")]
pub struct PyHistoricalReturnsAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyHistoricalReturnsAlphaModel {
    /// period: lookback in bars (default 1, matching C# default).
    /// insight_period_days: override for insight lifetime in days;
    ///   if None, defaults to period calendar days (matches C# `resolution * lookback`).
    #[new]
    #[pyo3(signature = (period=1, insight_period_days=None))]
    pub fn new(period: usize, insight_period_days: Option<i64>) -> Self {
        use lean_alpha::HistoricalReturnsAlphaModel;
        let days = insight_period_days.unwrap_or(period as i64);
        let insight_period = TimeSpan::from_nanos(days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(HistoricalReturnsAlphaModel::new(
                period,
                insight_period,
            ))),
        }
    }
}

#[pyclass(name = "PearsonCorrelationPairsTradingAlphaModel")]
pub struct PyPearsonCorrelationPairsTradingAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyPearsonCorrelationPairsTradingAlphaModel {
    /// lookback: number of bars for correlation calculation (C# default: 15).
    /// threshold: % deviation of ratio from EMA mean to trigger signal (C# default: 1.0).
    /// minimum_correlation: minimum Pearson r to consider a pair tradable (C# default: 0.5).
    /// insight_period_days: lifetime of emitted insights in days; defaults to lookback days.
    #[new]
    #[pyo3(signature = (lookback=15, threshold=1.0, minimum_correlation=0.5, insight_period_days=None))]
    pub fn new(
        lookback: usize,
        threshold: f64,
        minimum_correlation: f64,
        insight_period_days: Option<i64>,
    ) -> Self {
        use lean_alpha::PearsonCorrelationPairsTradingAlphaModel;
        let days = insight_period_days.unwrap_or(lookback as i64);
        let insight_period = TimeSpan::from_nanos(days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(PearsonCorrelationPairsTradingAlphaModel::new(
                lookback,
                insight_period,
                threshold,
                minimum_correlation,
            ))),
        }
    }
}

#[pyclass(name = "MacdAlphaModel")]
pub struct PyMacdAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyMacdAlphaModel {
    #[new]
    #[pyo3(signature = (fast_period=12, slow_period=26, signal_period=9, period_days=1))]
    pub fn new(
        fast_period: usize,
        slow_period: usize,
        signal_period: usize,
        period_days: i64,
    ) -> Self {
        use lean_alpha::MacdAlphaModel;
        let period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(MacdAlphaModel::new(
                fast_period,
                slow_period,
                signal_period,
                period,
            ))),
        }
    }
}

#[pyclass(name = "RsiAlphaModel")]
pub struct PyRsiAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyRsiAlphaModel {
    #[new]
    #[pyo3(signature = (period=14, period_days=1))]
    pub fn new(period: usize, period_days: i64) -> Self {
        use lean_alpha::RsiAlphaModel;
        let insight_period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(RsiAlphaModel::new(period, insight_period))),
        }
    }
}

// ─── PortfolioBias ────────────────────────────────────────────────────────────

/// LEAN PortfolioBias — controls whether the PCM may take short positions.
#[pyclass(name = "PortfolioBias", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyPortfolioBias {
    Short = -1,
    LongShort = 0,
    Long = 1,
}

impl From<PyPortfolioBias> for PortfolioBias {
    fn from(b: PyPortfolioBias) -> Self {
        match b {
            PyPortfolioBias::Short => PortfolioBias::Short,
            PyPortfolioBias::LongShort => PortfolioBias::LongShort,
            PyPortfolioBias::Long => PortfolioBias::Long,
        }
    }
}

// ─── Python PCM Wrappers ──────────────────────────────────────────────────────

#[pyclass(name = "EqualWeightingPortfolioConstructionModel")]
pub struct PyEqualWeightingPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyEqualWeightingPcm {
    #[new]
    #[pyo3(signature = (rebalance=None, portfolio_bias=PyPortfolioBias::LongShort, max_weight=None))]
    pub fn new(
        rebalance: Option<&Bound<'_, PyAny>>,
        portfolio_bias: PyPortfolioBias,
        max_weight: Option<f64>,
    ) -> Self {
        let max_weight = max_weight
            .filter(|value| value.is_finite() && *value > 0.0)
            .and_then(Decimal::from_f64_retain);
        let rebalance_period = py_rebalance_period(rebalance);
        Self {
            model: Some(Box::new(
                EqualWeightingPortfolioConstructionModel::with_bias_max_weight_and_rebalance(
                    portfolio_bias.into(),
                    max_weight,
                    rebalance_period,
                ),
            )),
        }
    }
}

fn py_rebalance_period(rebalance: Option<&Bound<'_, PyAny>>) -> Option<TimeSpan> {
    let Some(rebalance) = rebalance else {
        return Some(TimeSpan::ONE_DAY);
    };

    if let Ok(py_resolution) = rebalance.extract::<PyResolution>() {
        let resolution: Resolution = py_resolution.into();
        return resolution.to_time_span();
    }

    if let Ok(seconds_obj) = rebalance.call_method0("total_seconds") {
        if let Ok(seconds) = seconds_obj.extract::<f64>() {
            if seconds.is_finite() && seconds > 0.0 {
                return Some(TimeSpan::from_nanos((seconds * 1_000_000_000.0) as i64));
            }
        }
    }

    Some(TimeSpan::ONE_DAY)
}

impl Default for PyEqualWeightingPcm {
    fn default() -> Self {
        Self {
            model: Some(Box::new(EqualWeightingPortfolioConstructionModel::new())),
        }
    }
}

#[pyclass(name = "InsightWeightingPortfolioConstructionModel")]
pub struct PyInsightWeightingPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyInsightWeightingPcm {
    #[new]
    pub fn new() -> Self {
        use lean_portfolio_construction::InsightWeightingPortfolioConstructionModel;
        Self {
            model: Some(Box::new(InsightWeightingPortfolioConstructionModel::new())),
        }
    }
}

impl Default for PyInsightWeightingPcm {
    fn default() -> Self {
        Self::new()
    }
}

#[pyclass(name = "MeanVarianceOptimizationPortfolioConstructionModel")]
pub struct PyMeanVariancePcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyMeanVariancePcm {
    #[new]
    pub fn new() -> Self {
        use lean_portfolio_construction::MeanVariancePortfolioConstructionModel;
        Self {
            model: Some(Box::new(MeanVariancePortfolioConstructionModel::new())),
        }
    }
}

impl Default for PyMeanVariancePcm {
    fn default() -> Self {
        Self::new()
    }
}

#[pyclass(name = "MaximumSharpeRatioPortfolioConstructionModel")]
pub struct PyMaxSharpeRatioPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyMaxSharpeRatioPcm {
    #[new]
    pub fn new() -> Self {
        use lean_portfolio_construction::MaximumSharpeRatioPortfolioConstructionModel;
        Self {
            model: Some(Box::new(MaximumSharpeRatioPortfolioConstructionModel::new())),
        }
    }
}

impl Default for PyMaxSharpeRatioPcm {
    fn default() -> Self {
        Self::new()
    }
}

#[pyclass(name = "BlackLittermanOptimizationPortfolioConstructionModel")]
pub struct PyBlackLittermanPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyBlackLittermanPcm {
    /// Create a Black-Litterman PCM.  Matches C# LEAN parameter names:
    ///
    /// ```python
    /// BlackLittermanOptimizationPortfolioConstructionModel(
    ///     rebalance=timedelta(days=7),
    ///     portfolio_bias=PortfolioBias.Long,
    ///     lookback=1,
    ///     period=63,
    ///     resolution=Resolution.Daily,        # accepted, currently ignored
    ///     risk_free_rate=0.0,
    ///     delta=2.5,
    ///     tau=0.05,
    ///     target_gross=1.0,
    /// )
    /// ```
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (
        rebalance=None,
        portfolio_bias=PyPortfolioBias::LongShort,
        lookback=1,
        period=63,
        resolution=None,
        risk_free_rate=0.0,
        delta=2.5,
        tau=0.05,
        target_gross=1.0,
    ))]
    pub fn new(
        rebalance: Option<&Bound<'_, PyAny>>,
        portfolio_bias: PyPortfolioBias,
        lookback: usize,
        period: usize,
        resolution: Option<&Bound<'_, PyAny>>,
        risk_free_rate: f64,
        delta: f64,
        tau: f64,
        target_gross: f64,
    ) -> Self {
        let _ = resolution; // resolution hint accepted but unused
        let rebalance_period = py_rebalance_period(rebalance);
        Self {
            model: Some(Box::new(
                BlackLittermanOptimizationPortfolioConstructionModel::with_params_and_rebalance(
                    lookback,
                    period,
                    risk_free_rate,
                    delta,
                    tau,
                    portfolio_bias.into(),
                    rebalance_period,
                    target_gross,
                ),
            )),
        }
    }
}

#[pyclass(name = "RiskParityPortfolioConstructionModel")]
pub struct PyRiskParityPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyRiskParityPcm {
    /// Create a Risk Parity PCM.
    ///
    /// Parameters (all optional, match C# defaults):
    /// - lookback: ROC period in bars (default 1)
    /// - period: rolling window length for covariance estimation (default 252)
    #[new]
    #[pyo3(signature = (lookback=1, period=252))]
    pub fn new(lookback: usize, period: usize) -> Self {
        Self {
            model: Some(Box::new(RiskParityPortfolioConstructionModel::with_params(
                lookback, period,
            ))),
        }
    }
}

#[pyclass(name = "ConfidenceWeightedPortfolioConstructionModel")]
pub struct PyConfidenceWeightingPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyConfidenceWeightingPcm {
    #[new]
    pub fn new() -> Self {
        Self {
            model: Some(Box::new(
                ConfidenceWeightingPortfolioConstructionModel::new(),
            )),
        }
    }
}

impl Default for PyConfidenceWeightingPcm {
    fn default() -> Self {
        Self::new()
    }
}

#[pyclass(name = "AccumulativeInsightPortfolioConstructionModel")]
pub struct PyAccumulativeInsightPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyAccumulativeInsightPcm {
    /// Create an AccumulativeInsight PCM.
    ///
    /// `percent`: per-insight allocation fraction (default 0.03 = 3 %).
    #[new]
    #[pyo3(signature = (percent=0.03))]
    pub fn new(percent: f64) -> Self {
        use rust_decimal::prelude::FromPrimitive;
        let pct = Decimal::from_f64(percent).unwrap_or(Decimal::new(3, 2));
        Self {
            model: Some(Box::new(
                AccumulativeInsightPortfolioConstructionModel::with_percent(pct),
            )),
        }
    }
}

#[pyclass(name = "MeanReversionPortfolioConstructionModel")]
pub struct PyMeanReversionPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyMeanReversionPcm {
    /// Create a Mean Reversion (OLMAR) PCM.
    ///
    /// Parameters (match C# defaults):
    /// - `reversion_threshold`: ε (default 1.0)
    /// - `window_size`: SMA period (default 20)
    #[new]
    #[pyo3(signature = (reversion_threshold=1.0, window_size=20))]
    pub fn new(reversion_threshold: f64, window_size: usize) -> Self {
        Self {
            model: Some(Box::new(
                MeanReversionPortfolioConstructionModel::with_params(
                    reversion_threshold,
                    window_size,
                ),
            )),
        }
    }
}

// ─── Python Execution Model Wrappers ─────────────────────────────────────────

#[pyclass(name = "ImmediateExecutionModel")]
pub struct PyImmediateExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PyImmediateExecutionModel {
    #[new]
    pub fn new() -> Self {
        Self {
            model: Some(Box::new(ImmediateExecutionModel::new())),
        }
    }
}

#[pyclass(name = "NullExecutionModel")]
pub struct PyNullExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PyNullExecutionModel {
    #[new]
    pub fn new() -> Self {
        use lean_execution::NullExecutionModel;
        Self {
            model: Some(Box::new(NullExecutionModel::new())),
        }
    }
}

impl Default for PyImmediateExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for PyNullExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[pyclass(name = "VolumeWeightedAveragePriceExecutionModel")]
pub struct PyVwapExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
    maximum_order_quantity_percent_volume: f64,
}

#[pymethods]
impl PyVwapExecutionModel {
    #[new]
    #[pyo3(signature = (asynchronous=true))]
    pub fn new(asynchronous: bool) -> Self {
        let _ = asynchronous;
        Self::with_maximum_order_quantity_percent_volume(0.01)
    }

    #[getter]
    pub fn maximum_order_quantity_percent_volume(&self) -> f64 {
        self.maximum_order_quantity_percent_volume
    }

    #[setter]
    pub fn set_maximum_order_quantity_percent_volume(&mut self, value: f64) {
        let rate = value.abs();
        self.maximum_order_quantity_percent_volume = rate;
        self.model = Some(Box::new(lean_execution::VwapExecutionModel::new(
            Decimal::from_f64(rate).unwrap_or(Decimal::ZERO),
        )));
    }

    #[staticmethod]
    pub fn with_maximum_order_quantity_percent_volume(value: f64) -> Self {
        use lean_execution::VwapExecutionModel;
        let rate_f64 = value.abs();
        let rate = Decimal::from_f64(rate_f64).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(VwapExecutionModel::new(rate))),
            maximum_order_quantity_percent_volume: rate_f64,
        }
    }
}

#[pyclass(name = "SpreadExecutionModel")]
pub struct PySpreadExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PySpreadExecutionModel {
    #[new]
    #[pyo3(signature = (accepting_spread_percent=0.005, asynchronous=true))]
    pub fn new(accepting_spread_percent: f64, asynchronous: bool) -> Self {
        use lean_execution::SpreadExecutionModel;
        let _ = asynchronous;
        let pct = Decimal::from_f64(accepting_spread_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(SpreadExecutionModel::new(pct))),
        }
    }
}

#[pyclass(name = "PassiveMakerExecutionModel")]
pub struct PyPassiveMakerExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
    max_passive_attempts: usize,
    adverse_selection_threshold: f64,
    maximum_order_value: f64,
}

#[pymethods]
impl PyPassiveMakerExecutionModel {
    #[new]
    #[pyo3(signature = (max_passive_attempts=3, adverse_selection_threshold=0.001, maximum_order_value=0.0, asynchronous=true))]
    pub fn new(
        max_passive_attempts: usize,
        adverse_selection_threshold: f64,
        maximum_order_value: f64,
        asynchronous: bool,
    ) -> Self {
        let _ = asynchronous;
        let max_passive_attempts = max_passive_attempts.max(1);
        let threshold = adverse_selection_threshold.abs();
        let maximum_order_value = maximum_order_value.abs();
        Self {
            model: Some(Box::new(
                lean_execution::PassiveMakerExecutionModel::with_maximum_order_value(
                    max_passive_attempts,
                    Decimal::from_f64(threshold).unwrap_or(Decimal::ZERO),
                    Decimal::from_f64(maximum_order_value).unwrap_or(Decimal::ZERO),
                ),
            )),
            max_passive_attempts,
            adverse_selection_threshold: threshold,
            maximum_order_value,
        }
    }

    #[getter]
    pub fn max_passive_attempts(&self) -> usize {
        self.max_passive_attempts
    }

    #[getter]
    pub fn adverse_selection_threshold(&self) -> f64 {
        self.adverse_selection_threshold
    }

    #[getter]
    pub fn maximum_order_value(&self) -> f64 {
        self.maximum_order_value
    }
}

#[pyclass(name = "AdaptiveMakerTakerExecutionModel")]
pub struct PyAdaptiveMakerTakerExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
    accepting_spread_percent: f64,
    max_passive_attempts: usize,
    adverse_selection_threshold: f64,
    passive_duration_seconds: f64,
}

#[pymethods]
impl PyAdaptiveMakerTakerExecutionModel {
    #[new]
    #[pyo3(signature = (accepting_spread_percent=0.001, max_passive_attempts=5, adverse_selection_threshold=0.005, asynchronous=true, passive_duration_seconds=None))]
    pub fn new(
        accepting_spread_percent: f64,
        max_passive_attempts: usize,
        adverse_selection_threshold: f64,
        asynchronous: bool,
        passive_duration_seconds: Option<f64>,
    ) -> Self {
        let _ = asynchronous;
        let accepting_spread_percent = accepting_spread_percent.abs();
        let max_passive_attempts = max_passive_attempts.max(1);
        let adverse_selection_threshold = adverse_selection_threshold.abs();
        let passive_duration_seconds = passive_duration_seconds
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .unwrap_or(max_passive_attempts as f64 * 60.0);
        let passive_duration =
            TimeSpan::from_millis((passive_duration_seconds * 1000.0).round() as i64);
        Self {
            model: Some(Box::new(
                AdaptiveMakerTakerExecutionModel::with_passive_duration(
                    Decimal::from_f64(accepting_spread_percent).unwrap_or(Decimal::ZERO),
                    passive_duration,
                    Decimal::from_f64(adverse_selection_threshold).unwrap_or(Decimal::ZERO),
                ),
            )),
            accepting_spread_percent,
            max_passive_attempts,
            adverse_selection_threshold,
            passive_duration_seconds,
        }
    }

    #[getter]
    pub fn accepting_spread_percent(&self) -> f64 {
        self.accepting_spread_percent
    }

    #[getter]
    pub fn max_passive_attempts(&self) -> usize {
        self.max_passive_attempts
    }

    #[getter]
    pub fn adverse_selection_threshold(&self) -> f64 {
        self.adverse_selection_threshold
    }

    #[getter]
    pub fn passive_duration_seconds(&self) -> f64 {
        self.passive_duration_seconds
    }
}

#[pyclass(name = "MakerThenTakerExecutionModel")]
pub struct PyMakerThenTakerExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
    passive_duration_seconds: f64,
    adverse_selection_threshold: f64,
    maximum_order_value: f64,
}

#[pymethods]
impl PyMakerThenTakerExecutionModel {
    #[new]
    #[pyo3(signature = (passive_duration_seconds=300.0, adverse_selection_threshold=0.005, maximum_order_value=0.0, asynchronous=true))]
    pub fn new(
        passive_duration_seconds: f64,
        adverse_selection_threshold: f64,
        maximum_order_value: f64,
        asynchronous: bool,
    ) -> Self {
        let _ = asynchronous;
        let passive_duration_seconds = passive_duration_seconds.max(0.0);
        let passive_duration =
            TimeSpan::from_millis((passive_duration_seconds * 1000.0).round() as i64);
        let adverse_selection_threshold = adverse_selection_threshold.abs();
        let maximum_order_value = maximum_order_value.abs();
        Self {
            model: Some(Box::new(
                MakerThenTakerExecutionModel::with_maximum_order_value(
                    passive_duration,
                    Decimal::from_f64(adverse_selection_threshold).unwrap_or(Decimal::ZERO),
                    Decimal::from_f64(maximum_order_value).unwrap_or(Decimal::ZERO),
                ),
            )),
            passive_duration_seconds,
            adverse_selection_threshold,
            maximum_order_value,
        }
    }

    #[getter]
    pub fn passive_duration_seconds(&self) -> f64 {
        self.passive_duration_seconds
    }

    #[getter]
    pub fn adverse_selection_threshold(&self) -> f64 {
        self.adverse_selection_threshold
    }

    #[getter]
    pub fn maximum_order_value(&self) -> f64 {
        self.maximum_order_value
    }
}

#[pyclass(name = "AggressivePostOnlyExecutionModel")]
pub struct PyAggressivePostOnlyExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
    maximum_order_value: f64,
}

#[pymethods]
impl PyAggressivePostOnlyExecutionModel {
    #[new]
    #[pyo3(signature = (maximum_order_value=0.0, asynchronous=true))]
    pub fn new(maximum_order_value: f64, asynchronous: bool) -> Self {
        let _ = asynchronous;
        let maximum_order_value = maximum_order_value.abs();
        Self {
            model: Some(Box::new(
                AggressivePostOnlyExecutionModel::with_maximum_order_value(
                    Decimal::from_f64(maximum_order_value).unwrap_or(Decimal::ZERO),
                ),
            )),
            maximum_order_value,
        }
    }

    #[getter]
    pub fn maximum_order_value(&self) -> f64 {
        self.maximum_order_value
    }
}

#[pyclass(name = "StandardDeviationExecutionModel")]
pub struct PyStandardDeviationExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
    period: usize,
    deviations: f64,
    maximum_order_value: f64,
}

#[pymethods]
impl PyStandardDeviationExecutionModel {
    /// deviations: number of std deviations from the mean required to trigger execution (default 2.0).
    /// Mirrors C# StandardDeviationExecutionModel(int period = 60, decimal deviations = 2m, ...).
    #[new]
    #[pyo3(signature = (period=60, deviations=2.0, asynchronous=true))]
    pub fn new(period: i64, deviations: f64, asynchronous: bool) -> Self {
        let _ = asynchronous;
        Self::with_maximum_order_value(period, deviations, 20000.0)
    }

    #[getter]
    pub fn maximum_order_value(&self) -> f64 {
        self.maximum_order_value
    }

    #[setter]
    pub fn set_maximum_order_value(&mut self, value: f64) {
        self.maximum_order_value = value.abs();
        self.rebuild_model();
    }

    #[staticmethod]
    pub fn with_maximum_order_value(
        period: i64,
        deviations: f64,
        maximum_order_value: f64,
    ) -> Self {
        use lean_execution::StandardDeviationExecutionModel;
        let period = usize::try_from(period).unwrap_or(60).max(1);
        let devs = Decimal::from_f64(deviations).unwrap_or(Decimal::from(2));
        let max_order_value = maximum_order_value.abs();
        Self {
            model: Some(Box::new(
                StandardDeviationExecutionModel::with_maximum_order_value(
                    period,
                    devs,
                    Decimal::from_f64(max_order_value).unwrap_or(Decimal::ZERO),
                ),
            )),
            period,
            deviations,
            maximum_order_value: max_order_value,
        }
    }
}

impl PyStandardDeviationExecutionModel {
    fn rebuild_model(&mut self) {
        use lean_execution::StandardDeviationExecutionModel;
        let devs = Decimal::from_f64(self.deviations).unwrap_or(Decimal::from(2));
        let max_order_value = Decimal::from_f64(self.maximum_order_value).unwrap_or(Decimal::ZERO);
        self.model = Some(Box::new(
            StandardDeviationExecutionModel::with_maximum_order_value(
                self.period,
                devs,
                max_order_value,
            ),
        ));
    }
}

// ─── Python Risk Model Wrappers ───────────────────────────────────────────────

#[pyclass(name = "NullRiskManagementModel")]
pub struct PyNullRiskManagementModel {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyNullRiskManagementModel {
    #[new]
    pub fn new() -> Self {
        Self {
            model: Some(Box::new(NullRiskManagement)),
        }
    }
}

impl Default for PyNullRiskManagementModel {
    fn default() -> Self {
        Self::new()
    }
}

#[pyclass(name = "MaximumDrawdownPercentPerSecurity")]
pub struct PyMaxDrawdownPercentPerSecurity {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxDrawdownPercentPerSecurity {
    #[new]
    #[pyo3(signature = (maximum_drawdown_percent=0.05))]
    pub fn new(maximum_drawdown_percent: f64) -> Self {
        use lean_risk::MaximumDrawdownPercentPerSecurity;
        let pct = Decimal::from_f64(maximum_drawdown_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumDrawdownPercentPerSecurity::new(pct))),
        }
    }
}

#[pyclass(name = "TrailingStopRiskManagementModel")]
pub struct PyTrailingStopRiskModel {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyTrailingStopRiskModel {
    #[new]
    #[pyo3(signature = (trailing_amount=0.05))]
    pub fn new(trailing_amount: f64) -> Self {
        use lean_risk::TrailingStopRiskManagementModel;
        let pct = Decimal::from_f64(trailing_amount).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(TrailingStopRiskManagementModel::new(pct))),
        }
    }
}

#[pyclass(name = "MaximumSectorExposureRiskManagementModel")]
pub struct PyMaxSectorExposureRiskModel {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxSectorExposureRiskModel {
    #[new]
    #[pyo3(signature = (maximum_sector_exposure=0.20))]
    pub fn new(maximum_sector_exposure: f64) -> Self {
        let pct = Decimal::from_f64(maximum_sector_exposure).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumSectorExposureRiskManagementModel::new(pct))),
        }
    }
}

#[pyclass(name = "MaximumDrawdownPercentPortfolio")]
pub struct PyMaxDrawdownPercentPortfolio {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxDrawdownPercentPortfolio {
    #[new]
    #[pyo3(signature = (maximum_drawdown_percent=0.05, is_trailing=false))]
    pub fn new(maximum_drawdown_percent: f64, is_trailing: bool) -> Self {
        let pct = Decimal::from_f64(maximum_drawdown_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumDrawdownPercentPortfolio::new(
                pct,
                is_trailing,
            ))),
        }
    }
}

#[pyclass(name = "MaximumUnrealizedProfitPercentPerSecurity")]
pub struct PyMaxUnrealizedProfitPerSecurity {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxUnrealizedProfitPerSecurity {
    #[new]
    #[pyo3(signature = (maximum_unrealized_profit_percent=0.05))]
    pub fn new(maximum_unrealized_profit_percent: f64) -> Self {
        let pct = Decimal::from_f64(maximum_unrealized_profit_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumUnrealizedProfitPercentPerSecurity::new(
                pct,
            ))),
        }
    }
}

// ─── Insight types exposed to Python ─────────────────────────────────────────

#[pyclass(name = "InsightDirection", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyInsightDirection {
    Up = 1,
    Flat = 0,
    Down = -1,
}

/// LEAN-style insight manager exposed as ``algorithm.Insights``.
///
/// Holds a per-bar snapshot of the framework's scored insight collection
/// (active insights plus retained closed history) so PCMs can call
/// ``GetInsights()`` / ``GetActiveInsights()`` and read direction/magnitude
/// scores — mirroring C# Lean's ``InsightManager``.
#[pyclass(name = "InsightManager")]
pub struct PyInsightManager {
    pub insights: Vec<lean_alpha::Insight>,
    pub utc_now: lean_core::DateTime,
}

#[pymethods]
impl PyInsightManager {
    /// All insights (active + closed history) with scores; optional
    /// Python predicate ``filter(insight) -> bool``.
    #[pyo3(signature = (filter=None))]
    #[allow(non_snake_case)]
    pub fn GetInsights(
        &self,
        py: Python<'_>,
        filter: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let mut out: Vec<Py<PyAny>> = Vec::with_capacity(self.insights.len());
        for i in &self.insights {
            let obj = Py::new(py, PyInsight::from_alpha_insight(i))?.into_any();
            let keep = match filter {
                Some(f) => f.call1((obj.bind(py),))?.is_truthy().unwrap_or(true),
                None => true,
            };
            if keep {
                out.push(obj);
            }
        }
        Ok(pyo3::types::PyList::new(py, out)?.into_any().unbind())
    }

    #[pyo3(signature = (filter=None))]
    pub fn get_insights(
        &self,
        py: Python<'_>,
        filter: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.GetInsights(py, filter)
    }

    /// Currently-active insights (not yet closed).
    #[pyo3(signature = (_utc=None))]
    #[allow(non_snake_case)]
    pub fn GetActiveInsights(
        &self,
        py: Python<'_>,
        _utc: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let out: Vec<Py<PyAny>> = self
            .insights
            .iter()
            .filter(|i| i.is_active(self.utc_now))
            .map(|i| Py::new(py, PyInsight::from_alpha_insight(i)).map(|p| p.into_any()))
            .collect::<PyResult<Vec<_>>>()?;
        Ok(pyo3::types::PyList::new(py, out)?.into_any().unbind())
    }

    #[pyo3(signature = (_utc=None))]
    pub fn get_active_insights(
        &self,
        py: Python<'_>,
        _utc: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.GetActiveInsights(py, _utc)
    }
}

/// A single alpha signal.  Mirrors C# `Insight`.
#[pyclass(name = "Insight")]
pub struct PyInsight {
    pub id: u64,
    pub symbol: lean_core::Symbol,
    pub direction: PyInsightDirection,
    /// Insight validity in nanoseconds.
    pub period_nanos: i64,
    pub magnitude: Option<f64>,
    pub confidence: Option<f64>,
    pub source_model: String,
    /// Direction-accuracy score in [0,1] (None until scored). Mirrors C# Score.Direction.
    pub score_direction: Option<f64>,
    /// Magnitude-accuracy score in [0,1] (None until scored). Mirrors C# Score.Magnitude.
    pub score_magnitude: Option<f64>,
    pub is_final_score: bool,
    pub generated_time_utc: lean_core::DateTime,
    pub close_time_utc: lean_core::DateTime,
}

impl PyInsight {
    /// Build a Python insight (with scores) from a framework `lean_alpha::Insight`.
    pub fn from_alpha_insight(i: &lean_alpha::Insight) -> Self {
        let direction = match i.direction {
            lean_alpha::InsightDirection::Up => PyInsightDirection::Up,
            lean_alpha::InsightDirection::Down => PyInsightDirection::Down,
            lean_alpha::InsightDirection::Flat => PyInsightDirection::Flat,
        };
        use rust_decimal::prelude::ToPrimitive;
        Self {
            id: i.id,
            symbol: i.symbol.clone(),
            direction,
            period_nanos: i.period.nanos,
            magnitude: i.magnitude.and_then(|m| m.to_f64()),
            confidence: i.confidence.and_then(|c| c.to_f64()),
            source_model: i.source_model.clone(),
            score_direction: i.score_direction,
            score_magnitude: i.score_magnitude,
            is_final_score: i.is_final_score,
            generated_time_utc: i.generated_time_utc,
            close_time_utc: i.close_time_utc,
        }
    }
}

#[pymethods]
impl PyInsight {
    /// ``Insight(symbol, InsightDirection.Up, timedelta(days=1))``
    #[new]
    #[pyo3(signature = (symbol, direction, period, magnitude=None, confidence=None, source_model=None))]
    pub fn new(
        symbol: &Bound<'_, PyAny>,
        direction: PyInsightDirection,
        period: &Bound<'_, PyAny>,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
    ) -> PyResult<Self> {
        use crate::py_types::PySymbol;
        let lean_symbol = if let Ok(s) = symbol.cast::<PySymbol>() {
            s.borrow().inner.clone()
        } else {
            let s: String = symbol.extract()?;
            lean_core::Symbol::create_equity(&s, &lean_core::Market::usa())
        };

        // period can be datetime.timedelta or a plain float/int (interpreted as days).
        let nanos: i64 = if let Ok(days_float) = period.extract::<f64>() {
            (days_float * 86_400.0 * 1_000_000_000.0) as i64
        } else {
            let days: f64 = period.getattr("days")?.extract()?;
            let secs: f64 = period.getattr("seconds")?.extract().unwrap_or(0.0);
            let micros: f64 = period.getattr("microseconds")?.extract().unwrap_or(0.0);
            ((days * 86_400.0 + secs + micros / 1_000_000.0) * 1_000_000_000.0) as i64
        };

        let now = DateTime::now();
        Ok(Self {
            id: 0,
            symbol: lean_symbol,
            direction,
            period_nanos: nanos,
            magnitude,
            confidence,
            source_model: source_model.unwrap_or_default(),
            score_direction: None,
            score_magnitude: None,
            is_final_score: false,
            generated_time_utc: now,
            close_time_utc: now + TimeSpan::from_nanos(nanos),
        })
    }

    /// ``Insight.price(symbol, timedelta(days=1), InsightDirection.Up)``  (snake_case)
    #[staticmethod]
    #[pyo3(signature = (symbol, period, direction, magnitude=None, confidence=None, source_model=None, weight=None))]
    pub fn price(
        symbol: &Bound<'_, PyAny>,
        period: &Bound<'_, PyAny>,
        direction: PyInsightDirection,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
        weight: Option<f64>,
    ) -> PyResult<Self> {
        let _ = weight;
        Self::new(
            symbol,
            direction,
            period,
            magnitude,
            confidence,
            source_model,
        )
    }

    /// ``Insight.Price(symbol, timedelta(days=1), InsightDirection.Up)``  (LEAN PascalCase)
    #[staticmethod]
    #[pyo3(name = "Price", signature = (symbol, period, direction, magnitude=None, confidence=None, source_model=None, weight=None))]
    #[allow(non_snake_case)]
    pub fn Price(
        symbol: &Bound<'_, PyAny>,
        period: &Bound<'_, PyAny>,
        direction: PyInsightDirection,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
        weight: Option<f64>,
    ) -> PyResult<Self> {
        let _ = weight; // LEAN API compat — not used in rlean
        Self::new(
            symbol,
            direction,
            period,
            magnitude,
            confidence,
            source_model,
        )
    }

    #[getter]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[getter]
    pub fn symbol(&self) -> crate::py_types::PySymbol {
        crate::py_types::PySymbol {
            inner: self.symbol.clone(),
        }
    }

    #[getter]
    pub fn direction(&self) -> PyInsightDirection {
        self.direction
    }

    #[getter]
    pub fn magnitude(&self) -> Option<f64> {
        self.magnitude
    }

    #[getter]
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }

    #[getter]
    pub fn source_model(&self) -> &str {
        &self.source_model
    }

    /// Direction-accuracy score in [0,1] (None until the insight has been scored).
    #[getter]
    pub fn score_direction(&self) -> Option<f64> {
        self.score_direction
    }

    /// Magnitude-accuracy score in [0,1] (None until scored).
    #[getter]
    pub fn score_magnitude(&self) -> Option<f64> {
        self.score_magnitude
    }

    /// Convenience: combined score (currently the direction score).
    #[getter]
    pub fn score(&self) -> Option<f64> {
        self.score_direction
    }

    #[getter]
    pub fn is_final_score(&self) -> bool {
        self.is_final_score
    }

    #[getter]
    pub fn generated_time_utc(&self) -> PyResult<Py<PyAny>> {
        crate::py_qc_algorithm::ns_to_py_datetime(self.generated_time_utc.0)
    }

    #[getter]
    pub fn close_time_utc(&self) -> PyResult<Py<PyAny>> {
        crate::py_qc_algorithm::ns_to_py_datetime(self.close_time_utc.0)
    }

    #[getter]
    pub fn generated_time_utc_nanos(&self) -> i64 {
        self.generated_time_utc.0
    }

    #[getter]
    pub fn close_time_utc_nanos(&self) -> i64 {
        self.close_time_utc.0
    }

    /// Forward LEAN PascalCase attribute access to snake_case equivalents.
    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Insight' object has no attribute '{name}'"
        )))
    }
}

// ─── PortfolioTarget Python class ────────────────────────────────────────────

/// LEAN API: ``PortfolioTarget(symbol, quantity)`` / ``PortfolioTarget.Percent(algorithm, symbol, pct)``
/// Returned by Python PCM ``CreateTargets`` implementations.
#[pyclass(name = "PortfolioTarget")]
pub struct PyPortfolioTarget {
    pub symbol: lean_core::Symbol,
    /// Absolute quantity (if constructed directly).
    pub quantity: Option<f64>,
    /// Target percent of portfolio (e.g. 0.10 = 10%).
    pub percent: Option<f64>,
}

#[pymethods]
impl PyPortfolioTarget {
    #[new]
    pub fn new(symbol: &Bound<'_, PyAny>, quantity: f64) -> PyResult<Self> {
        let lean_symbol = extract_lean_symbol_from_py(symbol)?;
        Ok(Self {
            symbol: lean_symbol,
            quantity: Some(quantity),
            percent: None,
        })
    }

    /// ``PortfolioTarget.Percent(algorithm, symbol, percent)`` — deferred percent target.
    #[staticmethod]
    #[pyo3(name = "Percent")]
    #[allow(non_snake_case)]
    pub fn Percent(
        _algorithm: &Bound<'_, PyAny>,
        symbol: &Bound<'_, PyAny>,
        percent: f64,
    ) -> PyResult<Self> {
        let lean_symbol = extract_lean_symbol_from_py(symbol)?;
        Ok(Self {
            symbol: lean_symbol,
            quantity: None,
            percent: Some(percent),
        })
    }

    /// snake_case alias.
    #[staticmethod]
    pub fn percent(
        algorithm: &Bound<'_, PyAny>,
        symbol: &Bound<'_, PyAny>,
        pct: f64,
    ) -> PyResult<Self> {
        Self::Percent(algorithm, symbol, pct)
    }

    #[getter]
    fn symbol(&self) -> crate::py_types::PySymbol {
        crate::py_types::PySymbol {
            inner: self.symbol.clone(),
        }
    }

    fn __repr__(&self) -> String {
        if let Some(pct) = self.percent {
            format!(
                "PortfolioTarget('{}', {:.1}%)",
                self.symbol.value,
                pct * 100.0
            )
        } else {
            format!(
                "PortfolioTarget('{}', qty={:.0})",
                self.symbol.value,
                self.quantity.unwrap_or(0.0)
            )
        }
    }
}

fn extract_lean_symbol_from_py(symbol: &Bound<'_, PyAny>) -> PyResult<lean_core::Symbol> {
    if let Ok(s) = symbol.cast::<crate::py_types::PySymbol>() {
        return Ok(s.borrow().inner.clone());
    }
    let ticker: String = symbol.extract()?;
    Ok(lean_core::Symbol::create_equity(
        &ticker,
        &lean_core::Market::usa(),
    ))
}

// ─── Framework Base Classes (subclassable by Python strategies) ───────────────

/// Base class for Python-defined alpha models.
///
/// ```python
/// from AlgorithmImports import *
///
/// class MyAlpha(AlphaModel):
///     def update(self, algorithm, data):
///         # return a list of Insight objects
///         return [Insight(symbol, InsightDirection.Up, timedelta(days=1))
///                 for symbol in [self.spy]]
/// ```
#[pyclass(name = "AlphaModel", subclass)]
pub struct PyAlphaModelBase;

#[pymethods]
impl PyAlphaModelBase {
    /// Accept any constructor arguments so Python subclasses can add their own __init__.
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }

    /// Override in subclass — return a list of `Insight` objects.
    /// Matches LEAN's PascalCase API: ``def Update(self, algorithm, data): ...``
    #[pyo3(name = "Update")]
    #[allow(non_snake_case)]
    fn Update(&self, _algorithm: &Bound<'_, PyAny>, _data: &Bound<'_, PyAny>) -> Vec<Py<PyAny>> {
        vec![]
    }

    #[pyo3(name = "OnSecuritiesChanged")]
    #[allow(non_snake_case)]
    fn OnSecuritiesChanged(&self, _algorithm: &Bound<'_, PyAny>, _changes: &Bound<'_, PyAny>) {}
}

/// Base class for Python-defined portfolio construction models.
#[pyclass(name = "PortfolioConstructionModel", subclass)]
pub struct PyPortfolioConstructionModelBase;

#[pymethods]
impl PyPortfolioConstructionModelBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
}

/// Base class for Python-defined execution models.
#[pyclass(name = "ExecutionModel", subclass)]
pub struct PyExecutionModelBase;

#[pymethods]
impl PyExecutionModelBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
}

/// Base class for Python-defined risk management models.
#[pyclass(name = "RiskManagementModel", subclass)]
pub struct PyRiskManagementModelBase;

#[pymethods]
impl PyRiskManagementModelBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
}

// ─── Python-defined alpha model adapter ──────────────────────────────────────

/// Wraps a Python object that subclasses `AlphaModel` so it can be used as a
/// Rust `IAlphaModel`.  Calls the Python `Update(algorithm, data)` method on
/// each bar, passing the real `QCAlgorithm` instance and a `Slice` proxy.
struct PyAlphaAdapter {
    /// The Python AlphaModel subclass instance.
    obj: Py<PyAny>,
    /// The Python QCAlgorithm instance — passed as `algorithm` to `Update()`.
    alg_py: Py<PyAny>,
    /// Reusable Python Slice backing object for framework alpha updates.
    slice_proxy: Option<crate::py_data::FrameworkSliceProxy>,
}

impl IAlphaModel for PyAlphaAdapter {
    fn update(
        &mut self,
        slice: &lean_data::Slice,
        _securities: &[lean_core::Symbol],
    ) -> Vec<lean_alpha::Insight> {
        Python::attach(|py| {
            let slice_py: Py<PyAny> = match self.slice_proxy.as_mut() {
                Some(proxy) => proxy.update(py, slice),
                None => match crate::py_data::FrameworkSliceProxy::new(py) {
                    Ok(mut proxy) => {
                        let slice_py = proxy.update(py, slice);
                        self.slice_proxy = Some(proxy);
                        slice_py
                    }
                    Err(e) => {
                        tracing::warn!("PyAlphaAdapter: SliceProxy alloc error: {e}");
                        py.None()
                    }
                },
            };

            let alg = self.alg_py.bind(py);
            let result = self.obj.call_method1(py, "Update", (alg, slice_py));
            match result {
                Ok(list_obj) => extract_py_insights(py, list_obj.bind(py)),
                Err(e) => {
                    tracing::warn!("PyAlphaAdapter::Update error: {e}");
                    vec![]
                }
            }
        })
    }

    fn on_securities_changed(
        &mut self,
        added: &[lean_core::Symbol],
        removed: &[lean_core::Symbol],
    ) {
        Python::attach(|py| {
            let changes = lean_algorithm::algorithm::SecurityChanges {
                added: added.to_vec(),
                removed: removed.to_vec(),
            };
            let py_changes = crate::py_universe::PySecurityChanges::from_changes(&changes);
            let changes_obj = match Py::new(py, py_changes) {
                Ok(obj) => obj.into_any(),
                Err(e) => {
                    tracing::warn!("PyAlphaAdapter::OnSecuritiesChanged alloc error: {e}");
                    return;
                }
            };
            let alg = self.alg_py.bind(py);
            if let Err(e) = self
                .obj
                .call_method1(py, "OnSecuritiesChanged", (alg, changes_obj))
            {
                tracing::warn!("PyAlphaAdapter::OnSecuritiesChanged error: {e}");
            }
        });
    }
}

// ─── Python-defined PCM adapter ──────────────────────────────────────────────

/// Wraps a Python object that subclasses ``PortfolioConstructionModel`` so it
/// can be used as a Rust ``IPortfolioConstructionModel``.
/// Calls ``CreateTargets(algorithm, insights)`` in Python.
struct PyPcmAdapter {
    obj: Py<PyAny>,
    alg_py: Py<PyAny>,
}

impl IPortfolioConstructionModel for PyPcmAdapter {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<lean_portfolio_construction::PortfolioTarget> {
        Python::attach(|py| {
            // Build PyInsight list to pass as the `insights` argument.
            let py_insights: Vec<Py<PyAny>> = insights
                .iter()
                .map(|i| {
                    let direction = match i.direction {
                        PcmDir::Up => PyInsightDirection::Up,
                        PcmDir::Down => PyInsightDirection::Down,
                        PcmDir::Flat => PyInsightDirection::Flat,
                    };
                    let pi = PyInsight {
                        id: 0,
                        symbol: i.symbol.clone(),
                        direction,
                        period_nanos: 86_400 * 1_000_000_000,
                        magnitude: i.magnitude.and_then(|m| m.to_f64()),
                        confidence: i.confidence.and_then(|c| c.to_f64()),
                        source_model: i.source_model.clone(),
                        score_direction: None,
                        score_magnitude: None,
                        is_final_score: false,
                        generated_time_utc: DateTime::now(),
                        close_time_utc: DateTime::now() + TimeSpan::ONE_DAY,
                    };
                    Py::new(py, pi)
                        .map(|p| p.into_any())
                        .unwrap_or_else(|_| py.None())
                })
                .collect();

            let alg = self.alg_py.bind(py);
            let list = pyo3::types::PyList::new(py, py_insights).unwrap();

            match self.obj.call_method1(py, "CreateTargets", (alg, list)) {
                Ok(result) => extract_pcm_targets(py, result.bind(py), portfolio_value, prices),
                Err(e) => {
                    tracing::warn!("PyPcmAdapter::CreateTargets error: {e}");
                    vec![]
                }
            }
        })
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    fn use_all_active_insights(&self) -> bool {
        Python::attach(|py| {
            let obj = self.obj.bind(py);
            obj.getattr("GetTargetInsights").is_ok() || obj.getattr("get_target_insights").is_ok()
        })
    }
}

/// Convert a Python iterable of ``PortfolioTarget`` objects into Rust ``PortfolioTarget``s.
fn extract_pcm_targets(
    _py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    portfolio_value: Decimal,
    prices: &HashMap<String, Decimal>,
) -> Vec<lean_portfolio_construction::PortfolioTarget> {
    let Ok(iter) = obj.try_iter() else {
        return vec![];
    };
    iter.filter_map(|item| {
        let item = item.ok()?;

        // Fast path: typed PyPortfolioTarget.
        if let Ok(pt) = item.cast::<PyPortfolioTarget>() {
            let pt = pt.borrow();
            let sym = pt.symbol.clone();
            if let Some(pct) = pt.percent {
                let pct_dec = Decimal::from_f64(pct)?;
                let price = prices.get(&sym.value).copied().unwrap_or(Decimal::ONE);
                return Some(lean_portfolio_construction::PortfolioTarget::percent(
                    sym,
                    pct_dec,
                    portfolio_value,
                    price,
                ));
            }
            if let Some(qty) = pt.quantity {
                return Some(lean_portfolio_construction::PortfolioTarget::new(
                    sym,
                    Decimal::from_f64(qty)?,
                ));
            }
            return None;
        }

        // Duck-type fallback: any object with .symbol and .percent / .quantity attrs.
        let sym_attr = item.getattr("symbol").ok()?;
        let sym = if let Ok(s) = sym_attr.cast::<crate::py_types::PySymbol>() {
            s.borrow().inner.clone()
        } else {
            let ticker: String = sym_attr
                .extract()
                .or_else(|_| sym_attr.str().map(|s| s.to_string()))
                .ok()?;
            lean_core::Symbol::create_equity(&ticker, &lean_core::Market::usa())
        };

        if let Ok(pct_attr) = item.getattr("percent").or_else(|_| item.getattr("Percent")) {
            if let Ok(pct) = pct_attr.extract::<f64>() {
                let pct_dec = Decimal::from_f64(pct)?;
                let price = prices.get(&sym.value).copied().unwrap_or(Decimal::ONE);
                return Some(lean_portfolio_construction::PortfolioTarget::percent(
                    sym,
                    pct_dec,
                    portfolio_value,
                    price,
                ));
            }
        }
        if let Ok(qty_attr) = item
            .getattr("quantity")
            .or_else(|_| item.getattr("Quantity"))
        {
            if let Ok(qty) = qty_attr.extract::<f64>() {
                return Some(lean_portfolio_construction::PortfolioTarget::new(
                    sym,
                    Decimal::from_f64(qty)?,
                ));
            }
        }
        None
    })
    .collect()
}

/// Convert a Python iterable of `Insight` objects into Rust `Insight`s.
fn extract_py_insights(_py: Python<'_>, obj: &Bound<'_, PyAny>) -> Vec<lean_alpha::Insight> {
    let Ok(iter) = obj.try_iter() else {
        return vec![];
    };
    let mut out = Vec::new();
    for item in iter {
        let Ok(item) = item else { continue };
        if let Ok(pi) = item.cast::<PyInsight>() {
            let pi = pi.borrow();
            let period = lean_core::TimeSpan::from_nanos(pi.period_nanos);
            let dir = match pi.direction {
                PyInsightDirection::Up => lean_alpha::InsightDirection::Up,
                PyInsightDirection::Down => lean_alpha::InsightDirection::Down,
                PyInsightDirection::Flat => lean_alpha::InsightDirection::Flat,
            };
            let mag = pi.magnitude.and_then(Decimal::from_f64);
            let conf = pi.confidence.and_then(Decimal::from_f64);
            out.push(lean_alpha::Insight::new(
                pi.symbol.clone(),
                dir,
                period,
                mag,
                conf,
                &pi.source_model,
            ));
        } else {
            // Attempt duck-type extraction: symbol str + direction int + period timedelta
            let sym_obj = item.getattr("symbol").ok();
            let dir_obj = item.getattr("direction").ok();
            let per_obj = item.getattr("period").ok();
            if let (Some(sym), Some(dir_o), Some(per)) = (sym_obj, dir_obj, per_obj) {
                let symbol_str: String = sym
                    .extract()
                    .or_else(|_| sym.str().map(|s| s.to_string()))
                    .unwrap_or_default();
                if symbol_str.is_empty() {
                    continue;
                }
                let symbol =
                    lean_core::Symbol::create_equity(&symbol_str, &lean_core::Market::usa());
                let dir_val: i32 = dir_o.extract().unwrap_or(1);
                let dir = if dir_val > 0 {
                    lean_alpha::InsightDirection::Up
                } else if dir_val < 0 {
                    lean_alpha::InsightDirection::Down
                } else {
                    lean_alpha::InsightDirection::Flat
                };
                let days: f64 = per.getattr("days").and_then(|d| d.extract()).unwrap_or(1.0);
                let period =
                    lean_core::TimeSpan::from_nanos((days * 86_400.0 * 1_000_000_000.0) as i64);
                out.push(lean_alpha::Insight::new(
                    symbol, dir, period, None, None, "",
                ));
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use lean_algorithm::qc_algorithm::QcAlgorithm;
    use lean_core::{Market, Resolution};
    use lean_data::quote_bar::{Bar, QuoteBar};
    use rust_decimal_macros::dec;

    struct OneShotAlpha {
        symbol: Symbol,
        emitted: bool,
    }

    impl IAlphaModel for OneShotAlpha {
        fn update(
            &mut self,
            _slice: &lean_data::Slice,
            _securities: &[Symbol],
        ) -> Vec<lean_alpha::Insight> {
            if self.emitted {
                Vec::new()
            } else {
                self.emitted = true;
                vec![lean_alpha::Insight::new(
                    self.symbol.clone(),
                    AlphaDir::Up,
                    TimeSpan::from_mins(30),
                    None,
                    None,
                    "one-shot",
                )]
            }
        }
    }

    struct SilentAlpha;

    impl IAlphaModel for SilentAlpha {
        fn update(
            &mut self,
            _slice: &lean_data::Slice,
            _securities: &[Symbol],
        ) -> Vec<lean_alpha::Insight> {
            Vec::new()
        }
    }

    struct CountingPcm {
        symbol: Symbol,
        calls: Arc<Mutex<usize>>,
    }

    impl IPortfolioConstructionModel for CountingPcm {
        fn create_targets(
            &mut self,
            insights: &[InsightForPcm],
            _portfolio_value: Decimal,
            _prices: &HashMap<String, Decimal>,
        ) -> Vec<lean_portfolio_construction::PortfolioTarget> {
            *self.calls.lock().unwrap() += 1;
            assert!(!insights.is_empty());
            vec![lean_portfolio_construction::PortfolioTarget::new(
                self.symbol.clone(),
                dec!(10),
            )]
        }

        fn rebalance_period(&self) -> Option<TimeSpan> {
            Some(TimeSpan::ONE_DAY)
        }
    }

    struct RecordingPricePcm {
        symbol: Symbol,
        seen_price: Arc<Mutex<Option<Decimal>>>,
    }

    impl IPortfolioConstructionModel for RecordingPricePcm {
        fn create_targets(
            &mut self,
            _insights: &[InsightForPcm],
            _portfolio_value: Decimal,
            prices: &HashMap<String, Decimal>,
        ) -> Vec<lean_portfolio_construction::PortfolioTarget> {
            *self.seen_price.lock().unwrap() = prices.get(&self.symbol.value).copied();
            Vec::new()
        }
    }

    struct RecordingExecution {
        target_counts: Arc<Mutex<Vec<usize>>>,
    }

    type CapturedExecutionTargets = Arc<Mutex<Vec<(String, Decimal, Option<Decimal>)>>>;

    impl IExecutionModel for RecordingExecution {
        fn execute_with_context(
            &mut self,
            targets: &[ExecutionTarget],
            _context: &ExecutionContext<'_>,
        ) -> Vec<OrderRequest> {
            self.target_counts.lock().unwrap().push(targets.len());
            Vec::new()
        }
    }

    struct MutatingAlpha {
        alg_inner: Arc<Mutex<QcAlgorithm>>,
        symbol: Symbol,
        emitted: bool,
    }

    impl IAlphaModel for MutatingAlpha {
        fn update(
            &mut self,
            _slice: &lean_data::Slice,
            _securities: &[Symbol],
        ) -> Vec<lean_alpha::Insight> {
            if self.emitted {
                return Vec::new();
            }
            self.emitted = true;
            {
                let mut alg = self.alg_inner.lock().unwrap();
                let symbol = alg.add_equity(&self.symbol.value, Resolution::Minute);
                alg.securities.update_price(&symbol, dec!(5));
            }
            vec![lean_alpha::Insight::new(
                self.symbol.clone(),
                AlphaDir::Up,
                TimeSpan::from_mins(30),
                None,
                None,
                "mutating-alpha",
            )]
        }
    }

    struct CapturingExecution {
        targets: CapturedExecutionTargets,
    }

    impl IExecutionModel for CapturingExecution {
        fn execute_with_context(
            &mut self,
            targets: &[ExecutionTarget],
            context: &ExecutionContext<'_>,
        ) -> Vec<OrderRequest> {
            let mut captured = self.targets.lock().unwrap();
            for target in targets {
                captured.push((
                    target.symbol.value.clone(),
                    target.quantity,
                    context
                        .security(&target.symbol)
                        .map(|security| security.price),
                ));
            }
            Vec::new()
        }
    }

    struct EmptyRiskOverrides;

    impl RiskManagementModel for EmptyRiskOverrides {
        fn manage_risk(&mut self, _targets: &[RiskTarget]) -> Vec<RiskTarget> {
            Vec::new()
        }
    }

    struct CancelingRiskOverride {
        symbol: Symbol,
    }

    impl RiskManagementModel for CancelingRiskOverride {
        fn manage_risk(&mut self, _targets: &[RiskTarget]) -> Vec<RiskTarget> {
            vec![RiskTarget::new(self.symbol.clone(), Decimal::ZERO)]
        }

        fn canceled_insights(&mut self) -> Vec<Symbol> {
            vec![self.symbol.clone()]
        }
    }

    fn security_data(symbol: Symbol, time: lean_core::DateTime) -> SecurityData {
        SecurityData {
            symbol,
            price: dec!(100),
            bid: Some(dec!(99)),
            ask: Some(dec!(101)),
            volume: None,
            vwap_price: None,
            average_volume: None,
            daily_std_dev: None,
            end_time: Some(time),
            lot_size: dec!(1),
            minimum_price_variation: dec!(0.01),
            current_quantity: Decimal::ZERO,
            open_order_quantity: Decimal::ZERO,
        }
    }

    fn empty_risk_context() -> RiskContext {
        RiskContext {
            total_portfolio_value: dec!(100000),
            holdings: Vec::new(),
        }
    }

    #[test]
    fn active_insights_do_not_drive_pcm_between_alpha_updates() {
        let symbol = Symbol::create_equity("SPY", &lean_core::Market::usa());
        let pcm_calls = Arc::new(Mutex::new(0usize));
        let execution_target_counts = Arc::new(Mutex::new(Vec::new()));
        let mut framework = FrameworkState::new();
        framework.alpha_models.push(Box::new(OneShotAlpha {
            symbol: symbol.clone(),
            emitted: false,
        }));
        framework.pcm = Box::new(CountingPcm {
            symbol: symbol.clone(),
            calls: pcm_calls.clone(),
        });
        framework.exec_model = Box::new(RecordingExecution {
            target_counts: execution_target_counts.clone(),
        });

        let securities = vec![symbol.clone()];
        let prices = HashMap::from([(symbol.value.clone(), dec!(100))]);
        let first_security_data = HashMap::from([(
            symbol.value.clone(),
            security_data(symbol.clone(), lean_core::DateTime::from_secs(0)),
        )]);
        let second_security_data = HashMap::from([(
            symbol.value.clone(),
            security_data(symbol.clone(), lean_core::DateTime::from_secs(60)),
        )]);
        let open_orders = Vec::new();

        let first_slice = lean_data::Slice::new(lean_core::DateTime::from_secs(0));
        let first_context = ExecutionContext::new(
            first_slice.time,
            &first_security_data,
            &open_orders,
            dec!(100000),
        );
        framework.run_pipeline(
            &first_slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &first_context,
        );

        let second_slice = lean_data::Slice::new(lean_core::DateTime::from_secs(60));
        let second_context = ExecutionContext::new(
            second_slice.time,
            &second_security_data,
            &open_orders,
            dec!(100000),
        );
        framework.run_pipeline(
            &second_slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &second_context,
        );

        assert_eq!(*pcm_calls.lock().unwrap(), 1);
        assert_eq!(execution_target_counts.lock().unwrap().as_slice(), &[1]);
    }

    #[test]
    fn restored_active_insights_drive_first_pcm_rebalance() {
        let symbol = Symbol::create_equity("RKT", &lean_core::Market::usa());
        let now = lean_core::DateTime::from_secs(0);
        let mut insight = lean_alpha::Insight::new(
            symbol.clone(),
            AlphaDir::Up,
            TimeSpan::from_days(1),
            None,
            None,
            "restored-alpha",
        )
        .with_generated_time_utc(now);
        insight.reference_value = Some(dec!(10));

        let pcm_calls = Arc::new(Mutex::new(0usize));
        let execution_target_counts = Arc::new(Mutex::new(Vec::new()));
        let mut framework = FrameworkState::new();
        framework.alpha_models.push(Box::new(SilentAlpha));
        framework.pcm = Box::new(CountingPcm {
            symbol: symbol.clone(),
            calls: pcm_calls.clone(),
        });
        framework.exec_model = Box::new(RecordingExecution {
            target_counts: execution_target_counts.clone(),
        });
        framework.restore_insights(
            InsightCollectionSnapshot {
                active: vec![insight],
                closed: Vec::new(),
                total_count: 1,
            },
            now,
        );

        let restore_events = framework.take_insight_events();
        assert_eq!(restore_events.len(), 1);
        assert_eq!(restore_events[0].kind, InsightEventKind::Restored);

        let securities = vec![symbol.clone()];
        let prices = HashMap::from([(symbol.value.clone(), dec!(11))]);
        let security_data =
            HashMap::from([(symbol.value.clone(), security_data(symbol.clone(), now))]);
        let open_orders = Vec::new();
        let slice = lean_data::Slice::new(now);
        let execution_context =
            ExecutionContext::new(slice.time, &security_data, &open_orders, dec!(100000));

        framework.run_pipeline(
            &slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &execution_context,
        );

        assert_eq!(*pcm_calls.lock().unwrap(), 1);
        assert_eq!(execution_target_counts.lock().unwrap().as_slice(), &[1]);
    }

    #[test]
    fn restored_insights_expire_against_slice_time_not_wall_clock() {
        let symbol = Symbol::create_equity("SPY", &lean_core::Market::usa());
        let generated = lean_core::DateTime::from_secs(0);
        let mut insight = lean_alpha::Insight::new(
            symbol.clone(),
            AlphaDir::Up,
            TimeSpan::from_mins(2),
            None,
            None,
            "restored-alpha",
        )
        .with_generated_time_utc(generated);
        insight.reference_value = Some(dec!(100));

        let pcm_calls = Arc::new(Mutex::new(0usize));
        let execution_target_counts = Arc::new(Mutex::new(Vec::new()));
        let mut framework = FrameworkState::new();
        framework.alpha_models.push(Box::new(SilentAlpha));
        framework.pcm = Box::new(CountingPcm {
            symbol: symbol.clone(),
            calls: pcm_calls.clone(),
        });
        framework.exec_model = Box::new(RecordingExecution {
            target_counts: execution_target_counts.clone(),
        });
        framework.restore_insights(
            InsightCollectionSnapshot {
                active: vec![insight],
                closed: Vec::new(),
                total_count: 1,
            },
            generated + TimeSpan::from_days(1),
        );

        let restore_events = framework.take_insight_events();
        assert_eq!(restore_events.len(), 1);
        assert_eq!(restore_events[0].kind, InsightEventKind::Restored);

        let slice_time = generated + TimeSpan::from_mins(1);
        let securities = vec![symbol.clone()];
        let prices = HashMap::from([(symbol.value.clone(), dec!(101))]);
        let security_data =
            HashMap::from([(symbol.value.clone(), security_data(symbol, slice_time))]);
        let open_orders = Vec::new();
        let slice = lean_data::Slice::new(slice_time);
        let execution_context =
            ExecutionContext::new(slice.time, &security_data, &open_orders, dec!(100000));

        framework.run_pipeline(
            &slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &execution_context,
        );

        assert_eq!(*pcm_calls.lock().unwrap(), 1);
        assert_eq!(execution_target_counts.lock().unwrap().as_slice(), &[1]);
    }

    #[test]
    fn framework_records_emitted_and_flat_closed_events() {
        let symbol = Symbol::create_equity("SPY", &lean_core::Market::usa());
        let mut framework = FrameworkState::new();
        framework.alpha_models.push(Box::new(OneShotAlpha {
            symbol: symbol.clone(),
            emitted: false,
        }));
        framework.exec_model = Box::new(RecordingExecution {
            target_counts: Arc::new(Mutex::new(Vec::new())),
        });

        let securities = vec![symbol.clone()];
        let prices = HashMap::from([(symbol.value.clone(), dec!(100))]);
        let security_data = HashMap::from([(
            symbol.value.clone(),
            security_data(symbol.clone(), lean_core::DateTime::from_secs(0)),
        )]);
        let open_orders = Vec::new();
        let first_slice = lean_data::Slice::new(lean_core::DateTime::from_secs(0));
        let first_context =
            ExecutionContext::new(first_slice.time, &security_data, &open_orders, dec!(100000));
        framework.run_pipeline(
            &first_slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &first_context,
        );

        let emitted = framework.take_insight_events();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].kind, InsightEventKind::Emitted);

        let second_slice = lean_data::Slice::new(lean_core::DateTime::from_secs(60));
        let second_context = ExecutionContext::new(
            second_slice.time,
            &security_data,
            &open_orders,
            dec!(100000),
        );
        let flat = lean_alpha::Insight::new(
            symbol.clone(),
            AlphaDir::Flat,
            TimeSpan::from_mins(1),
            None,
            None,
            "one-shot",
        )
        .with_generated_time_utc(second_slice.time);
        framework.run_pipeline_from_alpha(
            &second_slice,
            vec![flat],
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &second_context,
        );

        let flat_closed = framework.take_insight_events();
        assert!(
            flat_closed
                .iter()
                .any(|event| event.kind == InsightEventKind::FlatClosed),
            "flat insight should close the active source insight"
        );
    }

    #[test]
    fn empty_risk_overrides_do_not_drop_pcm_targets() {
        let symbol = Symbol::create_equity("SPY", &lean_core::Market::usa());
        let pcm_calls = Arc::new(Mutex::new(0usize));
        let execution_target_counts = Arc::new(Mutex::new(Vec::new()));
        let mut framework = FrameworkState::new();
        framework.alpha_models.push(Box::new(OneShotAlpha {
            symbol: symbol.clone(),
            emitted: false,
        }));
        framework.pcm = Box::new(CountingPcm {
            symbol: symbol.clone(),
            calls: pcm_calls.clone(),
        });
        framework.risk_model = Box::new(EmptyRiskOverrides);
        framework.exec_model = Box::new(RecordingExecution {
            target_counts: execution_target_counts.clone(),
        });

        let securities = vec![symbol.clone()];
        let prices = HashMap::from([(symbol.value.clone(), dec!(100))]);
        let security_data = HashMap::from([(
            symbol.value.clone(),
            security_data(symbol.clone(), lean_core::DateTime::from_secs(0)),
        )]);
        let open_orders = Vec::new();
        let slice = lean_data::Slice::new(lean_core::DateTime::from_secs(0));
        let execution_context =
            ExecutionContext::new(slice.time, &security_data, &open_orders, dec!(100000));

        framework.run_pipeline(
            &slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &execution_context,
        );

        assert_eq!(*pcm_calls.lock().unwrap(), 1);
        assert_eq!(execution_target_counts.lock().unwrap().as_slice(), &[1]);
    }

    #[test]
    fn risk_cancellation_clears_active_insights() {
        let symbol = Symbol::create_equity("SPY", &lean_core::Market::usa());
        let pcm_calls = Arc::new(Mutex::new(0usize));
        let execution_target_counts = Arc::new(Mutex::new(Vec::new()));
        let mut framework = FrameworkState::new();
        framework.alpha_models.push(Box::new(OneShotAlpha {
            symbol: symbol.clone(),
            emitted: false,
        }));
        framework.pcm = Box::new(CountingPcm {
            symbol: symbol.clone(),
            calls: pcm_calls.clone(),
        });
        framework.risk_model = Box::new(CancelingRiskOverride {
            symbol: symbol.clone(),
        });
        framework.exec_model = Box::new(RecordingExecution {
            target_counts: execution_target_counts.clone(),
        });

        let securities = vec![symbol.clone()];
        let prices = HashMap::from([(symbol.value.clone(), dec!(100))]);
        let open_orders = Vec::new();

        let first_security_data = HashMap::from([(
            symbol.value.clone(),
            security_data(symbol.clone(), lean_core::DateTime::from_secs(0)),
        )]);
        let first_slice = lean_data::Slice::new(lean_core::DateTime::from_secs(0));
        let first_execution_context = ExecutionContext::new(
            first_slice.time,
            &first_security_data,
            &open_orders,
            dec!(100000),
        );
        framework.run_pipeline(
            &first_slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &first_execution_context,
        );

        assert!(!framework.insights.has_active(&symbol, first_slice.time));

        let second_security_data = HashMap::from([(
            symbol.value.clone(),
            security_data(symbol.clone(), lean_core::DateTime::from_secs(86_400)),
        )]);
        let second_slice = lean_data::Slice::new(lean_core::DateTime::from_secs(86_400));
        let second_execution_context = ExecutionContext::new(
            second_slice.time,
            &second_security_data,
            &open_orders,
            dec!(100000),
        );
        framework.run_pipeline(
            &second_slice,
            &securities,
            dec!(100000),
            &prices,
            &empty_risk_context(),
            &second_execution_context,
        );

        assert_eq!(*pcm_calls.lock().unwrap(), 1);
        assert_eq!(execution_target_counts.lock().unwrap().as_slice(), &[1, 0]);
    }

    #[test]
    fn framework_percent_targets_use_current_slice_price_snapshot() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut algorithm = QcAlgorithm::new("test", dec!(100000));
        algorithm.add_equity("SPY", Resolution::Minute);
        algorithm.securities.update_price(&symbol, dec!(1));

        let framework = Arc::new(Mutex::new(FrameworkState::new()));
        let seen_price = Arc::new(Mutex::new(None));
        {
            let mut fw = framework.lock().unwrap();
            fw.alpha_models.push(Box::new(OneShotAlpha {
                symbol: symbol.clone(),
                emitted: false,
            }));
            fw.pcm = Box::new(RecordingPricePcm {
                symbol: symbol.clone(),
                seen_price: seen_price.clone(),
            });
            fw.exec_model = Box::new(RecordingExecution {
                target_counts: Arc::new(Mutex::new(Vec::new())),
            });
        }

        let mut slice = lean_data::Slice::new(lean_core::DateTime::from_secs(0));
        slice.add_quote_bar(QuoteBar::new(
            symbol.clone(),
            lean_core::DateTime::from_secs(0),
            TimeSpan::from_mins(1),
            Some(Bar::new(dec!(49), dec!(49), dec!(49), dec!(49))),
            Some(Bar::new(dec!(51), dec!(51), dec!(51), dec!(51))),
            Decimal::ZERO,
            Decimal::ZERO,
        ));

        let algorithm = Arc::new(Mutex::new(algorithm));
        run_framework_pipeline(&framework, &algorithm, &slice);

        assert_eq!(*seen_price.lock().unwrap(), Some(dec!(50)));
    }

    #[test]
    fn framework_pipeline_uses_security_seeded_during_alpha_update() {
        let symbol = Symbol::create_equity("NIO", &Market::usa());
        let algorithm = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let captured_targets = Arc::new(Mutex::new(Vec::new()));
        let mut framework = FrameworkState::new();
        framework.alpha_models.push(Box::new(MutatingAlpha {
            alg_inner: algorithm.clone(),
            symbol: symbol.clone(),
            emitted: false,
        }));
        framework.exec_model = Box::new(CapturingExecution {
            targets: captured_targets.clone(),
        });
        let framework = Arc::new(Mutex::new(framework));
        let slice = lean_data::Slice::new(lean_core::DateTime::from_secs(0));

        run_framework_pipeline(&framework, &algorithm, &slice);

        let targets = captured_targets.lock().unwrap();
        assert_eq!(
            targets.as_slice(),
            &[("NIO".to_string(), dec!(20000), Some(dec!(5)))]
        );
    }

    #[test]
    fn python_pcm_get_target_insights_requests_all_active_insights() {
        crate::test_python::init();

        Python::attach(|py| {
            let locals = PyDict::new(py);
            py.run(
                c"
class LatestOnlyPcm:
    def CreateTargets(self, algorithm, insights):
        return []

class AllActivePcm:
    def CreateTargets(self, algorithm, insights):
        return []

    def GetTargetInsights(self):
        return []

latest_only = LatestOnlyPcm()
all_active = AllActivePcm()
",
                None,
                Some(&locals),
            )
            .unwrap();

            let latest_only = locals.get_item("latest_only").unwrap().unwrap();
            let all_active = locals.get_item("all_active").unwrap().unwrap();
            let latest_only_adapter = PyPcmAdapter {
                obj: latest_only.clone().unbind(),
                alg_py: py.None(),
            };
            let all_active_adapter = PyPcmAdapter {
                obj: all_active.clone().unbind(),
                alg_py: py.None(),
            };

            assert!(!latest_only_adapter.use_all_active_insights());
            assert!(all_active_adapter.use_all_active_insights());
        });
    }
}

// ─── Extraction helpers ───────────────────────────────────────────────────────

/// Try to extract an IAlphaModel Box from a Python object.
/// `alg_py` is the Python QCAlgorithm instance — passed as `algorithm` to `Update()`.
pub fn try_take_alpha(model: &Bound<'_, PyAny>, alg_py: Py<PyAny>) -> Option<Box<dyn IAlphaModel>> {
    if let Ok(m) = model.cast::<PyEmaCrossAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMacdAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyRsiAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyConstantAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyHistoricalReturnsAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyPearsonCorrelationPairsTradingAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    // Accept any Python object that subclasses AlphaModel.
    if model.is_instance_of::<PyAlphaModelBase>() {
        return Some(Box::new(PyAlphaAdapter {
            obj: model.clone().unbind(),
            alg_py,
            slice_proxy: None,
        }));
    }
    tracing::warn!("add_alpha: unrecognized model type — use a built-in AlphaModel class");
    None
}

/// Try to extract an IPortfolioConstructionModel Box from a Python object.
pub fn try_take_pcm(
    model: &Bound<'_, PyAny>,
    alg_py: Py<PyAny>,
) -> Option<Box<dyn IPortfolioConstructionModel>> {
    if let Ok(m) = model.cast::<PyEqualWeightingPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyInsightWeightingPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMeanVariancePcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMaxSharpeRatioPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyBlackLittermanPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyRiskParityPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyConfidenceWeightingPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyAccumulativeInsightPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMeanReversionPcm>() {
        return m.borrow_mut().model.take();
    }
    // Accept any Python object that subclasses PortfolioConstructionModel.
    if model.is_instance_of::<PyPortfolioConstructionModelBase>() {
        return Some(Box::new(PyPcmAdapter {
            obj: model.clone().unbind(),
            alg_py,
        }));
    }
    tracing::warn!(
        "set_portfolio_construction: unrecognized model type — subclass PortfolioConstructionModel or use a built-in PCM"
    );
    None
}

/// Try to extract an IExecutionModel Box from a Python object.
pub fn try_take_exec(model: &Bound<'_, PyAny>) -> Option<Box<dyn IExecutionModel>> {
    if let Ok(m) = model.cast::<PyImmediateExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyNullExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyVwapExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PySpreadExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyPassiveMakerExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyAdaptiveMakerTakerExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMakerThenTakerExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyAggressivePostOnlyExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyStandardDeviationExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    tracing::warn!("set_execution: unrecognized model type — use a built-in ExecutionModel class");
    None
}

/// Try to extract a RiskManagementModel Box from a Python object.
pub fn try_take_risk(model: &Bound<'_, PyAny>) -> Option<Box<dyn RiskManagementModel>> {
    if let Ok(m) = model.cast::<PyNullRiskManagementModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMaxDrawdownPercentPerSecurity>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyTrailingStopRiskModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMaxSectorExposureRiskModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMaxDrawdownPercentPortfolio>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.cast::<PyMaxUnrealizedProfitPerSecurity>() {
        return m.borrow_mut().model.take();
    }
    tracing::warn!(
        "set_risk_management: unrecognized model type — use a built-in RiskManagementModel class"
    );
    None
}
