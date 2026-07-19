use crate::risk_management::{PortfolioTarget, RiskContext, RiskManagementModel};
use rlean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionSide {
    Long,
    Short,
}

#[derive(Debug, Clone)]
struct HoldingsState {
    position: PositionSide,
    absolute_holdings_value: Decimal,
}

pub struct TrailingStopRiskManagementModel {
    pub trailing_pct: Decimal,
    trailing_holdings_state: HashMap<u64, HoldingsState>,
    canceled_symbols: Vec<Symbol>,
}

impl TrailingStopRiskManagementModel {
    pub fn new(trailing_pct: Decimal) -> Self {
        TrailingStopRiskManagementModel {
            trailing_pct: trailing_pct.abs(),
            trailing_holdings_state: HashMap::new(),
            canceled_symbols: Vec::new(),
        }
    }
}

impl RiskManagementModel for TrailingStopRiskManagementModel {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        targets.to_vec()
    }

    fn manage_risk_with_context(
        &mut self,
        _targets: &[PortfolioTarget],
        ctx: &RiskContext,
    ) -> Vec<PortfolioTarget> {
        let mut result = Vec::new();
        let mut invested_sids = std::collections::HashSet::new();
        self.canceled_symbols.clear();

        for holding in &ctx.holdings {
            let sid = holding.symbol.id.sid;
            if !holding.is_invested() {
                self.trailing_holdings_state.remove(&sid);
                continue;
            }

            invested_sids.insert(sid);

            let position = if holding.quantity > Decimal::ZERO {
                PositionSide::Long
            } else {
                PositionSide::Short
            };
            let absolute_holdings_value = (holding.quantity * holding.last_price).abs();
            let absolute_holdings_cost = (holding.quantity * holding.average_price).abs();

            let state = self
                .trailing_holdings_state
                .entry(sid)
                .or_insert_with(|| HoldingsState {
                    position,
                    absolute_holdings_value: absolute_holdings_cost,
                });

            // Reset the high/low watermark when the holding flips from long to short
            // or vice versa, matching LEAN's `HoldingsState.Position` behavior.
            if state.position != position {
                *state = HoldingsState {
                    position,
                    absolute_holdings_value: absolute_holdings_cost,
                };
            }

            let trailing_value = state.absolute_holdings_value;
            if (position == PositionSide::Long && trailing_value < absolute_holdings_value)
                || (position == PositionSide::Short && trailing_value > absolute_holdings_value)
            {
                state.absolute_holdings_value = absolute_holdings_value;
                continue;
            }

            if trailing_value.is_zero() {
                continue;
            }

            let drawdown = ((trailing_value - absolute_holdings_value) / trailing_value).abs();
            if self.trailing_pct < drawdown {
                self.trailing_holdings_state.remove(&sid);
                self.canceled_symbols.push(holding.symbol.clone());
                result.push(PortfolioTarget::new(holding.symbol.clone(), Decimal::ZERO));
            }
        }

        self.trailing_holdings_state
            .retain(|sid, _| invested_sids.contains(sid));

        result
    }

    fn canceled_insights(&mut self) -> Vec<Symbol> {
        std::mem::take(&mut self.canceled_symbols)
    }
}
