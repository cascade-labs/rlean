use rlean_core::{DataNormalizationMode, Market, Resolution, Symbol, SymbolOptionsExt};
use rlean_data::{
    CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata,
    FundamentalUniverseSubscriptionMetadata, OptionChainFilterMetadata,
    OptionChainSubscriptionMetadata, SubscriptionDataConfig,
};
use rlean_data_sidecar::{
    client_message, server_message, AddSubscription, ClientMessage, DataBatch, DataSidecarConfig,
    DeliveryMode, GetManifest, Initialize, Manifest, OpenBrokerage, OpenLiveDataFeed,
    ServerMessage, SidecarEndpoint, SubscriptionSpec, WireDataType, PROTOCOL_VERSION,
};

#[test]
fn endpoint_supports_local_and_remote_flight_transports() {
    assert_eq!(
        SidecarEndpoint::parse("grpc://127.0.0.1:7410").unwrap(),
        SidecarEndpoint::Tcp {
            authority: "127.0.0.1:7410".into(),
            tls: false,
        }
    );
    assert!(SidecarEndpoint::parse("grpc://data.example.com:7410").is_err());
    assert_eq!(
        SidecarEndpoint::parse("grpc+tls://data.example.com:443").unwrap(),
        SidecarEndpoint::Tcp {
            authority: "data.example.com:443".into(),
            tls: true,
        }
    );
    #[cfg(unix)]
    assert_eq!(
        SidecarEndpoint::parse("grpc+unix:///tmp/rlean-data.sock").unwrap(),
        SidecarEndpoint::Unix("/tmp/rlean-data.sock".into())
    );
}

#[test]
fn relay_uses_standard_subscription_and_data_batch_messages() {
    let route = SubscriptionSpec {
        source_type: "provider".into(),
        ticker: "series".into(),
        data_type: WireDataType::Custom as i32,
        ..Default::default()
    };
    let add = ClientMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 1,
        payload: Some(client_message::Payload::AddSubscription(AddSubscription {
            subscription_id: 9,
            mode: DeliveryMode::Relay as i32,
            subscription: Some(SubscriptionSpec::default()),
        })),
    };
    assert_eq!(
        ClientMessage::decode_flight_data(&add.clone().into_flight_data()).unwrap(),
        add
    );

    let message = ServerMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 0,
        payload: Some(server_message::Payload::DataBatch(Box::new(DataBatch {
            subscription_id: 9,
            query_id: 0,
            data_type: WireDataType::Custom as i32,
            subscription: Some(route),
        }))),
    };
    assert_eq!(
        ServerMessage::decode_flight_data(&message.clone().into_flight_data()).unwrap(),
        message
    );
}

#[test]
fn live_feed_and_execution_brokerage_are_independent_connections() {
    let feed = ClientMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 10,
        payload: Some(client_message::Payload::OpenLiveDataFeed(
            OpenLiveDataFeed {
                feed_connection_id: 1,
                provider: "tradier".into(),
                opaque_config_json: br#"{"access_token":"feed-token"}"#.to_vec(),
            },
        )),
    };
    let brokerage = ClientMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 11,
        payload: Some(client_message::Payload::OpenBrokerage(OpenBrokerage {
            brokerage_connection_id: 2,
            brokerage: "robinhood".into(),
            opaque_config_json: br#"{"account_number":"account"}"#.to_vec(),
        })),
    };

    assert_eq!(
        ClientMessage::decode_flight_data(&feed.clone().into_flight_data()).unwrap(),
        feed
    );
    assert_eq!(
        ClientMessage::decode_flight_data(&brokerage.clone().into_flight_data()).unwrap(),
        brokerage
    );
}

#[test]
fn initialize_is_versioned_protobuf_control_metadata() {
    let message = ClientMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 7,
        payload: Some(client_message::Payload::Initialize(Initialize {
            origin_id: "origin-a".into(),
            session_id: "session-a".into(),
        })),
    };
    let data = message.clone().into_flight_data();
    assert!(data.data_body.is_empty());
    assert_ne!(data.app_metadata.first(), Some(&b'{'));
    assert_eq!(ClientMessage::decode_flight_data(&data).unwrap(), message);
}

#[test]
fn manifest_uses_the_existing_exchange_control_metadata() {
    let request = ClientMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 12,
        payload: Some(client_message::Payload::GetManifest(GetManifest {})),
    };
    assert_eq!(
        ClientMessage::decode_flight_data(&request.clone().into_flight_data()).unwrap(),
        request
    );

    let response = ServerMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 12,
        payload: Some(server_message::Payload::Manifest(Manifest {
            content_type: "application/json".into(),
            body: br#"{"tables":[]}"#.to_vec(),
        })),
    };
    assert_eq!(
        ServerMessage::decode_flight_data(&response.clone().into_flight_data()).unwrap(),
        response
    );
}

