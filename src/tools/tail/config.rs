use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::{
    framework::{
        ConfigStore, ConfigValue, EditableSettings, SettingEdit, SettingField, SettingId,
        SettingsSection, SettingsSectionMeta,
    },
    tui::SplitRatio,
};

const TOOL: &str = "tail";
const AUTO_RECEIVE: SettingId = SettingId("auto_receive");
const MOUSE: SettingId = SettingId("mouse");
const SPLIT_RATIO: &str = "split_ratio";
const DEFAULT_SPLIT_RATIO: SplitRatio = SplitRatio::new(440);

#[derive(Clone, Debug)]
pub(super) struct Config {
    store: ConfigStore,
    auto_receive: bool,
    mouse: bool,
    split_ratio: SplitRatio,
}

#[derive(Debug, Deserialize, Serialize)]
struct Stored {
    #[serde(default = "enabled")]
    auto_receive: bool,
    #[serde(default = "enabled")]
    mouse: bool,
    #[serde(default = "default_split_ratio")]
    split_ratio: SplitRatio,
}

impl Default for Stored {
    fn default() -> Self {
        Self { auto_receive: true, mouse: true, split_ratio: DEFAULT_SPLIT_RATIO }
    }
}

impl Config {
    pub(super) fn load(store: ConfigStore) -> Result<Self> {
        let stored: Stored = store.load(TOOL)?;
        Ok(Self {
            store,
            auto_receive: stored.auto_receive,
            mouse: stored.mouse,
            split_ratio: stored.split_ratio,
        })
    }

    pub(super) const fn auto_receive(&self) -> bool {
        self.auto_receive
    }

    pub(super) const fn mouse(&self) -> bool {
        self.mouse
    }

    pub(super) const fn split_ratio(&self) -> SplitRatio {
        self.split_ratio
    }

    pub(super) fn set_split_ratio(&mut self, ratio: SplitRatio) -> Result<()> {
        self.store.set(TOOL, SPLIT_RATIO, ConfigValue::Integer(i64::from(ratio.value())))?;
        self.split_ratio = ratio;
        Ok(())
    }

    fn set_auto_receive(&mut self, enabled: bool) -> Result<()> {
        self.store.set(TOOL, AUTO_RECEIVE.0, ConfigValue::Bool(enabled))?;
        self.auto_receive = enabled;
        Ok(())
    }

    fn set_mouse(&mut self, enabled: bool) -> Result<()> {
        self.store.set(TOOL, MOUSE.0, ConfigValue::Bool(enabled))?;
        self.mouse = enabled;
        Ok(())
    }
}

impl EditableSettings for Config {
    fn fields(&self) -> Vec<SettingField> {
        vec![
            SettingField::Toggle {
                id: AUTO_RECEIVE,
                label: "Automatic receiving",
                description: "Watch the Taildrop inbox whenever the Tail TUI is open.",
                value: self.auto_receive,
            },
            SettingField::Toggle {
                id: MOUSE,
                label: "Mouse interaction",
                description: "Enable clicking, scrolling, context menus, and panel resizing.",
                value: self.mouse,
            },
        ]
    }

    fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
        match (id, edit) {
            (AUTO_RECEIVE, SettingEdit::Activate) => self.set_auto_receive(!self.auto_receive),
            (AUTO_RECEIVE, SettingEdit::Reset) => self.set_auto_receive(true),
            (MOUSE, SettingEdit::Activate) => self.set_mouse(!self.mouse),
            (MOUSE, SettingEdit::Reset) => self.set_mouse(true),
            (AUTO_RECEIVE | MOUSE, _) => bail!("toggle settings support activate or reset"),
            _ => bail!("unknown Tail Settings field '{}'", id.0),
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
            title: "Tail",
            description: "Tailscale sharing and receiving",
        },
        open,
    )
}

const fn enabled() -> bool {
    true
}

const fn default_split_ratio() -> SplitRatio {
    DEFAULT_SPLIT_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ConfigStore {
        ConfigStore::rooted(
            std::env::temp_dir().join(format!("kit-tail-config-{}", uuid::Uuid::new_v4())),
        )
    }

    #[test]
    fn defaults_enable_receiving_and_mouse() -> Result<()> {
        let store = store();
        let config = Config::load(store.clone())?;
        assert!(config.auto_receive());
        assert!(config.mouse());
        assert_eq!(config.split_ratio(), SplitRatio::new(440));
        let _ = std::fs::remove_dir_all(store.path(TOOL).parent().unwrap());
        Ok(())
    }

    #[test]
    fn split_ratio_round_trips_through_shared_config_store() -> Result<()> {
        let store = store();
        let mut config = Config::load(store.clone())?;
        config.set_split_ratio(SplitRatio::new(615))?;
        let reloaded = Config::load(store.clone())?;
        assert_eq!(reloaded.split_ratio(), SplitRatio::new(615));
        let _ = std::fs::remove_dir_all(store.path(TOOL).parent().unwrap());
        Ok(())
    }
}
