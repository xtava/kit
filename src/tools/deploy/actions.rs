use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeployActionMode {
    Browse,
    Versions,
    Review,
    Preparing,
    Running,
    Summary,
    ModalInput,
    ModalConfirm,
    LayoutDrag,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DeployActionContext {
    pub mode: DeployActionMode,
    pub split_available: bool,
    pub cloudflare_versions: bool,
    pub open_url_available: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeployCommand {
    Quit,
    CancelOrQuit,
    Escape,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    NextRegion,
    PreviousRegion,
    ResetLayout,
    ToggleSelection,
    ToggleAll,
    OpenVersions,
    Preview,
    Activate,
    RefreshVersions,
    DeleteVersion,
    ToggleVersionError,
    NoteOrDecline,
    OpenUrl,
    Confirm,
}

pub(super) type DeployActionRegistry = ActionRegistry<DeployActionContext, DeployCommand>;

pub(super) fn registry() -> Result<DeployActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    let actions = [
        ("deploy.quit", "Quit", DeployCommand::Quit),
        ("deploy.cancelOrQuit", "Cancel or quit", DeployCommand::CancelOrQuit),
        ("deploy.escape", "Back or cancel", DeployCommand::Escape),
        ("deploy.selection.previous", "Move or scroll up", DeployCommand::MoveUp),
        ("deploy.selection.next", "Move or scroll down", DeployCommand::MoveDown),
        ("deploy.region.left", "Focus left region", DeployCommand::MoveLeft),
        ("deploy.region.right", "Focus right region", DeployCommand::MoveRight),
        ("deploy.region.next", "Next region", DeployCommand::NextRegion),
        ("deploy.region.previous", "Previous region", DeployCommand::PreviousRegion),
        ("deploy.layout.reset", "Reset panel widths", DeployCommand::ResetLayout),
        ("deploy.target.toggle", "Toggle target", DeployCommand::ToggleSelection),
        ("deploy.target.toggleAll", "Toggle all targets", DeployCommand::ToggleAll),
        ("deploy.versions.open", "Open versions", DeployCommand::OpenVersions),
        ("deploy.preview.open", "Configure preview", DeployCommand::Preview),
        ("deploy.selection.activate", "Activate", DeployCommand::Activate),
        ("deploy.versions.refresh", "Refresh versions", DeployCommand::RefreshVersions),
        ("deploy.version.delete", "Delete version", DeployCommand::DeleteVersion),
        (
            "deploy.version.toggleError",
            "Toggle error annotation",
            DeployCommand::ToggleVersionError,
        ),
        ("deploy.version.noteOrDecline", "Add note or decline", DeployCommand::NoteOrDecline),
        ("deploy.url.open", "Open URL", DeployCommand::OpenUrl),
        ("deploy.confirm.accept", "Confirm", DeployCommand::Confirm),
    ];
    for (order, (id, title, command)) in actions.into_iter().enumerate() {
        builder.register_action(ActionSpec {
            id: ActionId::new(id),
            title,
            command,
            enablement: enabled,
            command_palette: CommandPalettePlacement::Visible {
                group: "Deploy",
                group_order: 10,
                order: order as i16,
            },
        });
    }

    for (code, modifiers, action, when) in [
        (
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            "deploy.quit",
            quittable as fn(&DeployActionContext) -> bool,
        ),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, "deploy.cancelOrQuit", always),
        (KeyCode::Esc, KeyModifiers::NONE, "deploy.escape", always),
        (KeyCode::Up, KeyModifiers::NONE, "deploy.selection.previous", movable),
        (KeyCode::Char('k'), KeyModifiers::NONE, "deploy.selection.previous", movable),
        (KeyCode::Down, KeyModifiers::NONE, "deploy.selection.next", movable),
        (KeyCode::Char('j'), KeyModifiers::NONE, "deploy.selection.next", movable),
        (KeyCode::Left, KeyModifiers::NONE, "deploy.region.left", split),
        (KeyCode::Right, KeyModifiers::NONE, "deploy.region.right", split),
        (KeyCode::Tab, KeyModifiers::NONE, "deploy.region.next", split),
        (KeyCode::BackTab, KeyModifiers::SHIFT, "deploy.region.previous", split),
        (KeyCode::Char('='), KeyModifiers::NONE, "deploy.layout.reset", split),
        (KeyCode::Char(' '), KeyModifiers::NONE, "deploy.target.toggle", browse),
        (KeyCode::Char('a'), KeyModifiers::NONE, "deploy.target.toggleAll", browse),
        (KeyCode::Char('v'), KeyModifiers::NONE, "deploy.versions.open", browse),
        (KeyCode::Char('p'), KeyModifiers::NONE, "deploy.preview.open", browse),
        (KeyCode::Enter, KeyModifiers::NONE, "deploy.selection.activate", activatable),
        (KeyCode::Char('r'), KeyModifiers::NONE, "deploy.versions.refresh", cloudflare_versions),
        (KeyCode::Char('d'), KeyModifiers::NONE, "deploy.version.delete", versions),
        (KeyCode::Char('e'), KeyModifiers::NONE, "deploy.version.toggleError", versions),
        (KeyCode::Char('n'), KeyModifiers::NONE, "deploy.version.noteOrDecline", note_or_decline),
        (KeyCode::Char('o'), KeyModifiers::NONE, "deploy.url.open", open_url),
        (KeyCode::Char('y'), KeyModifiers::NONE, "deploy.confirm.accept", confirm),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action: ActionId::new(action),
            when,
        });
    }
    builder.build()
}

fn enabled(_: &DeployActionContext) -> ActionState {
    ActionState::Enabled
}

fn always(_: &DeployActionContext) -> bool {
    true
}

fn quittable(context: &DeployActionContext) -> bool {
    !matches!(
        context.mode,
        DeployActionMode::Running | DeployActionMode::ModalInput | DeployActionMode::ModalConfirm
    )
}

fn movable(context: &DeployActionContext) -> bool {
    matches!(
        context.mode,
        DeployActionMode::Browse
            | DeployActionMode::Versions
            | DeployActionMode::Running
            | DeployActionMode::Summary
    )
}

fn split(context: &DeployActionContext) -> bool {
    context.split_available
        && matches!(
            context.mode,
            DeployActionMode::Browse | DeployActionMode::Versions | DeployActionMode::Running
        )
}

fn browse(context: &DeployActionContext) -> bool {
    context.mode == DeployActionMode::Browse
}

fn versions(context: &DeployActionContext) -> bool {
    context.mode == DeployActionMode::Versions
}

fn cloudflare_versions(context: &DeployActionContext) -> bool {
    versions(context) && context.cloudflare_versions
}

fn activatable(context: &DeployActionContext) -> bool {
    matches!(
        context.mode,
        DeployActionMode::Browse
            | DeployActionMode::Versions
            | DeployActionMode::Review
            | DeployActionMode::Summary
            | DeployActionMode::ModalInput
            | DeployActionMode::ModalConfirm
    )
}

fn note_or_decline(context: &DeployActionContext) -> bool {
    matches!(context.mode, DeployActionMode::Versions | DeployActionMode::ModalConfirm)
}

fn open_url(context: &DeployActionContext) -> bool {
    context.open_url_available
}

fn confirm(context: &DeployActionContext) -> bool {
    context.mode == DeployActionMode::ModalConfirm
}
