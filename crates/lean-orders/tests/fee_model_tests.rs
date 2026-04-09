use lean_core::{Market, NanosecondTimestamp, SecurityType, Symbol};
use lean_orders::{
    AlpacaFeeModel, BinanceFeeModel, BybitFeeModel, CharlesSchwabFeeModel, ConstantFeeModel,
    EtradeFeeModel, ExchangeFeeModel, FidelityFeeModel, FlatFeeModel, FxcmFeeModel, GDAXFeeModel,
    InteractiveBrokersFeeModel, KrakenFeeModel, NullFeeModel, OandaFeeModel, Order, OrderType,
    PercentFeeModel, RobinhoodFeeModel, TDAmeritradeFeeModel, TradierFeeModel, ZeroFeeModel,
    FeeModel,
};
use lean_orders::fee_model::OrderFeeParameters;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn forex_sym(ticker: &str) -> Symbol {
    Symbol::create_forex(ticker)
}

fn crypto_sym(ticker: &str) -> Symbol {
    Symbol::create_crypto(ticker, &Market::new("binance"))
}

/// Build a basic equity OrderFeeParameters — convenience wrapper.
fn equity_params<'a>(order: &'a Order, price: Decimal) -> OrderFeeParameters<'a> {
    OrderFeeParameters::equity(order, price)
}

/// Build full OrderFeeParameters for non-equity security types.
fn params_full<'a>(
    order: &'a Order,
    price: Decimal,
    security_type: SecurityType,
    quote_currency: &str,
    base_currency: Option<String>,
    contract_multiplier: Decimal,
) -> OrderFeeParameters<'a> {
    OrderFeeParameters {
        order,
        security_price: price,
        security_type,
        quote_currency: quote_currency.into(),
        base_currency,
        contract_multiplier,
    }
}

// ─── NullFeeModel ─────────────────────────────────────────────────────────────

#[test]
fn null_fee_always_zero() {
    let model = NullFeeModel;
    let order = Order::market(1, spy(), dec!(1000), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(100)));
    assert_eq!(fee.amount, dec!(0));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn null_fee_ignores_large_order() {
    let model = NullFeeModel;
    let order = Order::market(1, spy(), dec!(1_000_000), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(500)));
    assert_eq!(fee.amount, dec!(0));
}

// ─── ZeroFeeModel ─────────────────────────────────────────────────────────────

#[test]
fn zero_fee_always_zero() {
    let model = ZeroFeeModel;
    let order = Order::market(1, spy(), dec!(500), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(50)));
    assert_eq!(fee.amount, dec!(0));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn zero_fee_sell_order_zero() {
    let model = ZeroFeeModel;
    let order = Order::market(1, spy(), dec!(-200), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(200)));
    assert_eq!(fee.amount, dec!(0));
}

// ─── FlatFeeModel ─────────────────────────────────────────────────────────────

#[test]
fn flat_fee_constant_regardless_of_size() {
    let model = FlatFeeModel::new(dec!(4.95));
    let order_small = Order::market(1, spy(), dec!(1), ts(0), "");
    let order_large = Order::market(2, spy(), dec!(100_000), ts(0), "");
    let fee_small = model.get_order_fee(&equity_params(&order_small, dec!(10)));
    let fee_large = model.get_order_fee(&equity_params(&order_large, dec!(10)));
    assert_eq!(fee_small.amount, dec!(4.95));
    assert_eq!(fee_large.amount, dec!(4.95));
}

#[test]
fn flat_fee_with_currency() {
    let model = FlatFeeModel::with_currency(dec!(1.50), "EUR");
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(50)));
    assert_eq!(fee.amount, dec!(1.50));
    assert_eq!(fee.currency, "EUR");
}

// ─── ConstantFeeModel ─────────────────────────────────────────────────────────

#[test]
fn constant_fee_default_is_one_dollar() {
    let model = ConstantFeeModel::default();
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(50)));
    assert_eq!(fee.amount, dec!(1.00));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn constant_fee_custom_amount() {
    let model = ConstantFeeModel::new(dec!(9.99));
    let order = Order::market(1, spy(), dec!(500), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(100)));
    assert_eq!(fee.amount, dec!(9.99));
}

