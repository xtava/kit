use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeybindingPlacement, MenuId, MenuPlacement,
};

use super::{
    config::Keybindings, controller::ProjectState, engine::SessionHealth, model::ProjectId,
};

pub(super) const PROJECT_CONTEXT: MenuId = MenuId::new("sync.project.context");
pub(super) const DASHBOARD_ACTIONS: MenuId = MenuId::new("sync.dashboard.actions");

const ADD_PROJECT: ActionId = ActionId::new("sync.project.add");
const TOGGLE_PAUSE: ActionId = ActionId::new("sync.project.togglePause");
const FLUSH: ActionId = ActionId::new("sync.project.flush");
const DOCTOR: ActionId = ActionId::new("sync.project.doctor");
const REMOVE: ActionId = ActionId::new("sync.project.remove");
const REFRESH: ActionId = ActionId::new("sync.projects.refresh");
const OPEN_SETTINGS: ActionId = ActionId::new("sync.settings.open");
const PREVIOUS_PROJECT: ActionId = ActionId::new("sync.project.previous");
const NEXT_PROJECT: ActionId = ActionId::new("sync.project.next");
const HISTORY_BACK: ActionId = ActionId::new("sync.history.back");
const HISTORY_FORWARD: ActionId = ActionId::new("sync.history.forward");
const FOCUS_NEXT: ActionId = ActionId::new("sync.focus.next");
const FOCUS_PREVIOUS: ActionId = ActionId::new("sync.focus.previous");
const OPEN_COMMAND_PALETTE: ActionId = ActionId::new("sync.commandPalette.open");
const QUIT: ActionId = ActionId::new("sync.quit");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SyncRegion {
    Projects,
    Details,
}

#[derive(Clone, Copy)]
pub(super) struct SyncActionContext {
    pub target: Option<ProjectId>,
    pub state: Option<ProjectState>,
    pub region: SyncRegion,
    pub busy: bool,
    pub can_previous: bool,
    pub can_next: bool,
    pub can_history_back: bool,
    pub can_history_forward: bool,
    pub can_focus_next: bool,
    pub can_focus_previous: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SyncAction {
    AddProject,
    TogglePause,
    Flush,
    Doctor,
    Remove,
    Refresh,
    OpenSettings,
    PreviousProject,
    NextProject,
    HistoryBack,
    HistoryForward,
    FocusNext,
    FocusPrevious,
    OpenCommandPalette,
    Quit,
}

impl SyncAction {
    const ALL: [Self; 15] = [
        Self::AddProject,
        Self::TogglePause,
        Self::Flush,
        Self::Doctor,
        Self::Remove,
        Self::Refresh,
        Self::OpenSettings,
        Self::PreviousProject,
        Self::NextProject,
        Self::HistoryBack,
        Self::HistoryForward,
        Self::FocusNext,
        Self::FocusPrevious,
        Self::OpenCommandPalette,
        Self::Quit,
    ];

    const fn id(self) -> ActionId {
        match self {
            Self::AddProject => ADD_PROJECT,
            Self::TogglePause => TOGGLE_PAUSE,
            Self::Flush => FLUSH,
            Self::Doctor => DOCTOR,
            Self::Remove => REMOVE,
            Self::Refresh => REFRESH,
            Self::OpenSettings => OPEN_SETTINGS,
            Self::PreviousProject => PREVIOUS_PROJECT,
            Self::NextProject => NEXT_PROJECT,
            Self::HistoryBack => HISTORY_BACK,
            Self::HistoryForward => HISTORY_FORWARD,
            Self::FocusNext => FOCUS_NEXT,
            Self::FocusPrevious => FOCUS_PREVIOUS,
            Self::OpenCommandPalette => OPEN_COMMAND_PALETTE,
            Self::Quit => QUIT,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::AddProject => "Add Synced Project",
            Self::TogglePause => "Pause or resume",
            Self::Flush => "Sync now",
            Self::Doctor => "Diagnose",
            Self::Remove => "Remove Synced Project",
            Self::Refresh => "Refresh",
            Self::OpenSettings => "Open settings",
            Self::PreviousProject => "Previous project",
            Self::NextProject => "Next project",
            Self::HistoryBack => "Selection history back",
            Self::HistoryForward => "Selection history forward",
            Self::FocusNext => "Focus next panel",
            Self::FocusPrevious => "Focus previous panel",
            Self::OpenCommandPalette => "Show command palette",
            Self::Quit => "Quit",
        }
    }

