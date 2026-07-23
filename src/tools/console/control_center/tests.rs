use std::path::PathBuf;

use crate::{
    tailscale::Node,
    tools::console::service::{ConsoleServicePlatform, ConsoleStatus, RemoteFailureKind},
};

use super::model::{
    ActionEffectOwner, ConnectedSessionOutcome, ConsoleProbeState, ControlCenterOutcome,
    ControlCenterState, ControlCenterStory, MachineAction, MachineConnectionRequest,
    MachineDiscoveryState, MachineRole, MachineRowWidth, MachineState, UnixUserState,
};

fn peer(online: bool) -> Node {
    Node {
        id: "node-mac".to_owned(),
        dns_name: "tvxm.tail.example".to_owned(),
        host_name: "tvxm".to_owned(),
        os: "macOS".to_owned(),
        online,
        addresses: vec!["100.64.0.2".parse().unwrap()],
    }
}

fn build(version: &str, revision: &str, dirty: bool) -> wezterm_codec::BuildIdentity {
    wezterm_codec::BuildIdentity {
        product: "kit-console".to_owned(),
        version: version.to_owned(),
        source_revision: Some(revision.to_owned()),
        source_dirty: Some(dirty),
        embedded_wezterm_revision: Some("b".repeat(40)),
    }
}

fn machine_with(status: ConsoleStatus) -> MachineState {
    let mut machine = MachineState::from_tailnet_node(
        &peer(true),
        MachineRole::Peer,
        UnixUserState::Configured("tvx".to_owned()),
    );
    machine.console = ConsoleProbeState::Complete(Box::new(status));
    machine
}

#[test]
fn start_screen_story_is_exhaustive_for_discovery_and_empty_states() {
    for (discovery, machine_count, expected) in [
        (MachineDiscoveryState::Discovering, 0, ControlCenterStory::Discovering),
        (
            MachineDiscoveryState::AuthenticationRequired,
            0,
            ControlCenterStory::AuthenticationRequired,
        ),
        (MachineDiscoveryState::Ready, 0, ControlCenterStory::Empty),
        (MachineDiscoveryState::Ready, 1, ControlCenterStory::Machines),
    ] {
        let machines = (0..machine_count)
            .map(|_| {
                MachineState::from_tailnet_node(
                    &peer(true),
                    MachineRole::Peer,
                    UnixUserState::Missing,
                )
            })
            .collect();
        assert_eq!(
            ControlCenterState { discovery, machines, selected_machine: None }.story(),
            expected
        );
    }
    let unavailable = ControlCenterState {
        discovery: MachineDiscoveryState::Unavailable { detail: "daemon stopped".to_owned() },
        machines: Vec::new(),
        selected_machine: None,
    };
    assert_eq!(
        unavailable.story(),
        ControlCenterStory::Unavailable { detail: "daemon stopped".to_owned() }
    );
}

