use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Position;
use tokio::sync::{mpsc, Semaphore};

use crate::{
    framework::{process::ProcessSupervisor, start_external, Context, ExternalTarget},
    release::{ReleaseUpdater, UpdateAvailability},
    tailscale::{LoginEvent, Readiness, Status, TailscaleClient},
    tui::{
        theme::NORD, ActionInvocation, ActionRegistry, ActionUnavailable, CommandPalette,
        CommandPaletteOutcome, ContextMenu, ContextMenuOutcome, EventReader, KeyChord, LineEditor,
        NavigationHistory, Session, SessionOptions, SettingsEditor, SettingsFlow,
    },
};

use super::super::{
    build_identity,
    config::{self, Config},
    remote,
    service::{self, ConsoleStatus},
};
use super::{
    actions::{
        control_center_actions, ControlCenterActionContext, ControlCenterCommand,
        MACHINE_CONTEXT_MENU,
    },
    model::{
        compatibility_for_status, valid_unix_user, ConsoleProbeState, ControlCenterOutcome,
        ControlCenterState, MachineAction, MachineConnectionRequest, MachineDiscoveryState,
        MachineOperation, MachineOperationState, MachineReachability, MachineRole, MachineState,
        UnixUserState,
    },
    render::{render, ControlCenterRegions},
};

const REMOTE_PROBE_CONCURRENCY: usize = 4;

enum ControlCenterUpdate {
    DiscoveryCompleted {
        generation: u64,
        result: Result<Readiness, String>,
    },
    ProbeCompleted {
        generation: u64,
        stable_node_id: String,
        result: Result<ConsoleStatus, String>,
    },
    OperationCompleted {
        generation: u64,
        stable_node_id: String,
        operation: MachineOperation,
        result: Result<ConsoleStatus, String>,
    },
    ReleaseChecked(Result<UpdateAvailability, String>),
    TailscaleLogin(LoginEvent),
}

pub(super) enum ControlCenterOverlay {
    CommandPalette(CommandPalette<ControlCenterActionContext>),
    ContextMenu(ContextMenu<ControlCenterActionContext>),
    Details { stable_node_id: String },
    Settings(SettingsEditor),
    UnixUser { stable_node_id: String, input: LineEditor, notice: Option<String> },
}

pub(super) struct ControlCenterApp {
    pub(super) state: ControlCenterState,
    config: Config,
    expected_build: wezterm_codec::BuildIdentity,
    release_availability: Option<UpdateAvailability>,
    generation: u64,
    update_sender: mpsc::UnboundedSender<ControlCenterUpdate>,
    action_registry: ActionRegistry<ControlCenterActionContext, ControlCenterCommand>,
    pub(super) overlay: Option<ControlCenterOverlay>,
    prefix_pending: bool,
    pub(super) tailscale_login_cancel: Option<tokio::sync::watch::Sender<bool>>,
    pub(super) notice: Option<String>,
    history: NavigationHistory<String>,
}

pub(crate) async fn run(cx: &Context, config: Config) -> Result<ControlCenterOutcome> {
    let expected_build = build_identity()?;
    let (update_sender, mut updates) = mpsc::unbounded_channel();
    let mut app = ControlCenterApp {
        state: ControlCenterState {
            discovery: MachineDiscoveryState::Discovering,
            machines: Vec::new(),
            selected_machine: None,
        },
        config,
        expected_build,
        release_availability: None,
        generation: 0,
        update_sender,
        action_registry: control_center_actions()?,
        overlay: None,
        prefix_pending: false,
        tailscale_login_cancel: None,
        notice: None,
        history: NavigationHistory::default(),
    };
    app.refresh(cx.processes.clone())?;

    let mut terminal =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let mut regions = ControlCenterRegions::default();

    loop {
        terminal.draw(|frame| regions = render(frame, &mut app))?;
        tokio::select! {
            update = updates.recv() => {
                let Some(update) = update else {
                    return Ok(ControlCenterOutcome::Quit);
                };
                app.apply_update(cx.processes.clone(), update)?;
            }
            event = events.recv() => {
                let Some(event) = event else {
                    return Ok(ControlCenterOutcome::Quit);
                };
                if let Some(outcome) =
                    app.handle_event(cx.processes.clone(), event, &regions)?
                {
                    return Ok(outcome);
                }
            }
        }
    }
}

