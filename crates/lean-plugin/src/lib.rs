/// lean-plugin — C-stable plugin ABI for rlean
///
/// Every rlean plugin is a `cdylib` crate that exports one symbol:
///
/// ```rust
/// #[no_mangle]
/// pub extern "C" fn rlean_plugin_descriptor() -> PluginDescriptor {
///     PluginDescriptor {
///         name:     b"tradier\0".as_ptr(),
///         version:  b"0.1.0\0".as_ptr(),
///         kind:     PluginKind::Brokerage,
///     }
/// }
/// ```
///
/// rlean loads all `.dylib`/`.so` files in `~/.rlean/plugins/` at startup,
/// calls `rlean_plugin_descriptor()` on each, and routes provider/brokerage
/// registration accordingly.

/// The type of capability a plugin provides.
///
/// A single plugin crate may implement multiple kinds by exporting multiple
/// factory symbols (e.g. both `rlean_create_brokerage` and
/// `rlean_create_history_provider`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// Brokerage integration (order routing, account management)
    Brokerage              = 0,
    /// Historical data provider
    DataProviderHistorical = 1,
    /// Live data / quote stream
    DataProviderLive       = 2,
    /// Custom data source (alternative data, news, etc.)
    CustomData             = 3,
    /// AI / ML skill (signal generation, model inference)
    AiSkill                = 4,
}

impl PluginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Brokerage              => "brokerage",
            Self::DataProviderHistorical => "data-provider-historical",
            Self::DataProviderLive       => "data-provider-live",
            Self::CustomData             => "custom-data",
            Self::AiSkill                => "ai-skill",
        }
    }
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// C-stable descriptor returned by `rlean_plugin_descriptor()`.
///
/// All pointer fields must point to null-terminated UTF-8 strings with
/// `'static` lifetime (string literals are fine).
#[repr(C)]
pub struct PluginDescriptor {
    /// Plugin name as null-terminated C string (e.g. `b"tradier\0"`)
    pub name: *const u8,
    /// SemVer version as null-terminated C string (e.g. `b"0.1.0\0"`)
    pub version: *const u8,
    /// Primary capability kind
    pub kind: PluginKind,
}

// Safety: the pointers are static string literals.
unsafe impl Send for PluginDescriptor {}
unsafe impl Sync for PluginDescriptor {}

impl PluginDescriptor {
    pub fn name_str(&self) -> &str {
        unsafe { cstr(self.name) }
    }
    pub fn version_str(&self) -> &str {
        unsafe { cstr(self.version) }
    }
}

unsafe fn cstr<'a>(ptr: *const u8) -> &'a str {
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).unwrap_or("?")
}

// ─── C-stable factory function signatures ────────────────────────────────────

/// C-stable factory: create a history provider from a JSON config string.
///
/// Returns a heap-allocated `Box<Arc<dyn IHistoryProvider>>` cast to `*mut ()`.
/// Caller must free with `rlean_destroy_history_provider`.
pub type CreateHistoryProviderFn =
    unsafe extern "C" fn(config_json: *const std::os::raw::c_char) -> *mut ();

/// Free a provider created by `CreateHistoryProviderFn`.
pub type DestroyHistoryProviderFn = unsafe extern "C" fn(ptr: *mut ());

/// Convenience macro for plugin crates to implement the required export.
///
/// ```rust
/// use lean_plugin::{PluginKind, PluginDescriptor, rlean_plugin};
///
/// rlean_plugin! {
///     name    = "tradier",
///     version = "0.1.0",
///     kind    = PluginKind::Brokerage,
/// }
/// ```
#[macro_export]
macro_rules! rlean_plugin {
    (name = $name:literal, version = $ver:literal, kind = $kind:expr $(,)?) => {
        #[no_mangle]
        pub extern "C" fn rlean_plugin_descriptor() -> $crate::PluginDescriptor {
            $crate::PluginDescriptor {
                name:    concat!($name, "\0").as_ptr(),
                version: concat!($ver,  "\0").as_ptr(),
                kind:    $kind,
            }
        }
    };
}
