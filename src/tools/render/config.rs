use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::framework::{
    ConfigStore, ConfigValue, EditableSettings, SettingEdit, SettingField, SettingId,
    SettingsSection, SettingsSectionMeta,
};

const TOOL: &str = "render";
const SHOW_GIT_IGNORED: SettingId = SettingId("show_git_ignored");
const THEME: SettingId = SettingId("theme");

#[derive(Clone, Debug)]
pub struct Config {
    store: ConfigStore,
    show_git_ignored: bool,
    theme: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Stored {
    #[serde(default = "default_show_git_ignored")]
    show_git_ignored: bool,
    #[serde(default = "default_theme")]
    theme: String,
}

impl Default for Stored {
    fn default() -> Self {
        Self { show_git_ignored: default_show_git_ignored(), theme: default_theme() }
    }
}

impl Config {
    pub fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        Ok(Self { store, show_git_ignored: stored.show_git_ignored, theme: stored.theme })
    }

    pub fn show_git_ignored(&self) -> bool {
        self.show_git_ignored
    }

    pub fn set_show_git_ignored(&mut self, show: bool) -> Result<()> {
        self.store.set(TOOL, SHOW_GIT_IGNORED.0, ConfigValue::Bool(show))?;
        self.show_git_ignored = show;
        Ok(())
    }

    pub fn theme(&self) -> &str {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: &str) -> Result<()> {
        self.store.set(TOOL, "theme", ConfigValue::String(theme.to_owned()))?;
        self.theme = theme.to_owned();
        Ok(())
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.store.path(TOOL)
    }

    pub fn theme_dir(&self) -> std::path::PathBuf {
        self.path().with_file_name("themes")
    }

    pub(crate) fn store(&self) -> ConfigStore {
        self.store.clone()
    }

    fn theme_index(&self) -> usize {
        match self.theme.as_str() {
            "nord" => 0,
            "terminal" => 1,
            _ => 2,
        }
    }

    fn move_theme(&mut self, delta: isize) -> Result<()> {
        let theme = match (self.theme_index(), delta.is_negative()) {
            (0, _) => "terminal",
            (1, _) | (2, false) => "nord",
            (2, true) => "terminal",
            _ => "nord",
        };
        self.set_theme(theme)
    }
}

impl EditableSettings for Config {
    fn fields(&self) -> Vec<SettingField> {
        vec![
            SettingField::Toggle {
                id: SHOW_GIT_IGNORED,
                label: "Git-ignored Markdown",
                description: "Include ignored Markdown files in workspace discovery results.",
                value: self.show_git_ignored,
            },
            SettingField::Choice {
                id: THEME,
                label: "Theme",
                description:
                    "Choose a built-in palette; use Render's /theme command for a custom file.",
                selected: match self.theme_index() {
                    0 => "Nord",
                    1 => "Terminal",
                    _ => "Custom",
                },
            },
        ]
    }

    fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
        match (id, edit) {
            (SHOW_GIT_IGNORED, SettingEdit::Activate) => {
                self.set_show_git_ignored(!self.show_git_ignored)
            }
            (SHOW_GIT_IGNORED, SettingEdit::Reset) => {
                self.set_show_git_ignored(default_show_git_ignored())
            }
            (SHOW_GIT_IGNORED, _) => {
                bail!("show_git_ignored supports only activate or reset edits")
            }
            (THEME, SettingEdit::Activate | SettingEdit::Next) => self.move_theme(1),
            (THEME, SettingEdit::Previous) => self.move_theme(-1),
            (THEME, SettingEdit::Reset) => self.set_theme(&default_theme()),
            _ => bail!("unknown Render Settings field '{}'", id.0),
        }
    }
}

fn open(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
    Ok(Box::new(Config::load(store)?))
}

pub(super) fn settings() -> SettingsSection {
    SettingsSection::new(
        SettingsSectionMeta { id: TOOL, title: "Render", description: "Markdown discovery" },
        open,
    )
}

fn default_show_git_ignored() -> bool {
    true
}

fn default_theme() -> String {
    "nord".to_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("kit-render-config-test-{}-{nonce}-{sequence}", std::process::id()))
    }

    #[test]
    fn defaults_to_showing_ignored_markdown() -> Result<()> {
        let dir = temp_dir();
        let config = Config::load(ConfigStore::rooted(dir.clone()))?;
        let _ = std::fs::remove_dir_all(dir);
        assert!(config.show_git_ignored());
        assert_eq!(config.theme(), "nord");
        Ok(())
    }

    #[test]
    fn persists_ignored_visibility() -> Result<()> {
        let dir = temp_dir();
        let store = ConfigStore::rooted(dir.clone());
        let mut config = Config::load(store.clone())?;
        config.set_show_git_ignored(false)?;
        config.set_theme("terminal")?;

        let reloaded = Config::load(store)?;
        let _ = std::fs::remove_dir_all(dir);
        assert!(!reloaded.show_git_ignored());
        assert_eq!(reloaded.theme(), "terminal");
        Ok(())
    }

    #[test]
    fn theme_choice_leaves_custom_values_through_a_valid_builtin() -> Result<()> {
        let dir = temp_dir();
        let store = ConfigStore::rooted(dir.clone());
        let mut config = Config::load(store)?;

        config.set_theme("custom-theme.toml")?;
        config.edit(THEME, SettingEdit::Previous)?;
        assert_eq!(config.theme(), "terminal");
        config.set_theme("custom-theme.toml")?;
        config.edit(THEME, SettingEdit::Next)?;
        assert_eq!(config.theme(), "nord");

        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }
}
