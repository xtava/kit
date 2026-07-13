//! The tools hosted by kit. Each is a leaf, blind to the others, depending only on the framework,
//! the `tui` harness, and the shared `cdp` engine.

pub mod cdp;
pub mod domain;
pub mod record;
pub mod scout;
pub mod stats;
