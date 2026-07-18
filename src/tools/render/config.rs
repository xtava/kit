use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::framework::{
    ConfigStore, ConfigValue, EditableSettings, SettingEdit, SettingField, SettingId,
    SettingsSection, SettingsSectionMeta,
};

const TOOL: &str = "render";
const SHOW_GIT_IGNORED: SettingId = SettingId("show_git_ignored");
const THEME: SettingId = SettingId("theme");
const TOC_HEADING_DEPTH: SettingId = SettingId("toc_heading_depth");

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum TocHeadingDepth {
    #[default]
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
}

impl TocHeadingDepth {
    const fn level(self) -> u8 {
        match self {
            Self::H1 => 1,
            Self::H2 => 2,
            Self::H3 => 3,
            Self::H4 => 4,
            Self::H5 => 5,
            Self::H6 => 6,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::H1 => "h1",
            Self::H2 => "h2",
            Self::H3 => "h3",
            Self::H4 => "h4",
            Self::H5 => "h5",
            Self::H6 => "h6",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::H1 => "H1 only",
            Self::H2 => "H1-H2",
            Self::H3 => "H1-H3",
            Self::H4 => "H1-H4",
            Self::H5 => "H1-H5",
            Self::H6 => "H1-H6",
        }
    }

    fn from_level(level: u8) -> Result<Self> {
        match level {
            1 => Ok(Self::H1),
            2 => Ok(Self::H2),
            3 => Ok(Self::H3),
            4 => Ok(Self::H4),
            5 => Ok(Self::H5),
            6 => Ok(Self::H6),
            _ => bail!("TOC heading depth must be between 1 and 6, got {level}"),
        }
    }

    fn move_by(self, delta: isize) -> Self {
        match (self.level() as isize - 1 + delta).rem_euclid(6) {
            0 => Self::H1,
            1 => Self::H2,
            2 => Self::H3,
            3 => Self::H4,
            4 => Self::H5,
            _ => Self::H6,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    store: ConfigStore,
    show_git_ignored: bool,
    theme: String,
    toc_heading_depth: TocHeadingDepth,
}

#[derive(Debug, Deserialize, Serialize)]
struct Stored {
    #[serde(default = "default_show_git_ignored")]
    show_git_ignored: bool,
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default)]
    toc_heading_depth: TocHeadingDepth,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            show_git_ignored: default_show_git_ignored(),
            theme: default_theme(),
            toc_heading_depth: TocHeadingDepth::default(),
        }
    }
}

impl Config {
    pub fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        Ok(Self {
            store,
            show_git_ignored: stored.show_git_ignored,
            theme: stored.theme,
            toc_heading_depth: stored.toc_heading_depth,
        })
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
        self.store.set(TOOL, THEME.0, ConfigValue::String(theme.to_owned()))?;
        self.theme = theme.to_owned();
        Ok(())
    }

    pub(crate) const fn toc_heading_depth(&self) -> u8 {
        self.toc_heading_depth.level()
    }

    pub(crate) fn set_toc_heading_depth(&mut self, level: u8) -> Result<()> {
        self.persist_toc_heading_depth(TocHeadingDepth::from_level(level)?)
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

    fn persist_toc_heading_depth(&mut self, depth: TocHeadingDepth) -> Result<()> {
        self.store.set(
            TOOL,
            TOC_HEADING_DEPTH.0,
            ConfigValue::String(depth.as_str().to_owned()),
        )?;
        self.toc_heading_depth = depth;
        Ok(())
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
            SettingField::Choice {
                id: TOC_HEADING_DEPTH,
                label: "Contents heading depth",
                description: "Include headings through this level in the right-side contents.",
                selected: self.toc_heading_depth.label(),
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
            (TOC_HEADING_DEPTH, SettingEdit::Activate | SettingEdit::Next) => {
                self.set_toc_heading_depth(self.toc_heading_depth.move_by(1).level())
            }
            (TOC_HEADING_DEPTH, SettingEdit::Previous) => {
                self.set_toc_heading_depth(self.toc_heading_depth.move_by(-1).level())
            }
            (TOC_HEADING_DEPTH, SettingEdit::Reset) => self.set_toc_heading_depth(1),
            _ => bail!("unknown Render Settings field '{}'", id.0),
        }
    }
}

fn open(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
    Ok(Box::new(Config::load(store)?))
}

pub(super) fn settings() -> SettingsSection {
    SettingsSection::new(
        SettingsSectionMeta { id: TOOL, title: "Render", description: "Markdown viewer" },
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
    fn defaults_render_settings() -> Result<()> {
        let dir = temp_dir();
        let config = Config::load(ConfigStore::rooted(dir.clone()))?;
        let _ = std::fs::remove_dir_all(dir);
        assert!(config.show_git_ignored());
        assert_eq!(config.theme(), "nord");
        assert_eq!(config.toc_heading_depth(), 1);
        Ok(())
    }

    #[test]
    fn persists_render_settings() -> Result<()> {
        let dir = temp_dir();
        let store = ConfigStore::rooted(dir.clone());
        let mut config = Config::load(store.clone())?;
        config.set_show_git_ignored(false)?;
        config.set_theme("terminal")?;
        config.set_toc_heading_depth(3)?;

        let reloaded = Config::load(store)?;
        let _ = std::fs::remove_dir_all(dir);
        assert!(!reloaded.show_git_ignored());
        assert_eq!(reloaded.theme(), "terminal");
        assert_eq!(reloaded.toc_heading_depth(), 3);
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

    #[test]
    fn toc_heading_depth_choice_cycles_and_resets() -> Result<()> {
        let dir = temp_dir();
        let store = ConfigStore::rooted(dir.clone());
        let mut config = Config::load(store.clone())?;

        config.edit(TOC_HEADING_DEPTH, SettingEdit::Previous)?;
        assert_eq!(config.toc_heading_depth(), 6);
        config.edit(TOC_HEADING_DEPTH, SettingEdit::Next)?;
        assert_eq!(config.toc_heading_depth(), 1);
        config.set_toc_heading_depth(4)?;
        config.edit(TOC_HEADING_DEPTH, SettingEdit::Reset)?;

        assert_eq!(Config::load(store)?.toc_heading_depth(), 1);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn toc_heading_depth_rejects_levels_outside_markdown_range() -> Result<()> {
        let dir = temp_dir();
        let mut config = Config::load(ConfigStore::rooted(dir.clone()))?;

        assert!(config.set_toc_heading_depth(0).is_err());
        assert!(config.set_toc_heading_depth(7).is_err());
        assert_eq!(config.toc_heading_depth(), 1);

        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }
}
