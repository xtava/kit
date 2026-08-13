use crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::{
    ActionId, ActionRegistry, ActionRegistryBuilder, ActionRegistryError, ActionSpec, ActionState,
    CommandPalettePlacement, KeybindingPlacement, MenuId, MenuPlacement,
};

use super::config::Keybindings;

pub(super) const SKILL_CONTEXT: MenuId = MenuId::new("skills.skill.context");
pub(super) const DASHBOARD_ACTIONS: MenuId = MenuId::new("skills.dashboard.actions");

const CREATE_SKILL: ActionId = ActionId::new("skills.skill.create");
pub(super) const SEARCH: ActionId = ActionId::new("skills.search");
const TOGGLE_PROJECTION: ActionId = ActionId::new("skills.availability.toggle");
const ENABLE_THIS_PROJECT: ActionId = ActionId::new("skills.availability.enableThisProject");
const DISABLE_THIS_PROJECT: ActionId = ActionId::new("skills.availability.disableThisProject");
const ENABLE_ALL_PROJECTS: ActionId = ActionId::new("skills.availability.enableAllProjects");
const DISABLE_ALL_PROJECTS: ActionId = ActionId::new("skills.availability.disableAllProjects");
const DOCTOR: ActionId = ActionId::new("skills.doctor");
const SET_LIBRARY: ActionId = ActionId::new("skills.library.set");
const REFRESH: ActionId = ActionId::new("skills.refresh");
const OPEN_SETTINGS: ActionId = ActionId::new("skills.settings.open");
const HELP: ActionId = ActionId::new("skills.help");
const INSPECT: ActionId = ActionId::new("skills.skill.inspect");
const OPEN_CONTEXT: ActionId = ActionId::new("skills.context.open");
const PREVIOUS_SKILL: ActionId = ActionId::new("skills.skill.previous");
const NEXT_SKILL: ActionId = ActionId::new("skills.skill.next");
const PREVIOUS_PROJECTION: ActionId = ActionId::new("skills.availability.previousDestination");
const NEXT_PROJECTION: ActionId = ActionId::new("skills.availability.nextDestination");
const HISTORY_BACK: ActionId = ActionId::new("skills.history.back");
const HISTORY_FORWARD: ActionId = ActionId::new("skills.history.forward");
const PREVIOUS_TAB: ActionId = ActionId::new("skills.details.previousTab");
const NEXT_TAB: ActionId = ActionId::new("skills.details.nextTab");
const FOCUS_NEXT: ActionId = ActionId::new("skills.focus.next");
const FOCUS_PREVIOUS: ActionId = ActionId::new("skills.focus.previous");
const OPEN_COMMAND_PALETTE: ActionId = ActionId::new("skills.commandPalette.open");
const QUIT: ActionId = ActionId::new("skills.quit");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SkillsRegion {
    Catalog,
    Details,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FocusedProjection {
    Missing,
    Toggleable,
    Unsafe,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BulkProjection {
    Missing,
    Safe,
    Unsafe,
    Unavailable,
}

#[derive(Clone, Copy)]
pub(super) struct SkillsActionContext {
    pub configured: bool,
    pub projection: FocusedProjection,
    pub this_project: BulkProjection,
    pub all_projects: BulkProjection,
    pub region: SkillsRegion,
    pub can_previous_skill: bool,
    pub can_next_skill: bool,
    pub can_history_back: bool,
    pub can_history_forward: bool,
    pub can_focus_next: bool,
    pub can_focus_previous: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SkillsAction {
    CreateSkill,
    Search,
    ToggleProjection,
    EnableThisProject,
    DisableThisProject,
    EnableAllProjects,
    DisableAllProjects,
    Doctor,
    SetLibrary,
    Refresh,
    OpenSettings,
    Help,
    Inspect,
    OpenContext,
    PreviousSkill,
    NextSkill,
    PreviousProjection,
    NextProjection,
    HistoryBack,
    HistoryForward,
    PreviousTab,
    NextTab,
    FocusNext,
    FocusPrevious,
    OpenCommandPalette,
    Quit,
}

impl SkillsAction {
    const ALL: [Self; 26] = [
        Self::CreateSkill,
        Self::Search,
        Self::ToggleProjection,
        Self::EnableThisProject,
        Self::DisableThisProject,
        Self::EnableAllProjects,
        Self::DisableAllProjects,
        Self::Doctor,
        Self::SetLibrary,
        Self::Refresh,
        Self::OpenSettings,
        Self::Help,
        Self::Inspect,
        Self::OpenContext,
        Self::PreviousSkill,
        Self::NextSkill,
        Self::PreviousProjection,
        Self::NextProjection,
        Self::HistoryBack,
        Self::HistoryForward,
        Self::PreviousTab,
        Self::NextTab,
        Self::FocusNext,
        Self::FocusPrevious,
        Self::OpenCommandPalette,
        Self::Quit,
    ];

    const fn id(self) -> ActionId {
        match self {
            Self::CreateSkill => CREATE_SKILL,
            Self::Search => SEARCH,
            Self::ToggleProjection => TOGGLE_PROJECTION,
            Self::EnableThisProject => ENABLE_THIS_PROJECT,
            Self::DisableThisProject => DISABLE_THIS_PROJECT,
            Self::EnableAllProjects => ENABLE_ALL_PROJECTS,
            Self::DisableAllProjects => DISABLE_ALL_PROJECTS,
            Self::Doctor => DOCTOR,
            Self::SetLibrary => SET_LIBRARY,
            Self::Refresh => REFRESH,
            Self::OpenSettings => OPEN_SETTINGS,
            Self::Help => HELP,
            Self::Inspect => INSPECT,
            Self::OpenContext => OPEN_CONTEXT,
            Self::PreviousSkill => PREVIOUS_SKILL,
            Self::NextSkill => NEXT_SKILL,
            Self::PreviousProjection => PREVIOUS_PROJECTION,
            Self::NextProjection => NEXT_PROJECTION,
            Self::HistoryBack => HISTORY_BACK,
            Self::HistoryForward => HISTORY_FORWARD,
            Self::PreviousTab => PREVIOUS_TAB,
            Self::NextTab => NEXT_TAB,
            Self::FocusNext => FOCUS_NEXT,
            Self::FocusPrevious => FOCUS_PREVIOUS,
            Self::OpenCommandPalette => OPEN_COMMAND_PALETTE,
            Self::Quit => QUIT,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::CreateSkill => "Create canonical skill",
            Self::Search => "Search skills",
            Self::ToggleProjection => "Toggle selected availability",
            Self::EnableThisProject => "Enable for this project (Claude Code + Codex)",
            Self::DisableThisProject => "Disable for this project (Claude Code + Codex)",
            Self::EnableAllProjects => "Enable for all projects (Claude Code + Codex)",
            Self::DisableAllProjects => "Disable for all projects (Claude Code + Codex)",
            Self::Doctor => "Diagnose library and availability",
            Self::SetLibrary => "Set canonical library",
            Self::Refresh => "Refresh from disk",
            Self::OpenSettings => "Open settings",
            Self::Help => "Show help",
            Self::Inspect => "Inspect selected skill",
            Self::OpenContext => "Show skill actions",
            Self::PreviousSkill => "Previous skill",
            Self::NextSkill => "Next skill",
            Self::PreviousProjection => "Previous destination",
            Self::NextProjection => "Next destination",
            Self::HistoryBack => "Selection history back",
            Self::HistoryForward => "Selection history forward",
            Self::PreviousTab => "Previous detail tab",
            Self::NextTab => "Next detail tab",
            Self::FocusNext => "Focus next panel",
            Self::FocusPrevious => "Focus previous panel",
            Self::OpenCommandPalette => "Show command palette",
            Self::Quit => "Quit",
        }
    }

    const fn palette(self) -> CommandPalettePlacement {
        if matches!(self, Self::OpenContext) {
            return CommandPalettePlacement::Hidden;
        }
        let (group, group_order, order) = match self {
            Self::CreateSkill => ("Skills", 10, 10),
            Self::Search => ("Skills", 10, 20),
            Self::ToggleProjection => ("Skills", 10, 30),
            Self::EnableThisProject => ("Availability", 20, 10),
            Self::DisableThisProject => ("Availability", 20, 20),
            Self::EnableAllProjects => ("Availability", 20, 30),
            Self::DisableAllProjects => ("Availability", 20, 40),
            Self::Doctor => ("Skills", 10, 40),
            Self::SetLibrary => ("Library", 20, 10),
            Self::Refresh => ("Library", 20, 20),
            Self::PreviousSkill => ("Navigation", 30, 10),
            Self::NextSkill => ("Navigation", 30, 20),
            Self::PreviousProjection => ("Navigation", 30, 30),
            Self::NextProjection => ("Navigation", 30, 40),
            Self::HistoryBack => ("Navigation", 30, 50),
            Self::HistoryForward => ("Navigation", 30, 60),
            Self::PreviousTab => ("Navigation", 30, 70),
            Self::NextTab => ("Navigation", 30, 80),
            Self::FocusNext => ("Navigation", 30, 90),
            Self::FocusPrevious => ("Navigation", 30, 100),
            Self::OpenSettings => ("Manager", 40, 10),
            Self::Help => ("Manager", 40, 20),
            Self::OpenCommandPalette => ("Manager", 40, 30),
            Self::Quit => ("Manager", 40, 40),
            Self::Inspect => ("Navigation", 30, 5),
            Self::OpenContext => ("Navigation", 30, 5),
        };
        CommandPalettePlacement::Visible { group, group_order, order }
    }
}

pub(super) type SkillsActionRegistry = ActionRegistry<SkillsActionContext, SkillsAction>;

pub(super) fn registry(
    keybindings: &Keybindings,
) -> Result<SkillsActionRegistry, ActionRegistryError> {
    let mut builder = ActionRegistryBuilder::new();
    for action in SkillsAction::ALL {
        let enablement = match action {
            SkillsAction::CreateSkill | SkillsAction::Search | SkillsAction::Refresh => configured,
            SkillsAction::ToggleProjection => can_toggle,
            SkillsAction::EnableThisProject | SkillsAction::DisableThisProject => {
                can_this_project_bulk
            }
            SkillsAction::EnableAllProjects | SkillsAction::DisableAllProjects => {
                can_all_projects_bulk
            }
            SkillsAction::Inspect | SkillsAction::OpenContext => has_selection,
            SkillsAction::PreviousSkill => can_previous_skill,
            SkillsAction::NextSkill => can_next_skill,
            SkillsAction::PreviousProjection | SkillsAction::NextProjection => always_enabled,
            SkillsAction::HistoryBack => can_history_back,
            SkillsAction::HistoryForward => can_history_forward,
            SkillsAction::FocusNext => can_focus_next,
            SkillsAction::FocusPrevious => can_focus_previous,
            SkillsAction::Doctor
            | SkillsAction::SetLibrary
            | SkillsAction::OpenSettings
            | SkillsAction::Help
            | SkillsAction::PreviousTab
            | SkillsAction::NextTab
            | SkillsAction::OpenCommandPalette
            | SkillsAction::Quit => always_enabled,
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
        (SkillsAction::ToggleProjection, "availability", 10, 10),
        (SkillsAction::EnableThisProject, "availability", 10, 20),
        (SkillsAction::DisableThisProject, "availability", 10, 30),
        (SkillsAction::EnableAllProjects, "availability", 10, 40),
        (SkillsAction::DisableAllProjects, "availability", 10, 50),
        (SkillsAction::Inspect, "navigation", 20, 10),
        (SkillsAction::Doctor, "diagnostics", 30, 10),
        (SkillsAction::CreateSkill, "library", 40, 10),
        (SkillsAction::SetLibrary, "library", 40, 20),
    ] {
        builder.place_menu(MenuPlacement {
            menu: SKILL_CONTEXT,
            action: action.id(),
            group,
            group_order,
            order,
            when: always,
        });
    }

    for (action, group, group_order, order) in [
        (SkillsAction::ToggleProjection, "availability", 10, 10),
        (SkillsAction::CreateSkill, "skill", 20, 10),
        (SkillsAction::Doctor, "diagnostics", 30, 10),
        (SkillsAction::Refresh, "manager", 40, 10),
        (SkillsAction::SetLibrary, "manager", 40, 20),
        (SkillsAction::OpenSettings, "manager", 40, 30),
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
        (keybindings.command_palette, SkillsAction::OpenCommandPalette),
        (keybindings.create_skill, SkillsAction::CreateSkill),
        (keybindings.search, SkillsAction::Search),
        (keybindings.toggle_projection, SkillsAction::ToggleProjection),
        (keybindings.doctor, SkillsAction::Doctor),
        (keybindings.set_library, SkillsAction::SetLibrary),
        (keybindings.refresh, SkillsAction::Refresh),
        (keybindings.open_settings, SkillsAction::OpenSettings),
        (keybindings.help, SkillsAction::Help),
        (keybindings.previous_skill, SkillsAction::PreviousSkill),
        (keybindings.next_skill, SkillsAction::NextSkill),
        (keybindings.previous_projection, SkillsAction::PreviousProjection),
        (keybindings.next_projection, SkillsAction::NextProjection),
        (keybindings.history_back, SkillsAction::HistoryBack),
        (keybindings.history_forward, SkillsAction::HistoryForward),
        (keybindings.previous_tab, SkillsAction::PreviousTab),
        (keybindings.next_tab, SkillsAction::NextTab),
        (keybindings.focus_next, SkillsAction::FocusNext),
        (keybindings.focus_previous, SkillsAction::FocusPrevious),
        (keybindings.quit, SkillsAction::Quit),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: chord.into(),
            action: action.id(),
            when: match action {
                SkillsAction::PreviousSkill | SkillsAction::NextSkill => catalog_region,
                SkillsAction::PreviousProjection | SkillsAction::NextProjection => catalog_region,
                _ => always,
            },
        });
    }
    for (chord, action) in [
        (
            crate::tui::KeyChord::new(KeyCode::Char('k'), KeyModifiers::NONE),
            SkillsAction::PreviousSkill,
        ),
        (
            crate::tui::KeyChord::new(KeyCode::Char('j'), KeyModifiers::NONE),
            SkillsAction::NextSkill,
        ),
        (crate::tui::KeyChord::new(KeyCode::Enter, KeyModifiers::NONE), SkillsAction::Inspect),
        (crate::tui::KeyChord::new(KeyCode::F(10), KeyModifiers::SHIFT), SkillsAction::OpenContext),
    ] {
        if !keybindings.contains(chord) {
            builder.bind_key(KeybindingPlacement {
                binding: chord.into(),
                action: action.id(),
                when: catalog_region,
            });
        }
    }
    builder.build()
}

fn always(_: &SkillsActionContext) -> bool {
    true
}

fn catalog_region(context: &SkillsActionContext) -> bool {
    context.region == SkillsRegion::Catalog
}

fn always_enabled(_: &SkillsActionContext) -> ActionState {
    ActionState::Enabled
}

fn configured(context: &SkillsActionContext) -> ActionState {
    if context.configured {
        ActionState::Enabled
    } else {
        ActionState::disabled("configure the canonical Skills library first")
    }
}

fn can_toggle(context: &SkillsActionContext) -> ActionState {
    match context.projection {
        FocusedProjection::Toggleable => ActionState::Enabled,
        FocusedProjection::Missing => ActionState::disabled("no skill is selected"),
        FocusedProjection::Unsafe => {
            ActionState::disabled("the selected destination contains foreign or occupied content")
        }
        FocusedProjection::Unavailable => {
            ActionState::disabled("the selected destination is unavailable")
        }
    }
}

fn can_this_project_bulk(context: &SkillsActionContext) -> ActionState {
    can_bulk(context.this_project)
}

fn can_all_projects_bulk(context: &SkillsActionContext) -> ActionState {
    can_bulk(context.all_projects)
}

fn can_bulk(projection: BulkProjection) -> ActionState {
    match projection {
        BulkProjection::Safe => ActionState::Enabled,
        BulkProjection::Missing => ActionState::disabled("no skill is selected"),
        BulkProjection::Unsafe => {
            ActionState::disabled("one or more destinations contain foreign or occupied content")
        }
        BulkProjection::Unavailable => {
            ActionState::disabled("one or more destinations are unavailable")
        }
    }
}

fn has_selection(context: &SkillsActionContext) -> ActionState {
    if context.projection == FocusedProjection::Missing {
        ActionState::disabled("no skill is selected")
    } else {
        ActionState::Enabled
    }
}

fn can_previous_skill(context: &SkillsActionContext) -> ActionState {
    navigation(context.can_previous_skill, "there is no previous skill")
}

fn can_next_skill(context: &SkillsActionContext) -> ActionState {
    navigation(context.can_next_skill, "there is no next skill")
}

fn can_history_back(context: &SkillsActionContext) -> ActionState {
    navigation(context.can_history_back, "selection history has no earlier skill")
}

fn can_history_forward(context: &SkillsActionContext) -> ActionState {
    navigation(context.can_history_forward, "selection history has no later skill")
}

fn can_focus_next(context: &SkillsActionContext) -> ActionState {
    navigation(context.can_focus_next, "there is no next panel")
}

fn can_focus_previous(context: &SkillsActionContext) -> ActionState {
    navigation(context.can_focus_previous, "there is no previous panel")
}

fn navigation(enabled: bool, reason: &'static str) -> ActionState {
    if enabled {
        ActionState::Enabled
    } else {
        ActionState::disabled(reason)
    }
}