#[test]
fn primary_action_covers_every_service_state() {
    let platform = ConsoleServicePlatform::MacosLaunchAgent;
    let path = PathBuf::from("/tmp/console.sock");
    let expected = build("0.2.0", &"a".repeat(40), false);
    let actual = build("0.1.0", &"c".repeat(40), false);
    let cases = [
        (
            ConsoleStatus::NeedsTailscaleLogin { action: String::new() },
            MachineAction::AuthenticateTailscale,
        ),
        (
            ConsoleStatus::TailscaleCliUnavailable { detail: String::new(), action: String::new() },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::TailscaleDaemonUnavailable {
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::TailscalePermissionDenied {
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::TailscaleUnsupported { detail: String::new(), action: String::new() },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::PeerOffline { machine: "tvxm".to_owned(), action: String::new() },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::NeedsUnixUser {
                machine: "tvxm".to_owned(),
                stable_node_id: "node-mac".to_owned(),
                action: String::new(),
            },
            MachineAction::SetUnixUser,
        ),
        (
            ConsoleStatus::NeedsSshAuthentication {
                machine: "tvxm".to_owned(),
                url: "https://example.test".to_owned(),
                action: String::new(),
            },
            MachineAction::AuthenticateOpenSsh,
        ),
        (
            ConsoleStatus::RemoteFailure {
                machine: "tvxm".to_owned(),
                kind: RemoteFailureKind::Transport,
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::RemoteFailure {
                machine: "tvxm".to_owned(),
                kind: RemoteFailureKind::Timeout,
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::Refresh,
        ),
        (
            ConsoleStatus::NotInstalled { platform, action: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (ConsoleStatus::Stopped { platform, action: String::new() }, MachineAction::SetupOrRepair),
        (
            ConsoleStatus::ServiceFailed { platform, detail: String::new(), action: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::ServiceUnavailable {
                platform,
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::WrongOwner {
                platform,
                path: path.clone(),
                expected_uid: 1,
                actual_uid: 2,
                action: String::new(),
            },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::SocketMissing { platform, path: path.clone(), action: String::new() },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::SocketStale {
                platform,
                path: path.clone(),
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::SocketRejected {
                platform,
                path,
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::CodecIncompatible {
                platform,
                server_version: "0.1.0".to_owned(),
                server_codec: 1,
                action: String::new(),
            },
            MachineAction::ShowDetails,
        ),
        (
            ConsoleStatus::BuildIncompatible {
                platform,
                sessions: Some(0),
                expected,
                actual,
                action: String::new(),
            },
            MachineAction::UpdateKit,
        ),
        (
            ConsoleStatus::MuxUnavailable {
                platform,
                detail: String::new(),
                action: String::new(),
            },
            MachineAction::SetupOrRepair,
        ),
        (
            ConsoleStatus::Ready {
                platform,
                sessions: 2,
                build: build("0.2.0", &"d".repeat(40), false),
            },
            MachineAction::Connect,
        ),
    ];
    for (status, expected) in cases {
        assert_eq!(machine_with(status).primary_action(), expected);
    }
}

#[test]
fn row_widths_add_information_without_changing_state_or_action() {
    let machine = machine_with(ConsoleStatus::Ready {
        platform: ConsoleServicePlatform::MacosLaunchAgent,
        sessions: 3,
        build: build("0.2.0", &"a".repeat(40), true),
    });

    let compact = machine.row(MachineRowWidth::Compact);
    let normal = machine.row(MachineRowWidth::Normal);
    let wide = machine.row(MachineRowWidth::Wide);

    assert_eq!(compact.status, "ready");
    assert_eq!(compact.primary_action, MachineAction::Connect);
    assert!(compact.role.is_none());
    assert_eq!(normal.operating_system.as_deref(), Some("macOS"));
    assert_eq!(normal.sessions.as_deref(), Some("3 sessions"));
    assert_eq!(wide.unix_user.as_deref(), Some("tvx"));
    assert_eq!(wide.build.as_deref(), Some("0.2.0 aaaaaaaa dirty"));
    assert_eq!(wide.primary_action, compact.primary_action);
}

#[test]
fn action_contracts_cover_every_input_surface_and_effect_owner() {
    let actions = [
        MachineAction::Connect,
        MachineAction::NewSession,
        MachineAction::Refresh,
        MachineAction::AuthenticateTailscale,
        MachineAction::AuthenticateOpenSsh,
        MachineAction::SetUnixUser,
        MachineAction::SetupOrRepair,
        MachineAction::UpdateKit,
        MachineAction::RestartService,
        MachineAction::StopService,
        MachineAction::ShowDetails,
        MachineAction::CancelOperation,
    ];
    for action in actions {
        let contract = action.contract();
        assert_eq!(contract.action, action);
        assert!(!contract.title.is_empty());
        assert!(contract.mouse);
        assert!(contract.context_menu);
        assert!(contract.command_palette);
    }
    assert_eq!(MachineAction::Connect.contract().effect_owner, ActionEffectOwner::ConsoleRouter);
    assert_eq!(MachineAction::UpdateKit.contract().effect_owner, ActionEffectOwner::KitUpdater);
}

#[test]
fn routing_boundary_preserves_machine_identity_and_session_intent() {
    let connect = ControlCenterOutcome::Connect(MachineConnectionRequest::Remote {
        selector: "tvxm.tail.example".to_owned(),
        create_session: true,
    });
    assert_eq!(
        connect,
        ControlCenterOutcome::Connect(MachineConnectionRequest::Remote {
            selector: "tvxm.tail.example".to_owned(),
            create_session: true,
        })
    );
    assert_ne!(ConnectedSessionOutcome::ReturnToControlCenter, ConnectedSessionOutcome::Quit);
}
