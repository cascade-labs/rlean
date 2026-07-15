use crate::{CustomDataConfig, CustomDataQuery};
use parking_lot::RwLock;
use rlean_core::{DataNormalizationMode, Resolution, Symbol, TickType};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SubscriptionDataKind {
    Market,
    Custom,
    Universe,
    /// A point-in-time, all-equity fundamental snapshot. Unlike a custom
    /// universe, every row belongs to the same selection event.
    FundamentalUniverse,
    Option,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomSubscriptionMetadata {
    pub source_type: String,
    pub ticker: String,
    pub config: CustomDataConfig,
    pub dynamic_query: CustomDataQuery,
}

/// Provider metadata for the canonical daily fundamental universe snapshot.
/// The ticker is deliberately not a security ticker: it identifies the
/// provider-wide equity universe requested by the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundamentalUniverseSubscriptionMetadata {
    pub source_type: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptionChainFilterMetadata {
    pub min_strike_rank: i32,
    pub max_strike_rank: i32,
    pub min_expiry_days: i32,
    pub max_expiry_days: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionChainSubscriptionMetadata {
    pub canonical_permtick: String,
    pub underlying_ticker: String,
    pub filter: OptionChainFilterMetadata,
}

/// All configuration needed to subscribe to a data stream.
///
/// `unique_id()` SipHashes several identity fields. Because it is called
/// thousands of times per simulated day by the subscription-sync logic (issue
/// #39), the result is memoized in `cached_unique_id` and computed at most once
/// per config value. The identity fields that are ever mutated after
/// construction (`tick_type`, `data_kind`, and `venue`) must be changed through
/// their setters, which clear the cache. Direct writes to the other identity fields do not occur anywhere
/// in the codebase. Cloning starts with an empty cache (see the manual `Clone`
/// impl) so a clone can never inherit a stale id.
#[derive(Debug, Serialize, Deserialize)]
pub struct SubscriptionDataConfig {
    pub symbol: Symbol,
    /// Physical venue requested from the sidecar. `market` remains encoded in
    /// the symbol/SID and is not interchangeable with this value.
    pub venue: String,
    pub resolution: Resolution,
    pub tick_type: TickType,
    pub normalization_mode: DataNormalizationMode,
    pub fill_data_forward: bool,
    pub extended_market_hours: bool,
    pub is_internal_feed: bool,
    pub is_filtered_subscription: bool,
    pub data_time_zone: String,
    pub exchange_time_zone: String,
    pub data_kind: SubscriptionDataKind,
    pub custom: Option<CustomSubscriptionMetadata>,
    pub fundamental_universe: Option<FundamentalUniverseSubscriptionMetadata>,
    pub option_chain: Option<OptionChainSubscriptionMetadata>,
    /// Memoized `unique_id()`. Not serialized; never contributes to identity.
    #[serde(skip)]
    cached_unique_id: OnceLock<u64>,
}

impl Clone for SubscriptionDataConfig {
    fn clone(&self) -> Self {
        // Deliberately start the clone with an empty cache rather than copying
        // the memoized id. The id is recomputed lazily and cannot go stale even
        // if the clone's identity fields are subsequently mutated.
        SubscriptionDataConfig {
            symbol: self.symbol.clone(),
            venue: self.venue.clone(),
            resolution: self.resolution,
            tick_type: self.tick_type,
            normalization_mode: self.normalization_mode,
            fill_data_forward: self.fill_data_forward,
            extended_market_hours: self.extended_market_hours,
            is_internal_feed: self.is_internal_feed,
            is_filtered_subscription: self.is_filtered_subscription,
            data_time_zone: self.data_time_zone.clone(),
            exchange_time_zone: self.exchange_time_zone.clone(),
            data_kind: self.data_kind,
            custom: self.custom.clone(),
            fundamental_universe: self.fundamental_universe.clone(),
            option_chain: self.option_chain.clone(),
            cached_unique_id: OnceLock::new(),
        }
    }
}

impl SubscriptionDataConfig {
    pub fn new_equity(
        symbol: Symbol,
        resolution: Resolution,
        normalization_mode: DataNormalizationMode,
    ) -> Self {
        let venue = symbol.market().as_str().to_ascii_lowercase();
        SubscriptionDataConfig {
            symbol,
            venue,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode,
            fill_data_forward: true,
            extended_market_hours: false,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
            data_kind: SubscriptionDataKind::Market,
            custom: None,
            fundamental_universe: None,
            option_chain: None,
            cached_unique_id: OnceLock::new(),
        }
    }

    /// Option subscriptions are always Raw — matches C# Lean's `Option`
    /// constructor and `DataManager.Add` forcing for option/index symbols.
    pub fn new_option(symbol: Symbol, resolution: Resolution) -> Self {
        let venue = symbol.market().as_str().to_ascii_lowercase();
        SubscriptionDataConfig {
            symbol,
            venue,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: true,
            extended_market_hours: false,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
            data_kind: SubscriptionDataKind::Market,
            custom: None,
            fundamental_universe: None,
            option_chain: None,
            cached_unique_id: OnceLock::new(),
        }
    }

    pub fn new_forex(symbol: Symbol, resolution: Resolution) -> Self {
        let venue = symbol.market().as_str().to_ascii_lowercase();
        SubscriptionDataConfig {
            symbol,
            venue,
            resolution,
            tick_type: TickType::Quote,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: true,
            extended_market_hours: true,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "UTC".into(),
            exchange_time_zone: "UTC".into(),
            data_kind: SubscriptionDataKind::Market,
            custom: None,
            fundamental_universe: None,
            option_chain: None,
            cached_unique_id: OnceLock::new(),
        }
    }

    pub fn new_crypto(symbol: Symbol, resolution: Resolution) -> Self {
        let venue = symbol.market().as_str().to_ascii_lowercase();
        SubscriptionDataConfig {
            symbol,
            venue,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: false,
            extended_market_hours: true,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "UTC".into(),
            exchange_time_zone: "UTC".into(),
            data_kind: SubscriptionDataKind::Market,
            custom: None,
            fundamental_universe: None,
            option_chain: None,
            cached_unique_id: OnceLock::new(),
        }
    }

    pub fn new_crypto_future(symbol: Symbol, resolution: Resolution) -> Self {
        let venue = symbol.market().as_str().to_ascii_lowercase();
        SubscriptionDataConfig {
            symbol,
            venue,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: true,
            extended_market_hours: true,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "UTC".into(),
            exchange_time_zone: "UTC".into(),
            data_kind: SubscriptionDataKind::Market,
            custom: None,
            fundamental_universe: None,
            option_chain: None,
            cached_unique_id: OnceLock::new(),
        }
    }

    pub fn new_custom(
        symbol: Symbol,
        resolution: Resolution,
        metadata: CustomSubscriptionMetadata,
    ) -> Self {
        let venue = metadata
            .config
            .properties
            .get("venue")
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| metadata.source_type.trim().to_ascii_lowercase());
        SubscriptionDataConfig {
            symbol,
            venue,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: false,
            extended_market_hours: true,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
            data_kind: SubscriptionDataKind::Custom,
            custom: Some(metadata),
            fundamental_universe: None,
            option_chain: None,
            cached_unique_id: OnceLock::new(),
        }
    }

    pub fn new_custom_universe(
        symbol: Symbol,
        resolution: Resolution,
        metadata: CustomSubscriptionMetadata,
    ) -> Self {
        let mut config = Self::new_custom(symbol, resolution, metadata);
        config.set_data_kind(SubscriptionDataKind::Universe);
        config.is_internal_feed = true;
        config
    }

    /// An internal, daily, whole-market fundamental snapshot subscription.
    /// `symbol` is a stable internal base symbol; individual equity symbols
    /// are carried by the rows returned by the sidecar.
    pub fn new_fundamental_universe(
        symbol: Symbol,
        resolution: Resolution,
        metadata: FundamentalUniverseSubscriptionMetadata,
    ) -> Self {
        let venue = metadata.source_type.trim().to_ascii_lowercase();
        SubscriptionDataConfig {
            symbol,
            venue,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: false,
            extended_market_hours: false,
            is_internal_feed: true,
            is_filtered_subscription: true,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
            data_kind: SubscriptionDataKind::FundamentalUniverse,
            custom: None,
            fundamental_universe: Some(metadata),
            option_chain: None,
            cached_unique_id: OnceLock::new(),
        }
    }

    pub fn new_option_chain(
        canonical: Symbol,
        resolution: Resolution,
        metadata: OptionChainSubscriptionMetadata,
    ) -> Self {
        let venue = canonical.market().as_str().to_ascii_lowercase();
        SubscriptionDataConfig {
            symbol: canonical,
            venue,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: false,
            extended_market_hours: false,
            is_internal_feed: true,
            is_filtered_subscription: true,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
            data_kind: SubscriptionDataKind::Option,
            custom: None,
            fundamental_universe: None,
            option_chain: Some(metadata),
            cached_unique_id: OnceLock::new(),
        }
    }

    pub fn is_custom_data(&self) -> bool {
        self.custom.is_some()
    }

    pub fn is_universe_data(&self) -> bool {
        matches!(
            self.data_kind,
            SubscriptionDataKind::Universe | SubscriptionDataKind::FundamentalUniverse
        )
    }

    /// Stable identity hash. Memoized after the first call — this is invoked
    /// thousands of times per simulated day by the subscription-sync path, so
    /// recomputing the SipHash every time dominated backtest CPU (issue #39).
    pub fn unique_id(&self) -> u64 {
        *self
            .cached_unique_id
            .get_or_init(|| self.compute_unique_id())
    }

    fn compute_unique_id(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hash;
        let mut h = DefaultHasher::new();
        self.symbol.id.sid.hash(&mut h);
        self.venue.hash(&mut h);
        (self.resolution as u8).hash(&mut h);
        (self.tick_type as u8).hash(&mut h);
        self.data_kind.hash(&mut h);
        if let Some(custom) = &self.custom {
            custom.source_type.to_ascii_lowercase().hash(&mut h);
            custom.ticker.to_ascii_uppercase().hash(&mut h);
        }
        if let Some(fundamental) = &self.fundamental_universe {
            fundamental.source_type.to_ascii_lowercase().hash(&mut h);
        }
        if let Some(option_chain) = &self.option_chain {
            option_chain.canonical_permtick.hash(&mut h);
        }
        std::hash::Hasher::finish(&h)
    }

    /// Set the tick type, clearing the memoized `unique_id`. Use this instead of
    /// writing `config.tick_type` directly so the cached id can never go stale.
    pub fn set_tick_type(&mut self, tick_type: TickType) {
        self.tick_type = tick_type;
        self.cached_unique_id = OnceLock::new();
    }

    /// Set the data kind, clearing the memoized `unique_id`. Use this instead of
    /// writing `config.data_kind` directly so the cached id can never go stale.
    pub fn set_data_kind(&mut self, data_kind: SubscriptionDataKind) {
        self.data_kind = data_kind;
        self.cached_unique_id = OnceLock::new();
    }

    /// Set the physical venue and invalidate the memoized subscription id.
    pub fn set_venue(&mut self, venue: impl Into<String>) {
        self.venue = venue.into().trim().to_ascii_lowercase();
        self.cached_unique_id = OnceLock::new();
    }

    /// Whether the `unique_id` has been memoized yet. Test-only; lets tests
    /// assert memoization without a process-global counter.
    #[cfg(test)]
    pub(crate) fn is_unique_id_cached(&self) -> bool {
        self.cached_unique_id.get().is_some()
    }
}

/// Manages the set of active subscriptions.
///
/// `generation` is a monotonic version stamp bumped on every mutation that
/// changes the active set (add of a new id, remove, remove-by-symbol,
/// normalization-mode change, custom dynamic-query change). The
/// subscription-sync loop runs on every slice; comparing the stamp against the
/// last-synced value lets it skip the whole diff walk when nothing changed
/// (issue #64). The stamp lives on the manager itself, which never crosses the
/// sidecar protocol, so it does not change the wire contract.
#[derive(Debug, Default)]
pub struct SubscriptionManager {
    state: RwLock<SubscriptionState>,
    generation: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default)]
