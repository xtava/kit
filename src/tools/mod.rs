//! The tools hosted by kit. Each is a leaf, blind to the others, depending only on the framework,
//! the `tui` harness, and the shared `cdp` engine.

pub mod build;
#[cfg(unix)]
pub mod cdp;
pub mod deploy;
pub mod diff;
pub mod domain;
pub mod monitor;
pub mod ops;
pub mod process;
pub mod record;
pub mod render;
#[cfg(unix)]
pub mod scout;
pub mod secrets;
pub mod settings;
pub mod stats;
pub mod swarm;
pub mod tail;
pub mod update;
