//! The tools hosted by kit. Each is a leaf, blind to the others, depending only on the framework,
//! the `tui` harness, and the shared `cdp` engine.

pub mod build;
#[cfg(unix)]
pub mod cdp;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod console;
pub mod deploy;
pub mod diff;
pub mod domain;
pub mod monitor;
pub mod ops;
pub mod process;
pub mod record;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod remote;
pub mod render;
#[cfg(unix)]
pub mod scout;
pub mod secrets;
pub mod settings;
#[cfg(unix)]
pub mod skills;
pub mod stats;
#[cfg(target_os = "linux")]
pub mod stream;
#[cfg(target_os = "macos")]
#[path = "stream_macos/mod.rs"]
pub mod stream;
pub mod swarm;
pub mod sync;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod tail;
#[cfg(target_os = "linux")]
pub mod tsgo;
pub mod update;
