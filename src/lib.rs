//! kit — a personal toolbelt. One binary, many sharp tools over a shared framework.
//!
//! The dependency direction *is* the architecture: `tools → framework | tui | cdp`, never tool↔tool,
//! and the spine modules never reach up into `tools`. `cdp` is the Chrome DevTools Protocol engine —
//! a peer capability (like `tui`) that both `scout` and `cdp` build on (see `docs/adr/0001`).

#[cfg(unix)]
pub mod cdp;
pub mod framework;
pub mod onepassword;
pub mod tools;
pub mod tui;