impl ControlCenterApp {
    fn refresh(&mut self, processes: ProcessSupervisor) -> Result<()> {
        self.generation = self.generation.wrapping_add(1);
        self.state.discovery = MachineDiscoveryState::Discovering;
        self.notice = None;
        let generation = self.generation;
        let sender = self.update_sender.clone();
        let working_directory = std::env::current_dir()?;
        tokio::spawn(async move {
            let client = TailscaleClient::new(processes, working_directory);
            let result = client.readiness().await.map_err(|error| format!("{error:#}"));
            let _ = sender.send(ControlCenterUpdate::DiscoveryCompleted { generation, result });
        });
        let updater = ReleaseUpdater::new();
        let cached = updater.cached();
        if let Some(cached) = cached.as_ref() {
            self.release_availability = Some(cached.availability.clone());
        }
        if cached.as_ref().is_none_or(|cached| cached.stale) {
            let sender = self.update_sender.clone();
            tokio::spawn(async move {
                let result = updater.check().await.map_err(|error| format!("{error:#}"));
                let _ = sender.send(ControlCenterUpdate::ReleaseChecked(result));
            });
        }
        Ok(())
    }

    fn apply_update(
        &mut self,
        processes: ProcessSupervisor,
        update: ControlCenterUpdate,
    ) -> Result<()> {
        match update {
            ControlCenterUpdate::DiscoveryCompleted { generation, .. }
                if generation != self.generation =>
            {
                Ok(())
            }
            ControlCenterUpdate::DiscoveryCompleted { generation, result } => match result {
                Ok(Readiness::Ready(status)) => {
                    self.reconcile_machines(&status);
                    self.state.discovery = MachineDiscoveryState::Ready;
                    self.start_probes(processes, generation, status);
                    Ok(())
                }
                Ok(Readiness::NeedsLogin) => {
                    self.state.discovery = MachineDiscoveryState::AuthenticationRequired;
                    self.state.machines.clear();
                    self.state.selected_machine = None;
                    Ok(())
                }
                Ok(Readiness::CliUnavailable(detail))
                | Ok(Readiness::DaemonUnavailable(detail))
                | Ok(Readiness::PermissionDenied(detail))
                | Ok(Readiness::Unsupported(detail))
                | Err(detail) => {
                    self.state.discovery = MachineDiscoveryState::Unavailable { detail };
                    self.state.machines.clear();
                    self.state.selected_machine = None;
                    Ok(())
                }
            },
            ControlCenterUpdate::ProbeCompleted { generation, .. }
                if generation != self.generation =>
            {
                Ok(())
            }
            ControlCenterUpdate::ProbeCompleted { stable_node_id, result, .. } => {
                let Some(machine) = self
                    .state
                    .machines
                    .iter_mut()
                    .find(|machine| machine.identity.stable_node_id == stable_node_id)
                else {
                    return Ok(());
                };
                match result {
                    Ok(status) => {
                        machine.compatibility = compatibility_for_status(
                            &self.expected_build,
                            &status,
                            self.release_availability.as_ref(),
                        );
                        machine.console = ConsoleProbeState::Complete(Box::new(status));
                        machine.operation = MachineOperationState::Idle;
                    }
                    Err(detail) => {
                        machine.operation = MachineOperationState::Failed {
                            operation: MachineOperation::Probe,
                            detail: detail.clone(),
                        };
                        self.notice = Some(format!(
                            "Could not inspect {}: {detail}",
                            machine.identity.display_name
                        ));
                    }
                }
                Ok(())
            }
            ControlCenterUpdate::OperationCompleted { generation, .. }
                if generation != self.generation =>
            {
                Ok(())
            }
            ControlCenterUpdate::OperationCompleted {
                stable_node_id, operation, result, ..
            } => {
                let Some(machine) = self
                    .state
                    .machines
                    .iter_mut()
                    .find(|machine| machine.identity.stable_node_id == stable_node_id)
                else {
                    return Ok(());
                };
                match result {
                    Ok(status) => {
                        machine.compatibility = compatibility_for_status(
                            &self.expected_build,
                            &status,
                            self.release_availability.as_ref(),
                        );
                        machine.console = ConsoleProbeState::Complete(Box::new(status));
                        machine.operation = MachineOperationState::Idle;
                        self.notice = Some(format!("{} completed.", operation.completed_label()));
                    }
                    Err(detail) => {
                        machine.operation =
                            MachineOperationState::Failed { operation, detail: detail.clone() };
                        self.notice = Some(format!("{} failed: {detail}", operation.label()));
                    }
                }
                Ok(())
            }
            ControlCenterUpdate::ReleaseChecked(result) => {
                match result {
                    Ok(availability) => {
                        self.release_availability = Some(availability);
                        for machine in &mut self.state.machines {
                            if let ConsoleProbeState::Complete(status) = &machine.console {
                                machine.compatibility = compatibility_for_status(
                                    &self.expected_build,
                                    status,
                                    self.release_availability.as_ref(),
                                );
                            }
                        }
                    }
                    Err(detail) => {
                        self.notice = Some(format!("Could not check Kit releases: {detail}"));
                    }
                }
                Ok(())
            }
            ControlCenterUpdate::TailscaleLogin(event) => {
                match event {
                    LoginEvent::Url(url) => {
                        self.notice =
                            Some("Complete Tailscale authentication in your browser.".to_owned());
                        let receipt = start_external(
                            &processes,
                            ExternalTarget::Url(url.as_str().to_owned()),
                        )?;
                        tokio::spawn(async move {
                            let _ = receipt.completion().await;
                        });
                    }
                    LoginEvent::Ready(status) => {
                        self.tailscale_login_cancel = None;
                        self.reconcile_machines(&status);
                        self.state.discovery = MachineDiscoveryState::Ready;
                        self.start_probes(processes, self.generation, status);
                        self.notice = Some("Tailscale authentication complete.".to_owned());
                    }
                    LoginEvent::Failed(detail) => {
                        self.tailscale_login_cancel = None;
                        self.state.discovery =
                            MachineDiscoveryState::Unavailable { detail: detail.clone() };
                        self.notice = Some(format!("Tailscale authentication failed: {detail}"));
                    }
                    LoginEvent::Cancelled => {
                        self.tailscale_login_cancel = None;
                        self.state.discovery = MachineDiscoveryState::AuthenticationRequired;
                        self.notice = Some("Tailscale authentication cancelled.".to_owned());
                    }
                }
                Ok(())
            }
        }
    }