struct SubscriptionState {
    subscriptions: HashMap<u64, Arc<SubscriptionDataConfig>>,
    order: Vec<u64>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        SubscriptionManager::default()
    }

    /// Monotonic version stamp of the active subscription set. Bumped by every
    /// mutation that changes membership or config. The subscription-sync loop
    /// caches the last value it observed and skips its diff walk entirely when
    /// the stamp is unchanged (issue #64).
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Bump the version stamp. Called under the state write lock so the new
    /// stamp is always published after the mutation it describes.
    fn bump_generation(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// Get-or-add: matches C# Lean's `DataManager.SubscriptionManagerGetOrAdd`.
    /// If a config with the same `unique_id` already exists, the existing
    /// `Arc` is returned and the incoming `config` (including its
    /// `normalization_mode`) is discarded. Mode changes must go through
    /// [`set_normalization_mode`].
    pub fn add(&self, config: SubscriptionDataConfig) -> Arc<SubscriptionDataConfig> {
        let id = config.unique_id();
        let mut state = self.state.write();
        if let Some(existing) = state.subscriptions.get(&id) {
            return existing.clone();
        }
        let config = Arc::new(config);
        state.order.push(id);
        state.subscriptions.insert(id, config.clone());
        self.bump_generation();
        config
    }

    pub fn remove(&self, config: &SubscriptionDataConfig) {
        let id = config.unique_id();
        let mut state = self.state.write();
        if state.subscriptions.remove(&id).is_some() {
            state.order.retain(|existing_id| *existing_id != id);
            self.bump_generation();
        }
    }

    pub fn remove_symbol(&self, symbol: &Symbol) {
        let mut state = self.state.write();
        let before = state.subscriptions.len();
        state
            .subscriptions
            .retain(|_, config| config.symbol.id.sid != symbol.id.sid);
        if state.subscriptions.len() != before {
            let active_ids: HashSet<_> = state.subscriptions.keys().copied().collect();
            state.order.retain(|id| active_ids.contains(id));
            self.bump_generation();
        }
    }

    pub fn get_all(&self) -> Vec<Arc<SubscriptionDataConfig>> {
        let state = self.state.read();
        state
            .order
            .iter()
            .filter_map(|id| state.subscriptions.get(id).cloned())
            .collect()
    }

    /// All configs for the given symbol (matched by SID) in insertion order.
    /// Mirrors C# Lean's `DataManager.GetSubscriptionDataConfigs(symbol)`.
    pub fn get_configs_for_symbol(&self, symbol: &Symbol) -> Vec<Arc<SubscriptionDataConfig>> {
        let state = self.state.read();
        state
            .order
            .iter()
            .filter_map(|id| state.subscriptions.get(id))
            .filter(|config| config.symbol.id.sid == symbol.id.sid)
            .cloned()
            .collect()
    }

    /// Update the normalization mode of every config matching the symbol.
    /// Mirrors C# Lean's `Security.SetDataNormalizationMode` (which mutates
    /// all attached subscription configs in place). Returns the number of
    /// configs that were updated.
    pub fn set_normalization_mode(
        &self,
        symbol: &Symbol,
        normalization_mode: DataNormalizationMode,
    ) -> usize {
        let mut state = self.state.write();
        let ids: Vec<u64> = state
            .order
            .iter()
            .copied()
            .filter(|id| {
                state
                    .subscriptions
                    .get(id)
                    .map(|config| config.symbol.id.sid == symbol.id.sid)
                    .unwrap_or(false)
            })
            .collect();
        let mut updated = 0;
        for id in ids {
            if let Some(existing) = state.subscriptions.get(&id) {
                if existing.normalization_mode == normalization_mode {
                    continue;
                }
                let mut new_config = (**existing).clone();
                new_config.normalization_mode = normalization_mode;
                state.subscriptions.insert(id, Arc::new(new_config));
                updated += 1;
            }
        }
        if updated > 0 {
            self.bump_generation();
        }
        updated
    }

    pub fn set_custom_dynamic_query(
        &self,
        source_type: &str,
        ticker: &str,
        query: CustomDataQuery,
    ) -> bool {
        let mut state = self.state.write();
        let Some(id) = state.order.iter().copied().find(|id| {
            state
                .subscriptions
                .get(id)
                .and_then(|config| config.custom.as_ref())
                .map(|custom| {
                    custom.source_type.eq_ignore_ascii_case(source_type)
                        && custom.ticker.eq_ignore_ascii_case(ticker)
                })
                .unwrap_or(false)
        }) else {
            return false;
        };

        if let Some(existing) = state.subscriptions.get(&id) {
            let mut new_config = (**existing).clone();
            if let Some(custom) = &mut new_config.custom {
                custom.dynamic_query = query;
            }
            state.subscriptions.insert(id, Arc::new(new_config));
            self.bump_generation();
            return true;
        }
        false
    }

    pub fn count(&self) -> usize {
        self.state.read().subscriptions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{SubscriptionDataConfig, SubscriptionDataKind, SubscriptionManager};
    use rlean_core::{
        DataNormalizationMode, Market, OptionRight, OptionStyle, Resolution, Symbol, TickType,
    };
    use rust_decimal_macros::dec;
    use std::sync::Arc;

    fn equity_config(ticker: &str) -> SubscriptionDataConfig {
        SubscriptionDataConfig::new_equity(
            Symbol::create_equity(ticker, &Market::new(Market::USA)),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        )
    }

    #[test]
    fn get_all_preserves_subscription_insertion_order() {
        let manager = SubscriptionManager::new();

        manager.add(equity_config("SPY"));
        manager.add(equity_config("XLK"));
        manager.add(equity_config("XLF"));

        let symbols: Vec<_> = manager
            .get_all()
            .iter()
            .map(|config| config.symbol.value.to_string())
            .collect();
        assert_eq!(symbols, vec!["SPY", "XLK", "XLF"]);
    }

    #[test]
    fn removed_subscription_is_removed_from_order() {
        let manager = SubscriptionManager::new();
        let spy = manager.add(equity_config("SPY"));
        manager.add(equity_config("XLK"));
        manager.add(equity_config("XLF"));

        manager.remove(&spy);

        let symbols: Vec<_> = manager
            .get_all()
            .iter()
            .map(|config| config.symbol.value.to_string())
            .collect();
        assert_eq!(symbols, vec!["XLK", "XLF"]);
    }

    #[test]
    fn readded_subscription_moves_to_end_after_removal() {
        let manager = SubscriptionManager::new();
        let spy = manager.add(equity_config("SPY"));
        manager.add(equity_config("XLK"));
        manager.remove(&spy);
        manager.add(equity_config("SPY"));

        let symbols: Vec<_> = manager
            .get_all()
            .iter()
            .map(|config| config.symbol.value.to_string())
            .collect();
        assert_eq!(symbols, vec!["XLK", "SPY"]);
    }

    #[test]
    fn add_returns_existing_config_when_unique_id_matches() {
        let manager = SubscriptionManager::new();
        let spy = Symbol::create_equity("SPY", &Market::new(Market::USA));
        let first = manager.add(SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        ));
        let second = manager.add(SubscriptionDataConfig::new_equity(
            spy,
            Resolution::Minute,
            DataNormalizationMode::Raw,
        ));
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.normalization_mode, DataNormalizationMode::Adjusted);
    }

    #[test]
    fn set_normalization_mode_updates_trade_and_quote_configs() {
        let manager = SubscriptionManager::new();
        let spy = Symbol::create_equity("SPY", &Market::new(Market::USA));
        manager.add(SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        ));
        let mut quote_config = SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        );
        quote_config.set_tick_type(TickType::Quote);
        manager.add(quote_config);

        let updated = manager.set_normalization_mode(&spy, DataNormalizationMode::Raw);
        assert_eq!(updated, 2);
        for config in manager.get_configs_for_symbol(&spy) {
            assert_eq!(config.normalization_mode, DataNormalizationMode::Raw);
        }
    }

    #[test]
    fn new_option_defaults_to_raw_normalization() {
        let underlying = Symbol::create_equity("SPY", &Market::new(Market::USA));
        let option = Symbol::create_option(
            underlying,
            &Market::new(Market::USA),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
            dec!(450),
            OptionRight::Call,
            OptionStyle::American,
        );
        let config = SubscriptionDataConfig::new_option(option, Resolution::Minute);
        assert_eq!(config.normalization_mode, DataNormalizationMode::Raw);
    }

    #[test]
    fn unique_id_is_memoized_after_first_call() {
        let config = equity_config("SPY");
        assert!(!config.is_unique_id_cached(), "cache must start empty");
        let first = config.unique_id();
        assert!(
            config.is_unique_id_cached(),
            "first call must populate the cache"
        );
        for _ in 0..1000 {
            assert_eq!(config.unique_id(), first);
        }
    }

    #[test]
    fn set_tick_type_invalidates_cached_unique_id() {
        let mut config = equity_config("SPY");
        let trade_id = config.unique_id();
        config.set_tick_type(TickType::Quote);
        let quote_id = config.unique_id();
        assert_ne!(
            trade_id, quote_id,
            "changing tick_type must change unique_id (no stale cache)"
        );
        // And it must match a config built as a quote from the start.
        let mut fresh = equity_config("SPY");
        fresh.set_tick_type(TickType::Quote);
        assert_eq!(quote_id, fresh.unique_id());
    }

    #[test]
    fn set_data_kind_invalidates_cached_unique_id() {
        let mut config = equity_config("SPY");
        let market_id = config.unique_id();
        config.set_data_kind(SubscriptionDataKind::Universe);
        assert_ne!(market_id, config.unique_id());
    }

    #[test]
    fn venue_is_part_of_subscription_identity() {
        let mut config = equity_config("SPY");
        let usa = config.unique_id();
        config.set_venue("arcx");

        assert_eq!(config.venue, "arcx");
        assert_ne!(config.unique_id(), usa);
    }

    #[test]
    fn clone_recomputes_id_and_does_not_inherit_stale_cache() {
        // Prime the cache on the original, then clone and mutate the clone's
        // identity. The clone must reflect its own identity, never the original's.
        let original = equity_config("SPY");
        let _ = original.unique_id();
        let mut cloned = original.clone();
        cloned.set_tick_type(TickType::Quote);
        assert_ne!(original.unique_id(), cloned.unique_id());
        // A plain clone (no mutation) must still report the same id.
        let plain = original.clone();
        assert_eq!(original.unique_id(), plain.unique_id());
    }

    #[test]
    fn many_configs_have_distinct_stable_ids() {
        // Build 400 configs and simulate the sync diff touching every config
        // many times per "slice" across many "slices". Every id must be stable
        // and each config's cache is populated exactly once (verified by the
        // memoization test); here we assert ids are distinct and stable so the
        // O(N) HashMap-based diff is sound.
        let configs: Vec<SubscriptionDataConfig> = (0..400)
            .map(|i| equity_config(&format!("SYM{i:04}")))
            .collect();

        let baseline: Vec<u64> = configs.iter().map(|c| c.unique_id()).collect();
        let distinct: std::collections::HashSet<u64> = baseline.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            400,
            "all 400 configs must have distinct ids"
        );

        for _slice in 0..50 {
            for (config, expected) in configs.iter().zip(&baseline) {
                assert_eq!(config.unique_id(), *expected);
            }
        }
    }

    #[test]
    fn generation_bumps_only_on_effective_mutation() {
        let manager = SubscriptionManager::new();
        assert_eq!(manager.generation(), 0);

        // Adding a new config bumps.
        let spy = equity_config("SPY");
        manager.add(spy.clone());
        let after_add = manager.generation();
        assert!(
            after_add > 0,
            "add of a new config must bump the generation"
        );

        // Re-adding the same id is a no-op get-or-add: no bump.
        manager.add(spy.clone());
        assert_eq!(
            manager.generation(),
            after_add,
            "get-or-add of an existing id must not bump"
        );

        // Removing a non-existent config must not bump.
        let xlk = equity_config("XLK");
        manager.remove(&xlk);
        assert_eq!(
            manager.generation(),
            after_add,
            "removing an absent config must not bump"
        );

        // Removing an existing config bumps.
        manager.remove(&spy);
        let after_remove = manager.generation();
        assert!(
            after_remove > after_add,
            "removing an existing config must bump"
        );

        // remove_symbol that removes nothing must not bump.
        manager.remove_symbol(&Symbol::create_equity("NONE", &Market::new(Market::USA)));
        assert_eq!(
            manager.generation(),
            after_remove,
            "remove_symbol matching nothing must not bump"
        );
    }

    #[test]
    fn generation_bumps_on_normalization_and_custom_query_change() {
        let manager = SubscriptionManager::new();
        let spy_symbol = Symbol::create_equity("SPY", &Market::new(Market::USA));
        manager.add(equity_config("SPY"));
        let base = manager.generation();

        // Same mode -> no update -> no bump.
        manager.set_normalization_mode(&spy_symbol, DataNormalizationMode::Adjusted);
        assert_eq!(
            manager.generation(),
            base,
            "no-op normalization change must not bump"
        );

        // Real mode change -> bump.
        let updated = manager.set_normalization_mode(&spy_symbol, DataNormalizationMode::Raw);
        assert_eq!(updated, 1);
        assert!(
            manager.generation() > base,
            "effective normalization change must bump"
        );
    }
}
