use std::{collections::HashSet, path::PathBuf};

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

const TOOL: &str = "skills";
const SCHEMA_VERSION: u32 = 1;

const PANEL_WIDTH: SettingId = SettingId("panel_width");
const COMMAND_PALETTE: SettingId = SettingId("command_palette");
const CREATE_SKILL: SettingId = SettingId("create_skill");
const SEARCH: SettingId = SettingId("search");
const TOGGLE_PROJECTION: SettingId = SettingId("toggle_projection");
const DOCTOR: SettingId = SettingId("doctor");
const SET_LIBRARY: SettingId = SettingId("set_library");
const REFRESH: SettingId = SettingId("refresh");
const OPEN_SETTINGS: SettingId = SettingId("open_settings");
const HELP: SettingId = SettingId("help");
const PREVIOUS_SKILL: SettingId = SettingId("previous_skill");
const NEXT_SKILL: SettingId = SettingId("next_skill");
const PREVIOUS_PROJECTION: SettingId = SettingId("previous_projection");
const NEXT_PROJECTION: SettingId = SettingId("next_projection");
const HISTORY_BACK: SettingId = SettingId("history_back");
const HISTORY_FORWARD: SettingId = SettingId("history_forward");
const PREVIOUS_TAB: SettingId = SettingId("previous_tab");
const NEXT_TAB: SettingId = SettingId("next_tab");
const FOCUS_NEXT: SettingId = SettingId("focus_next");
const FOCUS_PREVIOUS: SettingId = SettingId("focus_previous");
const QUIT: SettingId = SettingId("quit");

const COMPACT_PANEL: SplitRatio = SplitRatio::new(360);
const BALANCED_PANEL: SplitRatio = SplitRatio::new(440);
const WIDE_PANEL: SplitRatio = SplitRatio::new(540);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    #[serde(default = "schema_version")]
    version: u32,
    #[serde(default)]
    library: Option<PathBuf>,
    #[serde(default)]
    ui: UiConfig,
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self { version: SCHEMA_VERSION, library: None, ui: UiConfig::default() }
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
    #[serde(default = "default_create_skill")]
    pub create_skill: KeyChord,
    #[serde(default = "default_search")]
    pub search: KeyChord,
    #[serde(default = "default_toggle_projection")]
    pub toggle_projection: KeyChord,
    #[serde(default = "default_doctor")]
    pub doctor: KeyChord,
    #[serde(default = "default_set_library")]
    pub set_library: KeyChord,
    #[serde(default = "default_refresh")]
    pub refresh: KeyChord,
    #[serde(default = "default_open_settings")]
    pub open_settings: KeyChord,
    #[serde(default = "default_help")]
    pub help: KeyChord,
    #[serde(default = "default_previous_skill")]
    pub previous_skill: KeyChord,
    #[serde(default = "default_next_skill")]
    pub next_skill: KeyChord,
    #[serde(default = "default_previous_projection")]
    pub previous_projection: KeyChord,
    #[serde(default = "default_next_projection")]
    pub next_projection: KeyChord,
    #[serde(default = "default_history_back")]
    pub history_back: KeyChord,
    #[serde(default = "default_history_forward")]
    pub history_forward: KeyChord,
    #[serde(default = "default_previous_tab")]
    pub previous_tab: KeyChord,
    #[serde(default = "default_next_tab")]
    pub next_tab: KeyChord,
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
            create_skill: default_create_skill(),
            search: default_search(),
            toggle_projection: default_toggle_projection(),
            doctor: default_doctor(),
            set_library: default_set_library(),
            refresh: default_refresh(),
            open_settings: default_open_settings(),
            help: default_help(),
            previous_skill: default_previous_skill(),
            next_skill: default_next_skill(),
            previous_projection: default_previous_projection(),
            next_projection: default_next_projection(),
            history_back: default_history_back(),
            history_forward: default_history_forward(),
            previous_tab: default_previous_tab(),
            next_tab: default_next_tab(),
            focus_next: default_focus_next(),
            focus_previous: default_focus_previous(),
            quit: default_quit(),
        }
    }
}

pub(super) struct Config {
    store: ConfigStore,
    library: Option<PathBuf>,
    ui: UiConfig,
}

impl Config {
    pub(super) fn load(store: ConfigStore) -> Result<Self> {
        let stored: StoredConfig = store.load(TOOL)?;
        if stored.version != SCHEMA_VERSION {
            bail!(
                "unsupported Skills configuration version {}; expected {SCHEMA_VERSION}",
                stored.version
            );
        }
        if let Some(path) = &stored.library {
            if !path.is_absolute() {
                bail!("configured Skills library path must be absolute: {}", path.display());
            }
        }
        stored.ui.keybindings.validate()?;
        Ok(Self { store, library: stored.library, ui: stored.ui })
    }

    pub(super) fn library(&self) -> Option<&std::path::Path> {
        self.library.as_deref()
    }

    pub(super) fn ui(&self) -> &UiConfig {
        &self.ui
    }

