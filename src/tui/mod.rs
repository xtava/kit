//! Interactive layer — the shared TUI building blocks every tool's TUI mounts.
//!
//! Not a forced event loop: tools own their `tokio::select!` loop (their async sources differ).
//! What's shared is the bug-prone, identical machinery — the RAII terminal [`Session`], the
//! [`EventReader`] that bridges events into async, the [`LineEditor`], and the slash-command
//! [`CommandSet`].

mod actions;
pub mod clipboard;
mod commandline;
mod context_menu;
mod editor;
mod events;
pub mod fuzzy;
pub mod markdown;
mod navigation;
mod search;
mod session;
pub mod settings;
mod split;
mod suggestions;
pub mod syntax;
pub mod theme;

pub use actions::{
    ActionId, ActionInvocation, ActionRegistry, ActionRegistryBuilder, ActionRegistryError,
    ActionSpec, ActionState, ActionUnavailable, KeyChord, KeybindingPlacement, MenuId,
    MenuPlacement, ResolvedAction, ResolvedMenu,
};
pub use commandline::{CommandSet, CommandSpec, ParsedInput};
pub use context_menu::{
    ContextMenu, ContextMenuItemLayout, ContextMenuLayout, ContextMenuOutcome, ContextMenuStyle,
};
pub use editor::LineEditor;
pub use events::EventReader;
pub use navigation::{Direction, NavigationMap, NavigationRegion};
pub use search::{Frecency, FrecencyError, FrecencyStore, FuzzyIndex, SearchMatch, SearchMode};
pub use session::{Session, SessionOptions};
pub use settings::{SettingsEditor, SettingsFlow};
pub use split::{
    render_split_divider, SplitDividerStyle, SplitDrag, SplitFrame, SplitMinimums, SplitRatio,
};
pub use suggestions::{Suggestion, SuggestionMenu};
