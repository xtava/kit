use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::{
    framework::{
        ConfigStore, ConfigValue, EditableSettings, SettingEdit, SettingField, SettingId,
        SettingsSection, SettingsSectionMeta,
    },
    tui::{KeyChord, SplitRatio},
};

const TOOL: &str = "console";
const SIDEBAR_WIDTH: SettingId = SettingId("sidebar_width");
const SIDEBAR_SPLIT_RATIO: &str = "sidebar_split_ratio";
const TERMINAL_SPLIT_RATIO: &str = "terminal_split_ratio";
const READY_NOTIFICATION: SettingId = SettingId("ready_notification");
const SELECTED_MACHINE: &str = "selected_machine";
const PREFIX: SettingId = SettingId("prefix");
const NEW_SESSION: SettingId = SettingId("new_session");
const TOGGLE_SESSIONS: SettingId = SettingId("toggle_sessions");
const COMMAND_PALETTE: SettingId = SettingId("command_palette");
const HELP: SettingId = SettingId("help");
const QUIT: SettingId = SettingId("quit");

const COMPACT_SIDEBAR: SplitRatio = SplitRatio::new(200);
const BALANCED_SIDEBAR: SplitRatio = SplitRatio::new(260);
const WIDE_SIDEBAR: SplitRatio = SplitRatio::new(360);
const BALANCED_TERMINALS: SplitRatio = SplitRatio::new(500);

#[derive(Clone, Debug)]
pub(super) struct Config {
    store: ConfigStore,
    sidebar_split_ratio: SplitRatio,
    terminal_split_ratio: SplitRatio,
    ready_notification: ReadyNotification,
    keybindings: Keybindings,
    users: BTreeMap<String, String>,
    selected_machine: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Stored {
    #[serde(default = "default_sidebar_split_ratio")]
    sidebar_split_ratio: SplitRatio,
    #[serde(default = "default_terminal_split_ratio")]
    terminal_split_ratio: SplitRatio,
    #[serde(default = "default_ready_notification")]
    ready_notification: ReadyNotification,
    #[serde(default)]
    keybindings: Keybindings,
    #[serde(default)]
    users: BTreeMap<String, String>,
    #[serde(default)]
    selected_machine: Option<String>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            sidebar_split_ratio: default_sidebar_split_ratio(),
            terminal_split_ratio: default_terminal_split_ratio(),
            ready_notification: default_ready_notification(),
            keybindings: Keybindings::default(),
            users: BTreeMap::new(),
            selected_machine: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Keybindings {
    #[serde(default = "default_prefix")]
    pub(super) prefix: KeyChord,
    #[serde(default = "default_new_session")]
    pub(super) new_session: KeyChord,
    #[serde(default = "default_toggle_sessions")]
    pub(super) toggle_sessions: KeyChord,
    #[serde(default = "default_command_palette")]
    pub(super) command_palette: KeyChord,
    #[serde(default = "default_help")]
    pub(super) help: KeyChord,
    #[serde(default = "default_quit")]
    pub(super) quit: KeyChord,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            prefix: default_prefix(),
            new_session: default_new_session(),
            toggle_sessions: default_toggle_sessions(),
            command_palette: default_command_palette(),
            help: default_help(),
            quit: default_quit(),
        }
    }
}

impl Keybindings {
    fn validate(&self) -> Result<()> {
        let mut configured = HashSet::new();
        for (name, chord) in [
            (NEW_SESSION.0, self.new_session),
            (TOGGLE_SESSIONS.0, self.toggle_sessions),
            (HELP.0, self.help),
            (QUIT.0, self.quit),
        ] {
            if chord == self.prefix {
                bail!("Console keybinding '{name}' conflicts with the prefix");
            }
            if !configured.insert(chord) {
                bail!("Console keybinding '{name}' duplicates another prefixed command");
            }
        }
        if self.command_palette == self.prefix {
            bail!("Console keybinding '{}' conflicts with the prefix", COMMAND_PALETTE.0);
        }
        Ok(())
    }

