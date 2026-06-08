//! kit — a personal toolbelt. One binary, many sharp tools over a shared framework.
//!
//! The dependency direction *is* the architecture: `tools → framework`/`tui`, never tool↔tool,
//! and `framework` never reaches up into `tui`.

pub mod framework;
pub mod tools;
pub mod tui;
