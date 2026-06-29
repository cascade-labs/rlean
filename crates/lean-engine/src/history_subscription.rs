use crate::data_feed::DataFeedContext;
use crate::slice_synchronizer::SliceSynchronizer;
use crate::subscription_reader::SubscriptionStream;
use lean_core::{DataNormalizationMode, Result as LeanResult, SecurityType, TickType};
use lean_data::{CustomDataPoint, QuoteBar, Slice, SubscriptionDataConfig, Tick, TradeBar};
use lean_data_providers::{DataType, HistoryRequest};

#[derive(Clone)]
pub struct SubscriptionHistoryProvider {
    context: DataFeedContext,
}

impl SubscriptionHistoryProvider {
    pub fn new(context: DataFeedContext) -> Self {
        Self { context }
    }

    pub async fn get_slices(
        &self,
        configs: Vec<SubscriptionDataConfig>,
        start: lean_core::DateTime,
        end: lean_core::DateTime,
    ) -> LeanResult<Vec<Slice>> {
        let streams = configs
            .into_iter()
            .map(|config| SubscriptionStream::new(config, self.context.clone(), start, end))
            .collect::<Vec<_>>();
        let mut synchronizer = SliceSynchronizer::new(streams, end);
        let mut slices = Vec::new();
        while let Some(slice) = synchronizer.next_slice().await? {
            slices.push(slice);
        }
        Ok(slices)
    }

    pub async fn get_trade_bars(
        &self,
        request: &HistoryRequest,
        normalization_mode: DataNormalizationMode,
    ) -> LeanResult<Vec<TradeBar>> {
        let config = config_from_history_request(request, normalization_mode);
        self.get_trade_bars_for_configs(vec![config], request.start, request.end)
            .await
    }

    pub async fn get_quote_bars(&self, request: &HistoryRequest) -> LeanResult<Vec<QuoteBar>> {
        let config = config_from_history_request(request, DataNormalizationMode::Raw);
        self.get_quote_bars_for_configs(vec![config], request.start, request.end)
            .await
    }

    pub async fn get_ticks(&self, request: &HistoryRequest) -> LeanResult<Vec<Tick>> {
        let config = config_from_history_request(request, DataNormalizationMode::Raw);
        self.get_ticks_for_configs(vec![config], request.start, request.end)
            .await
    }

    pub async fn get_trade_bars_for_configs(
        &self,
        configs: Vec<SubscriptionDataConfig>,
        start: lean_core::DateTime,
        end: lean_core::DateTime,
    ) -> LeanResult<Vec<TradeBar>> {
        let symbols = configs
            .iter()
            .map(|config| config.symbol.id.sid)
            .collect::<std::collections::HashSet<_>>();
        let slices = self.get_slices(configs, start, end).await?;
        let mut out = Vec::new();
        for slice in slices {
            for bar in slice.bars.values() {
                if symbols.contains(&bar.symbol.id.sid) {
                    out.push(bar.clone());
                }
            }
        }
        out.sort_by_key(|bar| (bar.end_time.0, bar.symbol.id.sid));
        Ok(out)
    }

    pub async fn get_quote_bars_for_configs(
        &self,
        configs: Vec<SubscriptionDataConfig>,
        start: lean_core::DateTime,
        end: lean_core::DateTime,
    ) -> LeanResult<Vec<QuoteBar>> {
        let symbols = configs
            .iter()
            .map(|config| config.symbol.id.sid)
            .collect::<std::collections::HashSet<_>>();
        let slices = self.get_slices(configs, start, end).await?;
        let mut out = Vec::new();
        for slice in slices {
            for bar in slice.quote_bars.values() {
                if symbols.contains(&bar.symbol.id.sid) {
                    out.push(bar.clone());
                }
            }
        }
        out.sort_by_key(|bar| (bar.end_time.0, bar.symbol.id.sid));
        Ok(out)
    }