    fn reconcile_machines(&mut self, status: &Status) {
        let selected = self
            .state
            .selected_machine
            .clone()
            .or_else(|| self.config.selected_machine().map(str::to_owned));
        let current_user = std::env::var("USER").unwrap_or_else(|_| "current user".to_owned());
        let mut machines = vec![MachineState::from_tailnet_node(
            &status.local,
            MachineRole::ThisMachine,
            UnixUserState::Current(current_user),
        )];
        machines.extend(status.peers.iter().map(|node| {
            let unix_user = self
                .config
                .unix_user(&node.id)
                .map(|user| UnixUserState::Configured(user.to_owned()))
                .unwrap_or(UnixUserState::Missing);
            MachineState::from_tailnet_node(node, MachineRole::Peer, unix_user)
        }));
        self.state.machines = machines;
        self.state.selected_machine = selected
            .filter(|selected| {
                self.state
                    .machines
                    .iter()
                    .any(|machine| &machine.identity.stable_node_id == selected)
            })
            .or_else(|| {
                self.state.machines.first().map(|machine| machine.identity.stable_node_id.clone())
            });
        if let Some(stable_node_id) = self.state.selected_machine.clone() {
            self.history.visit(stable_node_id);
        }
    }

    fn start_probes(&mut self, processes: ProcessSupervisor, generation: u64, status: Status) {
        for machine in &mut self.state.machines {
            let can_probe = machine.role == MachineRole::ThisMachine
                || (machine.reachability == MachineReachability::Online
                    && matches!(machine.unix_user, UnixUserState::Configured(_)));
            if can_probe {
                machine.operation = MachineOperationState::Running(MachineOperation::Probe);
            }
        }
        let semaphore = Arc::new(Semaphore::new(REMOTE_PROBE_CONCURRENCY));
        let sender = self.update_sender.clone();
        let local_id = status.local.id.clone();
        let local_processes = processes.clone();
        tokio::spawn(async move {
            let result =
                service::status(&local_processes).await.map_err(|error| format!("{error:#}"));
            let _ = sender.send(ControlCenterUpdate::ProbeCompleted {
                generation,
                stable_node_id: local_id,
                result,
            });
        });

        for node in status.peers {
            if !node.online {
                continue;
            }
            let Some(user) = self.config.unix_user(&node.id).map(str::to_owned) else {
                continue;
            };
            let mut config = self.config.clone();
            let sender = self.update_sender.clone();
            let processes = processes.clone();
            let semaphore = Arc::clone(&semaphore);
            tokio::spawn(async move {
                let result = async {
                    let _permit = semaphore
                        .acquire_owned()
                        .await
                        .map_err(|_| "machine probe coordinator stopped".to_owned())?;
                    match remote::resolve_node(&mut config, &node, Some(&user))
                        .map_err(|error| format!("{error:#}"))?
                    {
                        remote::Resolution::Ready(target) => remote::status(&processes, &target)
                            .await
                            .map_err(|error| format!("{error:#}")),
                        remote::Resolution::Status(status) => Ok(status),
                    }
                }
                .await;
                let _ = sender.send(ControlCenterUpdate::ProbeCompleted {
                    generation,
                    stable_node_id: node.id,
                    result,
                });
            });
        }
    }

