use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::{
    framework::{
        ConfigStore, ConfigValue, EditableSettings, SettingEdit, SettingField, SettingId,
        SettingsSection, SettingsSectionMeta,
    },
    tui::SplitRatio,
};

const TOOL: &str = "console";
const SIDEBAR_WIDTH: SettingId = SettingId("sidebar_width");
const SIDEBAR_SPLIT_RATIO: &str = "sidebar_split_ratio";

const COMPACT_SIDEBAR: SplitRatio = SplitRatio::new(200);
const BALANCED_SIDEBAR: SplitRatio = SplitRatio::new(260);
const WIDE_SIDEBAR: SplitRatio = SplitRatio::new(360);

#[derive(Clone, Debug)]
pub(super) struct Config {
    store: ConfigStore,
    sidebar_split_ratio: SplitRatio,
    users: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Stored {
    #[serde(default = "default_sidebar_split_ratio")]
    sidebar_split_ratio: SplitRatio,
    #[serde(default)]
    users: BTreeMap<String, String>,
}

impl Default for Stored {
    fn default() -> Self {
        Self { sidebar_split_ratio: default_sidebar_split_ratio(), users: BTreeMap::new() }
    }
}

impl Config {
    pub(super) fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        Ok(Self { store, sidebar_split_ratio: stored.sidebar_split_ratio, users: stored.users })
    }

    pub(super) const fn sidebar_split_ratio(&self) -> SplitRatio {
        self.sidebar_split_ratio
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

    pub(super) fn unix_user(&self, stable_node_id: &str) -> Option<&str> {
        self.users.get(stable_node_id).map(String::as_str)
    }

    pub(super) fn set_unix_user(&mut self, stable_node_id: &str, user: &str) -> Result<()> {
        self.store.set_table_string(TOOL, "users", stable_node_id, user)?;
        self.users.insert(stable_node_id.to_owned(), user.to_owned());
        Ok(())
    }

    fn sidebar_width(&self) -> SidebarWidth {
        SidebarWidth::from_ratio(self.sidebar_split_ratio())
    }

    fn set_sidebar_width(&mut self, width: SidebarWidth) -> Result<()> {
        self.set_sidebar_split_ratio(width.ratio())
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
        vec![SettingField::Choice {
            id: SIDEBAR_WIDTH,
            label: "Sidebar width",
            description:
                "Choose the initial sidebar width; divider dragging saves the exact ratio.",
            selected: self.sidebar_width().label(),
        }]
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
            _ => bail!("unknown Console Settings field '{}'", id.0),
        }
    }
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
    fn settings_section_exposes_only_the_consumed_sidebar_preference() -> Result<()> {
        let store = store();
        let config = Config::load(store.clone())?;
        let section = settings();

        assert_eq!(section.meta.id, TOOL);
        assert_eq!(section.meta.title, "Console");
        assert_eq!(config.fields().len(), 1);
        assert!(matches!(
            &config.fields()[0],
            SettingField::Choice {
                id: SIDEBAR_WIDTH,
                label: "Sidebar width",
                selected: "Balanced",
                ..
            }
        ));

        cleanup(&store);
        Ok(())
    }
}
