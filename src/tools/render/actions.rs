use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement,
};

pub(super) const OPEN_TARGET: ActionId = ActionId::new("render.target.open");
pub(super) const TOGGLE_CONTENTS: ActionId = ActionId::new("render.contents.toggle");

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RenderTarget {
    Viewer,
    Link(String),
    Heading(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RenderActionContext {
    pub target: RenderTarget,
    pub input_empty: bool,
    pub menu_open: bool,
    pub search_active: bool,
    pub contents_available: bool,
    pub split_available: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RenderCommand {
    OpenTarget,
    Find,
    ClearInput,
    Refresh,
    SearchNext,
    SearchPrevious,
    HistoryBack,
    HistoryForward,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Home,
    End,
    ToggleContents,
    NarrowDocument,
    WidenDocument,
    ResetSplit,
}

pub(super) type RenderActionRegistry = ActionRegistry<RenderActionContext, RenderCommand>;

pub(super) fn registry() -> Result<RenderActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    for (id, title, command, order) in [
        (OPEN_TARGET, "Open target", RenderCommand::OpenTarget, 10),
        (ActionId::new("render.document.find"), "Find in document", RenderCommand::Find, 20),
        (ActionId::new("render.input.clear"), "Clear input", RenderCommand::ClearInput, 30),
        (ActionId::new("render.workspace.refresh"), "Refresh", RenderCommand::Refresh, 40),
        (ActionId::new("render.search.next"), "Next match", RenderCommand::SearchNext, 50),
        (
            ActionId::new("render.search.previous"),
            "Previous match",
            RenderCommand::SearchPrevious,
            60,
        ),
        (ActionId::new("render.history.back"), "Back", RenderCommand::HistoryBack, 70),
        (ActionId::new("render.history.forward"), "Forward", RenderCommand::HistoryForward, 80),
        (ActionId::new("render.document.scrollUp"), "Scroll up", RenderCommand::ScrollUp, 90),
        (
            ActionId::new("render.document.scrollDown"),
            "Scroll down",
            RenderCommand::ScrollDown,
            100,
        ),
        (ActionId::new("render.document.pageUp"), "Page up", RenderCommand::PageUp, 110),
        (ActionId::new("render.document.pageDown"), "Page down", RenderCommand::PageDown, 120),
        (ActionId::new("render.document.home"), "Document start", RenderCommand::Home, 130),
        (ActionId::new("render.document.end"), "Document end", RenderCommand::End, 140),
        (TOGGLE_CONTENTS, "Toggle contents", RenderCommand::ToggleContents, 150),
        (
            ActionId::new("render.layout.narrowDocument"),
            "Narrow document panel",
            RenderCommand::NarrowDocument,
            160,
        ),
        (
            ActionId::new("render.layout.widenDocument"),
            "Widen document panel",
            RenderCommand::WidenDocument,
            170,
        ),
        (
            ActionId::new("render.layout.reset"),
            "Reset panel widths",
            RenderCommand::ResetSplit,
            180,
        ),
    ] {
        builder.register_action(ActionSpec {
            id,
            title,
            command,
            enablement: enabled,
            command_palette: CommandPalettePlacement::Visible {
                group: "Render",
                group_order: 10,
                order,
            },
        });
    }

    for (code, modifiers, action, when) in [
        (
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
            ActionId::new("render.document.find"),
            always as fn(&RenderActionContext) -> bool,
        ),
        (KeyCode::Char('u'), KeyModifiers::CONTROL, ActionId::new("render.input.clear"), always),
        (
            KeyCode::Char('r'),
            KeyModifiers::NONE,
            ActionId::new("render.workspace.refresh"),
            viewer_idle,
        ),
        (KeyCode::Char('n'), KeyModifiers::NONE, ActionId::new("render.search.next"), search_idle),
        (
            KeyCode::Char('N'),
            KeyModifiers::NONE,
            ActionId::new("render.search.previous"),
            search_idle,
        ),
        (KeyCode::Left, KeyModifiers::NONE, ActionId::new("render.history.back"), viewer_idle),
        (KeyCode::Right, KeyModifiers::NONE, ActionId::new("render.history.forward"), viewer_idle),
        (KeyCode::Up, KeyModifiers::NONE, ActionId::new("render.document.scrollUp"), viewer_idle),
        (
            KeyCode::Down,
            KeyModifiers::NONE,
            ActionId::new("render.document.scrollDown"),
            viewer_idle,
        ),
        (KeyCode::PageUp, KeyModifiers::NONE, ActionId::new("render.document.pageUp"), viewer_idle),
        (
            KeyCode::PageDown,
            KeyModifiers::NONE,
            ActionId::new("render.document.pageDown"),
            viewer_idle,
        ),
        (KeyCode::Home, KeyModifiers::NONE, ActionId::new("render.document.home"), viewer_idle),
        (KeyCode::End, KeyModifiers::NONE, ActionId::new("render.document.end"), viewer_idle),
        (KeyCode::Up, KeyModifiers::SHIFT, ActionId::new("render.document.home"), viewer_idle),
        (KeyCode::Down, KeyModifiers::SHIFT, ActionId::new("render.document.end"), viewer_idle),
        (KeyCode::Char('t'), KeyModifiers::CONTROL, TOGGLE_CONTENTS, contents),
        (
            KeyCode::Char('<'),
            KeyModifiers::NONE,
            ActionId::new("render.layout.narrowDocument"),
            split,
        ),
        (
            KeyCode::Char('>'),
            KeyModifiers::NONE,
            ActionId::new("render.layout.widenDocument"),
            split,
        ),
        (KeyCode::Char('='), KeyModifiers::NONE, ActionId::new("render.layout.reset"), split),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action,
            when,
        });
    }
    builder.build()
}

fn enabled(_: &RenderActionContext) -> ActionState {
    ActionState::Enabled
}

fn always(_: &RenderActionContext) -> bool {
    true
}

fn viewer_idle(context: &RenderActionContext) -> bool {
    context.input_empty && !context.menu_open
}

fn search_idle(context: &RenderActionContext) -> bool {
    viewer_idle(context) && context.search_active
}

fn contents(context: &RenderActionContext) -> bool {
    viewer_idle(context) && context.contents_available
}

fn split(context: &RenderActionContext) -> bool {
    viewer_idle(context) && context.split_available
}
