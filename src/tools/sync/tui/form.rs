use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::tui::LineEditor;

use super::AddRequest;

#[derive(Clone, Copy)]
pub(super) enum AddField {
    Name,
    Machine,
    RemoteRoot,
    User,
    LocalRoot,
    Excludes,
    Includes,
}

impl AddField {
    pub(super) const ALL: [Self; 7] = [
        Self::Name,
        Self::Machine,
        Self::RemoteRoot,
        Self::User,
        Self::LocalRoot,
        Self::Excludes,
        Self::Includes,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Machine => "Machine",
            Self::RemoteRoot => "Remote root",
            Self::User => "Remote user",
            Self::LocalRoot => "Local root",
            Self::Excludes => "Extra excludes",
            Self::Includes => "Extra includes",
        }
    }
}

pub(super) struct AddProjectForm {
    pub(super) inputs: [LineEditor; 7],
    pub(super) active: usize,
}

pub(super) enum AddFormOutcome {
    Captured,
    Cancelled,
    Submit(AddRequest),
}

#[derive(Clone, Debug, Default)]
pub(super) struct AddProjectLayout {
    pub(super) fields: Vec<Rect>,
    pub(super) submit: Rect,
    pub(super) cancel: Rect,
}

impl AddProjectForm {
    pub(super) fn new() -> Result<Self> {
        let mut inputs: [LineEditor; 7] = std::array::from_fn(|_| LineEditor::default());
        inputs[AddField::User as usize].set(std::env::var("USER").unwrap_or_default());
        inputs[AddField::LocalRoot as usize].set(std::env::current_dir()?.display().to_string());
        Ok(Self { inputs, active: 0 })
    }

    pub(super) fn on_event(&mut self, event: Event, layout: &AddProjectLayout) -> AddFormOutcome {
        match event {
            Event::Key(key) if !key.is_press() => AddFormOutcome::Captured,
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => AddFormOutcome::Cancelled,
            Event::Key(KeyEvent { code: KeyCode::Tab | KeyCode::Down, .. }) => {
                self.active = (self.active + 1) % self.inputs.len();
                AddFormOutcome::Captured
            }
            Event::Key(KeyEvent { code: KeyCode::BackTab | KeyCode::Up, .. }) => {
                self.active =
                    (self.active as isize - 1).rem_euclid(self.inputs.len() as isize) as usize;
                AddFormOutcome::Captured
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, modifiers, .. })
                if self.active + 1 == self.inputs.len()
                    || modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.submit()
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                self.active = (self.active + 1).min(self.inputs.len() - 1);
                AddFormOutcome::Captured
            }
            Event::Key(key) if key.is_press() => {
                self.inputs[self.active].apply_key(key);
                AddFormOutcome::Captured
            }
            Event::Paste(text) => {
                for character in text.chars().filter(|character| !character.is_control()) {
                    self.inputs[self.active].insert(character);
                }
                AddFormOutcome::Captured
            }
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                let position = Position { x: column, y: row };
                if let Some(index) = layout.fields.iter().position(|area| area.contains(position)) {
                    self.active = index;
                    AddFormOutcome::Captured
                } else if layout.submit.contains(position) {
                    self.submit()
                } else if layout.cancel.contains(position) {
                    AddFormOutcome::Cancelled
                } else {
                    AddFormOutcome::Captured
                }
            }
            _ => AddFormOutcome::Captured,
        }
    }

    fn submit(&self) -> AddFormOutcome {
        let value = |field: AddField| self.inputs[field as usize].value().trim();
        let patterns = |field| {
            value(field)
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect()
        };
        AddFormOutcome::Submit(AddRequest {
            name: value(AddField::Name).to_owned(),
            machine: value(AddField::Machine).to_owned(),
            remote_root: PathBuf::from(value(AddField::RemoteRoot)),
            user: value(AddField::User).to_owned(),
            local_root: (!value(AddField::LocalRoot).is_empty())
                .then(|| PathBuf::from(value(AddField::LocalRoot))),
            excludes: patterns(AddField::Excludes),
            includes: patterns(AddField::Includes),
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ConfirmationLayout {
    pub(super) confirm: Rect,
    pub(super) cancel: Rect,
}
