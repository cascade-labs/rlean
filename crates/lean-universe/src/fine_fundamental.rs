use lean_core::Symbol;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Fine fundamental data for a single security — financial statement data.
/// Mirrors C# FineFundamental with the most commonly used fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FineFundamental {
    pub symbol: Option<Symbol>,

    // Valuation ratios
    pub pe_ratio: Option<Decimal>,
    pub pb_ratio: Option<Decimal>,
    pub ps_ratio: Option<Decimal>,
    pub ev_to_ebitda: Option<Decimal>,
    pub peg_ratio: Option<Decimal>,

    // Income statement
    pub revenue: Option<Decimal>,
    pub gross_profit: Option<Decimal>,
    pub operating_income: Option<Decimal>,
    pub net_income: Option<Decimal>,
    pub ebitda: Option<Decimal>,
    pub eps: Option<Decimal>,
    pub eps_growth: Option<Decimal>,

    // Balance sheet
    pub total_assets: Option<Decimal>,
    pub total_debt: Option<Decimal>,
    pub book_value_per_share: Option<Decimal>,
    pub cash_and_equivalents: Option<Decimal>,

    // Per-share metrics
    pub revenue_per_share: Option<Decimal>,
    pub free_cash_flow_per_share: Option<Decimal>,
    pub dividend_per_share: Option<Decimal>,
    pub dividend_yield: Option<Decimal>,

    // Growth
    pub revenue_growth: Option<Decimal>,
    pub earnings_growth: Option<Decimal>,

    // Quality
    pub return_on_equity: Option<Decimal>,
    pub return_on_assets: Option<Decimal>,
    pub debt_to_equity: Option<Decimal>,
    pub current_ratio: Option<Decimal>,
    pub gross_margin: Option<Decimal>,
    pub operating_margin: Option<Decimal>,
    pub net_margin: Option<Decimal>,

    // Market data
    pub market_cap: Option<Decimal>,
    pub enterprise_value: Option<Decimal>,
    pub shares_outstanding: Option<Decimal>,
    pub float_shares: Option<Decimal>,
    pub short_interest: Option<Decimal>,

    // Sector/Industry
    pub sector: Option<String>,
    pub industry: Option<String>,
    pub asset_classification_sector: Option<i32>,
    pub asset_classification_industry: Option<i32>,
}

impl FineFundamental {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol: Some(symbol),
            ..Default::default()
        }
    }
}
