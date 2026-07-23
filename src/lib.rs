//! kit — a personal toolbelt. One binary, many sharp tools over a shared framework.
//!
//! The dependency direction *is* the architecture: tools consume peer capabilities and never one
//! another; shared modules never reach up into `tools`.

#[cfg(unix)]
pub mod cdp;
pub mod framework;
pub mod onepassword;
pub mod release;
pub mod tailscale;
pub mod tools;
pub mod tui;
