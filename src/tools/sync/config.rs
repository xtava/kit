use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::{
    framework::{
        ConfigStore, EditableSettings, SettingEdit, SettingField, SettingId, SettingsSection,
        SettingsSectionMeta,
    },
    tui::{KeyChord, SplitRatio},
};

use super::model::{ProjectId, ProjectLifecycle, SyncedProject};

const TOOL: &str = "sync";
const SCHEMA_VERSION: u32 = 2;
const PANEL_WIDTH: SettingId = SettingId("panel_width");
const COMMAND_PALETTE: SettingId = SettingId("command_palette");
const ADD_PROJECT: SettingId = SettingId("add_project");
const TOGGLE_PAUSE: SettingId = SettingId("toggle_pause");
const FLUSH: SettingId = SettingId("flush");
const DOCTOR: SettingId = SettingId("doctor");
const REMOVE: SettingId = SettingId("remove");
const REFRESH: SettingId = SettingId("refresh");
const OPEN_SETTINGS: SettingId = SettingId("open_settings");
const PREVIOUS_PROJECT: SettingId = SettingId("previous_project");
const NEXT_PROJECT: SettingId = SettingId("next_project");
const HISTORY_BACK: SettingId = SettingId("history_back");
const HISTORY_FORWARD: SettingId = SettingId("history_forward");
const FOCUS_NEXT: SettingId = SettingId("focus_next");
const FOCUS_PREVIOUS: SettingId = SettingId("focus_previous");
const QUIT: SettingId = SettingId("quit");

const COMPACT_PANEL: SplitRatio = SplitRatio::new(250);
const BALANCED_PANEL: SplitRatio = SplitRatio::new(330);
const WIDE_PANEL: SplitRatio = SplitRatio::new(420);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    #[serde(default = "schema_version")]
    version: u32,
    #[serde(default)]
    projects: Vec<SyncedProject>,
    #[serde(default)]
    ui: UiConfig,
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self { version: SCHEMA_VERSION, projects: Vec::new(), ui: UiConfig::default() }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UiConfig {
    #[serde(default = "default_panel_ratio")]
    panel_ratio: SplitRatio,
    #[serde(default)]
    keybindings: Keybindings,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { panel_ratio: default_panel_ratio(), keybindings: Keybindings::default() }
    }
}

impl UiConfig {
    pub(super) const fn panel_ratio(&self) -> SplitRatio {
        self.panel_ratio
    }

    pub(super) const fn keybindings(&self) -> &Keybindings {
        &self.keybindings
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Keybindings {
    #[serde(default = "default_command_palette")]
    pub command_palette: KeyChord,
    #[serde(default = "default_add_project")]
    pub add_project: KeyChord,
    #[serde(default = "default_toggle_pause")]
    pub toggle_pause: KeyChord,
    #[serde(default = "default_flush")]
    pub flush: KeyChord,
    #[serde(default = "default_doctor")]
    pub doctor: KeyChord,
    #[serde(default = "default_remove")]
    pub remove: KeyChord,
    #[serde(default = "default_refresh")]
    pub refresh: KeyChord,
    #[serde(default = "default_open_settings")]
    pub open_settings: KeyChord,
    #[serde(default = "default_previous_project")]
    pub previous_project: KeyChord,
    #[serde(default = "default_next_project")]
    pub next_project: KeyChord,
    #[serde(default = "default_history_back")]
    pub history_back: KeyChord,
    #[serde(default = "default_history_forward")]
    pub history_forward: KeyChord,
    #[serde(default = "default_focus_next")]
    pub focus_next: KeyChord,
    #[serde(default = "default_focus_previous")]
    pub focus_previous: KeyChord,
    #[serde(default = "default_quit")]
    pub quit: KeyChord,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            command_palette: default_command_palette(),
            add_project: default_add_project(),
            toggle_pause: default_toggle_pause(),
            flush: default_flush(),
            doctor: default_doctor(),
            remove: default_remove(),
            refresh: default_refresh(),
            open_settings: default_open_settings(),
            previous_project: default_previous_project(),
            next_project: default_next_project(),
            history_back: default_history_back(),
            history_forward: default_history_forward(),
            focus_next: default_focus_next(),
            focus_previous: default_focus_previous(),
            quit: default_quit(),
        }
    }
}

pub(super) struct Config {
    store: ConfigStore,
    projects: Vec<SyncedProject>,
    ui: UiConfig,
}

