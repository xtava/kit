use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::{
    framework::{
        ConfigStore, ConfigValue, EditableSettings, SettingEdit, SettingField, SettingId,
        SettingsSection, SettingsSectionMeta,
    },
    tui::{KeyChord, SplitRatio},
};

const TOOL: &str = "stream";
const SCHEMA_VERSION: u32 = 1;
const MOUSE: SettingId = SettingId("mouse");
const COMMAND_PALETTE: SettingId = SettingId("command_palette");
const REFRESH: SettingId = SettingId("refresh");
const START: SettingId = SettingId("start");
const STOP: SettingId = SettingId("stop");
const RECOVER: SettingId = SettingId("recover");
const OPEN_SETTINGS: SettingId = SettingId("open_settings");
const HISTORY_BACK: SettingId = SettingId("history_back");
const HISTORY_FORWARD: SettingId = SettingId("history_forward");
const QUIT: SettingId = SettingId("quit");

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    #[serde(default = "schema_version")]
    version: u32,
    #[serde(default)]
    preferred_host: Option<String>,
    #[serde(default)]
    ssh_users: BTreeMap<String, String>,
    #[serde(default)]
    executables: Executables,
    #[serde(default = "default_preset_name")]
    default_preset: String,
    #[serde(default = "default_presets")]
    presets: BTreeMap<String, StreamPreset>,
    #[serde(default)]
    client: ClientPreferences,
    #[serde(default)]
    ui: UiConfig,
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            preferred_host: None,
            ssh_users: BTreeMap::new(),
            executables: Executables::default(),
            default_preset: default_preset_name(),
            presets: default_presets(),
            client: ClientPreferences::default(),
            ui: UiConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Executables {
    #[serde(default = "default_hyprctl")]
    pub hyprctl: String,
    #[serde(default = "default_sunshine")]
    pub sunshine: String,
    #[serde(default = "default_moonlight")]
    pub moonlight: String,
    #[serde(default = "default_systemctl")]
    pub systemctl: String,
    #[serde(default = "default_socket_inspector")]
    pub socket_inspector: String,
    #[serde(default = "default_kit")]
    pub remote_kit: String,
}

