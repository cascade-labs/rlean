---
sidebar_position: 6
title: Plugin Development
---

# Plugin Development

Brokerages and data providers are runtime plugins — compiled `cdylib` crates
loaded from `~/.rlean/plugins/` at startup. rlean has no compile-time
dependencies on specific brokers or data sources. The `lean-plugin` crate
defines the plugin ABI.

## Managing plugins

```sh
rlean plugin list
rlean plugin install thetadata
rlean plugin install alpaca
rlean plugin upgrade thetadata
rlean plugin remove  alpaca
```

Install from a custom Git URL:

```sh
rlean plugin install https://github.com/my-org/rlean-plugin-myprovider
```

### Registries

The official registry is always included. Additional registries can be added:

```sh
rlean plugin registry list
rlean plugin registry add    https://raw.githubusercontent.com/my-org/my-plugins/main/registry.json
rlean plugin registry remove https://raw.githubusercontent.com/my-org/my-plugins/main/registry.json
```

## Writing a plugin

A plugin is a Rust `cdylib` crate that exports a descriptor and one or more
factory functions. Every plugin must export `rlean_plugin_descriptor`:

```rust
use lean_plugin::{PluginDescriptor, PluginKind};

#[no_mangle]
pub extern "C" fn rlean_plugin_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name:    c"myprovider",
        version: c"0.1.0",
        kind:    PluginKind::DataProvider,
    }
}
```

Then implement and export factory functions for the plugin's kind:

- **Data providers** implement `IHistoryProvider`.
- **Brokerages** implement `IBrokerageModel`.

For complete, working examples, see any crate under `brokerages/` or
`data_providers/` in the
[rlean-plugins](https://github.com/cascade-labs/rlean-plugins) repository. If you
need to add or modify a plugin, that is where it lives.
