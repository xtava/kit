use anyhow::Result;

use super::ConfigStore;

/// Stable identity for one editable field within a Settings section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingId(pub &'static str);

/// Presentation-safe snapshot of one tool-owned Setting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettingField {
    Toggle { id: SettingId, label: &'static str, description: &'static str, value: bool },
    Choice { id: SettingId, label: &'static str, description: &'static str, selected: &'static str },
}

impl SettingField {
    pub const fn id(&self) -> SettingId {
        match self {
            Self::Toggle { id, .. } | Self::Choice { id, .. } => *id,
        }
    }

    pub const fn label(&self) -> &'static str {
        match self {
            Self::Toggle { label, .. } | Self::Choice { label, .. } => label,
        }
    }

    pub const fn description(&self) -> &'static str {
        match self {
            Self::Toggle { description, .. } | Self::Choice { description, .. } => description,
        }
    }
}

/// A semantic edit requested by the shared Settings editor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingEdit {
    Activate,
    Previous,
    Next,
    Reset,
}

/// A tool-owned typed Settings model editable through the shared TUI.
pub trait EditableSettings: Send {
    fn fields(&self) -> Vec<SettingField>;
    fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()>;
}

/// Human-facing identity for one tool-owned Settings section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingsSectionMeta {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

/// Factory contribution that opens one tool-owned Settings model.
#[derive(Clone, Copy)]
pub struct SettingsSection {
    pub meta: SettingsSectionMeta,
    open: fn(ConfigStore) -> Result<Box<dyn EditableSettings>>,
}

impl SettingsSection {
    pub const fn new(
        meta: SettingsSectionMeta,
        open: fn(ConfigStore) -> Result<Box<dyn EditableSettings>>,
    ) -> Self {
        Self { meta, open }
    }

    pub fn open(self, store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
        (self.open)(store)
    }
}
