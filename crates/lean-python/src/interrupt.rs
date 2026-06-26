//! Foreground Ctrl+C / Python `KeyboardInterrupt` handling.
//!
//! Embedded Python callbacks historically swallowed `KeyboardInterrupt` and kept
//! running. This module centralises interrupt detection so backtest and live
//! loops can exit promptly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use anyhow::Result;
use pyo3::prelude::*;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static CTRL_C_HANDLER: Once = Once::new();

/// Error returned when a foreground run is stopped by Ctrl+C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// Clear any prior interrupt before starting a new foreground run.
pub fn reset() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

pub fn trigger() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

pub fn abort_if_interrupted() -> Result<()> {
    if interrupted() {
        Err(Interrupted.into())
    } else {
        Ok(())
    }
}

pub fn is_interrupted_error(err: &anyhow::Error) -> bool {
    if interrupted() {
        return true;
    }
    err.chain()
        .any(|cause| cause.downcast_ref::<Interrupted>().is_some())
}

pub fn is_keyboard_interrupt(py: Python<'_>, err: &PyErr) -> bool {
    err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py)
}

/// Handle a Python callback error. Returns `true` when the run should abort.
pub fn report_py_err(py: Python<'_>, err: &PyErr) -> bool {
    if is_keyboard_interrupt(py, err) {
        trigger();
        true
    } else {
        false
    }
}

/// Print a Python callback error unless it is a `KeyboardInterrupt`.
pub fn report_py_err_print(py: Python<'_>, err: PyErr) -> bool {
    if is_keyboard_interrupt(py, &err) {
        trigger();
        true
    } else {
        err.print(py);
        false
    }
}

/// Install a process-wide Ctrl+C handler (once). Sets the interrupt flag so
/// Rust-only work (Parquet I/O, live synchronizer waits) can be stopped too.
pub fn ensure_ctrl_c_handler() {
    CTRL_C_HANDLER.call_once(|| {
        tokio::spawn(async {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break;
                }
                trigger();
                eprintln!("\nInterrupted.");
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_error_round_trip() {
        reset();
        assert!(!interrupted());
        trigger();
        assert!(interrupted());
        assert!(abort_if_interrupted().is_err());
    }
}
