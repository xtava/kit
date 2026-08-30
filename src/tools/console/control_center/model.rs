use crate::{
    tailscale::{Node, OperatingSystem},
    tui::ActionId,
};

use super::super::service::ConsoleStatus;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ControlCenterState {
    pub(crate) discovery: MachineDiscoveryState,
    pub(crate) machines: Vec<MachineState>,
    pub(crate) selected_machine: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MachineDiscoveryState {
    Discovering,
    Ready,
    AuthenticationRequired,
    Unavailable { detail: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MachineState {
    pub(crate) identity: MachineIdentity,
    pub(crate) role: MachineRole,
    pub(crate) operating_system: OperatingSystem,
    pub(crate) reachability: MachineReachability,
    pub(crate) console: ConsoleProbeState,
    pub(crate) compatibility: MachineCompatibility,
    pub(crate) operation: MachineOperationState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MachineIdentity {
    pub(crate) stable_node_id: String,
    pub(crate) display_name: String,
    pub(crate) selector: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MachineRole {
    ThisMachine,
    Peer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MachineReachability {
    Online,
    Offline,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConsoleProbeState {
    Waiting,
    Probing,
    Complete(Box<ConsoleStatus>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MachineCompatibility {
    Unknown,
    Current,
    UpdateAvailable { version: String },
    DifferentBuild,
    DirtyBuild,
    CodecIncompatible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MachineOperationState {
    Idle,
    Running(MachineOperation),
    Failed { operation: MachineOperation, detail: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MachineOperation {
    Probe,
    Authenticate,
    SetupOrRepair,
    Update,
    Restart,
    Stop,
    Connect,
}

impl MachineOperation {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Probe => "Machine check",
            Self::Authenticate => "Authentication",
            Self::SetupOrRepair => "Console setup or repair",
            Self::Update => "Kit update",
            Self::Restart => "Console service restart",
            Self::Stop => "Console service stop",
            Self::Connect => "Connection",
        }
    }

    pub(super) const fn completed_label(self) -> &'static str {
        self.label()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum MachineAction {
    Connect,
    NewSession,
    Refresh,
    AuthenticateTailscale,
    StartConsole,
    SetupOrRepair,
    UpdateKit,
    RestartService,
    StopService,
    ShowDetails,
    CancelOperation,
}

impl MachineAction {
    pub(super) const ALL: [Self; 11] = [
        Self::Connect,
        Self::NewSession,
        Self::Refresh,
        Self::AuthenticateTailscale,
        Self::StartConsole,
        Self::SetupOrRepair,
        Self::UpdateKit,
        Self::RestartService,
        Self::StopService,
        Self::ShowDetails,
        Self::CancelOperation,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActionEffectOwner {
    ControlCenter,
    Tailscale,
    ConsoleService,
    KitUpdater,
    ConsoleRouter,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActionKeyboardAccess {
    Primary,
    NewSession,
    CommandPalette,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActionResultKind {
    RefreshControlCenter,
    RemainInControlCenter,
    OpenAuthentication,
    ConnectTerminal,
    ConfirmThenRefresh,
    ShowDetails,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MachineActionContract {
    pub(crate) action: MachineAction,
    pub(crate) title: &'static str,
    pub(crate) keyboard: ActionKeyboardAccess,
    pub(crate) mouse: bool,
    pub(crate) context_menu: bool,
    pub(crate) command_palette: bool,
    pub(crate) effect_owner: ActionEffectOwner,
    pub(crate) operation: Option<MachineOperation>,
    pub(crate) result: ActionResultKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MachineRowWidth {
    Compact,
    Normal,
    Wide,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MachineRowProjection {
    pub(crate) name: String,
    pub(crate) display_name: Option<String>,
    pub(crate) role: Option<&'static str>,
    pub(crate) operating_system: Option<String>,
    pub(crate) status: String,
    pub(crate) sessions: Option<String>,
    pub(crate) build: Option<String>,
    pub(crate) primary_action: MachineAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ControlCenterStory {
    Discovering,
    AuthenticationRequired,
    Empty,
    Machines,
    Unavailable { detail: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ControlCenterOutcome {
    Connect(MachineConnectionRequest),
    Updated,
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MachineConnectionRequest {
    Local { create_session: bool },
    Remote { machine: MachineIdentity, create_session: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectedSessionOutcome {
    ReturnToControlCenter,
    Quit,
}

impl ControlCenterState {
    pub(crate) fn story(&self) -> ControlCenterStory {
        match &self.discovery {
            MachineDiscoveryState::Discovering if self.machines.is_empty() => {
                ControlCenterStory::Discovering
            }
            MachineDiscoveryState::AuthenticationRequired => {
                ControlCenterStory::AuthenticationRequired
            }
            MachineDiscoveryState::Unavailable { detail } => {
                ControlCenterStory::Unavailable { detail: detail.clone() }
            }
            MachineDiscoveryState::Discovering | MachineDiscoveryState::Ready
                if self.machines.is_empty() =>
            {
                ControlCenterStory::Empty
            }
            MachineDiscoveryState::Discovering | MachineDiscoveryState::Ready => {
                ControlCenterStory::Machines
            }
        }
    }
}

impl MachineState {
    pub(crate) fn from_tailnet_node(node: &Node, role: MachineRole) -> Self {
        let selector =
            if node.dns_name.is_empty() { node.id.clone() } else { node.dns_name.clone() };
        Self {
            identity: MachineIdentity {
                stable_node_id: node.id.clone(),
                display_name: node.display_name().to_owned(),
                selector,
            },
            role,
            operating_system: node.operating_system.clone(),
            reachability: if role == MachineRole::ThisMachine || node.online {
                MachineReachability::Online
            } else {
                MachineReachability::Offline
            },
            console: if role == MachineRole::ThisMachine {
                ConsoleProbeState::Probing
            } else {
                ConsoleProbeState::Waiting
            },
            compatibility: MachineCompatibility::Unknown,
            operation: MachineOperationState::Idle,
        }
    }

    pub(crate) fn primary_action(&self) -> MachineAction {
        if matches!(self.operation, MachineOperationState::Running(_)) {
            return MachineAction::ShowDetails;
        }
        if self.role == MachineRole::Peer {
            return match self.reachability {
                MachineReachability::Online => MachineAction::Connect,
                MachineReachability::Offline => MachineAction::Refresh,
            };
        }
        match &self.console {
            ConsoleProbeState::Waiting | ConsoleProbeState::Probing => MachineAction::ShowDetails,
            ConsoleProbeState::Complete(status) => primary_action_for_status(status),
        }
    }

    pub(crate) fn available_actions(&self) -> Vec<MachineAction> {
        if self.role == MachineRole::Peer {
            return match self.reachability {
                MachineReachability::Online => vec![
                    MachineAction::Connect,
                    MachineAction::NewSession,
                    MachineAction::Refresh,
                    MachineAction::ShowDetails,
                ],
                MachineReachability::Offline => {
                    vec![MachineAction::Refresh, MachineAction::ShowDetails]
                }
            };
        }
        if !matches!(
            self.operation,
            MachineOperationState::Idle | MachineOperationState::Failed { .. }
        ) {
            return vec![MachineAction::ShowDetails];
        }

        let mut actions = vec![self.primary_action(), MachineAction::Refresh];
        if matches!(self.complete_status(), Some(ConsoleStatus::Ready { .. })) {
            actions.extend([
                MachineAction::NewSession,
                MachineAction::RestartService,
                MachineAction::StopService,
            ]);
        }
        if matches!(
            self.compatibility,
            MachineCompatibility::UpdateAvailable { .. }
                | MachineCompatibility::DifferentBuild
                | MachineCompatibility::DirtyBuild
                | MachineCompatibility::CodecIncompatible
        ) && self.update_allowed()
        {
            actions.push(MachineAction::UpdateKit);
        }
        actions.push(MachineAction::ShowDetails);
        actions.sort_by_key(|action| action.contract().title);
        actions.dedup();
        actions
    }

    pub(crate) fn row(&self, width: MachineRowWidth) -> MachineRowProjection {
        let status = self.status_label().to_owned();
        let (sessions, build) = match self.complete_status() {
            Some(ConsoleStatus::Ready { sessions, build, .. }) => (
                Some(format!("{sessions} {}", if *sessions == 1 { "session" } else { "sessions" })),
                Some(build_label(build)),
            ),
            _ => (None, None),
        };
        MachineRowProjection {
            name: self.identity.selector.clone(),
            display_name: (width != MachineRowWidth::Compact
                && self.identity.display_name != self.identity.selector)
                .then(|| self.identity.display_name.clone()),
            role: (width != MachineRowWidth::Compact).then_some(match self.role {
                MachineRole::ThisMachine => "this machine",
                MachineRole::Peer => "peer",
            }),
            operating_system: (width != MachineRowWidth::Compact)
                .then(|| self.operating_system.label().to_owned()),
            status,
            sessions: (width != MachineRowWidth::Compact).then_some(sessions).flatten(),
            build: (width == MachineRowWidth::Wide).then_some(build).flatten(),
            primary_action: self.primary_action(),
        }
    }

    pub(super) fn details(&self) -> Vec<(&'static str, String)> {
        let mut details = vec![
            ("Machine", self.identity.selector.clone()),
            ("Name", self.identity.display_name.clone()),
            ("Stable node ID", self.identity.stable_node_id.clone()),
            (
                "Role",
                match self.role {
                    MachineRole::ThisMachine => "This machine",
                    MachineRole::Peer => "Peer",
                }
                .to_owned(),
            ),
            ("Operating system", self.operating_system.label().to_owned()),
            ("Status", self.status_label().to_owned()),
        ];
        match self.complete_status() {
            Some(ConsoleStatus::Ready { sessions, build, .. }) => {
                details.push(("Sessions", sessions.to_string()));
                details.push(("Kit build", build_label(build)));
            }
            Some(
                status @ (ConsoleStatus::TailnetEndpointUnavailable { .. }
                | ConsoleStatus::TailnetAccessDenied { .. }
                | ConsoleStatus::TailnetProtocolIncompatible { .. }),
            ) => {
                if let ConsoleStatus::TailnetEndpointUnavailable { detail, .. }
                | ConsoleStatus::TailnetProtocolIncompatible { detail, .. } = status
                {
                    details.push(("Problem", detail.clone()));
                }
                if let Some(recovery) = status.recovery() {
                    details.push(("Next step", recovery.to_string()));
                }
            }
            _ => {}
        }
        if let MachineOperationState::Failed { detail, .. } = &self.operation {
            details.push(("Last operation", detail.clone()));
        }
        details
    }

    fn status_label(&self) -> &'static str {
        if matches!(self.operation, MachineOperationState::Running(_)) {
            return "working";
        }
        if matches!(self.operation, MachineOperationState::Failed { .. }) {
            return "failed";
        }
        if self.role == MachineRole::Peer {
            return match self.reachability {
                MachineReachability::Online => "online",
                MachineReachability::Offline => "offline",
            };
        }
        match self.compatibility {
            MachineCompatibility::UpdateAvailable { .. } => return "update available",
            MachineCompatibility::DifferentBuild => return "different build",
            MachineCompatibility::DirtyBuild => return "dirty build",
            MachineCompatibility::CodecIncompatible => return "incompatible",
            MachineCompatibility::Unknown | MachineCompatibility::Current => {}
        }
        match &self.console {
            ConsoleProbeState::Waiting => "waiting",
            ConsoleProbeState::Probing => "checking",
            ConsoleProbeState::Complete(status) => status_label(status),
        }
    }

    pub(super) fn update_allowed(&self) -> bool {
        self.role == MachineRole::ThisMachine
            && matches!(self.complete_status(), Some(ConsoleStatus::Ready { sessions: 0, .. }))
    }

    pub(super) fn complete_status(&self) -> Option<&ConsoleStatus> {
        match &self.console {
            ConsoleProbeState::Complete(status) => Some(status.as_ref()),
            ConsoleProbeState::Waiting | ConsoleProbeState::Probing => None,
        }
    }
}

impl MachineAction {
    pub(super) const fn id(self) -> ActionId {
        ActionId::new(match self {
            Self::Connect => "console.machine.connect",
            Self::NewSession => "console.machine.newSession",
            Self::Refresh => "console.machine.refresh",
            Self::AuthenticateTailscale => "console.machine.authenticateTailscale",
            Self::StartConsole => "console.machine.startConsole",
            Self::SetupOrRepair => "console.machine.setupOrRepair",
            Self::UpdateKit => "console.machine.updateKit",
            Self::RestartService => "console.machine.restartService",
            Self::StopService => "console.machine.stopService",
            Self::ShowDetails => "console.machine.showDetails",
            Self::CancelOperation => "console.machine.cancelOperation",
        })
    }

    pub(crate) const fn contract(self) -> MachineActionContract {
        let (title, keyboard, effect_owner, operation, result) = match self {
            Self::Connect => (
                "Connect",
                ActionKeyboardAccess::Primary,
                ActionEffectOwner::ConsoleRouter,
                Some(MachineOperation::Connect),
                ActionResultKind::ConnectTerminal,
            ),
            Self::NewSession => (
                "New session",
                ActionKeyboardAccess::NewSession,
                ActionEffectOwner::ConsoleRouter,
                Some(MachineOperation::Connect),
                ActionResultKind::ConnectTerminal,
            ),
            Self::Refresh => (
                "Refresh",
                ActionKeyboardAccess::CommandPalette,
                ActionEffectOwner::ControlCenter,
                Some(MachineOperation::Probe),
                ActionResultKind::RefreshControlCenter,
            ),
            Self::AuthenticateTailscale => (
                "Authenticate Tailscale",
                ActionKeyboardAccess::Primary,
                ActionEffectOwner::Tailscale,
                Some(MachineOperation::Authenticate),
                ActionResultKind::OpenAuthentication,
            ),
            Self::StartConsole => (
                "Start Console",
                ActionKeyboardAccess::Primary,
                ActionEffectOwner::ConsoleService,
                Some(MachineOperation::SetupOrRepair),
                ActionResultKind::ConfirmThenRefresh,
            ),
            Self::SetupOrRepair => (
                "Setup or repair Console",
                ActionKeyboardAccess::Primary,
                ActionEffectOwner::ConsoleService,
                Some(MachineOperation::SetupOrRepair),
                ActionResultKind::ConfirmThenRefresh,
            ),
            Self::UpdateKit => (
                "Update Kit",
                ActionKeyboardAccess::Primary,
                ActionEffectOwner::KitUpdater,
                Some(MachineOperation::Update),
                ActionResultKind::ConfirmThenRefresh,
            ),
            Self::RestartService => (
                "Restart Console service",
                ActionKeyboardAccess::CommandPalette,
                ActionEffectOwner::ConsoleService,
                Some(MachineOperation::Restart),
                ActionResultKind::ConfirmThenRefresh,
            ),
            Self::StopService => (
                "Stop Console service",
                ActionKeyboardAccess::CommandPalette,
                ActionEffectOwner::ConsoleService,
                Some(MachineOperation::Stop),
                ActionResultKind::ConfirmThenRefresh,
            ),
            Self::ShowDetails => (
                "Show machine details",
                ActionKeyboardAccess::CommandPalette,
                ActionEffectOwner::ControlCenter,
                None,
                ActionResultKind::ShowDetails,
            ),
            Self::CancelOperation => (
                "Cancel operation",
                ActionKeyboardAccess::Primary,
                ActionEffectOwner::ControlCenter,
                None,
                ActionResultKind::RemainInControlCenter,
            ),
        };
        MachineActionContract {
            action: self,
            title,
            keyboard,
            mouse: true,
            context_menu: true,
            command_palette: true,
            effect_owner,
            operation,
            result,
        }
    }
}

fn primary_action_for_status(status: &ConsoleStatus) -> MachineAction {
    use super::super::service::ConsoleRecovery;

    match status.recovery() {
        None => MachineAction::Connect,
        Some(ConsoleRecovery::AuthenticateTailscale) => MachineAction::AuthenticateTailscale,
        Some(
            ConsoleRecovery::InstallTailscale
            | ConsoleRecovery::StartTailscale
            | ConsoleRecovery::RestoreTailscaleAccess
            | ConsoleRecovery::UpdateTailscale
            | ConsoleRecovery::BringPeerOnline
            | ConsoleRecovery::Retry,
        ) => MachineAction::Refresh,
        Some(
            ConsoleRecovery::RunSetupOnTarget { .. }
            | ConsoleRecovery::GrantTailnetAccess { .. }
            | ConsoleRecovery::UpdateTarget { .. },
        ) => MachineAction::ShowDetails,
        Some(
            ConsoleRecovery::RunSetup
            | ConsoleRecovery::RestoreServiceManager
            | ConsoleRecovery::RemoveForeignServiceDefinition
            | ConsoleRecovery::RemoveRejectedSocket,
        ) => {
            if matches!(status, ConsoleStatus::Stopped { .. }) {
                MachineAction::StartConsole
            } else {
                MachineAction::SetupOrRepair
            }
        }
        Some(ConsoleRecovery::InspectServiceLog | ConsoleRecovery::CloseSessions) => {
            MachineAction::ShowDetails
        }
    }
}

fn status_label(status: &ConsoleStatus) -> &'static str {
    match status {
        ConsoleStatus::Ready { .. } => "ready",
        ConsoleStatus::NeedsTailscaleLogin => "login required",
        ConsoleStatus::TailscaleCliUnavailable { .. } => "Tailscale unavailable",
        ConsoleStatus::TailscaleDaemonUnavailable { .. } => "Tailscale stopped",
        ConsoleStatus::TailscalePermissionDenied { .. } => "permission denied",
        ConsoleStatus::TailscaleUnsupported { .. } => "unsupported",
        ConsoleStatus::PeerOffline { .. } => "offline",
        ConsoleStatus::TailnetEndpointUnavailable { .. } => "endpoint unavailable",
        ConsoleStatus::TailnetAccessDenied { .. } => "access denied",
        ConsoleStatus::TailnetProtocolIncompatible { .. } => "target update required",
        ConsoleStatus::NotInstalled { .. } => "setup required",
        ConsoleStatus::Stopped { .. } => "stopped",
        ConsoleStatus::ServiceFailed { .. } => "service failed",
        ConsoleStatus::ServiceUnavailable { .. } => "service unavailable",
        ConsoleStatus::WrongOwner { .. } => "ownership mismatch",
        ConsoleStatus::SocketMissing { .. } => "starting",
        ConsoleStatus::SocketStale { .. } => "stale",
        ConsoleStatus::SocketRejected { .. } => "socket rejected",
        ConsoleStatus::CodecIncompatible { .. } => "update required",
        ConsoleStatus::ActivationDeferred { .. } => "activation deferred",
        ConsoleStatus::RepairBusy { .. } => "repair busy",
        ConsoleStatus::MuxUnavailable { .. } => "terminal unavailable",
    }
}

fn build_label(build: &wezterm_codec::BuildIdentity) -> String {
    let revision = build
        .source_revision
        .as_deref()
        .map(|revision| &revision[..revision.len().min(8)])
        .unwrap_or("unknown");
    let dirty = if build.source_dirty == Some(true) { " dirty" } else { "" };
    format!("{} {revision}{dirty}", build.version)
}

pub(super) fn compatibility_for_status(
    expected: &wezterm_codec::BuildIdentity,
    status: &ConsoleStatus,
) -> MachineCompatibility {
    match status {
        ConsoleStatus::CodecIncompatible { .. } => MachineCompatibility::CodecIncompatible,
        ConsoleStatus::Ready { build, .. } => compatibility_for_build(expected, build),
        _ => MachineCompatibility::Unknown,
    }
}

pub(super) fn compatibility_for_build(
    expected: &wezterm_codec::BuildIdentity,
    actual: &wezterm_codec::BuildIdentity,
) -> MachineCompatibility {
    if actual.source_dirty == Some(true) {
        MachineCompatibility::DirtyBuild
    } else if actual == expected {
        MachineCompatibility::Current
    } else if actual.version != expected.version {
        MachineCompatibility::UpdateAvailable { version: expected.version.clone() }
    } else {
        MachineCompatibility::DifferentBuild
    }
}