    pub(super) fn store(&self) -> ConfigStore {
        self.store.clone()
    }

    pub(super) fn set_library(&mut self, path: PathBuf) -> Result<()> {
        if !path.is_absolute() {
            bail!("Skills library path must be absolute: {}", path.display());
        }
        let previous = self.library.replace(path);
        if let Err(error) = self.save() {
            self.library = previous;
            return Err(error);
        }
        Ok(())
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

    fn save(&self) -> Result<()> {
        self.store
            .save(
                TOOL,
                &StoredConfig {
                    version: SCHEMA_VERSION,
                    library: self.library.clone(),
                    ui: self.ui.clone(),
                },
            )
            .context("save Skills configuration")
    }

    fn panel_width(&self) -> PanelWidth {
        PanelWidth::from_ratio(self.ui.panel_ratio)
    }
}

impl Keybindings {
    pub(super) fn contains(&self, chord: KeyChord) -> bool {
        self.entries().into_iter().any(|(_, configured)| configured == chord)
    }

    fn validate(&self) -> Result<()> {
        let mut configured = HashSet::new();
        for (id, chord) in self.entries() {
            if !configured.insert(chord) {
                bail!("Skills keybinding '{}' duplicates another command", id.0);
            }
        }
        Ok(())
    }

    fn replace(&mut self, id: SettingId, chord: KeyChord) -> Result<()> {
        match id {
            COMMAND_PALETTE => self.command_palette = chord,
            CREATE_SKILL => self.create_skill = chord,
            SEARCH => self.search = chord,
            TOGGLE_PROJECTION => self.toggle_projection = chord,
            DOCTOR => self.doctor = chord,
            SET_LIBRARY => self.set_library = chord,
            REFRESH => self.refresh = chord,
            OPEN_SETTINGS => self.open_settings = chord,
            HELP => self.help = chord,
            PREVIOUS_SKILL => self.previous_skill = chord,
            NEXT_SKILL => self.next_skill = chord,
            PREVIOUS_PROJECTION => self.previous_projection = chord,
            NEXT_PROJECTION => self.next_projection = chord,
            HISTORY_BACK => self.history_back = chord,
            HISTORY_FORWARD => self.history_forward = chord,
            PREVIOUS_TAB => self.previous_tab = chord,
            NEXT_TAB => self.next_tab = chord,
            FOCUS_NEXT => self.focus_next = chord,
            FOCUS_PREVIOUS => self.focus_previous = chord,
            QUIT => self.quit = chord,
            _ => bail!("unknown Skills keybinding '{}'", id.0),
        }
        self.validate()
    }

