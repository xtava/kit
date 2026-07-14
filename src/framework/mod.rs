//! The framework spine: the plug-in contract and the shared services every tool draws on.
//!
//! There is no DI container — `main` builds one [`Context`] (the composition root) and borrows
//! it into every tool's [`Tool::run`]. Resolution is field access; lifetimes are the borrow
//! checker's job.

mod atomic_file;
mod config;
mod context;
mod output;
mod registry;
mod settings;
mod terminal;
mod tool;

pub(crate) use atomic_file::{AtomicFileError, AtomicFileWriter};
pub use config::{ConfigStore, ConfigValue};
pub use context::Context;
pub use output::{Output, OutputFormat};
pub use registry::Registry;
pub use settings::{
    EditableSettings, SettingEdit, SettingField, SettingId, SettingsSection, SettingsSectionMeta,
};
pub use terminal::Terminal;
pub use tool::{Tool, ToolMeta};
