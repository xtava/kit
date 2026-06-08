//! The framework spine: the plug-in contract and the shared services every tool draws on.
//!
//! There is no DI container — `main` builds one [`Context`] (the composition root) and borrows
//! it into every tool's [`Tool::run`]. Resolution is field access; lifetimes are the borrow
//! checker's job.

mod config;
mod context;
mod output;
mod registry;
mod terminal;
mod tool;

pub use config::ConfigStore;
pub use context::Context;
pub use output::{Output, OutputFormat};
pub use registry::Registry;
pub use terminal::Terminal;
pub use tool::{Tool, ToolMeta};
