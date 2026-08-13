use serde::{Deserialize, Serialize};

pub(super) const SLOT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct WindowFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl WindowFrame {
    pub(super) fn distance_from(self, other: Self) -> f64 {
        (self.x - other.x).abs()
            + (self.y - other.y).abs()
            + (self.width - other.width).abs()
            + (self.height - other.height).abs()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct WindowSnapshot {
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    pub original_frame: WindowFrame,
    pub streamed_frame: WindowFrame,
}

impl WindowSnapshot {
    pub(super) fn identifies(&self, other: &Self) -> bool {
        if self.pid != other.pid {
            return false;
        }
        match (&self.identifier, &other.identifier) {
            (Some(left), Some(right)) => left == right,
            _ => self.title == other.title,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct StreamResources {
    pub display_id: u32,
    pub display_connected_by_kit: bool,
    pub sunshine_started_by_kit: bool,
    pub previous_output_name: Option<String>,
    pub output_name_changed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SlotPhase {
    Preparing,
    Active,
    Restoring,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct SlotState {
    pub schema_version: u32,
    pub phase: SlotPhase,
    pub window: WindowSnapshot,
    pub resources: StreamResources,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToggleAction {
    Sent,
    Recalled,
    Switched,
    Recovered,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ToggleReport {
    pub schema_version: u32,
    pub action: ToggleAction,
    pub app_name: String,
    pub window_title: String,
    pub display_name: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SlotStatus {
    pub schema_version: u32,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<SlotPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    pub shortcut: &'static str,
}
