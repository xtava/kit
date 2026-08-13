use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::framework::process::ProcessSupervisor;

use super::{
    display::{DisplayController, DISPLAY_NAME},
    model::{
        SlotPhase, SlotState, SlotStatus, StreamResources, ToggleAction, ToggleReport,
        WindowSnapshot, SLOT_SCHEMA_VERSION,
    },
    shortcut::{self, ShortcutStatus},
    state::SlotStore,
    sunshine::{self, SunshineController},
    window::FocusedWindow,
};

#[derive(Clone)]
pub(super) struct StreamController {
    processes: ProcessSupervisor,
    working_directory: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct DashboardStatus {
    pub slot: SlotStatus,
    pub display_connected: bool,
    pub sunshine_running: bool,
    pub sunshine_owned: bool,
    pub shortcut_installed: bool,
    pub accessibility_granted: bool,
}

impl StreamController {
    pub(super) fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        Self { processes, working_directory }
    }

    pub(super) async fn toggle(&self) -> Result<ToggleReport> {
        let store = SlotStore::bootstrap()?;
        let _lock = store.lock()?;
        let focused = FocusedWindow::capture()?;
        if let Some(state) = store.load()? {
            let same_window = focused.identifies(&state.window);
            let focused = focused.snapshot();
            let report = self.restore_locked(&store, state, ToggleAction::Recalled).await?;
            if same_window {
                return Ok(report);
            }
            return self.send_locked(&store, focused, ToggleAction::Switched).await;
        }
        self.send_locked(&store, focused.snapshot(), ToggleAction::Sent).await
    }

    pub(super) async fn recall(&self) -> Result<Option<ToggleReport>> {
        let store = SlotStore::bootstrap()?;
        let _lock = store.lock()?;
        let Some(state) = store.load()? else {
            return Ok(None);
        };
        Ok(Some(self.restore_locked(&store, state, ToggleAction::Recalled).await?))
    }

    pub(super) async fn recover(&self) -> Result<Option<ToggleReport>> {
        let store = SlotStore::bootstrap()?;
        let _lock = store.lock()?;
        let Some(state) = store.load()? else {
            return Ok(None);
        };
        Ok(Some(self.restore_locked(&store, state, ToggleAction::Recovered).await?))
    }

    pub(super) async fn status(&self) -> Result<SlotStatus> {
        let store = SlotStore::bootstrap()?;
        let state = store.load()?;
        Ok(match state {
            Some(state) => SlotStatus {
                schema_version: SLOT_SCHEMA_VERSION,
                active: state.phase == SlotPhase::Active,
                phase: Some(state.phase),
                app_name: Some(state.window.app_name),
                window_title: Some(state.window.title),
                shortcut: "Cmd+Shift+M",
            },
            None => SlotStatus {
                schema_version: SLOT_SCHEMA_VERSION,
                active: false,
                phase: None,
                app_name: None,
                window_title: None,
                shortcut: "Cmd+Shift+M",
            },
        })
    }

    pub(super) async fn dashboard_status(&self) -> Result<DashboardStatus> {
        let display_controller = self.display();
        let sunshine = self.sunshine();
        let (display, sunshine_running, sunshine_owned) = tokio::try_join!(
            display_controller.inspect(),
            sunshine.running(),
            sunshine.owned_service_loaded(),
        )?;
        Ok(DashboardStatus {
            slot: self.status().await?,
            display_connected: display.frame.is_some(),
            sunshine_running,
            sunshine_owned,
            shortcut_installed: self.shortcut_status()? == ShortcutStatus::Installed,
            accessibility_granted: super::window::accessibility_granted(),
        })
    }

    pub(super) fn install_shortcut(&self) -> Result<bool> {
        let changed = shortcut::install(&Self::shortcut_executable()?)?;
        let _ = super::window::request_accessibility();
        Ok(changed)
    }

    pub(super) fn remove_shortcut(&self) -> Result<bool> {
        shortcut::remove()
    }

    pub(super) fn shortcut_status(&self) -> Result<ShortcutStatus> {
        shortcut::status(&Self::shortcut_executable()?)
    }

