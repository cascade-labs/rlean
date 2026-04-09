use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Symbol, TimeSpan};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Fundamental data point — wraps financial statement and valuation metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundamentalData {
    pub symbol: Symbol,
    pub time: DateTime,
    pub company_reference: CompanyReference,
    pub earnings_ratios: EarningsRatios,
    pub valuation_ratios: ValuationRatios,
    pub financial_statements: FinancialStatements,
    pub security_reference: SecurityReference,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompanyReference {
    pub company_id: String,
    pub short_name: String,
    pub industry_template_code: String,
    pub primary_exchange_id: String,
    pub currency_id: String,
    pub fiscal_year_end: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EarningsRatios {
    pub basic_eps_growth: Option<Decimal>,
    pub diluted_eps_growth: Option<Decimal>,
    pub equity_per_share_growth: Option<Decimal>,
    pub revenue_growth: Option<Decimal>,
    pub fcf_per_share_growth: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ValuationRatios {
    pub pe_ratio: Option<Decimal>,
    pub pb_ratio: Option<Decimal>,
    pub ps_ratio: Option<Decimal>,
    pub peg_ratio: Option<Decimal>,
    pub pcf_ratio: Option<Decimal>,
    pub dividend_yield: Option<Decimal>,
    pub ev_to_ebitda: Option<Decimal>,
    pub forward_pe: Option<Decimal>,
    pub book_value_per_share: Option<Decimal>,
    pub earnings_yield: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FinancialStatements {
    pub total_revenue: Option<Decimal>,
    pub gross_profit: Option<Decimal>,
    pub ebitda: Option<Decimal>,
    pub net_income: Option<Decimal>,
    pub total_assets: Option<Decimal>,
    pub total_liabilities: Option<Decimal>,
    pub stockholders_equity: Option<Decimal>,
    pub total_debt: Option<Decimal>,
    pub free_cash_flow: Option<Decimal>,
    pub capital_expenditure: Option<Decimal>,
    pub shares_outstanding: Option<Decimal>,
    pub shares_outstanding_with_dilution: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SecurityReference {
    pub security_type: String,
    pub exchange_id: String,
    pub currency_id: String,
    pub depositary_receipt_ratio: Option<Decimal>,
}

impl FundamentalData {
    pub fn new(symbol: Symbol, time: DateTime) -> Self {
        FundamentalData {
            symbol,
            time,
            company_reference: Default::default(),
            earnings_ratios: Default::default(),
            valuation_ratios: Default::default(),
            financial_statements: Default::default(),
            security_reference: Default::default(),
        }
    }
}

impl BaseData for FundamentalData {
    fn data_type(&self) -> BaseDataType { BaseDataType::Fundamental }
    fn symbol(&self) -> &Symbol { &self.symbol }
    fn time(&self) -> DateTime { self.time }
    fn end_time(&self) -> DateTime { self.time + TimeSpan::ONE_DAY }
    fn price(&self) -> Price { dec!(0) }
    fn clone_box(&self) -> Box<dyn BaseData> { Box::new(self.clone()) }
}