    fn handle_event(
        &mut self,
        processes: ProcessSupervisor,
        event: Event,
        regions: &ControlCenterRegions,
    ) -> Result<Option<ControlCenterOutcome>> {
        if matches!(self.overlay, Some(ControlCenterOverlay::Details { .. })) {
            if matches!(
                event,
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && matches!(key.code, KeyCode::Esc | KeyCode::Enter)
            ) || matches!(event, Event::Mouse(_))
            {
                self.overlay = None;
            }
            return Ok(None);
        }
        if matches!(self.overlay, Some(ControlCenterOverlay::Settings(_))) {
            let flow = match (&mut self.overlay, event) {
                (Some(ControlCenterOverlay::Settings(editor)), Event::Key(key)) => {
                    editor.on_key(key)
                }
                (Some(ControlCenterOverlay::Settings(editor)), Event::Mouse(mouse)) => {
                    editor.on_mouse(mouse)
                }
                _ => SettingsFlow::Continue,
            };
            if flow == SettingsFlow::Exit {
                self.overlay = None;
                self.config = Config::load(self.config.store())?;
            }
            return Ok(None);
        }
        if matches!(self.overlay, Some(ControlCenterOverlay::ContextMenu(_))) {
            let Some(layout) = regions.context_menu.as_ref() else {
                self.overlay = None;
                return Ok(None);
            };
            let outcome = match self.overlay.as_mut() {
                Some(ControlCenterOverlay::ContextMenu(context_menu)) => {
                    context_menu.on_event(event, layout)
                }
                _ => unreachable!("context menu overlay checked above"),
            };
            return match outcome {
                ContextMenuOutcome::Captured => Ok(None),
                ContextMenuOutcome::Dismissed => {
                    self.overlay = None;
                    Ok(None)
                }
                ContextMenuOutcome::Unavailable { reason, .. } => {
                    self.overlay = None;
                    self.notice = Some(reason.into_owned());
                    Ok(None)
                }
                ContextMenuOutcome::Invoke(invocation) => {
                    self.overlay = None;
                    let invocation =
                        ActionInvocation::new(invocation.action, self.action_context());
                    match self.action_registry.command_for(&invocation) {
                        Ok(command) => self.invoke_command(processes, command),
                        Err(ActionUnavailable::Disabled { reason, .. }) => {
                            self.notice = Some(reason.into_owned());
                            Ok(None)
                        }
                        Err(ActionUnavailable::Unknown { .. }) => {
                            self.notice =
                                Some("That Console command is no longer available.".to_owned());
                            Ok(None)
                        }
                    }
                }
            };
        }
        if matches!(self.overlay, Some(ControlCenterOverlay::CommandPalette(_))) {
            let Some(layout) = regions.command_palette.as_ref() else {
                self.overlay = None;
                return Ok(None);
            };
            let outcome = match self.overlay.as_mut() {
                Some(ControlCenterOverlay::CommandPalette(command_palette)) => {
                    command_palette.on_event(event, layout)
                }
                _ => unreachable!("command palette overlay checked above"),
            };
            return match outcome {
                CommandPaletteOutcome::Captured => Ok(None),
                CommandPaletteOutcome::Dismissed => {
                    self.overlay = None;
                    Ok(None)
                }
                CommandPaletteOutcome::Invoke(invocation) => {
                    self.overlay = None;
                    let invocation =
                        ActionInvocation::new(invocation.action, self.action_context());
                    match self.action_registry.command_for(&invocation) {
                        Ok(command) => self.invoke_command(processes, command),
                        Err(ActionUnavailable::Disabled { reason, .. }) => {
                            self.notice = Some(reason.into_owned());
                            Ok(None)
                        }
                        Err(ActionUnavailable::Unknown { .. }) => {
                            self.notice =
                                Some("That Console command is no longer available.".to_owned());
                            Ok(None)
                        }
                    }
                }
            };
        }
        if matches!(self.overlay, Some(ControlCenterOverlay::UnixUser { .. })) {
            return self.handle_unix_user_event(processes, event);
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                let Some(chord) = KeyChord::from_event(key) else {
                    return Ok(None);
                };
                let keybindings = self.config.keybindings();
                if chord == keybindings.command_palette {
                    self.open_command_palette();
                    return Ok(None);
                }
                if self.prefix_pending {
                    self.prefix_pending = false;
                    if chord == keybindings.help {
                        self.open_command_palette();
                        return Ok(None);
                    }
                    if chord == keybindings.new_session {
                        return Ok(self.connection_outcome(true));
                    }
                    if chord == keybindings.quit {
                        return Ok(Some(ControlCenterOutcome::Quit));
                    }
                    return Ok(None);
                }
                if chord == keybindings.prefix {
                    self.prefix_pending = true;
                    return Ok(None);
                }
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc if !control => {
                        return Ok(Some(ControlCenterOutcome::Quit));
                    }
                    KeyCode::Char('c') if control => {
                        return Ok(Some(ControlCenterOutcome::Quit));
                    }
                    KeyCode::Up | KeyCode::Char('k') if !control => self.select_relative(-1)?,
                    KeyCode::Down | KeyCode::Char('j') if !control => self.select_relative(1)?,
                    KeyCode::Left if !control => self.navigate_history(-1)?,
                    KeyCode::Right if !control => self.navigate_history(1)?,
                    KeyCode::Home => self.select_index(0)?,
                    KeyCode::End => {
                        self.select_index(self.state.machines.len().saturating_sub(1))?;
                    }
                    KeyCode::Char('r') if !control => self.refresh(processes)?,
                    KeyCode::Enter => return self.invoke_primary(processes),
                    _ => {}
                }
            }
            Event::Mouse(mouse) => {
                let position = Position::new(mouse.column, mouse.row);
                if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
                    self.select_relative(if mouse.kind == MouseEventKind::ScrollUp {
                        -1
                    } else {
                        1
                    })?;
                } else if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    if let Some((_, stable_node_id)) =
                        regions.machine_rows.iter().find(|(area, _)| area.contains(position))
                    {
                        self.select_machine(stable_node_id)?;
                    } else if regions.primary_action.is_some_and(|area| area.contains(position)) {
                        return self.invoke_primary(processes);
                    } else if regions.new_session_action.is_some_and(|area| area.contains(position))
                    {
                        return Ok(self.connection_outcome(true));
                    } else if regions.refresh_action.is_some_and(|area| area.contains(position)) {
                        self.refresh(processes)?;
                    }
                } else if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
                    if let Some((_, stable_node_id)) =
                        regions.machine_rows.iter().find(|(area, _)| area.contains(position))
                    {
                        self.select_machine(stable_node_id)?;
                        let context = self.action_context();
                        let items =
                            self.action_registry.resolve_menu(MACHINE_CONTEXT_MENU, &context);
                        self.overlay = ContextMenu::open(position, context, items)
                            .map(ControlCenterOverlay::ContextMenu);
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn select_relative(&mut self, delta: isize) -> Result<()> {
        if self.state.machines.is_empty() {
            return Ok(());
        }
        let current = self
            .selected_index()
            .unwrap_or_default()
            .saturating_add_signed(delta)
            .min(self.state.machines.len() - 1);
        self.select_index(current)
    }

    fn select_index(&mut self, index: usize) -> Result<()> {
        let Some(stable_node_id) =
            self.state.machines.get(index).map(|machine| machine.identity.stable_node_id.clone())
        else {
            return Ok(());
        };
        self.select_machine(&stable_node_id)
    }

    fn select_machine(&mut self, stable_node_id: &str) -> Result<()> {
        self.persist_selected_machine(stable_node_id)?;
        self.history.visit(stable_node_id.to_owned());
        Ok(())
    }

    fn navigate_history(&mut self, delta: isize) -> Result<()> {
        let mut offset = delta;
        while let Some((cursor, stable_node_id)) = self
            .history
            .target(offset)
            .map(|(cursor, stable_node_id)| (cursor, stable_node_id.clone()))
        {
            if self
                .state
                .machines
                .iter()
                .any(|machine| machine.identity.stable_node_id == stable_node_id)
            {
                self.persist_selected_machine(&stable_node_id)?;
                self.history.select(cursor);
                return Ok(());
            }
            offset += delta;
        }
        Ok(())
    }

    fn persist_selected_machine(&mut self, stable_node_id: &str) -> Result<()> {
        self.state.selected_machine = Some(stable_node_id.to_owned());
        self.config.set_selected_machine(stable_node_id)
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        let selected = self.state.selected_machine.as_ref()?;
        self.state.machines.iter().position(|machine| &machine.identity.stable_node_id == selected)
    }

    pub(super) fn selected_machine(&self) -> Option<&MachineState> {
        self.selected_index().and_then(|index| self.state.machines.get(index))
    }

    fn action_context(&self) -> ControlCenterActionContext {
        let mut available_actions = self
            .selected_machine()
            .map(MachineState::available_actions)
            .unwrap_or_else(|| vec![MachineAction::Refresh]);
        if matches!(self.state.discovery, MachineDiscoveryState::AuthenticationRequired) {
            available_actions.push(MachineAction::AuthenticateTailscale);
        }
        if self.tailscale_login_cancel.is_some() {
            available_actions.push(MachineAction::CancelOperation);
        }
        ControlCenterActionContext { available_actions }
    }

    fn open_command_palette(&mut self) {
        let context = self.action_context();
        self.overlay = Some(ControlCenterOverlay::CommandPalette(CommandPalette::open(
            context,
            &self.action_registry,
        )));
    }

    fn handle_unix_user_event(
        &mut self,
        processes: ProcessSupervisor,
        event: Event,
    ) -> Result<Option<ControlCenterOutcome>> {
        match event {
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && key.code == KeyCode::Esc =>
            {
                self.overlay = None;
            }
            Event::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                    && key.code == KeyCode::Enter =>
            {
                let (stable_node_id, user) = match self.overlay.as_ref() {
                    Some(ControlCenterOverlay::UnixUser { stable_node_id, input, .. }) => {
                        (stable_node_id.clone(), input.value().trim().to_owned())
                    }
                    _ => return Ok(None),
                };
                if !valid_unix_user(&user) {
                    if let Some(ControlCenterOverlay::UnixUser { notice, .. }) =
                        self.overlay.as_mut()
                    {
                        *notice = Some(
                            "Use a Unix account name without spaces, @, or shell characters."
                                .to_owned(),
                        );
                    }
                    return Ok(None);
                }
                self.config.set_unix_user(&stable_node_id, &user)?;
                if let Some(machine) = self
                    .state
                    .machines
                    .iter_mut()
                    .find(|machine| machine.identity.stable_node_id == stable_node_id)
                {
                    machine.unix_user = UnixUserState::Configured(user);
                }
                self.overlay = None;
                self.notice = Some("Unix user saved. Checking the machine again…".to_owned());
                self.refresh(processes)?;
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if let Some(ControlCenterOverlay::UnixUser { input, notice, .. }) =
                    self.overlay.as_mut()
                {
                    input.apply_key(key);
                    *notice = None;
                }
            }
            Event::Paste(text) => {
                if let Some(ControlCenterOverlay::UnixUser { input, notice, .. }) =
                    self.overlay.as_mut()
                {
                    for character in text.chars().filter(|character| !character.is_control()) {
                        input.insert(character);
                    }
                    *notice = None;
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn invoke_primary(
        &mut self,
        processes: ProcessSupervisor,
    ) -> Result<Option<ControlCenterOutcome>> {
        let action = if self.tailscale_login_cancel.is_some() {
            MachineAction::CancelOperation
        } else if matches!(self.state.discovery, MachineDiscoveryState::AuthenticationRequired) {
            MachineAction::AuthenticateTailscale
        } else if let Some(action) = self.selected_machine().map(MachineState::primary_action) {
            action
        } else {
            return Ok(None);
        };
        self.invoke_action(processes, action)
    }

    fn invoke_command(
        &mut self,
        processes: ProcessSupervisor,
        command: ControlCenterCommand,
    ) -> Result<Option<ControlCenterOutcome>> {
        match command {
            ControlCenterCommand::Machine(action) => self.invoke_action(processes, action),
            ControlCenterCommand::OpenSettings => {
                self.overlay = Some(ControlCenterOverlay::Settings(SettingsEditor::open(
                    self.config.store(),
                    vec![config::settings()],
                    NORD,
                )));
                Ok(None)
            }
            ControlCenterCommand::Quit => Ok(Some(ControlCenterOutcome::Quit)),
        }
    }

    fn invoke_action(
        &mut self,
        processes: ProcessSupervisor,
        action: MachineAction,
    ) -> Result<Option<ControlCenterOutcome>> {
        match action {
            MachineAction::Connect => Ok(self.connection_outcome(false)),
            MachineAction::NewSession => Ok(self.connection_outcome(true)),
            MachineAction::Refresh => {
                self.refresh(processes)?;
                Ok(None)
            }
            MachineAction::SetUnixUser => {
                let Some(machine) = self.selected_machine() else {
                    return Ok(None);
                };
                let stable_node_id = machine.identity.stable_node_id.clone();
                let mut input = LineEditor::default();
                if let UnixUserState::Configured(user) = &machine.unix_user {
                    input.set(user.clone());
                }
                self.overlay =
                    Some(ControlCenterOverlay::UnixUser { stable_node_id, input, notice: None });
                Ok(None)
            }
            MachineAction::AuthenticateTailscale => {
                let client = TailscaleClient::new(processes, std::env::current_dir()?);
                let (mut events, cancel, owner) = client.start_login();
                self.tailscale_login_cancel = Some(cancel);
                self.notice = Some("Starting Tailscale authentication…".to_owned());
                let sender = self.update_sender.clone();
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        let _ = sender.send(ControlCenterUpdate::TailscaleLogin(event));
                    }
                    let _ = owner.await;
                });
                Ok(None)
            }
            MachineAction::AuthenticateOpenSsh => {
                let url =
                    self.selected_machine().and_then(|machine| match machine.complete_status() {
                        Some(ConsoleStatus::NeedsSshAuthentication { url, .. }) => {
                            Some(url.clone())
                        }
                        _ => None,
                    });
                if let Some(url) = url {
                    let receipt = start_external(&processes, ExternalTarget::Url(url))?;
                    tokio::spawn(async move {
                        let _ = receipt.completion().await;
                    });
                    self.notice =
                        Some("Complete OpenSSH authentication in your browser.".to_owned());
                }
                Ok(None)
            }
            MachineAction::SetupOrRepair => {
                self.start_service_operation(processes, MachineOperation::SetupOrRepair)?;
                Ok(None)
            }
            MachineAction::UpdateKit => {
                self.start_update(processes)?;
                Ok(None)
            }
            MachineAction::RestartService => {
                self.start_service_operation(processes, MachineOperation::Restart)?;
                Ok(None)
            }
            MachineAction::StopService => {
                self.start_service_operation(processes, MachineOperation::Stop)?;
                Ok(None)
            }
            MachineAction::ShowDetails => {
                if let Some(machine) = self.selected_machine() {
                    self.overlay = Some(ControlCenterOverlay::Details {
                        stable_node_id: machine.identity.stable_node_id.clone(),
                    });
                }
                Ok(None)
            }
            MachineAction::CancelOperation => {
                if let Some(cancel) = self.tailscale_login_cancel.take() {
                    cancel.send_replace(true);
                }
                Ok(None)
            }
        }
    }

    fn start_service_operation(
        &mut self,
        processes: ProcessSupervisor,
        operation: MachineOperation,
    ) -> Result<()> {
        let Some(machine) = self.selected_machine() else {
            return Ok(());
        };
        let stable_node_id = machine.identity.stable_node_id.clone();
        let role = machine.role;
        let selector = machine.identity.selector.clone();
        let mut config = self.config.clone();
        let generation = self.generation;
        let sender = self.update_sender.clone();
        if let Some(machine) = self
            .state
            .machines
            .iter_mut()
            .find(|machine| machine.identity.stable_node_id == stable_node_id)
        {
            machine.operation = MachineOperationState::Running(operation);
        }
        self.notice = Some(format!("{}…", operation.label()));
        tokio::spawn(async move {
            let result = async {
                match role {
                    MachineRole::ThisMachine => {
                        run_local_service_operation(&processes, operation).await
                    }
                    MachineRole::Peer => {
                        let target = match remote::resolve(&processes, &mut config, &selector)
                            .await
                            .map_err(|error| format!("{error:#}"))?
                        {
                            remote::Resolution::Ready(target) => target,
                            remote::Resolution::Status(status) => return Ok(status),
                        };
                        run_remote_service_operation(&processes, &target, operation).await
                    }
                }
            }
            .await;
            let _ = sender.send(ControlCenterUpdate::OperationCompleted {
                generation,
                stable_node_id,
                operation,
                result,
            });
        });
        Ok(())
    }

    fn start_update(&mut self, processes: ProcessSupervisor) -> Result<()> {
        let Some(machine) = self.selected_machine() else {
            return Ok(());
        };
        if !machine.update_allowed() {
            self.notice = Some("Close active sessions before updating this machine.".to_owned());
            return Ok(());
        }
        let stable_node_id = machine.identity.stable_node_id.clone();
        let role = machine.role;
        let selector = machine.identity.selector.clone();
        let mut config = self.config.clone();
        let generation = self.generation;
        let sender = self.update_sender.clone();
        if let Some(machine) = self
            .state
            .machines
            .iter_mut()
            .find(|machine| machine.identity.stable_node_id == stable_node_id)
        {
            machine.operation = MachineOperationState::Running(MachineOperation::Update);
        }
        self.notice = Some("Updating Kit and reconciling Console…".to_owned());
        tokio::spawn(async move {
            let result = async {
                match role {
                    MachineRole::ThisMachine => {
                        ReleaseUpdater::new()
                            .install(false)
                            .await
                            .map_err(|error| format!("{error:#}"))?;
                        service::setup(&processes).await.map_err(|error| format!("{error:#}"))
                    }
                    MachineRole::Peer => {
                        let target = match remote::resolve(&processes, &mut config, &selector)
                            .await
                            .map_err(|error| format!("{error:#}"))?
                        {
                            remote::Resolution::Ready(target) => target,
                            remote::Resolution::Status(status) => return Ok(status),
                        };
                        remote::update(&processes, &target)
                            .await
                            .map_err(|error| format!("{error:#}"))?;
                        remote::setup(&processes, &target)
                            .await
                            .map_err(|error| format!("{error:#}"))
                    }
                }
            }
            .await;
            let _ = sender.send(ControlCenterUpdate::OperationCompleted {
                generation,
                stable_node_id,
                operation: MachineOperation::Update,
                result,
            });
        });
        Ok(())
    }

    fn connection_outcome(&self, create_session: bool) -> Option<ControlCenterOutcome> {
        let machine = self.selected_machine()?;
        matches!(machine.complete_status(), Some(ConsoleStatus::Ready { .. })).then(|| {
            let request = match machine.role {
                MachineRole::ThisMachine => MachineConnectionRequest::Local { create_session },
                MachineRole::Peer => MachineConnectionRequest::Remote {
                    selector: machine.identity.selector.clone(),
                    create_session,
                },
            };
            ControlCenterOutcome::Connect(request)
        })
    }
}

async fn run_local_service_operation(
    processes: &ProcessSupervisor,
    operation: MachineOperation,
) -> Result<ConsoleStatus, String> {
    match operation {
        MachineOperation::SetupOrRepair => {
            service::setup(processes).await.map_err(|error| format!("{error:#}"))
        }
        MachineOperation::Stop => {
            service::stop(processes, false).await.map_err(|error| format!("{error:#}"))
        }
        MachineOperation::Restart => {
            service::stop(processes, false).await.map_err(|error| format!("{error:#}"))?;
            service::setup(processes).await.map_err(|error| format!("{error:#}"))
        }
        _ => Err(format!("{} is not a service operation", operation.label())),
    }
}

async fn run_remote_service_operation(
    processes: &ProcessSupervisor,
    target: &remote::RemoteTarget,
    operation: MachineOperation,
) -> Result<ConsoleStatus, String> {
    match operation {
        MachineOperation::SetupOrRepair => {
            remote::setup(processes, target).await.map_err(|error| format!("{error:#}"))
        }
        MachineOperation::Stop => {
            remote::stop(processes, target, false).await.map_err(|error| format!("{error:#}"))
        }
        MachineOperation::Restart => {
            remote::stop(processes, target, false).await.map_err(|error| format!("{error:#}"))?;
            remote::setup(processes, target).await.map_err(|error| format!("{error:#}"))
        }
        _ => Err(format!("{} is not a service operation", operation.label())),
    }
}