    fn replace(&mut self, id: SettingId, chord: KeyChord) -> Result<&'static str> {
        let key = match id {
            PREFIX => {
                self.prefix = chord;
                PREFIX.0
            }
            NEW_SESSION => {
                self.new_session = chord;
                NEW_SESSION.0
            }
            TOGGLE_SESSIONS => {
                self.toggle_sessions = chord;
                TOGGLE_SESSIONS.0
            }
            COMMAND_PALETTE => {
                self.command_palette = chord;
                COMMAND_PALETTE.0
            }
            HELP => {
                self.help = chord;
                HELP.0
            }
            QUIT => {
                self.quit = chord;
                QUIT.0
            }
            _ => bail!("unknown Console keybinding '{}'", id.0),
        };
        self.validate()?;
        Ok(key)
    }
}

impl Config {
    pub(super) fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        stored.keybindings.validate()?;
        Ok(Self {
            store,
            sidebar_split_ratio: stored.sidebar_split_ratio,
            terminal_split_ratio: stored.terminal_split_ratio,
            ready_notification: stored.ready_notification,
            keybindings: stored.keybindings,
            users: stored.users,
            selected_machine: stored.selected_machine,
        })
    }

    pub(super) const fn sidebar_split_ratio(&self) -> SplitRatio {
        self.sidebar_split_ratio
    }

    pub(super) const fn terminal_split_ratio(&self) -> SplitRatio {
        self.terminal_split_ratio
    }

    pub(super) const fn keybindings(&self) -> &Keybindings {
        &self.keybindings
    }

    pub(super) const fn ready_notification(&self) -> ReadyNotification {
        self.ready_notification
    }

    pub(super) fn store(&self) -> ConfigStore {
        self.store.clone()
    }

    fn set_keybinding(&mut self, id: SettingId, chord: KeyChord) -> Result<()> {
        let mut keybindings = self.keybindings.clone();
        let key = keybindings.replace(id, chord)?;
        self.store.set_table_string(TOOL, "keybindings", key, &chord.to_string())?;
        self.keybindings = keybindings;
        Ok(())
    }

    fn reset_keybinding(&mut self, id: SettingId) -> Result<()> {
        let defaults = Keybindings::default();
        let chord = match id {
            PREFIX => defaults.prefix,
            NEW_SESSION => defaults.new_session,
            TOGGLE_SESSIONS => defaults.toggle_sessions,
            COMMAND_PALETTE => defaults.command_palette,
            HELP => defaults.help,
            QUIT => defaults.quit,
            _ => bail!("unknown Console keybinding '{}'", id.0),
        };
        self.set_keybinding(id, chord)
    }

    pub(super) fn set_sidebar_split_ratio(&mut self, ratio: SplitRatio) -> Result<()> {
        self.store.set(
            TOOL,
            SIDEBAR_SPLIT_RATIO,
            ConfigValue::Integer(i64::from(ratio.value())),
        )?;
        self.sidebar_split_ratio = ratio;
        Ok(())
    }

    pub(super) fn set_terminal_split_ratio(&mut self, ratio: SplitRatio) -> Result<()> {
        self.store.set(
            TOOL,
            TERMINAL_SPLIT_RATIO,
            ConfigValue::Integer(i64::from(ratio.value())),
        )?;
        self.terminal_split_ratio = ratio;
        Ok(())
    }

    pub(super) fn unix_user(&self, stable_node_id: &str) -> Option<&str> {
        self.users.get(stable_node_id).map(String::as_str)
    }

    pub(super) fn set_unix_user(&mut self, stable_node_id: &str, user: &str) -> Result<()> {
        self.store.set_table_string(TOOL, "users", stable_node_id, user)?;
        self.users.insert(stable_node_id.to_owned(), user.to_owned());
        Ok(())
    }

    pub(super) fn selected_machine(&self) -> Option<&str> {
        self.selected_machine.as_deref()
    }

    pub(super) fn set_selected_machine(&mut self, stable_node_id: &str) -> Result<()> {
        self.store.set(TOOL, SELECTED_MACHINE, ConfigValue::String(stable_node_id.to_owned()))?;
        self.selected_machine = Some(stable_node_id.to_owned());
        Ok(())
    }

    fn sidebar_width(&self) -> SidebarWidth {
        SidebarWidth::from_ratio(self.sidebar_split_ratio())
    }

    fn set_sidebar_width(&mut self, width: SidebarWidth) -> Result<()> {
        self.set_sidebar_split_ratio(width.ratio())
    }

    fn set_ready_notification(&mut self, notification: ReadyNotification) -> Result<()> {
        self.store.set(
            TOOL,
            READY_NOTIFICATION.0,
            ConfigValue::String(notification.as_str().to_owned()),
        )?;
        self.ready_notification = notification;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ReadyNotification {
    Off,
    SystemSound,
    TerminalBell,
}

impl ReadyNotification {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::SystemSound => "system-sound",
            Self::TerminalBell => "terminal-bell",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::SystemSound => "System sound",
            Self::TerminalBell => "Terminal bell",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::TerminalBell => Self::Off,
            Self::Off => Self::SystemSound,
            Self::SystemSound => Self::TerminalBell,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::TerminalBell => Self::SystemSound,
            Self::SystemSound => Self::Off,
            Self::Off => Self::TerminalBell,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidebarWidth {
    Compact,
    Balanced,
    Wide,
}

impl SidebarWidth {
    const fn from_ratio(ratio: SplitRatio) -> Self {
        match ratio.value() {
            1..=229 => Self::Compact,
            230..=319 => Self::Balanced,
            _ => Self::Wide,
        }
    }

    const fn ratio(self) -> SplitRatio {
        match self {
            Self::Compact => COMPACT_SIDEBAR,
            Self::Balanced => BALANCED_SIDEBAR,
            Self::Wide => WIDE_SIDEBAR,
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
        vec![
            SettingField::Choice {
                id: SIDEBAR_WIDTH,
                label: "Sidebar width",
                description:
                    "Choose the initial sidebar width; divider dragging saves the exact ratio.",
                selected: self.sidebar_width().label(),
            },
            SettingField::Choice {
                id: READY_NOTIFICATION,
                label: "Ready notification",
                description: "Choose the local sound played when an agent finishes working.",
                selected: self.ready_notification.label(),
            },
            keybinding_field(
                PREFIX,
                "Prefix",
                "Start a Console key sequence.",
                self.keybindings.prefix,
            ),
            keybinding_field(
                NEW_SESSION,
                "New session",
                "Create a persistent terminal session after the prefix.",
                self.keybindings.new_session,
            ),
            keybinding_field(
                TOGGLE_SESSIONS,
                "Toggle sessions",
                "Show or hide the sessions panel after the prefix.",
                self.keybindings.toggle_sessions,
            ),
            keybinding_field(
                COMMAND_PALETTE,
                "Command palette",
                "Open searchable Console commands directly.",
                self.keybindings.command_palette,
            ),
            keybinding_field(
                HELP,
                "Prefix help",
                "Open the command palette after the prefix.",
                self.keybindings.help,
            ),
            keybinding_field(
                QUIT,
                "Quit",
                "Close this Console client after the prefix.",
                self.keybindings.quit,
            ),
        ]
    }

    fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
        match (id, edit) {
            (SIDEBAR_WIDTH, SettingEdit::Activate | SettingEdit::Next) => {
                self.set_sidebar_width(self.sidebar_width().next())
            }
            (SIDEBAR_WIDTH, SettingEdit::Previous) => {
                self.set_sidebar_width(self.sidebar_width().previous())
            }
            (SIDEBAR_WIDTH, SettingEdit::Reset) => {
                self.set_sidebar_split_ratio(default_sidebar_split_ratio())
            }
            (READY_NOTIFICATION, SettingEdit::Activate | SettingEdit::Next) => {
                self.set_ready_notification(self.ready_notification.next())
            }
            (READY_NOTIFICATION, SettingEdit::Previous) => {
                self.set_ready_notification(self.ready_notification.previous())
            }
            (READY_NOTIFICATION, SettingEdit::Reset) => {
                self.set_ready_notification(default_ready_notification())
            }
            (
                PREFIX | NEW_SESSION | TOGGLE_SESSIONS | COMMAND_PALETTE | HELP | QUIT,
                SettingEdit::SetKeybinding(value),
            ) => self.set_keybinding(id, value.parse()?),
            (
                PREFIX | NEW_SESSION | TOGGLE_SESSIONS | COMMAND_PALETTE | HELP | QUIT,
                SettingEdit::Reset,
            ) => self.reset_keybinding(id),
            _ => bail!("unknown Console Settings field '{}'", id.0),
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

fn open(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
    Ok(Box::new(Config::load(store)?))
}

pub(super) fn settings() -> SettingsSection {
    SettingsSection::new(
        SettingsSectionMeta {
            id: TOOL,
            title: "Console",
            description: "Persistent terminal session presentation",
        },
        open,
    )
}

const fn default_sidebar_split_ratio() -> SplitRatio {
    BALANCED_SIDEBAR
}

const fn default_terminal_split_ratio() -> SplitRatio {
    BALANCED_TERMINALS
}

const fn default_ready_notification() -> ReadyNotification {
    ReadyNotification::TerminalBell
}

fn default_prefix() -> KeyChord {
    KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
}

fn default_new_session() -> KeyChord {
    KeyChord::new(KeyCode::Char('n'), KeyModifiers::NONE)
}

fn default_toggle_sessions() -> KeyChord {
    KeyChord::new(KeyCode::Char('s'), KeyModifiers::NONE)
}

fn default_command_palette() -> KeyChord {
    KeyChord::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
}

fn default_help() -> KeyChord {
    KeyChord::new(KeyCode::Char('?'), KeyModifiers::NONE)
}

fn default_quit() -> KeyChord {
    KeyChord::new(KeyCode::Char('q'), KeyModifiers::NONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ConfigStore {
        ConfigStore::rooted(
            std::env::temp_dir().join(format!("kit-console-config-{}", uuid::Uuid::new_v4())),
        )
    }

    fn cleanup(store: &ConfigStore) {
        if let Some(root) = store.path(TOOL).parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn defaults_to_the_current_balanced_sidebar() -> Result<()> {
        let store = store();
        let config = Config::load(store.clone())?;

        assert_eq!(config.sidebar_split_ratio(), SplitRatio::new(260));
        assert_eq!(config.sidebar_width(), SidebarWidth::Balanced);
        assert_eq!(config.ready_notification(), ReadyNotification::TerminalBell);
        assert_eq!(config.keybindings().prefix.to_string(), "Ctrl+B");
        assert_eq!(config.keybindings().new_session.to_string(), "n");
        assert_eq!(config.keybindings().command_palette.to_string(), "Ctrl+P");

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn configured_keybindings_are_parsed_by_the_shared_keyboard_system() -> Result<()> {
        let store = store();
        std::fs::create_dir_all(store.path(TOOL).parent().expect("Console config parent"))?;
        std::fs::write(
            store.path(TOOL),
            "[keybindings]\nprefix = \"Ctrl+A\"\nnew_session = \"c\"\nhelp = \"h\"\n",
        )?;

        let config = Config::load(store.clone())?;
        assert_eq!(config.keybindings().prefix.to_string(), "Ctrl+A");
        assert_eq!(config.keybindings().new_session.to_string(), "c");
        assert_eq!(config.keybindings().toggle_sessions.to_string(), "s");
        assert_eq!(config.keybindings().help.to_string(), "h");

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn settings_edits_persist_validated_keybindings() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;
        config.edit(NEW_SESSION, SettingEdit::SetKeybinding("c".to_owned()))?;

        let reloaded = Config::load(store.clone())?;
        assert_eq!(reloaded.keybindings().new_session.to_string(), "c");
        assert!(reloaded.fields().iter().any(|field| {
            matches!(
                field,
                SettingField::Keybinding {
                    id: NEW_SESSION,
                    value,
                    ..
                } if value == "c"
            )
        }));

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn machine_identity_preferences_persist_without_live_status() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;
        config.set_unix_user("node-mac", "tvx")?;
        config.set_selected_machine("node-mac")?;

        let reloaded = Config::load(store.clone())?;
        assert_eq!(reloaded.unix_user("node-mac"), Some("tvx"));
        assert_eq!(reloaded.selected_machine(), Some("node-mac"));

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn conflicting_keybinding_edit_is_rejected_without_mutating_config() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;
        let original = config.keybindings().new_session;
        let prefix = config.keybindings().prefix.to_string();

        let error = config
            .edit(NEW_SESSION, SettingEdit::SetKeybinding(prefix))
            .expect_err("prefix conflict must fail");
        assert!(error.to_string().contains("conflicts with the prefix"));
        assert_eq!(config.keybindings().new_session, original);

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn exact_dragged_ratio_round_trips_without_quantization() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;
        config.set_sidebar_split_ratio(SplitRatio::new(413))?;

        let reloaded = Config::load(store.clone())?;
        assert_eq!(reloaded.sidebar_split_ratio(), SplitRatio::new(413));
        assert_eq!(reloaded.sidebar_width(), SidebarWidth::Wide);

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn semantic_width_choice_cycles_and_resets() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;

        config.edit(SIDEBAR_WIDTH, SettingEdit::Next)?;
        assert_eq!(config.sidebar_split_ratio(), WIDE_SIDEBAR);
        config.edit(SIDEBAR_WIDTH, SettingEdit::Next)?;
        assert_eq!(config.sidebar_split_ratio(), COMPACT_SIDEBAR);
        config.edit(SIDEBAR_WIDTH, SettingEdit::Previous)?;
        assert_eq!(config.sidebar_split_ratio(), WIDE_SIDEBAR);
        config.edit(SIDEBAR_WIDTH, SettingEdit::Reset)?;
        assert_eq!(config.sidebar_split_ratio(), BALANCED_SIDEBAR);

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn ready_notification_can_be_disabled_and_reset() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;

        config.edit(READY_NOTIFICATION, SettingEdit::Next)?;
        assert_eq!(config.ready_notification(), ReadyNotification::Off);
        assert!(std::fs::read_to_string(store.path(TOOL))?.contains("ready_notification = \"off\""));

        let reloaded = Config::load(store.clone())?;
        assert_eq!(reloaded.ready_notification(), ReadyNotification::Off);

        config.edit(READY_NOTIFICATION, SettingEdit::Reset)?;
        assert_eq!(config.ready_notification(), ReadyNotification::TerminalBell);

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn invalid_persisted_ratio_is_rejected() -> Result<()> {
        let store = store();
        std::fs::create_dir_all(store.path(TOOL).parent().expect("Console config parent"))?;
        std::fs::write(store.path(TOOL), "sidebar_split_ratio = 1000\n")?;

        let error = Config::load(store.clone()).expect_err("invalid ratio must fail closed");
        assert!(format!("{error:#}").contains("split ratio must be between 1 and 999"));

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn unix_users_are_keyed_by_stable_node_id_and_round_trip() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;
        config.set_unix_user("node-123", "tvx")?;

        let reloaded = Config::load(store.clone())?;
        assert_eq!(reloaded.unix_user("node-123"), Some("tvx"));
        assert_eq!(reloaded.unix_user("tvxm"), None);

        cleanup(&store);
        Ok(())
    }

    #[test]
    fn settings_section_exposes_sidebar_and_consumed_keybindings() -> Result<()> {
        let store = store();
        let config = Config::load(store.clone())?;
        let section = settings();

        assert_eq!(section.meta.id, TOOL);
        assert_eq!(section.meta.title, "Console");
        assert_eq!(config.fields().len(), 8);
        assert!(matches!(
            &config.fields()[0],
            SettingField::Choice {
                id: SIDEBAR_WIDTH,
                label: "Sidebar width",
                selected: "Balanced",
                ..
            }
        ));
        assert!(matches!(
            &config.fields()[1],
            SettingField::Choice {
                id: READY_NOTIFICATION,
                label: "Ready notification",
                selected: "Terminal bell",
                ..
            }
        ));

        cleanup(&store);
        Ok(())
    }
}