    const fn palette(self) -> CommandPalettePlacement {
        let (group, group_order, order) = match self {
            Self::AddProject => ("Project", 10, 10),
            Self::TogglePause => ("Project", 10, 20),
            Self::Flush => ("Project", 10, 30),
            Self::Doctor => ("Project", 10, 40),
            Self::Remove => ("Project", 10, 50),
            Self::PreviousProject => ("Navigation", 20, 10),
            Self::NextProject => ("Navigation", 20, 20),
            Self::HistoryBack => ("Navigation", 20, 30),
            Self::HistoryForward => ("Navigation", 20, 40),
            Self::FocusNext => ("Navigation", 20, 50),
            Self::FocusPrevious => ("Navigation", 20, 60),
            Self::Refresh => ("Synced Projects", 30, 10),
            Self::OpenSettings => ("Synced Projects", 30, 20),
            Self::OpenCommandPalette => ("Synced Projects", 30, 30),
            Self::Quit => ("Synced Projects", 30, 40),
        };
        CommandPalettePlacement::Visible { group, group_order, order }
    }
}

pub(super) type SyncActionRegistry = ActionRegistry<SyncActionContext, SyncAction>;

pub(super) fn registry(
    keybindings: &Keybindings,
) -> Result<SyncActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    for action in SyncAction::ALL {
        let enablement = match action {
            SyncAction::AddProject | SyncAction::Doctor | SyncAction::Refresh => available,
            SyncAction::OpenSettings | SyncAction::OpenCommandPalette | SyncAction::Quit => {
                always_enabled
            }
            SyncAction::TogglePause => can_toggle_pause,
            SyncAction::Flush => can_flush,
            SyncAction::Remove => has_target,
            SyncAction::PreviousProject => can_previous,
            SyncAction::NextProject => can_next,
            SyncAction::HistoryBack => can_history_back,
            SyncAction::HistoryForward => can_history_forward,
            SyncAction::FocusNext => can_focus_next,
            SyncAction::FocusPrevious => can_focus_previous,
        };
        builder.register_action(ActionSpec {
            id: action.id(),
            title: action.title(),
            command: action,
            enablement,
            command_palette: action.palette(),
        });
    }
    for (action, group, group_order, order) in [
        (SyncAction::TogglePause, "lifecycle", 10, 10),
        (SyncAction::Flush, "lifecycle", 10, 20),
        (SyncAction::Doctor, "recovery", 20, 10),
        (SyncAction::Remove, "destructive", 30, 10),
    ] {
        builder.place_menu(MenuPlacement {
            menu: PROJECT_CONTEXT,
            action: action.id(),
            group,
            group_order,
            order,
            when: always,
        });
    }
    for (action, group, group_order, order) in [
        (SyncAction::AddProject, "project", 10, 10),
        (SyncAction::TogglePause, "project", 10, 20),
        (SyncAction::Flush, "project", 10, 30),
        (SyncAction::Doctor, "recovery", 20, 10),
        (SyncAction::Refresh, "dashboard", 30, 10),
        (SyncAction::OpenSettings, "dashboard", 30, 20),
        (SyncAction::Remove, "destructive", 40, 10),
    ] {
        builder.place_menu(MenuPlacement {
            menu: DASHBOARD_ACTIONS,
            action: action.id(),
            group,
            group_order,
            order,
            when: always,
        });
    }
    for (chord, action) in [
        (keybindings.command_palette, SyncAction::OpenCommandPalette),
        (keybindings.add_project, SyncAction::AddProject),
        (keybindings.toggle_pause, SyncAction::TogglePause),
        (keybindings.flush, SyncAction::Flush),
        (keybindings.doctor, SyncAction::Doctor),
        (keybindings.remove, SyncAction::Remove),
        (keybindings.refresh, SyncAction::Refresh),
        (keybindings.open_settings, SyncAction::OpenSettings),
        (keybindings.previous_project, SyncAction::PreviousProject),
        (keybindings.next_project, SyncAction::NextProject),
        (keybindings.history_back, SyncAction::HistoryBack),
        (keybindings.history_forward, SyncAction::HistoryForward),
        (keybindings.focus_next, SyncAction::FocusNext),
        (keybindings.focus_previous, SyncAction::FocusPrevious),
        (keybindings.quit, SyncAction::Quit),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: chord.into(),
            action: action.id(),
            when: if matches!(action, SyncAction::PreviousProject | SyncAction::NextProject) {
                projects_region
            } else {
                always
            },
        });
    }
    builder.build()
}

fn always(_: &SyncActionContext) -> bool {
    true
}

fn always_enabled(_: &SyncActionContext) -> ActionState {
    ActionState::Enabled
}

fn projects_region(context: &SyncActionContext) -> bool {
    context.region == SyncRegion::Projects
}

fn available(context: &SyncActionContext) -> ActionState {
    if context.busy {
        ActionState::disabled("an operation is already running")
    } else {
        ActionState::Enabled
    }
}

fn has_target(context: &SyncActionContext) -> ActionState {
    if context.target.is_none() {
        ActionState::disabled("no Synced Project is selected")
    } else {
        available(context)
    }
}

fn can_toggle_pause(context: &SyncActionContext) -> ActionState {
    if !matches!(
        context.state,
        Some(ProjectState::Session(_) | ProjectState::NeedsPause | ProjectState::NeedsResume)
    ) {
        ActionState::disabled("the selected project has no current session")
    } else {
        available(context)
    }
}

fn can_flush(context: &SyncActionContext) -> ActionState {
    match context.state {
        Some(ProjectState::Session(SessionHealth::Paused) | ProjectState::NeedsResume) => {
            ActionState::disabled("resume the selected project before syncing")
        }
        Some(ProjectState::Session(_)) => available(context),
        _ => ActionState::disabled("the selected project has no ready session"),
    }
}

fn can_previous(context: &SyncActionContext) -> ActionState {
    navigation_availability(context.can_previous, "there is no previous project")
}

fn can_next(context: &SyncActionContext) -> ActionState {
    navigation_availability(context.can_next, "there is no next project")
}

fn can_history_back(context: &SyncActionContext) -> ActionState {
    navigation_availability(context.can_history_back, "selection history has no earlier project")
}

fn can_history_forward(context: &SyncActionContext) -> ActionState {
    navigation_availability(context.can_history_forward, "selection history has no later project")
}

fn can_focus_next(context: &SyncActionContext) -> ActionState {
    navigation_availability(context.can_focus_next, "there is no next panel")
}

fn can_focus_previous(context: &SyncActionContext) -> ActionState {
    navigation_availability(context.can_focus_previous, "there is no previous panel")
}

fn navigation_availability(enabled: bool, reason: &'static str) -> ActionState {
    if enabled {
        ActionState::Enabled
    } else {
        ActionState::disabled(reason)
    }
}
