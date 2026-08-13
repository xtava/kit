use std::path::PathBuf;

use anyhow::Context;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction as LayoutDirection, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};

use super::{
    fit_terminal_text, theme::TuiTheme, CellAlignment, CellOverflow, Direction, KeyChord,
    NavigationMap, NavigationRegion,
};
use crate::framework::{ConfigStore, EditableSettings, SettingEdit, SettingField, SettingsSection};

const MIN_WIDTH: u16 = 58;
const MIN_HEIGHT: u16 = 12;

/// Result of routing one key through an embedded Settings editor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsFlow {
    Continue,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRegion {
    Sections,
    Fields,
}

#[derive(Default)]
struct Regions {
    sections: Rect,
    fields: Rect,
    section_rows: Vec<(Rect, usize)>,
    field_rows: Vec<(Rect, usize)>,
}

enum SectionState {
    Ready(Box<dyn EditableSettings>),
    Invalid(String),
}

struct OpenSection {
    contribution: SettingsSection,
    path: PathBuf,
    state: SectionState,
}

enum Notice {
    None,
    Saved,
    Error(String),
}

/// Stateful Settings component shared by the standalone editor and tool-local surfaces.
pub struct SettingsEditor {
    sections: Vec<OpenSection>,
    selected_section: usize,
    selected_field: usize,
    section_scroll: usize,
    field_scroll: usize,
    active_region: ActiveRegion,
    capturing: Option<crate::framework::SettingId>,
    notice: Notice,
    theme: TuiTheme,
    regions: Regions,
}

