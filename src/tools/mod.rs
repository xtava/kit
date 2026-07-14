//! The tools hosted by kit. Each is a leaf, blind to the others, depending only on the framework,
//! the `tui` harness, and the shared `cdp` engine.

pub mod cdp;
pub mod deploy;
pub mod diff;
pub mod domain;
pub mod record;
pub mod render;
pub mod scout;
pub mod settings;
pub mod stats;
pub mod update;
