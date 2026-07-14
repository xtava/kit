//! Interactive layer — the shared TUI building blocks every tool's TUI mounts.
//!
//! Not a forced event loop: tools own their `tokio::select!` loop (their async sources differ).
//! What's shared is the bug-prone, identical machinery — the RAII terminal [`Session`], the
//! [`EventReader`] that bridges events into async, the [`LineEditor`], and the slash-command
//! [`CommandSet`].

pub mod clipboard;
mod commandline;
mod editor;
mod events;
pub mod fuzzy;
pub mod markdown;
mod navigation;
mod session;
mod suggestions;
pub mod syntax;
pub mod theme;

pub use commandline::{CommandSet, CommandSpec, ParsedInput};
pub use editor::LineEditor;
pub use events::EventReader;
pub use navigation::{Direction, NavigationMap, NavigationRegion};
pub use session::{Session, SessionOptions};
pub use suggestions::{Suggestion, SuggestionMenu};
