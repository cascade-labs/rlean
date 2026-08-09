//! Shared foreground interruption state for SDK and engine loops.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use anyhow::Result;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static CTRL_C_HANDLER: Once = Once::new();

/// Error returned when a foreground run is stopped by Ctrl+C or a host interrupt.
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

/// Install a process-wide Ctrl+C handler (once). Sets the interrupt flag so
/// Rust-only work (provider I/O, live synchronizer waits) can be stopped too.
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
