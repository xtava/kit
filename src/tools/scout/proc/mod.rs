//! Process plane: scan `/proc`, read PSS, classify by role, group into instances.

mod classify;
mod fleet;
mod sample;

pub use fleet::{scan_fleet, total_pss};