    fn entries(&self) -> [(SettingId, KeyChord); 20] {
        [
            (COMMAND_PALETTE, self.command_palette),
            (CREATE_SKILL, self.create_skill),
            (SEARCH, self.search),
            (TOGGLE_PROJECTION, self.toggle_projection),
            (DOCTOR, self.doctor),
            (SET_LIBRARY, self.set_library),
            (REFRESH, self.refresh),
            (OPEN_SETTINGS, self.open_settings),
            (HELP, self.help),
            (PREVIOUS_SKILL, self.previous_skill),
            (NEXT_SKILL, self.next_skill),
            (PREVIOUS_PROJECTION, self.previous_projection),
            (NEXT_PROJECTION, self.next_projection),
            (HISTORY_BACK, self.history_back),
            (HISTORY_FORWARD, self.history_forward),
            (PREVIOUS_TAB, self.previous_tab),
            (NEXT_TAB, self.next_tab),
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
            1..=399 => Self::Compact,
            400..=489 => Self::Balanced,
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

impl EditableSettings for Config {
    fn fields(&self) -> Vec<SettingField> {
        let mut fields = vec![SettingField::Choice {
            id: PANEL_WIDTH,
            label: "Skills panel width",
            description: "Choose the initial width; dragging the divider saves the exact ratio.",
            selected: self.panel_width().label(),
        }];
        fields.extend([
            keybinding_field(
                COMMAND_PALETTE,
                "Command palette",
                "Search every Skills command.",
                self.ui.keybindings.command_palette,
            ),
            keybinding_field(
                CREATE_SKILL,
                "Create skill",
                "Create a canonical skill.",
                self.ui.keybindings.create_skill,
            ),
            keybinding_field(
                SEARCH,
                "Search",
                "Filter the canonical skill catalog.",
                self.ui.keybindings.search,
            ),
            keybinding_field(
                TOGGLE_PROJECTION,
                "Toggle availability",
                "Enable or disable the selected destination.",
                self.ui.keybindings.toggle_projection,
            ),
            keybinding_field(
                DOCTOR,
                "Doctor",
                "Show invalid skills and unsafe availability paths.",
                self.ui.keybindings.doctor,
            ),
            keybinding_field(
                SET_LIBRARY,
                "Set library",
                "Choose the canonical skill library.",
                self.ui.keybindings.set_library,
            ),
            keybinding_field(
                REFRESH,
                "Refresh",
                "Re-read the library and every availability destination.",
                self.ui.keybindings.refresh,
            ),
            keybinding_field(
                OPEN_SETTINGS,
                "Settings",
                "Open Skills settings.",
                self.ui.keybindings.open_settings,
            ),
            keybinding_field(
                HELP,
                "Help",
                "Show the Skills keyboard and availability legend.",
                self.ui.keybindings.help,
            ),
            keybinding_field(
                PREVIOUS_SKILL,
                "Previous skill",
                "Select the previous skill.",
                self.ui.keybindings.previous_skill,
            ),
            keybinding_field(
                NEXT_SKILL,
                "Next skill",
                "Select the next skill.",
                self.ui.keybindings.next_skill,
            ),
            keybinding_field(
                PREVIOUS_PROJECTION,
                "Previous destination",
                "Focus the previous availability column.",
                self.ui.keybindings.previous_projection,
            ),
            keybinding_field(
                NEXT_PROJECTION,
                "Next destination",
                "Focus the next availability column.",
                self.ui.keybindings.next_projection,
            ),
            keybinding_field(
                HISTORY_BACK,
                "Selection history back",
                "Return to the previously selected skill.",
                self.ui.keybindings.history_back,
            ),
            keybinding_field(
                HISTORY_FORWARD,
                "Selection history forward",
                "Move forward through skill selection history.",
                self.ui.keybindings.history_forward,
            ),
            keybinding_field(
                PREVIOUS_TAB,
                "Previous detail tab",
                "Open the previous detail tab.",
                self.ui.keybindings.previous_tab,
            ),
            keybinding_field(
                NEXT_TAB,
                "Next detail tab",
                "Open the next detail tab.",
                self.ui.keybindings.next_tab,
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
            keybinding_field(QUIT, "Quit", "Close the Skills manager.", self.ui.keybindings.quit),
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
            (id, SettingEdit::SetKeybinding(value)) if is_keybinding(id) => {
                self.set_keybinding(id, value.parse()?)
            }
            (id, SettingEdit::Reset) if is_keybinding(id) => {
                let default = Keybindings::default()
                    .entries()
                    .into_iter()
                    .find_map(|(candidate, chord)| (candidate == id).then_some(chord))
                    .context("resolve default Skills keybinding")?;
                self.set_keybinding(id, default)
            }
            _ => bail!("unknown Skills Settings field '{}'", id.0),
        }
    }
}

fn is_keybinding(id: SettingId) -> bool {
    Keybindings::default().entries().into_iter().any(|(candidate, _)| candidate == id)
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
            title: "Skills",
            description: "Canonical library and skill availability presentation",
        },
        open_settings,
    )
}

const fn schema_version() -> u32 {
    SCHEMA_VERSION
}

const fn default_panel_ratio() -> SplitRatio {
    BALANCED_PANEL
}

fn default_command_palette() -> KeyChord {
    KeyChord::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
}

fn default_create_skill() -> KeyChord {
    KeyChord::new(KeyCode::Char('n'), KeyModifiers::NONE)
}

fn default_search() -> KeyChord {
    KeyChord::new(KeyCode::Char('/'), KeyModifiers::NONE)
}

fn default_toggle_projection() -> KeyChord {
    KeyChord::new(KeyCode::Char(' '), KeyModifiers::NONE)
}

fn default_doctor() -> KeyChord {
    KeyChord::new(KeyCode::Char('D'), KeyModifiers::NONE)
}

fn default_set_library() -> KeyChord {
    KeyChord::new(KeyCode::Char('L'), KeyModifiers::NONE)
}

fn default_refresh() -> KeyChord {
    KeyChord::new(KeyCode::Char('r'), KeyModifiers::NONE)
}

fn default_open_settings() -> KeyChord {
    KeyChord::new(KeyCode::Char('s'), KeyModifiers::NONE)
}

fn default_help() -> KeyChord {
    KeyChord::new(KeyCode::Char('?'), KeyModifiers::NONE)
}

fn default_previous_skill() -> KeyChord {
    KeyChord::new(KeyCode::Up, KeyModifiers::NONE)
}

fn default_next_skill() -> KeyChord {
    KeyChord::new(KeyCode::Down, KeyModifiers::NONE)
}

fn default_previous_projection() -> KeyChord {
    KeyChord::new(KeyCode::Left, KeyModifiers::NONE)
}

fn default_next_projection() -> KeyChord {
    KeyChord::new(KeyCode::Right, KeyModifiers::NONE)
}

fn default_history_back() -> KeyChord {
    KeyChord::new(KeyCode::Left, KeyModifiers::ALT)
}

fn default_history_forward() -> KeyChord {
    KeyChord::new(KeyCode::Right, KeyModifiers::ALT)
}

fn default_previous_tab() -> KeyChord {
    KeyChord::new(KeyCode::Char('['), KeyModifiers::NONE)
}

fn default_next_tab() -> KeyChord {
    KeyChord::new(KeyCode::Char(']'), KeyModifiers::NONE)
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