impl Default for Executables {
    fn default() -> Self {
        Self {
            hyprctl: default_hyprctl(),
            sunshine: default_sunshine(),
            moonlight: default_moonlight(),
            systemctl: default_systemctl(),
            socket_inspector: default_socket_inspector(),
            remote_kit: default_kit(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StreamPreset {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub bitrate_kbps: u32,
    #[serde(default = "default_scale")]
    pub scale: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientPreferences {
    #[serde(default)]
    pub preferred_client: Option<String>,
    #[serde(default)]
    pub absolute_mouse: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UiConfig {
    #[serde(default = "enabled")]
    pub mouse: bool,
    #[serde(default = "default_machine_panel_ratio")]
    pub machine_panel_ratio: SplitRatio,
    #[serde(default)]
    pub keybindings: Keybindings,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            mouse: true,
            machine_panel_ratio: default_machine_panel_ratio(),
            keybindings: Keybindings::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Keybindings {
    #[serde(default = "default_command_palette")]
    pub command_palette: KeyChord,
    #[serde(default = "default_refresh")]
    pub refresh: KeyChord,
    #[serde(default = "default_start")]
    pub start: KeyChord,
    #[serde(default = "default_stop")]
    pub stop: KeyChord,
    #[serde(default = "default_recover")]
    pub recover: KeyChord,
    #[serde(default = "default_open_settings")]
    pub open_settings: KeyChord,
    #[serde(default = "default_history_back")]
    pub history_back: KeyChord,
    #[serde(default = "default_history_forward")]
    pub history_forward: KeyChord,
    #[serde(default = "default_quit")]
    pub quit: KeyChord,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            command_palette: default_command_palette(),
            refresh: default_refresh(),
            start: default_start(),
            stop: default_stop(),
            recover: default_recover(),
            open_settings: default_open_settings(),
            history_back: default_history_back(),
            history_forward: default_history_forward(),
            quit: default_quit(),
        }
    }
}

pub(super) struct Config {
    store: ConfigStore,
    stored: StoredConfig,
}

impl Config {
    pub(super) fn load(store: ConfigStore) -> Result<Self> {
        let stored: StoredConfig = store.load(TOOL)?;
        validate(&stored)?;
        Ok(Self { store, stored })
    }

    pub(super) fn preferred_host(&self) -> Option<&str> {
        self.stored.preferred_host.as_deref()
    }

    pub(super) fn ssh_user(&self, stable_node_id: &str) -> Option<&str> {
        self.stored.ssh_users.get(stable_node_id).map(String::as_str)
    }

    pub(super) fn executables(&self) -> &Executables {
        &self.stored.executables
    }

    pub(super) fn default_preset(&self) -> (&str, &StreamPreset) {
        let name = self.stored.default_preset.as_str();
        let preset = self
            .stored
            .presets
            .get(name)
            .expect("validated Stream config contains its default preset");
        (name, preset)
    }

    pub(super) fn set_ssh_user(&mut self, stable_node_id: &str, user: &str) -> Result<()> {
        validate_scalar("stable node ID", stable_node_id)?;
        validate_scalar("SSH user", user)?;
        self.store.set_table_string(TOOL, "ssh_users", stable_node_id, user)?;
        self.stored.ssh_users.insert(stable_node_id.to_owned(), user.to_owned());
        Ok(())
    }

    pub(super) fn set_preferred_host(&mut self, stable_node_id: &str) -> Result<()> {
        validate_scalar("preferred host", stable_node_id)?;
        self.store.set(TOOL, "preferred_host", ConfigValue::String(stable_node_id.to_owned()))?;
        self.stored.preferred_host = Some(stable_node_id.to_owned());
        Ok(())
    }

    fn save(&self) -> Result<()> {
        self.store.save(TOOL, &self.stored)
    }

    fn set_mouse(&mut self, value: bool) -> Result<()> {
        let previous = self.stored.ui.mouse;
        self.stored.ui.mouse = value;
        if let Err(error) = self.save() {
            self.stored.ui.mouse = previous;
            return Err(error);
        }
        Ok(())
    }

    fn set_keybinding(&mut self, id: SettingId, chord: KeyChord) -> Result<()> {
        let previous = self.stored.ui.keybindings.clone();
        let target = match id {
            COMMAND_PALETTE => &mut self.stored.ui.keybindings.command_palette,
            REFRESH => &mut self.stored.ui.keybindings.refresh,
            START => &mut self.stored.ui.keybindings.start,
            STOP => &mut self.stored.ui.keybindings.stop,
            RECOVER => &mut self.stored.ui.keybindings.recover,
            OPEN_SETTINGS => &mut self.stored.ui.keybindings.open_settings,
            HISTORY_BACK => &mut self.stored.ui.keybindings.history_back,
            HISTORY_FORWARD => &mut self.stored.ui.keybindings.history_forward,
            QUIT => &mut self.stored.ui.keybindings.quit,
            _ => bail!("unknown Stream keybinding '{}'", id.0),
        };
        *target = chord;
        if let Err(error) = self.save() {
            self.stored.ui.keybindings = previous;
            return Err(error);
        }
        Ok(())
    }
}

impl EditableSettings for Config {
    fn fields(&self) -> Vec<SettingField> {
        let keybindings = &self.stored.ui.keybindings;
        vec![
            SettingField::Toggle {
                id: MOUSE,
                label: "Mouse interaction",
                description: "Enable clicking, scrolling, context menus, and panel resizing.",
                value: self.stored.ui.mouse,
            },
            keybinding_field(
                COMMAND_PALETTE,
                "Command palette",
                "Search every Stream command.",
                keybindings.command_palette,
            ),
            keybinding_field(
                REFRESH,
                "Refresh",
                "Refresh host and source state.",
                keybindings.refresh,
            ),
            keybinding_field(
                START,
                "Start",
                "Start the selected Stream session.",
                keybindings.start,
            ),
            keybinding_field(STOP, "Stop", "Stop the active Stream session.", keybindings.stop),
            keybinding_field(
                RECOVER,
                "Recover",
                "Recover an interrupted Stream session.",
                keybindings.recover,
            ),
            keybinding_field(
                OPEN_SETTINGS,
                "Settings",
                "Open Stream settings.",
                keybindings.open_settings,
            ),
            keybinding_field(
                HISTORY_BACK,
                "History back",
                "Return to the previous Stream selection.",
                keybindings.history_back,
            ),
            keybinding_field(
                HISTORY_FORWARD,
                "History forward",
                "Move forward through Stream selection history.",
                keybindings.history_forward,
            ),
            keybinding_field(QUIT, "Quit", "Close Stream.", keybindings.quit),
        ]
    }

    fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
        match (id, edit) {
            (MOUSE, SettingEdit::Activate) => self.set_mouse(!self.stored.ui.mouse),
            (MOUSE, SettingEdit::Reset) => self.set_mouse(true),
            (
                COMMAND_PALETTE | REFRESH | START | STOP | RECOVER | OPEN_SETTINGS | HISTORY_BACK
                | HISTORY_FORWARD | QUIT,
                SettingEdit::SetKeybinding(value),
            ) => self.set_keybinding(id, value.parse()?),
            (
                COMMAND_PALETTE | REFRESH | START | STOP | RECOVER | OPEN_SETTINGS | HISTORY_BACK
                | HISTORY_FORWARD | QUIT,
                SettingEdit::Reset,
            ) => {
                let defaults = Keybindings::default();
                let value = match id {
                    COMMAND_PALETTE => defaults.command_palette,
                    REFRESH => defaults.refresh,
                    START => defaults.start,
                    STOP => defaults.stop,
                    RECOVER => defaults.recover,
                    OPEN_SETTINGS => defaults.open_settings,
                    HISTORY_BACK => defaults.history_back,
                    HISTORY_FORWARD => defaults.history_forward,
                    QUIT => defaults.quit,
                    _ => unreachable!("matched Stream keybinding"),
                };
                self.set_keybinding(id, value)
            }
            _ => bail!("unknown Stream Settings field '{}'", id.0),
        }
    }
}

pub(super) fn settings() -> SettingsSection {
    SettingsSection::new(
        SettingsSectionMeta {
            id: TOOL,
            title: "Stream",
            description: "Linux streaming hosts, presets, and controls",
        },
        |store| Ok(Box::new(Config::load(store)?)),
    )
}

fn validate(stored: &StoredConfig) -> Result<()> {
    if stored.version != SCHEMA_VERSION {
        bail!("unsupported Stream config version {}; expected {SCHEMA_VERSION}", stored.version);
    }
    if let Some(host) = stored.preferred_host.as_deref() {
        validate_scalar("preferred host", host)?;
    }
    for (node, user) in &stored.ssh_users {
        validate_scalar("stable node ID", node)?;
        validate_scalar("SSH user", user)?;
    }
    for (name, value) in [
        ("hyprctl", &stored.executables.hyprctl),
        ("sunshine", &stored.executables.sunshine),
        ("moonlight", &stored.executables.moonlight),
        ("systemctl", &stored.executables.systemctl),
        ("socket inspector", &stored.executables.socket_inspector),
        ("remote Kit", &stored.executables.remote_kit),
    ] {
        validate_scalar(name, value)?;
    }
    if stored.presets.is_empty() {
        bail!("Stream config requires at least one preset");
    }
    for (name, preset) in &stored.presets {
        validate_scalar("preset name", name)?;
        if !(320..=8192).contains(&preset.width)
            || !(240..=8192).contains(&preset.height)
            || !(1..=360).contains(&preset.fps)
            || !(1_000..=500_000).contains(&preset.bitrate_kbps)
            || !preset.scale.is_finite()
            || !(0.5..=4.0).contains(&preset.scale)
        {
            bail!("Stream preset {name:?} has unsupported dimensions, rate, bitrate, or scale");
        }
    }
    stored.presets.get(&stored.default_preset).with_context(|| {
        format!("default Stream preset {:?} is not configured", stored.default_preset)
    })?;
    if let Some(client) = stored.client.preferred_client.as_deref() {
        validate_scalar("preferred client", client)?;
    }
    Ok(())
}

fn validate_scalar(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        bail!("{label} is empty, too long, or contains control characters");
    }
    Ok(())
}

fn keybinding_field(
    id: SettingId,
    label: &'static str,
    description: &'static str,
    value: KeyChord,
) -> SettingField {
    SettingField::Keybinding { id, label, description, value: value.to_string() }
}

fn default_presets() -> BTreeMap<String, StreamPreset> {
    BTreeMap::from([
        (
            "1080p60".to_owned(),
            StreamPreset { width: 1920, height: 1080, fps: 60, bitrate_kbps: 30_000, scale: 1.0 },
        ),
        (
            "1440p120".to_owned(),
            StreamPreset { width: 2560, height: 1440, fps: 120, bitrate_kbps: 80_000, scale: 1.0 },
        ),
    ])
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

fn default_preset_name() -> String {
    "1440p120".to_owned()
}

fn default_hyprctl() -> String {
    "hyprctl".to_owned()
}

fn default_sunshine() -> String {
    "sunshine".to_owned()
}

fn default_moonlight() -> String {
    "moonlight".to_owned()
}

fn default_systemctl() -> String {
    "systemctl".to_owned()
}

fn default_socket_inspector() -> String {
    "ss".to_owned()
}

fn default_kit() -> String {
    "kit".to_owned()
}

fn default_scale() -> f64 {
    1.0
}

const fn default_machine_panel_ratio() -> SplitRatio {
    SplitRatio::new(280)
}

const fn enabled() -> bool {
    true
}

fn default_command_palette() -> KeyChord {
    KeyChord::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
}

fn default_refresh() -> KeyChord {
    KeyChord::new(KeyCode::Char('r'), KeyModifiers::NONE)
}

fn default_start() -> KeyChord {
    KeyChord::new(KeyCode::Char('s'), KeyModifiers::NONE)
}

fn default_stop() -> KeyChord {
    KeyChord::new(KeyCode::Char('x'), KeyModifiers::NONE)
}

fn default_recover() -> KeyChord {
    KeyChord::new(KeyCode::Char('e'), KeyModifiers::NONE)
}

fn default_open_settings() -> KeyChord {
    KeyChord::new(KeyCode::Char(','), KeyModifiers::CONTROL)
}

fn default_history_back() -> KeyChord {
    KeyChord::new(KeyCode::Left, KeyModifiers::ALT)
}

fn default_history_forward() -> KeyChord {
    KeyChord::new(KeyCode::Right, KeyModifiers::ALT)
}

fn default_quit() -> KeyChord {
    KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE)
}