    async fn send_locked(
        &self,
        store: &SlotStore,
        mut window: WindowSnapshot,
        action: ToggleAction,
    ) -> Result<ToggleReport> {
        let display = self.display().connect().await?;
        let target = inset(display.frame, 12.0);
        window.streamed_frame = target;
        let plan = match self.sunshine().plan(display.display_id).await {
            Ok(plan) => plan,
            Err(error) => {
                if display.connected_by_kit {
                    if let Err(cleanup) = self.display().disconnect().await {
                        bail!(
                            "prepare Sunshine failed: {error:#}; disconnecting the TV display also failed: {cleanup:#}"
                        );
                    }
                }
                return Err(error);
            }
        };
        let resources = StreamResources {
            display_id: display.display_id,
            display_connected_by_kit: display.connected_by_kit,
            sunshine_started_by_kit: plan.started_by_kit,
            previous_output_name: plan.previous_output_name.clone(),
            output_name_changed: plan.output_name_changed,
        };
        let mut state = SlotState {
            schema_version: SLOT_SCHEMA_VERSION,
            phase: SlotPhase::Preparing,
            window,
            resources,
        };
        if let Err(error) = store.save(&state) {
            if state.resources.display_connected_by_kit {
                if let Err(cleanup) = self.display().disconnect().await {
                    bail!(
                        "record Stream Slot state failed: {error:#}; disconnecting the TV display also failed: {cleanup:#}"
                    );
                }
            }
            return Err(error).context("record Stream Slot state; disconnected the TV display");
        }
        if let Err(error) = self.sunshine().apply(&plan).await {
            let rollback = self.restore_resources(store, &mut state).await;
            if rollback.is_ok() {
                store.clear()?;
            }
            return match rollback {
                Ok(()) => Err(error).context("prepare Sunshine; rolled back Stream resources"),
                Err(rollback) => Err(anyhow::anyhow!(
                    "prepare Sunshine failed: {error:#}; rollback also failed: {rollback:#}; run `kit stream recover`"
                )),
            };
        }
        let moved_frame = super::window::move_snapshot(&state.window, target);
        if let Err(error) = moved_frame {
            let rollback = self.restore_resources(store, &mut state).await;
            if rollback.is_ok() {
                store.clear()?;
            }
            return match rollback {
                Ok(()) => Err(error).context("move focused window to Stream; rolled back resources"),
                Err(rollback) => Err(anyhow::anyhow!(
                    "move focused window to Stream failed: {error:#}; rollback also failed: {rollback:#}; run `kit stream recover`"
                )),
            };
        }
        state.window.streamed_frame = moved_frame.unwrap_or(target);
        state.phase = SlotPhase::Active;
        store.save(&state).context(
            "record the active Stream Slot; the window moved, so run `kit stream recover` if needed",
        )?;
        Ok(ToggleReport {
            schema_version: SLOT_SCHEMA_VERSION,
            action,
            app_name: state.window.app_name.clone(),
            window_title: state.window.title.clone(),
            display_name: DISPLAY_NAME,
        })
    }

    async fn restore_locked(
        &self,
        store: &SlotStore,
        mut state: SlotState,
        action: ToggleAction,
    ) -> Result<ToggleReport> {
        state.phase = SlotPhase::Restoring;
        store.save(&state)?;
        let window_result = super::window::restore(&state.window);
        let resources_result = self.restore_resources(store, &mut state).await;
        match (&window_result, &resources_result) {
            (Ok(()), Ok(())) => store.clear()?,
            _ => {
                let mut failures = Vec::new();
                if let Err(error) = window_result {
                    failures.push(format!("window: {error:#}"));
                }
                if let Err(error) = resources_result {
                    failures.push(format!("resources: {error:#}"));
                }
                if let Err(error) = store.save(&state) {
                    failures.push(format!("record recovery progress: {error:#}"));
                }
                bail!(
                    "Stream recall is incomplete ({}); fix the issue and run `kit stream recover`",
                    failures.join("; ")
                );
            }
        }
        Ok(ToggleReport {
            schema_version: SLOT_SCHEMA_VERSION,
            action,
            app_name: state.window.app_name,
            window_title: state.window.title,
            display_name: DISPLAY_NAME,
        })
    }

    async fn restore_resources(&self, store: &SlotStore, state: &mut SlotState) -> Result<()> {
        let mut failures = Vec::new();
        if state.resources.sunshine_started_by_kit {
            match self.sunshine().stop_owned().await {
                Ok(()) => {
                    state.resources.sunshine_started_by_kit = false;
                    if let Err(error) = store.save(state) {
                        failures.push(format!("record stopped Sunshine: {error:#}"));
                    }
                }
                Err(error) => failures.push(format!("stop Sunshine: {error:#}")),
            }
        }
        if state.resources.output_name_changed {
            let selected_output_name = state.resources.display_id.to_string();
            match sunshine::restore_output_name(
                state.resources.previous_output_name.as_deref(),
                &selected_output_name,
            ) {
                Ok(()) => {
                    state.resources.output_name_changed = false;
                    state.resources.previous_output_name = None;
                    if let Err(error) = store.save(state) {
                        failures.push(format!("record restored Sunshine output: {error:#}"));
                    }
                }
                Err(error) => failures.push(format!("restore Sunshine output: {error:#}")),
            }
        }
        if state.resources.display_connected_by_kit {
            match self.display().disconnect().await {
                Ok(()) => {
                    state.resources.display_connected_by_kit = false;
                    if let Err(error) = store.save(state) {
                        failures.push(format!("record disconnected TV display: {error:#}"));
                    }
                }
                Err(error) => failures.push(format!("disconnect TV display: {error:#}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!("{}", failures.join("; "))
        }
    }

    fn display(&self) -> DisplayController {
        DisplayController::new(self.processes.clone(), self.working_directory.clone())
    }

    fn sunshine(&self) -> SunshineController {
        SunshineController::new(self.processes.clone(), self.working_directory.clone())
    }

    fn shortcut_executable() -> Result<PathBuf> {
        let current = std::env::current_exe().context("resolve current Kit executable")?;
        Ok(directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().join(".local/bin/kit"))
            .filter(|path| path.is_file())
            .unwrap_or(current))
    }
}

fn inset(frame: super::model::WindowFrame, margin: f64) -> super::model::WindowFrame {
    super::model::WindowFrame {
        x: frame.x + margin,
        y: frame.y + margin,
        width: (frame.width - margin * 2.0).max(320.0),
        height: (frame.height - margin * 2.0).max(240.0),
    }
}