#[test]
fn constant_fee_negative_input_becomes_positive() {
    // ConstantFeeModel::new takes abs()
    let model = ConstantFeeModel::new(dec!(-5.00));
    let order = Order::market(1, spy(), dec!(10), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(20)));
    assert_eq!(fee.amount, dec!(5.00));
}

// ─── PercentFeeModel ──────────────────────────────────────────────────────────

#[test]
fn percent_fee_default_is_01_pct() {
    // default rate = 0.001; 100 shares * $50 * 0.001 = $5
    let model = PercentFeeModel::default();
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(50)));
    assert_eq!(fee.amount, dec!(5));
}

#[test]
fn percent_fee_custom_rate() {
    // 0.5% of 200 shares * $100 = $100
    let model = PercentFeeModel::new(dec!(0.005));
    let order = Order::market(1, spy(), dec!(200), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(100)));
    assert_eq!(fee.amount, dec!(100));
}

#[test]
fn percent_fee_large_trade() {
    // 0.1% of $10,000 notional = $10
    let model = PercentFeeModel::default();
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(100)));
    assert!((fee.amount - dec!(10)).abs() < dec!(0.01));
}

// ─── InteractiveBrokersFeeModel ───────────────────────────────────────────────

#[test]
fn ib_equity_minimum_fee() {
    // 100 shares * $10 → raw = $0.50, below min $1.00, max = 100*10*0.01=$10 → fee = $1.00
    let model = InteractiveBrokersFeeModel::default();
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(10)));
    assert_eq!(fee.amount, dec!(1.00));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn ib_equity_large_order() {
    // 1000 shares * $50 → raw = $5.00, max = 1000*50*0.01 = $500 → fee = $5.00
    let model = InteractiveBrokersFeeModel::default();
    let order = Order::market(1, spy(), dec!(1000), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(50)));
    assert_eq!(fee.amount, dec!(5.00));
}

#[test]
fn ib_equity_max_fee_cap() {
    // 1 share at $0.001 → raw = $0.000005, min = $1.00, max cap = 0.001*0.01 = $0.00001
    // fee = max(0.000005, 1.0).min(0.00001) = 1.0.min(0.00001) = $0.00001
    let model = InteractiveBrokersFeeModel::default();
    let order = Order::market(1, spy(), dec!(1), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(0.001)));
    let max_fee = dec!(0.001) * dec!(1) * dec!(0.01);
    assert!(fee.amount <= max_fee + dec!(0.0000001));
}

#[test]
fn ib_options_per_contract() {
    // 5 contracts * $0.70/contract = $3.50
    let model = InteractiveBrokersFeeModel::default();
    let sym = Symbol::create_equity("AAPL", &Market::usa());
    let order = Order::market(1, sym, dec!(5), ts(0), "");
    let p = params_full(
        &order,
        dec!(150),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(3.50));
}

#[test]
fn ib_futures_per_contract() {
    // 10 contracts * $0.85/contract = $8.50
    let model = InteractiveBrokersFeeModel::default();
    let sym = Symbol::create_equity("ES", &Market::usa());
    let order = Order::market(1, sym, dec!(10), ts(0), "");
    let p = params_full(
        &order,
        dec!(4200),
        SecurityType::Future,
        "USD",
        None,
        dec!(50),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(8.50));
}

#[test]
fn ib_forex_minimum_fee() {
    // tiny notional → fee enforced at $2.00 minimum
    let model = InteractiveBrokersFeeModel::default();
    let order = Order::market(1, forex_sym("EURUSD"), dec!(1000), ts(0), "");
    let p = params_full(
        &order,
        dec!(1.1),
        SecurityType::Forex,
        "USD",
        None,
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    // 1000 * 1.1 * 0.000002 = $0.0022, below min $2.00
    assert_eq!(fee.amount, dec!(2.00));
}

#[test]
fn ib_option_exercise_is_free() {
    let model = InteractiveBrokersFeeModel::default();
    let sym = Symbol::create_equity("MSFT", &Market::usa());
    let mut order = Order::market(1, sym, dec!(10), ts(0), "");
    order.order_type = OrderType::OptionExercise;
    let p = params_full(
        &order,
        dec!(300),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0));
}

