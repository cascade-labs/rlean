//! Library surface for the rlean CLI crate.
//!
//! Historically this exported the live supervisor used by `rleand`. That daemon
//! is gone; the crate remains a library so integration tests and tooling can
//! share selected helpers if needed.
