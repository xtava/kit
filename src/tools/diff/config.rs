use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::framework::{
    ConfigStore, ConfigValue, EditableSettings, SettingEdit, SettingField, SettingId,
    SettingsSection, SettingsSectionMeta,
};

const TOOL: &str = "diff";
const LINE_NUMBERS: SettingId = SettingId("line_numbers");

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LineNumbers {
    #[default]
    Auto,
    Always,
    Never,
}

impl LineNumbers {
    pub(crate) const fn show(self, split: bool) -> bool {
        match self {
            Self::Auto => split,
            Self::Always => true,
            Self::Never => false,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Auto => 0,
            Self::Always => 1,
            Self::Never => 2,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Always => "Always",
            Self::Never => "Never",
        }
    }

    fn move_by(self, delta: isize) -> Self {
        match (self.index() as isize + delta).rem_euclid(3) {
            0 => Self::Auto,
            1 => Self::Always,
            _ => Self::Never,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    store: ConfigStore,
    line_numbers: LineNumbers,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Stored {
    #[serde(default)]
    line_numbers: LineNumbers,
}

impl Config {
    pub(crate) fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        Ok(Self { store, line_numbers: stored.line_numbers })
    }

    pub(crate) const fn line_numbers(&self) -> LineNumbers {
        self.line_numbers
    }

    fn set_line_numbers(&mut self, line_numbers: LineNumbers) -> Result<()> {
        self.store.set(
            TOOL,
            LINE_NUMBERS.0,
            ConfigValue::String(line_numbers.as_str().to_owned()),
        )?;
        self.line_numbers = line_numbers;
        Ok(())
    }
}

impl EditableSettings for Config {
    fn fields(&self) -> Vec<SettingField> {
        vec![SettingField::Choice {
            id: LINE_NUMBERS,
            label: "Line numbers",
            description: "Auto shows gutters in split mode; Always and Never override every view.",
            selected: self.line_numbers.label(),
        }]
    }

    fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
        match (id, edit) {
            (LINE_NUMBERS, SettingEdit::Activate | SettingEdit::Next) => {
                self.set_line_numbers(self.line_numbers.move_by(1))
            }
            (LINE_NUMBERS, SettingEdit::Previous) => {
                self.set_line_numbers(self.line_numbers.move_by(-1))
            }
            (LINE_NUMBERS, SettingEdit::Reset) => self.set_line_numbers(LineNumbers::default()),
            _ => bail!("unknown Diff Settings field '{}'", id.0),
        }
    }
}

fn open(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
    Ok(Box::new(Config::load(store)?))
}

pub(super) fn settings() -> SettingsSection {
    SettingsSection::new(
        SettingsSectionMeta { id: TOOL, title: "Diff", description: "Git review presentation" },
        open,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_number_setting_round_trips_and_preserves_comments() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("kit-diff-settings-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        std::fs::create_dir_all(&dir)?;
        std::fs::write(store.path(TOOL), "# presentation\nline_numbers = 'auto' # keep me\n")?;

        let mut config = Config::load(store.clone())?;
        config.set_line_numbers(LineNumbers::Never)?;
        let reloaded = Config::load(store.clone())?;
        let raw = std::fs::read_to_string(store.path(TOOL))?;

        assert_eq!(reloaded.line_numbers(), LineNumbers::Never);
        assert!(raw.contains("# presentation"));
        assert!(raw.contains("# keep me"));
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn semantic_choice_edits_keep_line_numbers_typed() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("kit-diff-setting-edits-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let mut config = Config::load(store.clone())?;

        config.edit(LINE_NUMBERS, SettingEdit::Next)?;
        assert_eq!(config.line_numbers(), LineNumbers::Always);
        config.edit(LINE_NUMBERS, SettingEdit::Previous)?;
        assert_eq!(config.line_numbers(), LineNumbers::Auto);
        config.edit(LINE_NUMBERS, SettingEdit::Next)?;
        config.edit(LINE_NUMBERS, SettingEdit::Reset)?;

        assert_eq!(Config::load(store)?.line_numbers(), LineNumbers::Auto);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn unknown_keys_do_not_block_settings_edits() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("kit-diff-extra-config-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        std::fs::create_dir_all(&dir)?;
        std::fs::write(store.path(TOOL), "line_numbers = 'auto'\nfuture_option = true\n")?;

        let mut config = Config::load(store.clone())?;
        config.set_line_numbers(LineNumbers::Always)?;

        let raw = std::fs::read_to_string(store.path(TOOL))?;
        assert!(raw.contains("future_option = true"));
        assert_eq!(Config::load(store)?.line_numbers(), LineNumbers::Always);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }
}
