use crate::{Market, OptionRight, OptionStyle, Symbol};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString, FromRepr};

// ---------------------------------------------------------------------------
// SettlementType
// ---------------------------------------------------------------------------

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
    Display, EnumString, EnumIter, FromRepr,
)]
#[repr(u8)]
pub enum SettlementType {
    #[strum(serialize = "PhysicalDelivery")]
    PhysicalDelivery = 0,
    #[strum(serialize = "Cash")]
    Cash = 1,
}

impl Default for SettlementType {
    fn default() -> Self {
        SettlementType::PhysicalDelivery
    }
}

// ---------------------------------------------------------------------------
// Greeks
// ---------------------------------------------------------------------------

/// Option sensitivity measures (the "Greeks").
///
/// All values are expressed in standard conventions:
/// - `delta`  ∂V/∂S  — rate of change of option price w.r.t. underlying price
/// - `gamma`  ∂²V/∂S² — rate of change of delta w.r.t. underlying price
/// - `vega`   ∂V/∂σ  — rate of change w.r.t. implied volatility (per 1-pt move)
/// - `theta`  ∂V/∂τ  — time decay per year (convert to daily via `theta_per_day`)
/// - `rho`    ∂V/∂r  — rate of change w.r.t. risk-free rate
/// - `lambda` percentage delta (delta × S / V), also called "leverage"
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Greeks {
    pub delta: Decimal,
    pub gamma: Decimal,
    pub vega: Decimal,
    pub theta: Decimal,
    pub rho: Decimal,
    pub lambda: Decimal,
}

impl Greeks {
    /// Convenience: theta expressed as daily decay (theta / 365).
    pub fn theta_per_day(&self) -> Decimal {
        self.theta / Decimal::from(365)
    }
}

// ---------------------------------------------------------------------------
// OptionSymbolId
// ---------------------------------------------------------------------------

/// Option-specific metadata stored alongside a `Symbol`.
///
/// This supplements `SecurityIdentifier` with a typed reference to the
/// underlying `Symbol` so callers don't have to re-construct it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OptionSymbolId {
    /// The equity, index, or future that this option is written on.
    pub underlying: Box<Symbol>,
    pub strike: Decimal,
    pub expiry: NaiveDate,
    pub right: OptionRight,
    pub style: OptionStyle,
}

// ---------------------------------------------------------------------------
// OSI ticker helper
// ---------------------------------------------------------------------------

/// Format an OSI-style option ticker.
///
/// Format: `{UNDERLYING}{YYMMDD}{C|P}{strike*1000:08}`
///
/// Example: SPY 450.00 Call expiring 2025-01-17 → `SPY250117C00450000`
pub fn format_option_ticker(
    underlying: &str,
    strike: Decimal,
    expiry: NaiveDate,
    right: OptionRight,
) -> String {
    let right_char = if right == OptionRight::Call { 'C' } else { 'P' };
    let strike_int = (strike * Decimal::from(1000))
        .to_u64()
        .unwrap_or(0);
    format!(
        "{}{}{}{}",
        underlying.to_uppercase(),
        expiry.format("%y%m%d"),
        right_char,
        format!("{:08}", strike_int),
    )
}

// ---------------------------------------------------------------------------
// Symbol extension methods for options
// ---------------------------------------------------------------------------

/// Extension methods on `Symbol` for option-specific construction and queries.
///
/// These are free functions rather than inherent `Symbol` methods because
/// `Symbol` lives in `symbol.rs` and we want to keep options logic here.
/// Use the `SymbolOptionsExt` trait to call them ergonomically on a `Symbol`.
pub trait SymbolOptionsExt {
    /// Create a specific (strike + expiry) option symbol in OSI ticker format.
    fn create_option_osi(
        underlying: Symbol,
        strike: Decimal,
        expiry: NaiveDate,
        right: OptionRight,
        style: OptionStyle,
        market: &Market,
    ) -> Symbol;

    /// Create a canonical (chain-level) option symbol — no specific strike/expiry.
    /// The value is `?{underlying_ticker}`, mirroring LEAN's canonical option convention.
    fn create_canonical_option(underlying: &Symbol, market: &Market) -> Symbol;

    /// Returns `true` if this symbol represents any option (vanilla, index, future).
    fn is_option(&self) -> bool;

    /// Returns `true` if this is a canonical option chain symbol (no strike/expiry).
    fn is_canonical_option(&self) -> bool;

    /// Returns the `OptionSymbolId` if one was attached at construction time.
    ///
    /// Note: canonical options and options constructed via `Symbol::create_option`
    /// (the original method in `symbol.rs`) will return `None` here because
    /// `option_symbol_id` is stored separately.  Use `id.expiry`, `id.strike`,
    /// and `id.option_right` on the underlying `SecurityIdentifier` for those.
    fn option_symbol_id(&self) -> Option<OptionSymbolId>;
}

impl SymbolOptionsExt for Symbol {
    fn create_option_osi(
        underlying: Symbol,
        strike: Decimal,
        expiry: NaiveDate,
        right: OptionRight,
        style: OptionStyle,
        market: &Market,
    ) -> Symbol {
        use crate::symbol::SecurityIdentifier;

        let id = SecurityIdentifier::generate_option(
            &underlying.permtick,
            market,
            expiry,
            strike,
            right,
            style,
        );
        let osi = format_option_ticker(&underlying.permtick, strike, expiry, right);
        Symbol {
            value: osi.clone(),
            permtick: osi,
            id,
            underlying: Some(Box::new(underlying)),
        }
    }

    fn create_canonical_option(underlying: &Symbol, market: &Market) -> Symbol {
        use crate::symbol::SecurityIdentifier;

        // Canonical option: value = "?{TICKER}", no strike/expiry.
        // The SecurityIdentifier uses the underlying ticker with Option type but
        // no expiry/strike/right/style so it hashes differently from any specific contract.
        let canonical_ticker = format!("?{}", underlying.permtick);
        let id = SecurityIdentifier::generate_option(
            &canonical_ticker,
            market,
            // Sentinel date for canonical — chrono::NaiveDate::MIN equivalent
            chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap(),
            Decimal::ZERO,
            OptionRight::Call,  // arbitrary; canonical has no right
            OptionStyle::American,
        );
        // Override security_type — generate_option already sets it to Option.
        // We create the Symbol directly so we can leave underlying = None to signal
        // "canonical" (is_canonical_option checks option_id absence via SecurityIdentifier).
        Symbol {
            value: canonical_ticker.clone(),
            permtick: canonical_ticker,
            id,
            underlying: Some(Box::new(underlying.clone())),
        }
    }

    fn is_option(&self) -> bool {
        self.id.security_type.is_option_like()
    }

    fn is_canonical_option(&self) -> bool {
        // A canonical option has the "?" prefix convention and no expiry encoded.
        self.is_option() && self.permtick.starts_with('?')
    }

    /// Always returns `None` for `Symbol` as-is; `OptionSymbolId` is a
    /// richer view that callers build themselves when needed.  See
    /// `OptionSymbolId` doc for rationale.
    fn option_symbol_id(&self) -> Option<OptionSymbolId> {
        // Build on demand from SecurityIdentifier fields when all are present.
        let sid = &self.id;
        match (sid.expiry, sid.strike, sid.option_right, sid.option_style) {
            (Some(expiry), Some(strike), Some(right), Some(style)) => {
                self.underlying.as_ref().map(|u| OptionSymbolId {
                    underlying: u.clone(),
                    strike,
                    expiry,
                    right,
                    style,
                })
            }
            _ => None,
        }
    }
}
