//! No-op markers parsed by `build.rs` until all bindings are migrated to direct pyo3 attrs.
//! These expand to nothing at compile time.

#[macro_export]
macro_rules! sdk_bind {
    ($($tt:tt)*) => {};
}

#[macro_export]
macro_rules! sdk_getter {
    ($($tt:tt)*) => {};
}

#[macro_export]
macro_rules! sdk_method {
    ($($tt:tt)*) => {};
}

#[macro_export]
macro_rules! sdk_new {
    ($($tt:tt)*) => {};
}

#[macro_export]
macro_rules! sdk_static {
    ($($tt:tt)*) => {};
}

#[macro_export]
macro_rules! sdk_setter {
    ($($tt:tt)*) => {};
}

#[macro_export]
macro_rules! sdk_callback_adapter {
    ($($tt:tt)*) => {};
}
