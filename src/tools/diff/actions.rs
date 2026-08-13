use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::framework::{ExternalFileAction, ExternalFileCapabilities};
use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement,
};

pub(super) const TOGGLE_STAGE: ActionId = ActionId::new("diff.file.toggleStage");
pub(super) const COPY_FILENAME: ActionId = ActionId::new("diff.file.copyName");
pub(super) const OPEN_FILE: ActionId = ActionId::new("diff.file.open");
pub(super) const REVEAL_FILE: ActionId = ActionId::new("diff.file.reveal");
pub(super) const PREVIEW_FILE: ActionId = ActionId::new("diff.file.preview");
pub(super) const TOGGLE_TREE: ActionId = ActionId::new("diff.layout.toggleTree");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiffCommand {
    Quit,
    Refresh,
    ToggleStage,
    CopyFilename,
    OpenFile,
    RevealFile,
    PreviewFile,
    MoveDown,
    MoveUp,
    PageDown,
    PageUp,
    NextHunk,
    PreviousHunk,
    ToggleMode,
    NextRegion,
    PreviousRegion,
    RegionRight,
    RegionLeft,
    PanRight,
    PanLeft,
    NarrowTree,
    WidenTree,
    FitTree,
    ToggleTree,
    ResetLayout,
    Home,
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DiffActionContext {
    pub has_document: bool,
    pub repository_idle: bool,
    pub filename: Option<String>,
    pub file: Option<PathBuf>,
    pub file_capabilities: ExternalFileCapabilities,
    pub resizable_tree: bool,
}

pub(super) type DiffActionRegistry = ActionRegistry<DiffActionContext, DiffCommand>;

pub(super) fn registry() -> Result<DiffActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    let actions = [
        ("diff.quit", "Quit", DiffCommand::Quit),
        ("diff.repository.refresh", "Refresh repository", DiffCommand::Refresh),
        (TOGGLE_STAGE.as_str(), "Stage or unstage file", DiffCommand::ToggleStage),
        (COPY_FILENAME.as_str(), "Copy filename", DiffCommand::CopyFilename),
        (OPEN_FILE.as_str(), "Open selected file", DiffCommand::OpenFile),
        (REVEAL_FILE.as_str(), "Reveal selected file", DiffCommand::RevealFile),
        (PREVIEW_FILE.as_str(), "Preview selected file", DiffCommand::PreviewFile),
        ("diff.selection.next", "Move down", DiffCommand::MoveDown),
        ("diff.selection.previous", "Move up", DiffCommand::MoveUp),
        ("diff.document.pageDown", "Page down", DiffCommand::PageDown),
        ("diff.document.pageUp", "Page up", DiffCommand::PageUp),
        ("diff.hunk.next", "Next change", DiffCommand::NextHunk),
        ("diff.hunk.previous", "Previous change", DiffCommand::PreviousHunk),
        ("diff.view.toggle", "Toggle inline or split view", DiffCommand::ToggleMode),
        ("diff.region.next", "Next region", DiffCommand::NextRegion),
        ("diff.region.previous", "Previous region", DiffCommand::PreviousRegion),
        ("diff.region.right", "Focus region right", DiffCommand::RegionRight),
        ("diff.region.left", "Focus region left", DiffCommand::RegionLeft),
        ("diff.document.panRight", "Pan right", DiffCommand::PanRight),
        ("diff.document.panLeft", "Pan left", DiffCommand::PanLeft),
        ("diff.layout.narrowTree", "Narrow changes panel", DiffCommand::NarrowTree),
        ("diff.layout.widenTree", "Widen changes panel", DiffCommand::WidenTree),
        ("diff.layout.fitTree", "Fit changes panel to paths", DiffCommand::FitTree),
        (TOGGLE_TREE.as_str(), "Toggle changes panel", DiffCommand::ToggleTree),
        ("diff.layout.reset", "Reset changes panel width", DiffCommand::ResetLayout),
        ("diff.document.home", "Document start", DiffCommand::Home),
        ("diff.document.end", "Document end", DiffCommand::End),
    ];
    for (order, (id, title, command)) in actions.into_iter().enumerate() {
        builder.register_action(ActionSpec {
            id: ActionId::new(id),
            title,
            command,
            enablement: match command {
                DiffCommand::ToggleStage => selected_document_when_idle,
                DiffCommand::Refresh => repository_idle,
                DiffCommand::CopyFilename => filename,
                DiffCommand::OpenFile => open_file,
                DiffCommand::RevealFile => reveal_file,
                DiffCommand::PreviewFile => preview_file,
                _ => enabled,
            },
            command_palette: CommandPalettePlacement::Visible {
                group: "Diff",
                group_order: 10,
                order: order as i16,
            },
        });
    }
    for (code, modifiers, id) in [
        (KeyCode::Char('q'), KeyModifiers::NONE, "diff.quit"),
        (KeyCode::Esc, KeyModifiers::NONE, "diff.quit"),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, "diff.quit"),
        (KeyCode::Char('d'), KeyModifiers::CONTROL, "diff.quit"),
        (KeyCode::Char('r'), KeyModifiers::NONE, "diff.repository.refresh"),
        (KeyCode::Char('s'), KeyModifiers::NONE, "diff.file.toggleStage"),
        (KeyCode::Char('o'), KeyModifiers::NONE, OPEN_FILE.as_str()),
        (KeyCode::Char('O'), KeyModifiers::NONE, REVEAL_FILE.as_str()),
        (KeyCode::Char('p'), KeyModifiers::NONE, PREVIEW_FILE.as_str()),
        (KeyCode::Down, KeyModifiers::NONE, "diff.selection.next"),
        (KeyCode::Char('j'), KeyModifiers::NONE, "diff.selection.next"),
        (KeyCode::Up, KeyModifiers::NONE, "diff.selection.previous"),
        (KeyCode::Char('k'), KeyModifiers::NONE, "diff.selection.previous"),
        (KeyCode::PageDown, KeyModifiers::NONE, "diff.document.pageDown"),
        (KeyCode::PageUp, KeyModifiers::NONE, "diff.document.pageUp"),
        (KeyCode::Char('n'), KeyModifiers::NONE, "diff.hunk.next"),
        (KeyCode::Char(']'), KeyModifiers::NONE, "diff.hunk.next"),
        (KeyCode::Char('N'), KeyModifiers::NONE, "diff.hunk.previous"),
        (KeyCode::Char('['), KeyModifiers::NONE, "diff.hunk.previous"),
        (KeyCode::Char('v'), KeyModifiers::NONE, "diff.view.toggle"),
        (KeyCode::Tab, KeyModifiers::NONE, "diff.region.next"),
        (KeyCode::BackTab, KeyModifiers::SHIFT, "diff.region.previous"),
        (KeyCode::Right, KeyModifiers::NONE, "diff.region.right"),
        (KeyCode::Left, KeyModifiers::NONE, "diff.region.left"),
        (KeyCode::Char('l'), KeyModifiers::NONE, "diff.document.panRight"),
        (KeyCode::Char('h'), KeyModifiers::NONE, "diff.document.panLeft"),
        (KeyCode::Char('<'), KeyModifiers::NONE, "diff.layout.narrowTree"),
        (KeyCode::Char('>'), KeyModifiers::NONE, "diff.layout.widenTree"),
        (KeyCode::Char('F'), KeyModifiers::NONE, "diff.layout.fitTree"),
        (KeyCode::Char('t'), KeyModifiers::CONTROL, TOGGLE_TREE.as_str()),
        (KeyCode::Char('='), KeyModifiers::NONE, "diff.layout.reset"),
        (KeyCode::Home, KeyModifiers::NONE, "diff.document.home"),
        (KeyCode::End, KeyModifiers::NONE, "diff.document.end"),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action: ActionId::new(id),
            when: match id {
                "diff.layout.narrowTree"
                | "diff.layout.widenTree"
                | "diff.layout.fitTree"
                | "diff.layout.reset" => resizable_tree,
                _ => always,
            },
        });
    }
    builder.build()
}