    pub async fn get_ticks_for_configs(
        &self,
        configs: Vec<SubscriptionDataConfig>,
        start: lean_core::DateTime,
        end: lean_core::DateTime,
    ) -> LeanResult<Vec<Tick>> {
        let symbols = configs
            .iter()
            .map(|config| config.symbol.id.sid)
            .collect::<std::collections::HashSet<_>>();
        let slices = self.get_slices(configs, start, end).await?;
        let mut out = Vec::new();
        for slice in slices {
            for (sid, ticks) in &slice.ticks {
                if symbols.contains(sid) {
                    out.extend(ticks.iter().cloned());
                }
            }
        }
        out.sort_by_key(|tick| (tick.time.0, tick.symbol.id.sid, tick.tick_type as u8));
        Ok(out)
    }

    pub async fn get_custom_points(
        &self,
        config: SubscriptionDataConfig,
        start: lean_core::DateTime,
        end: lean_core::DateTime,
    ) -> LeanResult<Vec<CustomDataPoint>> {
        let symbol = config.symbol.clone();
        let slices = self.get_slices(vec![config], start, end).await?;
        let mut out = Vec::new();
        for slice in slices {
            if let Some(points) = slice.get_custom_data(&symbol) {
                out.extend(points.iter().cloned());
            }
        }
        Ok(out)
    }
}

pub fn config_from_history_request(
    request: &HistoryRequest,
    normalization_mode: DataNormalizationMode,
) -> SubscriptionDataConfig {
    let mut config = match request.symbol.security_type() {
        SecurityType::Equity => SubscriptionDataConfig::new_equity(
            request.symbol.clone(),
            request.resolution,
            normalization_mode,
        ),
        SecurityType::Option | SecurityType::IndexOption | SecurityType::FutureOption => {
            SubscriptionDataConfig::new_option(request.symbol.clone(), request.resolution)
        }
        SecurityType::Forex | SecurityType::Cfd => {
            SubscriptionDataConfig::new_forex(request.symbol.clone(), request.resolution)
        }
        SecurityType::Crypto | SecurityType::CryptoFuture => {
            SubscriptionDataConfig::new_crypto(request.symbol.clone(), request.resolution)
        }
        _ => SubscriptionDataConfig::new_equity(
            request.symbol.clone(),
            request.resolution,
            normalization_mode,
        ),
    };
    config.tick_type = match request.data_type {
        DataType::QuoteBar => TickType::Quote,
        DataType::Tick => TickType::Trade,
        _ => TickType::Trade,
    };
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{Market, OptionRight, OptionStyle, Resolution, SymbolOptionsExt, TimeSpan};
    use lean_data::{TradeBar, TradeBarData};
    use lean_storage::IcebergStore;
    use rust_decimal_macros::dec;
    use std::sync::Arc;

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> lean_core::DateTime {
        lean_core::DateTime::from(
            Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()),
        )
    }

    #[tokio::test]
    async fn selected_option_contract_history_uses_subscription_streams() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let underlying = lean_core::Symbol::create_equity("SPY", &Market::usa());
        let expiry = NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let contract = lean_core::Symbol::create_option_osi(
            underlying,
            dec!(475),
            expiry,
            OptionRight::Call,
            OptionStyle::American,
            &Market::usa(),
        );
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let bar = TradeBar::new(
            contract.clone(),
            dt(day - chrono::Duration::days(1), 16, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(5), dec!(6), dec!(4), dec!(5.5), dec!(100)),
        );
        store
            .append_trade_bars(
                &[bar],
                contract.security_type(),
                contract.market().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();

        let provider = SubscriptionHistoryProvider::new(DataFeedContext::new(store));
        let config = SubscriptionDataConfig::new_option(contract.clone(), Resolution::Daily);
        let bars = provider
            .get_trade_bars_for_configs(vec![config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].symbol, contract);
        assert_eq!(bars[0].close, dec!(5.5));
    }
}
