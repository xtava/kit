use std::path::PathBuf;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::tui::LineEditor;

pub(super) struct CreateRequest {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Copy)]
pub(super) enum CreateField {
    Name,
    Description,
}

impl CreateField {
    pub(super) const ALL: [Self; 2] = [Self::Name, Self::Description];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Description => "Description and when to use it",
        }
    }
}

pub(super) struct CreateSkillForm {
    pub(super) inputs: [LineEditor; 2],
    pub(super) active: usize,
}

pub(super) enum CreateFormOutcome {
    Captured,
    Cancelled,
    Submit(CreateRequest),
}

#[derive(Clone, Debug, Default)]
pub(super) struct CreateSkillLayout {
    pub fields: Vec<Rect>,
    pub submit: Rect,
    pub cancel: Rect,
}

impl CreateSkillForm {
    pub(super) fn new() -> Self {
        Self { inputs: std::array::from_fn(|_| LineEditor::default()), active: 0 }
    }

    pub(super) fn on_event(
        &mut self,
        event: Event,
        layout: &CreateSkillLayout,
    ) -> CreateFormOutcome {
        match event {
            Event::Key(key) if !key.is_press() => CreateFormOutcome::Captured,
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => CreateFormOutcome::Cancelled,
            Event::Key(KeyEvent { code: KeyCode::Tab | KeyCode::Down, .. }) => {
                self.active = (self.active + 1) % self.inputs.len();
                CreateFormOutcome::Captured
            }
            Event::Key(KeyEvent { code: KeyCode::BackTab | KeyCode::Up, .. }) => {
                self.active =
                    (self.active as isize - 1).rem_euclid(self.inputs.len() as isize) as usize;
                CreateFormOutcome::Captured
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, modifiers, .. })
                if self.active + 1 == self.inputs.len()
                    || modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.submit()
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                self.active = (self.active + 1).min(self.inputs.len() - 1);
                CreateFormOutcome::Captured
            }
            Event::Key(key) if key.is_press() => {
                self.inputs[self.active].apply_key(key);
                CreateFormOutcome::Captured
            }
            Event::Paste(text) => {
                for character in text.chars().filter(|character| !character.is_control()) {
                    self.inputs[self.active].insert(character);
                }
                CreateFormOutcome::Captured
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
                    CreateFormOutcome::Captured
                } else if layout.submit.contains(position) {
                    self.submit()
                } else if layout.cancel.contains(position) {
                    CreateFormOutcome::Cancelled
                } else {
                    CreateFormOutcome::Captured
                }
            }
            _ => CreateFormOutcome::Captured,
        }
    }

    fn submit(&self) -> CreateFormOutcome {
        CreateFormOutcome::Submit(CreateRequest {
            name: self.inputs[CreateField::Name as usize].value().trim().to_owned(),
            description: self.inputs[CreateField::Description as usize].value().trim().to_owned(),
        })
    }
}

pub(super) struct LibraryRequest {
    pub path: PathBuf,
    pub create: bool,
    pub required: bool,
}

pub(super) struct LibraryForm {
    pub(super) path: LineEditor,
    pub(super) active: usize,
    pub(super) required: bool,
}

pub(super) enum LibraryFormOutcome {
    Captured,
    Cancelled,
    Submit(LibraryRequest),
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LibraryLayout {
    pub path: Rect,
    pub configure: Rect,
    pub create: Rect,
    pub cancel: Rect,
}

impl LibraryForm {
    pub(super) fn new(path: Option<&std::path::Path>, required: bool) -> Self {
        let mut input = LineEditor::default();
        if let Some(path) = path {
            input.set(path.display().to_string());
        }
        Self { path: input, active: 0, required }
    }

    pub(super) fn on_event(&mut self, event: Event, layout: LibraryLayout) -> LibraryFormOutcome {
        match event {
            Event::Key(key) if !key.is_press() => LibraryFormOutcome::Captured,
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => LibraryFormOutcome::Cancelled,
            Event::Key(KeyEvent { code: KeyCode::Tab | KeyCode::Down, .. }) => {
                self.active = (self.active + 1) % 4;
                LibraryFormOutcome::Captured
            }
            Event::Key(KeyEvent { code: KeyCode::BackTab | KeyCode::Up, .. }) => {
                self.active = (self.active as isize - 1).rem_euclid(4) as usize;
                LibraryFormOutcome::Captured
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => match self.active {
                0 => {
                    self.active = 1;
                    LibraryFormOutcome::Captured
                }
                1 => self.submit(false),
                2 => self.submit(true),
                _ => LibraryFormOutcome::Cancelled,
            },
            Event::Key(key) if key.is_press() && self.active == 0 => {
                self.path.apply_key(key);
                LibraryFormOutcome::Captured
            }
            Event::Paste(text) if self.active == 0 => {
                for character in text.chars().filter(|character| !character.is_control()) {
                    self.path.insert(character);
                }
                LibraryFormOutcome::Captured
            }
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                let position = Position { x: column, y: row };
                if layout.path.contains(position) {
                    self.active = 0;
                    LibraryFormOutcome::Captured
                } else if layout.configure.contains(position) {
                    self.active = 1;
                    self.submit(false)
                } else if layout.create.contains(position) {
                    self.active = 2;
                    self.submit(true)
                } else if layout.cancel.contains(position) {
                    self.active = 3;
                    LibraryFormOutcome::Cancelled
                } else {
                    LibraryFormOutcome::Captured
                }
            }
            _ => LibraryFormOutcome::Captured,
        }
    }

    fn submit(&self, create: bool) -> LibraryFormOutcome {
        LibraryFormOutcome::Submit(LibraryRequest {
            path: PathBuf::from(self.path.value().trim()),
            create,
            required: self.required,
        })
    }
}
