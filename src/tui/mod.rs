//! Interactive layer — the shared TUI building blocks every tool's TUI mounts.
//!
//! Not a forced event loop: tools own their `tokio::select!` loop (their async sources differ).
//! What's shared is the bug-prone, identical machinery — the RAII terminal [`Session`], the
//! [`EventReader`] that bridges events into async, the [`LineEditor`], and the slash-command
//! [`CommandSet`].

mod actions;
pub mod clipboard;
mod command_palette;
mod commandline;
mod context_menu;
mod editor;
mod events;
pub mod fuzzy;
mod history;
pub mod markdown;
mod navigation;
mod search;
mod selection;
mod session;
pub mod settings;
mod split;
mod suggestions;
pub mod syntax;
mod text;
pub mod theme;
mod viewport;

pub use actions::{
    ActionId, ActionInvocation, ActionRegistry, ActionRegistryBuilder, ActionRegistryError,
    ActionSpec, ActionState, ActionUnavailable, CommandPalettePlacement, KeyChord, Keybinding,
    KeybindingPlacement, KeybindingResolution, KeybindingState, MenuId, MenuPlacement,
    ResolvedAction, ResolvedActions,
};
pub use command_palette::{CommandPalette, CommandPaletteLayout, CommandPaletteOutcome};
pub use commandline::{CommandSet, CommandSpec, ParsedInput};
pub use context_menu::{
    ContextMenu, ContextMenuItemLayout, ContextMenuLayout, ContextMenuOutcome, ContextMenuStyle,
};
pub use editor::LineEditor;
pub use events::EventReader;
pub use history::NavigationHistory;
pub use navigation::{Direction, NavigationMap, NavigationRegion};
pub use search::{Frecency, FrecencyError, FrecencyStore, FuzzyIndex, SearchMatch, SearchMode};
pub use selection::{SelectableRegion, SelectionMode, SelectionOutcome, TextPoint, TextSelection};
pub use session::{Session, SessionOptions};
pub use settings::{SettingsEditor, SettingsFlow};
pub use split::{
    render_split_divider, SplitDividerStyle, SplitDrag, SplitFrame, SplitMinimums, SplitRatio,
};
pub use suggestions::{Suggestion, SuggestionMenu};
pub use text::{
    fit_terminal_text, terminal_text_width, truncate_terminal_text, CellAlignment, CellOverflow,
};
pub use viewport::{
    render_vertical_scrollbar, FollowViewport, ScrollbarDrag, ScrollbarLayout, ScrollbarStyle,
    Viewport, ViewportMetrics,
};