fn enabled(_: &DiffActionContext) -> ActionState {
    ActionState::Enabled
}

fn selected_document_when_idle(context: &DiffActionContext) -> ActionState {
    if !context.repository_idle {
        ActionState::disabled("another operation is already running")
    } else if context.has_document {
        ActionState::Enabled
    } else {
        ActionState::disabled("no selected file")
    }
}

fn repository_idle(context: &DiffActionContext) -> ActionState {
    if context.repository_idle {
        ActionState::Enabled
    } else {
        ActionState::disabled("repository operation already running")
    }
}

fn filename(context: &DiffActionContext) -> ActionState {
    if context.filename.is_some() {
        ActionState::Enabled
    } else {
        ActionState::disabled("no filename target")
    }
}

fn open_file(context: &DiffActionContext) -> ActionState {
    file_action(context, ExternalFileAction::Open)
}

fn reveal_file(context: &DiffActionContext) -> ActionState {
    file_action(context, ExternalFileAction::Reveal)
}

fn preview_file(context: &DiffActionContext) -> ActionState {
    file_action(context, ExternalFileAction::Preview)
}

fn file_action(context: &DiffActionContext, action: ExternalFileAction) -> ActionState {
    if !context.repository_idle {
        ActionState::disabled("another operation is already running")
    } else if context.file.is_none() {
        ActionState::disabled("no selected file")
    } else if context.file_capabilities.supports(action) {
        ActionState::Enabled
    } else {
        ActionState::disabled(format!("{action} is unavailable on this platform"))
    }
}

fn resizable_tree(context: &DiffActionContext) -> bool {
    context.resizable_tree
}

fn always(_: &DiffActionContext) -> bool {
    true
}
