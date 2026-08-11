use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeyChord, KeybindingPlacement,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BuildActionMode {
    Select,
    Running,
    Terminal,
    EvidenceList,
    EvidenceInspect,
    EvidenceConfirm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BuildActionContext {
    pub mode: BuildActionMode,
    pub activate_available: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BuildCommand {
    Quit,
    CancelOrQuit,
    Escape,
    OpenEvidence,
    MoveUp,
    MoveDown,
    Activate,
    PageUp,
    PageDown,
    NextPane,
    PreviousPane,
    Cancel,
    ChooseWorkflow,
    Back,
    Forget,
    Confirm,
    Decline,
}

pub(super) type BuildActionRegistry = ActionRegistry<BuildActionContext, BuildCommand>;

pub(super) fn registry() -> Result<BuildActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    let actions = [
        (
            "build.quit",
            "Quit",
            BuildCommand::Quit,
            enabled as fn(&BuildActionContext) -> ActionState,
        ),
        ("build.cancelOrQuit", "Cancel or quit", BuildCommand::CancelOrQuit, enabled),
        ("build.escape", "Back or quit", BuildCommand::Escape, enabled),
        ("build.evidence.open", "Open evidence", BuildCommand::OpenEvidence, enabled),
        ("build.selection.previous", "Move up", BuildCommand::MoveUp, enabled),
        ("build.selection.next", "Move down", BuildCommand::MoveDown, enabled),
        ("build.selection.activate", "Activate", BuildCommand::Activate, activate),
        ("build.viewport.pageUp", "Page up", BuildCommand::PageUp, enabled),
        ("build.viewport.pageDown", "Page down", BuildCommand::PageDown, enabled),
        ("build.pane.next", "Next pane", BuildCommand::NextPane, enabled),
        ("build.pane.previous", "Previous pane", BuildCommand::PreviousPane, enabled),
        ("build.run.cancel", "Cancel build", BuildCommand::Cancel, enabled),
        ("build.workflow.choose", "Choose workflow", BuildCommand::ChooseWorkflow, enabled),
        ("build.evidence.back", "Back", BuildCommand::Back, enabled),
        ("build.evidence.forget", "Forget evidence", BuildCommand::Forget, enabled),
        ("build.confirm.accept", "Confirm", BuildCommand::Confirm, enabled),
        ("build.confirm.decline", "Decline", BuildCommand::Decline, enabled),
    ];
    for (order, (id, title, command, enablement)) in actions.into_iter().enumerate() {
        builder.register_action(ActionSpec {
            id: ActionId::new(id),
            title,
            command,
            enablement,
            command_palette: CommandPalettePlacement::Visible {
                group: "Build",
                group_order: 10,
                order: order as i16,
            },
        });
    }
    for (code, modifiers, action, when) in [
        (
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            "build.quit",
            always as fn(&BuildActionContext) -> bool,
        ),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, "build.cancelOrQuit", always),
        (KeyCode::Esc, KeyModifiers::NONE, "build.escape", always),
        (KeyCode::Char('e'), KeyModifiers::NONE, "build.evidence.open", evidence_available),
        (KeyCode::Up, KeyModifiers::NONE, "build.selection.previous", movable),
        (KeyCode::Char('k'), KeyModifiers::NONE, "build.selection.previous", movable),
        (KeyCode::Down, KeyModifiers::NONE, "build.selection.next", movable),
        (KeyCode::Char('j'), KeyModifiers::NONE, "build.selection.next", movable),
        (KeyCode::Enter, KeyModifiers::NONE, "build.selection.activate", activatable_mode),
        (KeyCode::Char('i'), KeyModifiers::NONE, "build.selection.activate", evidence_list),
        (KeyCode::PageUp, KeyModifiers::NONE, "build.viewport.pageUp", pageable),
        (KeyCode::PageDown, KeyModifiers::NONE, "build.viewport.pageDown", pageable),
        (KeyCode::Tab, KeyModifiers::NONE, "build.pane.next", run_surface),
        (KeyCode::Right, KeyModifiers::NONE, "build.pane.next", run_surface),
        (KeyCode::Char('l'), KeyModifiers::NONE, "build.pane.next", run_surface),
        (KeyCode::BackTab, KeyModifiers::SHIFT, "build.pane.previous", run_surface),
        (KeyCode::Left, KeyModifiers::NONE, "build.pane.previous", run_surface),
        (KeyCode::Char('h'), KeyModifiers::NONE, "build.pane.previous", run_surface),
        (KeyCode::Char('c'), KeyModifiers::NONE, "build.run.cancel", running),
        (KeyCode::Char('r'), KeyModifiers::NONE, "build.workflow.choose", terminal),
        (KeyCode::Char('b'), KeyModifiers::NONE, "build.evidence.back", evidence_surface),
        (KeyCode::Char('f'), KeyModifiers::NONE, "build.evidence.forget", evidence_list),
        (KeyCode::Char('y'), KeyModifiers::NONE, "build.confirm.accept", evidence_confirm),
        (KeyCode::Char('n'), KeyModifiers::NONE, "build.confirm.decline", evidence_confirm),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action: ActionId::new(action),
            when,
        });
    }
    builder.build()
}

fn enabled(_: &BuildActionContext) -> ActionState {
    ActionState::Enabled
}

fn activate(context: &BuildActionContext) -> ActionState {
    if context.activate_available {
        ActionState::Enabled
    } else {
        ActionState::disabled("the selected item cannot be activated")
    }
}

fn always(_: &BuildActionContext) -> bool {
    true
}

fn running(context: &BuildActionContext) -> bool {
    context.mode == BuildActionMode::Running
}

fn terminal(context: &BuildActionContext) -> bool {
    context.mode == BuildActionMode::Terminal
}

fn run_surface(context: &BuildActionContext) -> bool {
    matches!(context.mode, BuildActionMode::Running | BuildActionMode::Terminal)
}

fn evidence_available(context: &BuildActionContext) -> bool {
    matches!(context.mode, BuildActionMode::Select | BuildActionMode::Terminal)
}

fn evidence_surface(context: &BuildActionContext) -> bool {
    matches!(
        context.mode,
        BuildActionMode::EvidenceList
            | BuildActionMode::EvidenceInspect
            | BuildActionMode::EvidenceConfirm
    )
}

fn evidence_list(context: &BuildActionContext) -> bool {
    context.mode == BuildActionMode::EvidenceList
}

fn evidence_confirm(context: &BuildActionContext) -> bool {
    context.mode == BuildActionMode::EvidenceConfirm
}

fn movable(context: &BuildActionContext) -> bool {
    !matches!(context.mode, BuildActionMode::EvidenceConfirm)
}

fn pageable(context: &BuildActionContext) -> bool {
    matches!(
        context.mode,
        BuildActionMode::Running | BuildActionMode::Terminal | BuildActionMode::EvidenceInspect
    )
}

fn activatable_mode(context: &BuildActionContext) -> bool {
    matches!(context.mode, BuildActionMode::Select | BuildActionMode::EvidenceList)
}