impl SettingsEditor {
    pub fn open(store: ConfigStore, sections: Vec<SettingsSection>, theme: TuiTheme) -> Self {
        let sections = sections
            .into_iter()
            .map(|contribution| {
                let path = store.path(contribution.meta.id);
                let state = match contribution.open(store.clone()) {
                    Ok(model) => SectionState::Ready(model),
                    Err(error) => SectionState::Invalid(format!("{error:#}")),
                };
                OpenSection { contribution, path, state }
            })
            .collect();
        Self {
            sections,
            selected_section: 0,
            selected_field: 0,
            section_scroll: 0,
            field_scroll: 0,
            active_region: ActiveRegion::Sections,
            capturing: None,
            notice: Notice::None,
            theme,
            regions: Regions::default(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SettingsFlow {
        if let Some(id) = self.capturing {
            if key.code == KeyCode::Esc {
                self.capturing = None;
                return SettingsFlow::Continue;
            }
            if let Some(chord) = KeyChord::from_event(key) {
                self.capturing = None;
                self.apply_edit(id, SettingEdit::SetKeybinding(chord.to_string()));
            }
            return SettingsFlow::Continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return SettingsFlow::Exit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return SettingsFlow::Exit,
            KeyCode::Tab => self.move_tab(false),
            KeyCode::BackTab => self.move_tab(true),
            KeyCode::Left => self.move_region(Direction::Left),
            KeyCode::Right => self.move_region(Direction::Right),
            KeyCode::Up | KeyCode::Char('k') => self.select_relative(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select_relative(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.apply(SettingEdit::Activate),
            KeyCode::Char(']') => self.apply(SettingEdit::Next),
            KeyCode::Char('[') => self.apply(SettingEdit::Previous),
            KeyCode::Char('r') => self.apply(SettingEdit::Reset),
            _ => {}
        }
        SettingsFlow::Continue
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> SettingsFlow {
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, index)) =
                    self.regions.section_rows.iter().find(|(area, _)| area.contains(position))
                {
                    self.active_region = ActiveRegion::Sections;
                    self.selected_section = *index;
                    self.selected_field = 0;
                    self.field_scroll = 0;
                    self.notice = Notice::None;
                } else if let Some((_, index)) =
                    self.regions.field_rows.iter().find(|(area, _)| area.contains(position))
                {
                    self.active_region = ActiveRegion::Fields;
                    self.selected_field = *index;
                    self.apply(SettingEdit::Activate);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.regions.fields.contains(position) {
                    self.active_region = ActiveRegion::Fields;
                } else if self.regions.sections.contains(position) {
                    self.active_region = ActiveRegion::Sections;
                }
                self.select_relative(-1);
            }
            MouseEventKind::ScrollDown => {
                if self.regions.fields.contains(position) {
                    self.active_region = ActiveRegion::Fields;
                } else if self.regions.sections.contains(position) {
                    self.active_region = ActiveRegion::Sections;
                }
                self.select_relative(1);
            }
            _ => {}
        }
        SettingsFlow::Continue
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.regions.section_rows.clear();
        self.regions.field_rows.clear();
        let compact = area.width < MIN_WIDTH || area.height < MIN_HEIGHT;
        let header_height = u16::from(area.height >= 3);
        let footer_height = if area.height >= 5 { 2 } else { u16::from(area.height >= 2) };
        let rows = Layout::vertical([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(area);
        if compact {
            self.render_compact(frame, rows[1]);
            if header_height > 0 {
                render_header(frame, rows[0], self);
            }
            if footer_height > 0 {
                render_footer(frame, rows[2], self);
            }
            return;
        }
        let columns = Layout::default()
            .direction(LayoutDirection::Horizontal)
            .constraints([Constraint::Length(25), Constraint::Min(32)])
            .split(rows[1]);
        self.regions.sections = columns[0];
        self.regions.fields = columns[1];
        render_header(frame, rows[0], self);
        render_sections(frame, columns[0], self);
        render_fields(frame, columns[1], self);
        render_footer(frame, rows[2], self);
    }

    fn render_compact(&mut self, frame: &mut Frame<'_>, area: Rect) {
        match self.active_region {
            ActiveRegion::Sections => {
                self.regions.sections = area;
                self.regions.fields = Rect::default();
                render_sections(frame, area, self);
            }
            ActiveRegion::Fields => {
                self.regions.sections = Rect::default();
                self.regions.fields = area;
                render_fields(frame, area, self);
            }
        }
    }

    fn select_relative(&mut self, delta: isize) {
        match self.active_region {
            ActiveRegion::Sections if !self.sections.is_empty() => {
                self.selected_section =
                    self.selected_section.saturating_add_signed(delta).min(self.sections.len() - 1);
                self.selected_field = 0;
                self.field_scroll = 0;
                self.notice = Notice::None;
            }
            ActiveRegion::Fields => {
                let count = self.fields().len();
                if count > 0 {
                    self.selected_field =
                        self.selected_field.saturating_add_signed(delta).min(count - 1);
                }
            }
            ActiveRegion::Sections => {}
        }
    }

    fn apply(&mut self, edit: SettingEdit) {
        if self.active_region == ActiveRegion::Sections {
            self.active_region = ActiveRegion::Fields;
            return;
        }
        let fields = self.fields();
        let Some(field) = fields.get(self.selected_field) else {
            return;
        };
        let id = field.id();
        if matches!(field, SettingField::Keybinding { .. }) && matches!(edit, SettingEdit::Activate)
        {
            self.capturing = Some(id);
            self.notice = Notice::None;
            return;
        }
        let edit = if matches!(field, SettingField::Toggle { .. })
            && matches!(edit, SettingEdit::Previous | SettingEdit::Next)
        {
            SettingEdit::Activate
        } else {
            edit
        };
        self.apply_edit(id, edit);
    }

    fn apply_edit(&mut self, id: crate::framework::SettingId, edit: SettingEdit) {
        let result = self
            .current_model_mut()
            .context("selected Settings section could not be loaded")
            .and_then(|model| model.edit(id, edit));
        self.notice = match result {
            Ok(()) => Notice::Saved,
            Err(error) => Notice::Error(format!("Could not save Settings: {error:#}")),
        };
    }

    fn fields(&self) -> Vec<SettingField> {
        match self.sections.get(self.selected_section).map(|section| &section.state) {
            Some(SectionState::Ready(model)) => model.fields(),
            Some(SectionState::Invalid(_)) | None => Vec::new(),
        }
    }

    fn current_model_mut(&mut self) -> Option<&mut (dyn EditableSettings + '_)> {
        match &mut self.sections.get_mut(self.selected_section)?.state {
            SectionState::Ready(model) => Some(model.as_mut()),
            SectionState::Invalid(_) => None,
        }
    }

    fn navigation(&self) -> NavigationMap<ActiveRegion> {
        NavigationMap::new([
            NavigationRegion::new(ActiveRegion::Sections, self.regions.sections),
            NavigationRegion::new(ActiveRegion::Fields, self.regions.fields),
        ])
    }

    fn move_region(&mut self, direction: Direction) {
        if let Some(next) = self.navigation().neighbor(self.active_region, direction) {
            self.active_region = next;
        } else if matches!(direction, Direction::Left | Direction::Right)
            && (self.regions.sections == Rect::default() || self.regions.fields == Rect::default())
        {
            self.toggle_region();
        }
    }

    fn move_tab(&mut self, reverse: bool) {
        let navigation = self.navigation();
        let next = if reverse {
            navigation.previous(self.active_region)
        } else {
            navigation.next(self.active_region)
        };
        match next {
            Some(next) if next != self.active_region => self.active_region = next,
            _ if self.regions.sections == Rect::default()
                || self.regions.fields == Rect::default() =>
            {
                self.toggle_region();
            }
            _ => {}
        }
    }

    fn toggle_region(&mut self) {
        self.active_region = match self.active_region {
            ActiveRegion::Sections => ActiveRegion::Fields,
            ActiveRegion::Fields => ActiveRegion::Sections,
        };
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &SettingsEditor) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "settings",
                Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  /  operator preferences", Style::default().fg(app.theme.text_muted)),
        ])),
        area,
    );
}

fn render_sections(frame: &mut Frame<'_>, area: Rect, app: &mut SettingsEditor) {
    let block = panel(" sections ", app.active_region == ActiveRegion::Sections, app.theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = inner.height as usize;
    if app.selected_section < app.section_scroll {
        app.section_scroll = app.selected_section;
    } else if app.selected_section >= app.section_scroll.saturating_add(height.max(1)) {
        app.section_scroll = app.selected_section.saturating_sub(height.saturating_sub(1));
    }
    let lines = app
        .sections
        .iter()
        .enumerate()
        .skip(app.section_scroll)
        .take(height)
        .map(|(index, section)| {
            let selected = index == app.selected_section;
            Line::from(vec![
                Span::styled(
                    if selected { "▌ " } else { "  " },
                    Style::default().fg(app.theme.accent),
                ),
                Span::styled(
                    section.contribution.meta.title,
                    Style::default()
                        .fg(if selected { app.theme.text_strong } else { app.theme.text })
                        .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
                ),
            ])
        })
        .collect::<Vec<_>>();
    app.regions.section_rows = (app.section_scroll..app.section_scroll + lines.len())
        .enumerate()
        .map(|(row, index)| {
            (Rect::new(inner.x, inner.y + u16::try_from(row).unwrap_or(0), inner.width, 1), index)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_fields(frame: &mut Frame<'_>, area: Rect, app: &mut SettingsEditor) {
    let title = app
        .sections
        .get(app.selected_section)
        .map_or("Settings", |section| section.contribution.meta.description);
    let title = format!(" {title} ");
    let block = panel(&title, app.active_region == ActiveRegion::Fields, app.theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(section) = app.sections.get(app.selected_section) else {
        frame.render_widget(Paragraph::new("No Settings sections registered."), inner);
        return;
    };
    if let SectionState::Invalid(error) = &section.state {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "Could not load this Settings section.",
                    Style::default().fg(app.theme.danger).add_modifier(Modifier::BOLD),
                ),
                Line::from(""),
                Line::styled(error.clone(), Style::default().fg(app.theme.text)),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    let fields = app.fields();
    keep_field_visible(app, &fields, inner);
    let mut lines = Vec::new();
    let mut row = 0_u16;
    for (index, field) in fields.iter().enumerate().skip(app.field_scroll) {
        let height = u16::try_from(field_height(field, inner.width)).unwrap_or(u16::MAX);
        if row >= inner.height {
            break;
        }
        app.regions.field_rows.push((
            Rect::new(inner.x, inner.y + row, inner.width, height.min(inner.height - row)),
            index,
        ));
        let selected = index == app.selected_field;
        lines.push(Line::from(vec![
            Span::styled(if selected { "▌ " } else { "  " }, Style::default().fg(app.theme.accent)),
            Span::styled(
                fit_terminal_text(field.label(), 25, CellAlignment::Left, CellOverflow::Clip),
                Style::default()
                    .fg(if selected { app.theme.text_strong } else { app.theme.text })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            ),
            Span::styled(display_value(field), Style::default().fg(app.theme.accent_alt)),
        ]));
        lines.push(Line::styled(
            format!("    {}", field.description()),
            Style::default().fg(app.theme.text_muted),
        ));
        lines.push(Line::from(""));
        row = row.saturating_add(height);
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn keep_field_visible(app: &mut SettingsEditor, fields: &[SettingField], area: Rect) {
    if fields.is_empty() {
        app.field_scroll = 0;
        return;
    }
    app.selected_field = app.selected_field.min(fields.len() - 1);
    app.field_scroll = app.field_scroll.min(app.selected_field);
    while app.field_scroll < app.selected_field {
        let used = fields[app.field_scroll..=app.selected_field]
            .iter()
            .map(|field| field_height(field, area.width))
            .sum::<usize>();
        if used <= area.height as usize {
            break;
        }
        app.field_scroll += 1;
    }
}

fn field_height(field: &SettingField, width: u16) -> usize {
    Paragraph::new(vec![
        Line::from(format!("  {:<25}{}", field.label(), display_value(field))),
        Line::from(format!("    {}", field.description())),
        Line::from(""),
    ])
    .wrap(Wrap { trim: false })
    .line_count(width.max(1))
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &SettingsEditor) {
    let (status, color) = if app.capturing.is_some() {
        ("Press the new keybinding · Esc cancel".to_owned(), app.theme.accent)
    } else {
        match &app.notice {
            Notice::None => (
                "↑/↓ select · ←/→ region · Enter/Space change · r reset · q quit".to_owned(),
                app.theme.text_muted,
            ),
            Notice::Saved => ("Saved".to_owned(), app.theme.text_muted),
            Notice::Error(error) => (error.clone(), app.theme.danger),
        }
    };
    let path = app
        .sections
        .get(app.selected_section)
        .map(|section| section.path.display().to_string())
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(status, Style::default().fg(color)),
            Line::from(vec![
                Span::styled("file  ", Style::default().fg(app.theme.text_muted)),
                Span::styled(path, Style::default().fg(app.theme.text)),
            ]),
        ]),
        area,
    );
}

fn display_value(field: &SettingField) -> String {
    match field {
        SettingField::Toggle { value: true, .. } => "[●] On".to_owned(),
        SettingField::Toggle { value: false, .. } => "[○] Off".to_owned(),
        SettingField::Choice { selected, .. } => format!("‹ {selected} ›"),
        SettingField::Keybinding { value, .. } => format!("‹ {value} ›"),
    }
}

fn panel(title: &str, active: bool, theme: TuiTheme) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if active { theme.accent } else { theme.border }))
        .title(title)
        .title_style(Style::default().fg(theme.text_strong).add_modifier(Modifier::BOLD))
        .padding(Padding::horizontal(1))
}

#[cfg(test)]
mod tests {
    use anyhow::{bail, Result};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use serde::Deserialize;

    use super::*;
    use crate::framework::{ConfigValue, SettingId, SettingsSectionMeta};
    use crate::tui::theme::NORD;

    const ENABLED: SettingId = SettingId("enabled");
    const SHORTCUT: SettingId = SettingId("shortcut");

    struct TestSettings {
        store: ConfigStore,
        enabled: bool,
        field_count: usize,
    }

    #[derive(Default, Deserialize)]
    struct Stored {
        #[serde(default)]
        enabled: bool,
    }

    #[derive(Default, Deserialize)]
    struct ShortcutStored {
        shortcut: String,
    }

    impl EditableSettings for TestSettings {
        fn fields(&self) -> Vec<SettingField> {
            (0..self.field_count)
                .map(|_| SettingField::Toggle {
                    id: ENABLED,
                    label: "Enabled",
                    description: "Exercise the shared toggle control.",
                    value: self.enabled,
                })
                .collect()
        }

        fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
            match (id, edit) {
                (ENABLED, SettingEdit::Activate) => {
                    self.enabled = !self.enabled;
                    self.store.set("test", ENABLED.0, ConfigValue::Bool(self.enabled))
                }
                (ENABLED, SettingEdit::Reset) => {
                    self.enabled = false;
                    self.store.set("test", ENABLED.0, ConfigValue::Bool(false))
                }
                (ENABLED, _) => bail!("enabled supports only activate or reset edits"),
                _ => bail!("invalid test Setting"),
            }
        }
    }

    fn open_test(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
        let stored: Stored = store.load("test")?;
        Ok(Box::new(TestSettings { store, enabled: stored.enabled, field_count: 1 }))
    }

    fn open_many(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
        Ok(Box::new(TestSettings { store, enabled: false, field_count: 12 }))
    }

    struct KeybindingSettings {
        store: ConfigStore,
        shortcut: String,
    }

    impl EditableSettings for KeybindingSettings {
        fn fields(&self) -> Vec<SettingField> {
            vec![SettingField::Keybinding {
                id: SHORTCUT,
                label: "Shortcut",
                description: "Exercise shared keybinding capture.",
                value: self.shortcut.clone(),
            }]
        }

        fn edit(&mut self, id: SettingId, edit: SettingEdit) -> Result<()> {
            match (id, edit) {
                (SHORTCUT, SettingEdit::SetKeybinding(value)) => {
                    self.store.set("keys", SHORTCUT.0, ConfigValue::String(value.clone()))?;
                    self.shortcut = value;
                    Ok(())
                }
                _ => bail!("invalid keybinding edit"),
            }
        }
    }

    fn open_keybinding(store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
        Ok(Box::new(KeybindingSettings { store, shortcut: "Ctrl+B".to_owned() }))
    }

    fn open_invalid(_store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
        bail!("broken test Settings")
    }

    fn open_failing(_store: ConfigStore) -> Result<Box<dyn EditableSettings>> {
        struct FailingSettings;

        impl EditableSettings for FailingSettings {
            fn fields(&self) -> Vec<SettingField> {
                vec![SettingField::Toggle {
                    id: ENABLED,
                    label: "Enabled",
                    description: "Exercise a failed Settings write.",
                    value: false,
                }]
            }

            fn edit(&mut self, _id: SettingId, _edit: SettingEdit) -> Result<()> {
                bail!("disk unavailable")
            }
        }

        Ok(Box::new(FailingSettings))
    }

    fn section(open: fn(ConfigStore) -> Result<Box<dyn EditableSettings>>) -> SettingsSection {
        SettingsSection::new(
            SettingsSectionMeta { id: "test", title: "Test", description: "Test preferences" },
            open,
        )
    }

    #[test]
    fn keyboard_navigation_updates_the_typed_file() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("kit-settings-tui-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let mut app = SettingsEditor::open(store.clone(), vec![section(open_test)], NORD);
        let mut terminal = Terminal::new(TestBackend::new(90, 20))?;
        terminal.draw(|frame| app.render(frame, frame.area()))?;

        assert_eq!(app.active_region, ActiveRegion::Sections);
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let stored: Stored = store.load("test")?;
        assert!(stored.enabled);
        assert!(matches!(app.notice, Notice::Saved));
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let stored: Stored = store.load("test")?;
        assert!(!stored.enabled);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn mouse_uses_rendered_field_geometry_and_updates_the_typed_file() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "kit-settings-mouse-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let store = ConfigStore::rooted(dir.clone());
        let mut app = SettingsEditor::open(store.clone(), vec![section(open_test)], NORD);
        let mut terminal = Terminal::new(TestBackend::new(90, 20))?;
        terminal.draw(|frame| app.render(frame, frame.area()))?;
        let field = app.regions.field_rows[0].0;

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: field.x,
            row: field.y,
            modifiers: KeyModifiers::NONE,
        });