impl Config {
    pub(super) fn load(store: ConfigStore) -> Result<Self> {
        let stored: StoredConfig = store.load(TOOL)?;
        validate(&stored)?;
        stored.ui.keybindings.validate()?;
        Ok(Self { store, projects: stored.projects, ui: stored.ui })
    }

    pub(super) fn projects(&self) -> &[SyncedProject] {
        &self.projects
    }

    pub(super) fn ui(&self) -> &UiConfig {
        &self.ui
    }

    pub(super) fn store(&self) -> ConfigStore {
        self.store.clone()
    }

    pub(super) fn project(&self, id: ProjectId) -> Option<&SyncedProject> {
        self.projects.iter().find(|project| project.id() == id)
    }

    fn project_by_name(&self, name: &str) -> Option<&SyncedProject> {
        self.projects.iter().find(|project| project.name() == name)
    }

    pub(super) fn resolve(&self, selector: &str) -> Result<&SyncedProject> {
        selector
            .parse()
            .ok()
            .and_then(|id| self.project(id))
            .or_else(|| self.project_by_name(selector))
            .with_context(|| format!("no Synced Project exactly matches {selector:?}"))
    }

    pub(super) fn add(&mut self, project: SyncedProject) -> Result<()> {
        self.ensure_addable(&project)?;
        self.projects.push(project);
        if let Err(error) = self.save() {
            self.projects.pop();
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn ensure_addable(&self, project: &SyncedProject) -> Result<()> {
        project.validate()?;
        if self.projects.iter().any(|current| current.id() == project.id()) {
            bail!("Synced Project identity {} is already configured", project.id());
        }
        if self.projects.iter().any(|current| current.name() == project.name()) {
            bail!("Synced Project name {:?} is already configured", project.name());
        }
        ensure_distinct_roots(&self.projects, project)?;
        Ok(())
    }

    pub(super) fn remove(&mut self, id: ProjectId) -> Result<Option<SyncedProject>> {
        let Some(index) = self.projects.iter().position(|project| project.id() == id) else {
            return Ok(None);
        };
        let project = self.projects.remove(index);
        if let Err(error) = self.save() {
            self.projects.insert(index, project);
            return Err(error);
        }
        Ok(Some(project))
    }

    pub(super) fn set_lifecycle(
        &mut self,
        id: ProjectId,
        lifecycle: ProjectLifecycle,
    ) -> Result<ProjectLifecycle> {
        let index =
            self.projects.iter().position(|project| project.id() == id).with_context(|| {
                format!("Synced Project {id} disappeared during lifecycle update")
            })?;
        let previous = self.projects[index].lifecycle();
        self.projects[index].set_lifecycle(lifecycle);
        if let Err(error) = self.save() {
            self.projects[index].set_lifecycle(previous);
            return Err(error);
        }
        Ok(previous)
    }

    fn save(&self) -> Result<()> {
        self.store
            .save(
                TOOL,
                &StoredConfig {
                    version: SCHEMA_VERSION,
                    projects: self.projects.clone(),
                    ui: self.ui.clone(),
                },
            )
            .context("save Synced Project configuration")
    }
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

fn validate(config: &StoredConfig) -> Result<()> {
    if config.version != SCHEMA_VERSION {
        bail!(
            "unsupported Synced Project configuration version {}; expected {SCHEMA_VERSION}",
            config.version
        );
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for project in &config.projects {
        project.validate()?;
        if !ids.insert(project.id()) {
            bail!("Synced Project identity {} is configured more than once", project.id());
        }
        if !names.insert(project.name()) {
            bail!("Synced Project name {:?} is configured more than once", project.name());
        }
        let index = config
            .projects
            .iter()
            .position(|current| current.id() == project.id())
            .expect("validated project belongs to the configuration");
        ensure_distinct_roots(&config.projects[..index], project)?;
    }
    Ok(())
}

fn ensure_distinct_roots(projects: &[SyncedProject], candidate: &SyncedProject) -> Result<()> {
    for current in projects {
        if paths_overlap(current.local().root(), candidate.local().root()) {
            bail!(
                "local synchronization root {} overlaps Synced Project {:?}",
                candidate.local().root().display(),
                current.name()
            );
        }
        if current.remote().stable_node_id() == candidate.remote().stable_node_id()
            && paths_overlap(current.remote().root(), candidate.remote().root())
        {
            bail!(
                "remote synchronization root {} overlaps Synced Project {:?} on the same machine",
                candidate.remote().root().display(),
                current.name()
            );
        }
    }
    Ok(())
}

fn paths_overlap(left: &std::path::Path, right: &std::path::Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

impl Keybindings {
    fn validate(&self) -> Result<()> {
        let mut configured = HashSet::new();
        for (id, chord) in self.entries() {
            if !configured.insert(chord) {
                bail!("Synced Projects keybinding '{}' duplicates another command", id.0);
            }
        }
        Ok(())
    }

    fn replace(&mut self, id: SettingId, chord: KeyChord) -> Result<()> {
        match id {
            COMMAND_PALETTE => self.command_palette = chord,
            ADD_PROJECT => self.add_project = chord,
            TOGGLE_PAUSE => self.toggle_pause = chord,
            FLUSH => self.flush = chord,
            DOCTOR => self.doctor = chord,
            REMOVE => self.remove = chord,
            REFRESH => self.refresh = chord,
            OPEN_SETTINGS => self.open_settings = chord,
            PREVIOUS_PROJECT => self.previous_project = chord,
            NEXT_PROJECT => self.next_project = chord,
            HISTORY_BACK => self.history_back = chord,
            HISTORY_FORWARD => self.history_forward = chord,
            FOCUS_NEXT => self.focus_next = chord,
            FOCUS_PREVIOUS => self.focus_previous = chord,
            QUIT => self.quit = chord,
            _ => bail!("unknown Synced Projects keybinding '{}'", id.0),
        }
        self.validate()
    }

    fn entries(&self) -> [(SettingId, KeyChord); 15] {
        [
            (COMMAND_PALETTE, self.command_palette),
            (ADD_PROJECT, self.add_project),
            (TOGGLE_PAUSE, self.toggle_pause),
            (FLUSH, self.flush),
            (DOCTOR, self.doctor),
            (REMOVE, self.remove),
            (REFRESH, self.refresh),
            (OPEN_SETTINGS, self.open_settings),
            (PREVIOUS_PROJECT, self.previous_project),
            (NEXT_PROJECT, self.next_project),
            (HISTORY_BACK, self.history_back),
            (HISTORY_FORWARD, self.history_forward),
            (FOCUS_NEXT, self.focus_next),
            (FOCUS_PREVIOUS, self.focus_previous),
            (QUIT, self.quit),
        ]
    }
}

#[derive(Clone, Copy)]
enum PanelWidth {
    Compact,
    Balanced,
    Wide,
}

impl PanelWidth {
    const fn from_ratio(ratio: SplitRatio) -> Self {
        match ratio.value() {
            1..=289 => Self::Compact,
            290..=379 => Self::Balanced,
            _ => Self::Wide,
        }
    }

    const fn ratio(self) -> SplitRatio {
        match self {
            Self::Compact => COMPACT_PANEL,
            Self::Balanced => BALANCED_PANEL,
            Self::Wide => WIDE_PANEL,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Balanced => "Balanced",
            Self::Wide => "Wide",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Compact => Self::Balanced,
            Self::Balanced => Self::Wide,
            Self::Wide => Self::Compact,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::Compact => Self::Wide,
            Self::Balanced => Self::Compact,
            Self::Wide => Self::Balanced,
        }
    }
}

impl Config {
    fn panel_width(&self) -> PanelWidth {
        PanelWidth::from_ratio(self.ui.panel_ratio)
    }

    pub(super) fn set_panel_ratio(&mut self, ratio: SplitRatio) -> Result<()> {
        let previous = self.ui.panel_ratio;
        self.ui.panel_ratio = ratio;
        if let Err(error) = self.save() {
            self.ui.panel_ratio = previous;
            return Err(error);
        }
        Ok(())
    }

    fn set_keybinding(&mut self, id: SettingId, chord: KeyChord) -> Result<()> {
        let previous = self.ui.keybindings.clone();
        let mut next = previous.clone();
        next.replace(id, chord)?;
        self.ui.keybindings = next;
        if let Err(error) = self.save() {
            self.ui.keybindings = previous;
            return Err(error);
        }
        Ok(())
    }
}

impl EditableSettings for Config {
    fn fields(&self) -> Vec<SettingField> {
        let mut fields = vec![SettingField::Choice {
            id: PANEL_WIDTH,
            label: "Project panel width",
            description: "Choose the initial width; dragging the divider saves the exact ratio.",
            selected: self.panel_width().label(),
        }];
        fields.extend([
            keybinding_field(
                COMMAND_PALETTE,
                "Command palette",
                "Search every Synced Projects command.",
                self.ui.keybindings.command_palette,
            ),
            keybinding_field(
                ADD_PROJECT,
                "Add project",
                "Open the new Synced Project form.",
                self.ui.keybindings.add_project,
            ),
            keybinding_field(
                TOGGLE_PAUSE,
                "Pause or resume",
                "Toggle synchronization for the selected project.",
                self.ui.keybindings.toggle_pause,
            ),
            keybinding_field(
                FLUSH,
                "Flush",
                "Synchronize pending changes now.",
                self.ui.keybindings.flush,
            ),
            keybinding_field(
                DOCTOR,
                "Doctor",
                "Diagnose the selected project.",
                self.ui.keybindings.doctor,
            ),
            keybinding_field(
                REMOVE,
                "Remove",
                "Remove the selected project while preserving files.",
                self.ui.keybindings.remove,
            ),
            keybinding_field(
                REFRESH,
                "Refresh",
                "Refresh every project state.",
                self.ui.keybindings.refresh,
            ),
            keybinding_field(
                OPEN_SETTINGS,
                "Settings",
                "Open Synced Projects settings.",
                self.ui.keybindings.open_settings,
            ),
            keybinding_field(
                PREVIOUS_PROJECT,
                "Previous project",
                "Select the previous project.",
                self.ui.keybindings.previous_project,
            ),
            keybinding_field(
                NEXT_PROJECT,
                "Next project",
                "Select the next project.",
                self.ui.keybindings.next_project,
            ),
            keybinding_field(
                HISTORY_BACK,
                "History back",
                "Return to the previously selected project.",
                self.ui.keybindings.history_back,
            ),
            keybinding_field(
                HISTORY_FORWARD,
                "History forward",
                "Move forward through project selection history.",
                self.ui.keybindings.history_forward,
            ),
            keybinding_field(
                FOCUS_NEXT,
                "Focus next",
                "Move focus to the next panel.",
                self.ui.keybindings.focus_next,
            ),
            keybinding_field(
                FOCUS_PREVIOUS,
                "Focus previous",
                "Move focus to the previous panel.",
                self.ui.keybindings.focus_previous,
            ),
            keybinding_field(
                QUIT,
                "Quit",
                "Close the Synced Projects dashboard.",
                self.ui.keybindings.quit,
            ),
        ]);
        fields
    }

    fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
        match (id, edit) {
            (PANEL_WIDTH, SettingEdit::Activate | SettingEdit::Next) => {
                self.set_panel_ratio(self.panel_width().next().ratio())
            }
            (PANEL_WIDTH, SettingEdit::Previous) => {
                self.set_panel_ratio(self.panel_width().previous().ratio())
            }
            (PANEL_WIDTH, SettingEdit::Reset) => self.set_panel_ratio(default_panel_ratio()),
            (
                COMMAND_PALETTE | ADD_PROJECT | TOGGLE_PAUSE | FLUSH | DOCTOR | REMOVE | REFRESH
                | OPEN_SETTINGS | PREVIOUS_PROJECT | NEXT_PROJECT | HISTORY_BACK | HISTORY_FORWARD
                | FOCUS_NEXT | FOCUS_PREVIOUS | QUIT,
                SettingEdit::SetKeybinding(value),
            ) => self.set_keybinding(id, value.parse()?),
            (
                COMMAND_PALETTE | ADD_PROJECT | TOGGLE_PAUSE | FLUSH | DOCTOR | REMOVE | REFRESH
                | OPEN_SETTINGS | PREVIOUS_PROJECT | NEXT_PROJECT | HISTORY_BACK | HISTORY_FORWARD
                | FOCUS_NEXT | FOCUS_PREVIOUS | QUIT,
                SettingEdit::Reset,
            ) => {
                let default = Keybindings::default()
                    .entries()
                    .into_iter()
                    .find_map(|(candidate, chord)| (candidate == id).then_some(chord))
                    .context("resolve default Synced Projects keybinding")?;
                self.set_keybinding(id, default)
            }
            _ => bail!("unknown Synced Projects Settings field '{}'", id.0),
        }
    }
}

fn keybinding_field(
    id: SettingId,
    label: &'static str,
    description: &'static str,
    value: KeyChord,
) -> SettingField {
    SettingField::Keybinding { id, label, description, value: value.to_string() }
}

fn open_settings(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
    Ok(Box::new(Config::load(store)?))
}

pub(super) fn settings() -> SettingsSection {
    SettingsSection::new(
        SettingsSectionMeta {
            id: TOOL,
            title: "Synced Projects",
            description: "Bidirectional source synchronization presentation",
        },
        open_settings,
    )
}

const fn default_panel_ratio() -> SplitRatio {
    BALANCED_PANEL
}

fn default_command_palette() -> KeyChord {
    KeyChord::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
}

fn default_add_project() -> KeyChord {
    KeyChord::new(KeyCode::Char('n'), KeyModifiers::NONE)
}

fn default_toggle_pause() -> KeyChord {
    KeyChord::new(KeyCode::Char(' '), KeyModifiers::NONE)
}

fn default_flush() -> KeyChord {
    KeyChord::new(KeyCode::Char('f'), KeyModifiers::NONE)
}

fn default_doctor() -> KeyChord {
    KeyChord::new(KeyCode::Char('d'), KeyModifiers::NONE)
}

fn default_remove() -> KeyChord {
    KeyChord::new(KeyCode::Char('x'), KeyModifiers::NONE)
}

fn default_refresh() -> KeyChord {
    KeyChord::new(KeyCode::Char('r'), KeyModifiers::NONE)
}

fn default_open_settings() -> KeyChord {
    KeyChord::new(KeyCode::Char(','), KeyModifiers::NONE)
}

fn default_previous_project() -> KeyChord {
    KeyChord::new(KeyCode::Up, KeyModifiers::NONE)
}

fn default_next_project() -> KeyChord {
    KeyChord::new(KeyCode::Down, KeyModifiers::NONE)
}

fn default_history_back() -> KeyChord {
    KeyChord::new(KeyCode::Left, KeyModifiers::NONE)
}

fn default_history_forward() -> KeyChord {
    KeyChord::new(KeyCode::Right, KeyModifiers::NONE)
}

fn default_focus_next() -> KeyChord {
    KeyChord::new(KeyCode::Tab, KeyModifiers::NONE)
}

fn default_focus_previous() -> KeyChord {
    KeyChord::new(KeyCode::BackTab, KeyModifiers::SHIFT)
}

fn default_quit() -> KeyChord {
    KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::tools::sync::model::{LocalEndpoint, RemoteEndpoint, SourcePolicy};

    fn store(name: &str) -> ConfigStore {
        ConfigStore::rooted(
            std::env::temp_dir().join(format!("kit-sync-config-{name}-{}", uuid::Uuid::new_v4())),
        )
    }

    fn project(name: &str) -> SyncedProject {
        SyncedProject::new(
            name,
            LocalEndpoint::new(PathBuf::from(format!("/work/{name}"))).unwrap(),
            RemoteEndpoint::new(
                format!("node-{name}"),
                "remote-user",
                PathBuf::from(format!("/workspace/{name}")),
            )
            .unwrap(),
            SourcePolicy::default(),
        )
        .unwrap()
    }

    #[test]
    fn missing_config_is_empty_and_add_remove_round_trips_atomically() -> Result<()> {
        let store = store("round-trip");
        let mut config = Config::load(store.clone())?;
        assert!(config.projects().is_empty());

        let project = project("kit");
        let id = project.id();
        config.add(project)?;
        assert_eq!(Config::load(store.clone())?.project(id).unwrap().name(), "kit");
        assert_eq!(config.set_lifecycle(id, ProjectLifecycle::Paused)?, ProjectLifecycle::Creating);
        assert_eq!(
            Config::load(store.clone())?.project(id).unwrap().lifecycle(),
            ProjectLifecycle::Paused
        );

        let removed = config.remove(id)?.expect("configured project");
        assert_eq!(removed.name(), "kit");
        assert!(Config::load(store)?.projects().is_empty());
        Ok(())
    }

    #[test]
    fn duplicate_names_and_unknown_fields_fail_without_replacing_configuration() -> Result<()> {
        let store = store("strict");
        let mut config = Config::load(store.clone())?;
        config.add(project("kit"))?;
        let before = std::fs::read_to_string(store.path(TOOL))?;

        let error = config.add(project("kit")).expect_err("duplicate name must fail");
        assert!(format!("{error:#}").contains("already configured"));
        assert_eq!(std::fs::read_to_string(store.path(TOOL))?, before);

        std::fs::write(store.path(TOOL), "version = 1\nunexpected = true\n")?;
        let error = Config::load(store).err().expect("unknown field must fail");
        assert!(format!("{error:#}").contains("unknown field"));
        Ok(())
    }

    #[test]
    fn unsupported_schema_version_fails_explicitly() -> Result<()> {
        let store = store("version");
        std::fs::create_dir_all(store.path(TOOL).parent().unwrap())?;
        std::fs::write(store.path(TOOL), "version = 3\nprojects = []\n")?;

        let error = Config::load(store).err().expect("unsupported version must fail");
        assert!(format!("{error:#}").contains("expected 2"));
        Ok(())
    }
}