// ─── BinanceFeeModel ──────────────────────────────────────────────────────────

#[test]
fn binance_taker_fee_sell() {
    // Sell (taker market order) 1 BTC at $30,000 → fee = 30000 * 0.001 = $30 in USD
    let model = BinanceFeeModel::default();
    let order = Order::market(1, crypto_sym("BTCUSD"), dec!(-1), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(30));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn binance_maker_fee_buy_in_base_currency() {
    // Buy (limit = maker) 2 BTC: fee = qty * maker_rate = 2 * 0.001 = 0.002 BTC
    let model = BinanceFeeModel::default();
    let order = Order::limit(1, crypto_sym("BTCUSD"), dec!(2), dec!(30_000), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0.002));
    assert_eq!(fee.currency, "BTC");
}

#[test]
fn binance_fee_on_large_trade() {
    // Sell 10 units at $10,000 each → notional $100,000 * 0.001 = $100
    let model = BinanceFeeModel::default();
    let order = Order::market(1, crypto_sym("ETHUSD"), dec!(-10), ts(0), "");
    let p = params_full(
        &order,
        dec!(10_000),
        SecurityType::Crypto,
        "USD",
        Some("ETH".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(100));
}

#[test]
fn binance_custom_fees() {
    // maker 0.05%, taker 0.1%
    let model = BinanceFeeModel::new(dec!(0.0005), dec!(0.001));
    // Sell (taker): 100 units at $50 → notional $5000 * 0.001 = $5
    let order = Order::market(1, crypto_sym("SOLUSD"), dec!(-100), ts(0), "");
    let p = params_full(
        &order,
        dec!(50),
        SecurityType::Crypto,
        "USD",
        Some("SOL".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(5));
}

// ─── AlpacaFeeModel ───────────────────────────────────────────────────────────

#[test]
fn alpaca_equity_is_free() {
    let model = AlpacaFeeModel;
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(400)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn alpaca_crypto_taker_sell() {
    // taker sell: notional * 0.25% = 1 * 30000 * 0.0025 = $75
    let model = AlpacaFeeModel;
    let order = Order::market(1, crypto_sym("BTCUSD"), dec!(-1), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(75));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn alpaca_crypto_maker_buy_in_base() {
    // maker buy: 1 * 0.0015 = 0.0015 BTC
    let model = AlpacaFeeModel;
    let order = Order::limit(1, crypto_sym("BTCUSD"), dec!(1), dec!(30_000), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0.0015));
    assert_eq!(fee.currency, "BTC");
}

// ─── TradierFeeModel ──────────────────────────────────────────────────────────

#[test]
fn tradier_equity_is_free() {
    let model = TradierFeeModel;
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(400)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn tradier_options_per_contract() {
    // 10 contracts * $0.35 = $3.50
    let model = TradierFeeModel;
    let sym = Symbol::create_equity("AAPL", &Market::usa());
    let order = Order::market(1, sym, dec!(10), ts(0), "");
    let p = params_full(
        &order,
        dec!(5),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(3.50));
}

// ─── GDAXFeeModel / CoinbaseFeeModel ─────────────────────────────────────────

#[test]
fn gdax_taker_fee() {
    // taker market order: 1 BTC * $30,000 * 0.008 = $240
    let model = GDAXFeeModel::default();
    let order = Order::market(1, crypto_sym("BTCUSD"), dec!(1), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(240));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn gdax_maker_fee() {
    // maker limit order: 2 ETH * $2,000 * 0.006 = $24
    let model = GDAXFeeModel::default();
    let order = Order::limit(1, crypto_sym("ETHUSD"), dec!(2), dec!(2_000), ts(0), "");
    let p = params_full(
        &order,
        dec!(2_000),
        SecurityType::Crypto,
        "USD",
        Some("ETH".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(24));
}

// ─── KrakenFeeModel ───────────────────────────────────────────────────────────

#[test]
fn kraken_taker_buy_in_quote() {
    // taker buy: 1 BTC * $30,000 * 0.0026 = $78
    let model = KrakenFeeModel::default();
    let order = Order::market(1, crypto_sym("XBTUSD"), dec!(1), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("XBT".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(78));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn kraken_maker_sell_in_base() {
    // maker limit sell: 2 BTC * 0.0016 = 0.0032 BTC
    let model = KrakenFeeModel::default();
    let order = Order::limit(1, crypto_sym("XBTUSD"), dec!(-2), dec!(30_000), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("XBT".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0.0032));
    assert_eq!(fee.currency, "XBT");
}

// ─── BybitFeeModel ────────────────────────────────────────────────────────────

#[test]
fn bybit_spot_taker_sell_in_quote() {
    // taker sell: 1 BTC * $30,000 * 0.001 = $30
    let model = BybitFeeModel::default();
    let order = Order::market(1, crypto_sym("BTCUSD"), dec!(-1), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(30));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn bybit_perpetuals_preset() {
    let model = BybitFeeModel::perpetuals();
    // taker rate = 0.00055; 10 * $30,000 * 0.00055 = $165
    let order = Order::market(1, crypto_sym("BTCPERP"), dec!(10), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::CryptoFuture,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(165));
}

// ─── CharlesSchwabFeeModel ────────────────────────────────────────────────────

#[test]
fn schwab_equity_is_free() {
    let model = CharlesSchwabFeeModel;
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(400)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn schwab_option_065_per_contract() {
    // 4 contracts * $0.65 = $2.60
    let model = CharlesSchwabFeeModel;
    let sym = Symbol::create_equity("AMZN", &Market::usa());
    let order = Order::market(1, sym, dec!(4), ts(0), "");
    let p = params_full(
        &order,
        dec!(10),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(2.60));
}

#[test]
fn schwab_index_option_100_per_contract() {
    // 3 contracts * $1.00 = $3.00
    let model = CharlesSchwabFeeModel;
    let sym = Symbol::create_equity("SPX", &Market::usa());
    let order = Order::market(1, sym, dec!(3), ts(0), "");
    let p = params_full(
        &order,
        dec!(50),
        SecurityType::IndexOption,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(3.00));
}

// ─── TDAmeritradeFeeModel ─────────────────────────────────────────────────────

#[test]
fn tda_equity_is_free() {
    let model = TDAmeritradeFeeModel;
    let order = Order::market(1, spy(), dec!(200), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(400)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn tda_options_per_contract() {
    // 5 contracts * $0.65 = $3.25
    let model = TDAmeritradeFeeModel;
    let sym = Symbol::create_equity("TSLA", &Market::usa());
    let order = Order::market(1, sym, dec!(5), ts(0), "");
    let p = params_full(
        &order,
        dec!(20),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(3.25));
}

// ─── RobinhoodFeeModel ────────────────────────────────────────────────────────

#[test]
fn robinhood_equity_zero() {
    let model = RobinhoodFeeModel;
    let order = Order::market(1, spy(), dec!(1000), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(400)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn robinhood_always_zero_regardless_of_type() {
    let model = RobinhoodFeeModel;
    let sym = Symbol::create_equity("GOOG", &Market::usa());
    let order = Order::market(1, sym, dec!(100), ts(0), "");
    let p = params_full(
        &order,
        dec!(100),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0));
}

// ─── EtradeFeeModel ───────────────────────────────────────────────────────────

#[test]
fn etrade_equity_is_free() {
    let model = EtradeFeeModel;
    let order = Order::market(1, spy(), dec!(50), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(400)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn etrade_options_per_contract() {
    // 2 contracts * $0.65 = $1.30
    let model = EtradeFeeModel;
    let sym = Symbol::create_equity("META", &Market::usa());
    let order = Order::market(1, sym, dec!(2), ts(0), "");
    let p = params_full(
        &order,
        dec!(15),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(1.30));
}

// ─── FidelityFeeModel ─────────────────────────────────────────────────────────

#[test]
fn fidelity_equity_is_free() {
    let model = FidelityFeeModel;
    let order = Order::market(1, spy(), dec!(300), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(450)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn fidelity_options_per_contract() {
    // 6 contracts * $0.65 = $3.90
    let model = FidelityFeeModel;
    let sym = Symbol::create_equity("NVDA", &Market::usa());
    let order = Order::market(1, sym, dec!(6), ts(0), "");
    let p = params_full(
        &order,
        dec!(50),
        SecurityType::Option,
        "USD",
        None,
        dec!(100),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(3.90));
}

// ─── OandaFeeModel ────────────────────────────────────────────────────────────

#[test]
fn oanda_always_zero() {
    let model = OandaFeeModel;
    let order = Order::market(1, forex_sym("EURUSD"), dec!(100_000), ts(0), "");
    let p = params_full(
        &order,
        dec!(1.1),
        SecurityType::Forex,
        "USD",
        None,
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn oanda_zero_on_large_forex_trade() {
    let model = OandaFeeModel;
    let order = Order::market(1, forex_sym("GBPUSD"), dec!(1_000_000), ts(0), "");
    let p = params_full(
        &order,
        dec!(1.25),
        SecurityType::Forex,
        "USD",
        None,
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0));
}

// ─── FxcmFeeModel ─────────────────────────────────────────────────────────────

#[test]
fn fxcm_major_pair_lower_rate() {
    // EURUSD is major: $0.04 / 1k lot; 10,000 units → fee = 10000 / 1000 * 0.04 = $0.40
    let model = FxcmFeeModel::default();
    let order = Order::market(1, forex_sym("EURUSD"), dec!(10_000), ts(0), "");
    let p = params_full(
        &order,
        dec!(1.1),
        SecurityType::Forex,
        "USD",
        None,
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert!((fee.amount - dec!(0.40)).abs() < dec!(0.001));
}

#[test]
fn fxcm_non_forex_is_free() {
    let model = FxcmFeeModel::default();
    let order = Order::market(1, spy(), dec!(100), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(400)));
    assert_eq!(fee.amount, dec!(0));
}

// ─── ExchangeFeeModel ─────────────────────────────────────────────────────────

#[test]
fn exchange_fee_equity_sell_has_sec_and_finra() {
    let model = ExchangeFeeModel;
    // Sell 1000 shares at $50 → sell_value = $50,000
    // SEC = 50000 * 0.0000278 = $1.39
    // FINRA TAF = 1000 * 0.000145 = $0.145 (below cap of $7.27)
    // total = $1.535
    let order = Order::market(1, spy(), dec!(-1000), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(50)));
    let expected = dec!(50_000) * dec!(0.0000278) + dec!(1000) * dec!(0.000145);
    assert!((fee.amount - expected).abs() < dec!(0.0001));
    assert_eq!(fee.currency, "USD");
}

#[test]
fn exchange_fee_equity_buy_is_zero() {
    // Regulatory fees only on sells
    let model = ExchangeFeeModel;
    let order = Order::market(1, spy(), dec!(1000), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(50)));
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn exchange_fee_non_equity_is_zero() {
    let model = ExchangeFeeModel;
    let order = Order::market(1, crypto_sym("BTCUSD"), dec!(-10), ts(0), "");
    let p = params_full(
        &order,
        dec!(30_000),
        SecurityType::Crypto,
        "USD",
        Some("BTC".into()),
        dec!(1),
    );
    let fee = model.get_order_fee(&p);
    assert_eq!(fee.amount, dec!(0));
}

#[test]
fn exchange_fee_finra_taf_cap() {
    // 100,000 shares → FINRA TAF = 100000 * 0.000145 = $14.50 → capped at $7.27
    let model = ExchangeFeeModel;
    let order = Order::market(1, spy(), dec!(-100_000), ts(0), "");
    let fee = model.get_order_fee(&equity_params(&order, dec!(10)));
    // SEC: 100000 * 10 * 0.0000278 = $27.80
    // FINRA: capped at $7.27
    let expected_sec = dec!(1_000_000) * dec!(0.0000278);
    let expected_finra = dec!(7.27);
    let expected = expected_sec + expected_finra;
    assert!((fee.amount - expected).abs() < dec!(0.001));
}