        let stored: Stored = store.load("test")?;
        assert!(stored.enabled);
        assert_eq!(app.active_region, ActiveRegion::Fields);
        assert!(matches!(app.notice, Notice::Saved));
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn keybinding_fields_capture_the_next_chord() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("kit-settings-keybinding-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let mut app = SettingsEditor::open(store.clone(), vec![section(open_keybinding)], NORD);
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.capturing, Some(SHORTCUT));
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));

        let stored: ShortcutStored = store.load("keys")?;
        assert_eq!(stored.shortcut, "Ctrl+A");
        assert_eq!(app.capturing, None);
        assert!(matches!(app.notice, Notice::Saved));

        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn long_field_lists_keep_the_selection_visible() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("kit-settings-scroll-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let mut app = SettingsEditor::open(store, vec![section(open_many)], NORD);
        let mut terminal = Terminal::new(TestBackend::new(70, 12))?;
        terminal.draw(|frame| app.render(frame, frame.area()))?;
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for _ in 0..11 {
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        terminal.draw(|frame| app.render(frame, frame.area()))?;

        assert_eq!(app.selected_field, 11);
        assert!(app.field_scroll > 0);
        assert!(app.field_scroll <= app.selected_field);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn long_section_lists_keep_the_selection_visible() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("kit-settings-sections-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let mut app = SettingsEditor::open(store, vec![section(open_test); 12], NORD);
        let mut terminal = Terminal::new(TestBackend::new(70, 12))?;
        terminal.draw(|frame| app.render(frame, frame.area()))?;
        for _ in 0..11 {
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        terminal.draw(|frame| app.render(frame, frame.area()))?;

        assert_eq!(app.selected_section, 11);
        assert!(app.section_scroll > 0);
        assert!(app.section_scroll <= app.selected_section);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn invalid_sections_are_explicit_and_do_not_block_other_sections() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("kit-settings-error-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let invalid = SettingsSection::new(
            SettingsSectionMeta {
                id: "invalid",
                title: "Invalid",
                description: "Invalid preferences",
            },
            open_invalid,
        );
        let mut app = SettingsEditor::open(store, vec![invalid, section(open_test)], NORD);
        let mut terminal = Terminal::new(TestBackend::new(90, 20))?;
        terminal.draw(|frame| app.render(frame, frame.area()))?;
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(screen.contains("Could not load this Settings section"));
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected_section, 1);
        assert!(matches!(app.sections[1].state, SectionState::Ready(_)));
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn compact_terminals_remain_navigable() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("kit-settings-compact-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let mut app = SettingsEditor::open(store, vec![section(open_test)], NORD);
        let mut terminal = Terminal::new(TestBackend::new(30, 8))?;

        terminal.draw(|frame| app.render(frame, frame.area()))?;
        let sections = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(sections.contains("Test"));
        assert!(!sections.contains("needs at least"));

        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        terminal.draw(|frame| app.render(frame, frame.area()))?;
        let fields = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(fields.contains("Enabled"));

        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.active_region, ActiveRegion::Sections);

        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn failed_writes_remain_visible_in_the_editor() {
        let dir = std::env::temp_dir().join(format!("kit-settings-write-{}", std::process::id()));
        let store = ConfigStore::rooted(dir.clone());
        let mut app = SettingsEditor::open(store, vec![section(open_failing)], NORD);
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(&app.notice, Notice::Error(error) if error.contains("disk unavailable")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