#[test]
fn subscription_definition_has_no_query_time_range() {
    let subscription = SubscriptionSpec {
        config_id: 42,
        symbol_sid: 123,
        symbol_value: "SPY".into(),
        permanent_ticker: "SPY".into(),
        security_type: 1,
        market: "usa".into(),
        venue: "usa".into(),
        resolution: 4,
        tick_type: 0,
        data_type: WireDataType::TradeBar as i32,
        extended_market_hours: false,
        source_type: String::new(),
        ticker: String::new(),
        custom_query: None,
        properties: Default::default(),
        option_underlying_ticker: String::new(),
        option_min_strike_rank: 0,
        option_max_strike_rank: 0,
        option_min_expiry_days: 0,
        option_max_expiry_days: 0,
    };
    let message = ClientMessage {
        protocol_version: PROTOCOL_VERSION,
        request_id: 8,
        payload: Some(client_message::Payload::AddSubscription(AddSubscription {
            subscription_id: 1,
            mode: DeliveryMode::Backtest as i32,
            subscription: Some(subscription),
        })),
    };
    assert_eq!(
        ClientMessage::decode_flight_data(&message.clone().into_flight_data()).unwrap(),
        message
    );
}

#[test]
fn custom_universe_subscription_uses_universe_wire_contract() {
    let properties =
        std::collections::HashMap::from([("venue".to_string(), "dark-pool".to_string())]);
    let metadata = CustomSubscriptionMetadata {
        source_type: "tradealert".into(),
        ticker: "UNIVERSE".into(),
        config: CustomDataConfig {
            ticker: "UNIVERSE".into(),
            source_type: "tradealert".into(),
            resolution: Resolution::Daily,
            properties,
            query: CustomDataQuery::default(),
        },
        dynamic_query: CustomDataQuery::default(),
    };
    let config = SubscriptionDataConfig::new_custom_universe(
        Symbol::create_equity("UNIVERSE", &Market::new(Market::USA)),
        Resolution::Daily,
        metadata,
    );

    assert_eq!(
        SubscriptionSpec::from(&config).data_type,
        WireDataType::Universe as i32
    );
    assert_eq!(SubscriptionSpec::from(&config).source_type, "tradealert");
    assert_eq!(SubscriptionSpec::from(&config).venue, "dark-pool");
    assert_eq!(config.normalization_mode, DataNormalizationMode::Raw);
}

#[test]
fn fundamental_universe_subscription_uses_typed_snapshot_wire_contract() {
    let config = SubscriptionDataConfig::new_fundamental_universe(
        Symbol::create_base("fundamental_universe", "massive", &Market::usa()),
        Resolution::Daily,
        FundamentalUniverseSubscriptionMetadata {
            source_type: "massive".into(),
        },
    );

    let spec = SubscriptionSpec::from(&config);
    assert_eq!(spec.data_type, WireDataType::FundamentalUniverse as i32);
    assert_eq!(spec.source_type, "massive");
    assert_eq!(spec.ticker, "*");
    assert_eq!(config.normalization_mode, DataNormalizationMode::Raw);
    assert!(config.is_universe_data());
}

#[test]
fn option_universe_subscription_carries_filter_metadata() {
    let underlying = Symbol::create_equity("SPY", &Market::usa());
    let canonical = Symbol::create_canonical_option(&underlying, &Market::usa());
    let config = SubscriptionDataConfig::new_option_chain(
        canonical,
        Resolution::Minute,
        OptionChainSubscriptionMetadata {
            canonical_permtick: "?SPY".into(),
            underlying_ticker: "SPY".into(),
            filter: OptionChainFilterMetadata {
                min_strike_rank: -5,
                max_strike_rank: 5,
                min_expiry_days: 0,
                max_expiry_days: 0,
            },
        },
    );

    let spec = SubscriptionSpec::from(&config);
    assert_eq!(spec.data_type, WireDataType::OptionUniverse as i32);
    assert_eq!(spec.option_underlying_ticker, "SPY");
    assert_eq!(spec.option_min_strike_rank, -5);
    assert_eq!(spec.option_max_strike_rank, 5);
    assert_eq!(spec.option_min_expiry_days, 0);
    assert_eq!(spec.option_max_expiry_days, 0);
}

#[test]
fn token_is_optional_and_never_part_of_protocol_messages() {
    let config: DataSidecarConfig = serde_json::from_value(serde_json::json!({
        "endpoint": "grpc://localhost:7410"
    }))
    .unwrap();
    assert!(config.token.is_none());
    assert_eq!(config.connect_timeout_ms, 10_000);
}

#[test]
fn brokerage_order_wire_contract_preserves_decimal_and_symbol_identity() {
    let symbol = rlean_core::Symbol::create_equity("SPY", &rlean_core::Market::usa());
    let order = rlean_orders::Order::limit(
        17,
        symbol.clone(),
        "12.345".parse().unwrap(),
        "499.125".parse().unwrap(),
        rlean_core::DateTime::from_secs(1_700_000_000),
        "wire-test",
    );

    let wire = rlean_data_sidecar::WireOrder::from(&order);
    assert_eq!(wire.quantity, "12.345");
    assert_eq!(wire.limit_price.as_deref(), Some("499.125"));
    let decoded = rlean_orders::Order::try_from(wire).unwrap();
    assert_eq!(decoded.id, order.id);
    assert_eq!(decoded.symbol, symbol);
    assert_eq!(decoded.quantity, order.quantity);
    assert_eq!(decoded.limit_price, order.limit_price);
}
