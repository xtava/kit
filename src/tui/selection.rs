//! Rendered-cell text selection shared by interactive Kit tools.
//!
//! Tools publish only safe selectable rectangles after rendering. This module owns pointer
//! gestures, logical ranges, exact cell snapshots, highlighting, and plain-text extraction; the
//! caller retains event precedence and performs clipboard effects through [`super::Session`].

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::{Position, Rect},
    style::Style,
    Frame,
};
use unicode_width::UnicodeWidthStr;

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(350);

/// One tool-owned output surface that is safe for passive pointer selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectableRegion<Id> {
    pub id: Id,
    pub area: Rect,
    pub row_origin: i64,
    pub column_origin: usize,
    pub revision: u64,
}

impl<Id> SelectableRegion<Id> {
    pub const fn new(
        id: Id,
        area: Rect,
        row_origin: i64,
        column_origin: usize,
        revision: u64,
    ) -> Self {
        Self { id, area, row_origin, column_origin, revision }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TextPoint {
    pub row: i64,
    pub column: usize,
}

impl TextPoint {
    pub const fn new(row: i64, column: usize) -> Self {
        Self { row, column }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionMode {
    Cell,
    Word,
    Line,
    Rectangular,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionOutcome<Id> {
    Unhandled,
    Captured,
    Changed,
    CopyReady(String),
    EdgeScroll { surface: Id, lines: isize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SelectionRange {
    anchor: TextPoint,
    focus: TextPoint,
}

impl SelectionRange {
    fn ordered(self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    fn contains(self, point: TextPoint, rectangular: bool) -> bool {
        let (start, end) = self.ordered();
        if rectangular {
            let first_column = self.anchor.column.min(self.focus.column);
            let last_column = self.anchor.column.max(self.focus.column);
            point.row >= start.row
                && point.row <= end.row
                && point.column >= first_column
                && point.column <= last_column
        } else {
            point >= start && point <= end
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveSelection<Id> {
    surface: Id,
    revision: u64,
    range: SelectionRange,
    mode: SelectionMode,
}

#[derive(Clone, Copy, Debug)]
struct ClickState<Id> {
    surface: Id,
    revision: u64,
    point: TextPoint,
    count: u8,
    at: Instant,
}

#[derive(Clone, Debug, Default)]
struct CapturedRow {
    cells: BTreeMap<usize, String>,
}

impl CapturedRow {
    fn merge(&mut self, other: &Self) {
        self.cells.extend(other.cells.iter().map(|(column, text)| (*column, text.clone())));
    }

    fn normalize_column(&self, column: usize) -> usize {
        if self.cells.contains_key(&column) {
            return column;
        }
        self.cells
            .range(..column)
            .next_back()
            .and_then(|(start, text)| {
                let width = UnicodeWidthStr::width(text.as_str()).max(1);
                ((*start).saturating_add(width) > column).then_some(*start)
            })
            .unwrap_or(column)
    }

    fn bounds(&self) -> Option<(usize, usize)> {
        Some((*self.cells.first_key_value()?.0, *self.cells.last_key_value()?.0))
    }

    fn word_range(&self, column: usize) -> Option<(usize, usize)> {
        let column = self.normalize_column(column);
        let class = word_class(self.cells.get(&column)?);
        let mut start = column;
        let mut end = column;
        for (candidate, text) in self.cells.range(..column).rev() {
            if word_class(text) != class {
                break;
            }
            start = *candidate;
        }
        for (candidate, text) in self.cells.range(column.saturating_add(1)..) {
            if word_class(text) != class {
                break;
            }
            end = *candidate;
        }
        Some((start, end))
    }

    fn text(&self, first_column: usize, last_column: usize) -> String {
        let mut text = String::new();
        for (_, symbol) in self.cells.range(first_column..=last_column) {
            text.push_str(symbol);
        }
        text.trim_end_matches(' ').to_owned()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WordClass {
    Whitespace,
    Word,
    Punctuation,
}

fn word_class(text: &str) -> WordClass {
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return WordClass::Whitespace;
    };
    if first.is_whitespace() {
        WordClass::Whitespace
    } else if first.is_alphanumeric() || first == '_' || characters.any(char::is_alphanumeric) {
        WordClass::Word
    } else {
        WordClass::Punctuation
    }
}

#[derive(Clone, Debug)]
struct RegionSnapshot<Id> {
    region: SelectableRegion<Id>,
    rows: BTreeMap<i64, CapturedRow>,
}

impl<Id: Copy> RegionSnapshot<Id> {
    fn point_at(&self, position: Position) -> Option<TextPoint> {
        if !self.region.area.contains(position) {
            return None;
        }
        let row = self
            .region
            .row_origin
            .saturating_add(i64::from(position.y.saturating_sub(self.region.area.y)));
        let column = self
            .region
            .column_origin
            .saturating_add(usize::from(position.x.saturating_sub(self.region.area.x)));
        let column = self.rows.get(&row).map_or(column, |line| line.normalize_column(column));
        Some(TextPoint::new(row, column))
    }
}

/// One active rendered-text selection for a tool run.
#[derive(Clone, Debug)]
pub struct TextSelection<Id> {
    active: Option<ActiveSelection<Id>>,
    dragging: bool,
    clicks: Option<ClickState<Id>>,
    selected_rows: BTreeMap<i64, CapturedRow>,
    frame: Vec<RegionSnapshot<Id>>,
}

impl<Id> Default for TextSelection<Id> {
    fn default() -> Self {
        Self {
            active: None,
            dragging: false,
            clicks: None,
            selected_rows: BTreeMap::new(),
            frame: Vec::new(),
        }
    }
}

impl<Id: Copy + Eq> TextSelection<Id> {
    pub const fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub const fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn surface(&self) -> Option<Id> {
        self.active.map(|active| active.surface)
    }

    pub fn clear(&mut self) -> bool {
        let changed = self.active.take().is_some();
        self.dragging = false;
        self.selected_rows.clear();
        changed
    }

    /// Captures safe rendered cells and applies selection highlighting in the same frame.
    pub fn capture_frame(
        &mut self,
        frame: &mut Frame<'_>,
        regions: &[SelectableRegion<Id>],
        selection_style: Style,
    ) {
        let snapshots = {
            let buffer = frame.buffer_mut();
            regions.iter().filter_map(|region| capture_region(buffer, *region)).collect::<Vec<_>>()
        };

        if let Some(active) = self.active {
            let matching = snapshots
                .iter()
                .filter(|snapshot| {
                    snapshot.region.id == active.surface
                        && snapshot.region.revision == active.revision
                })
                .collect::<Vec<_>>();
            if matching.is_empty() {
                self.clear();
            } else if !selection_is_visible(&matching, active) {
                self.clear();
            } else if matching
                .iter()
                .any(|snapshot| selection_changed(snapshot, active, &self.selected_rows))
            {
                self.clear();
            } else {
                for snapshot in &matching {
                    for (row, cells) in &snapshot.rows {
                        self.selected_rows.entry(*row).or_default().merge(cells);
                    }
                }
                let buffer = frame.buffer_mut();
                for snapshot in matching {
                    highlight_snapshot(buffer, snapshot, active, selection_style);
                }
            }
        }
        self.frame = snapshots;
    }

    /// Handles only passive-selection pointer gestures. Callers retain higher-precedence routes.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> SelectionOutcome<Id> {
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.begin(position, mouse.modifiers),
            MouseEventKind::Drag(MouseButton::Left) => self.extend(position),
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                SelectionOutcome::Captured
            }
            _ => SelectionOutcome::Unhandled,
        }
    }

    /// Handles selection-only keys. An unhandled key must continue through the tool's normal path.
    pub fn on_key(&mut self, key: KeyEvent) -> SelectionOutcome<Id> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return SelectionOutcome::Unhandled;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self
                .copy_text()
                .map(SelectionOutcome::CopyReady)
                .unwrap_or(SelectionOutcome::Unhandled);
        }
        if key.code == KeyCode::Esc && self.clear() {
            return SelectionOutcome::Changed;
        }
        SelectionOutcome::Unhandled
    }

    pub fn copy_text(&self) -> Option<String> {
        let active = self.active?;
        let (start, end) = active.range.ordered();
        let rectangular = active.mode == SelectionMode::Rectangular;
        let first_rectangle_column = active.range.anchor.column.min(active.range.focus.column);
        let last_rectangle_column = active.range.anchor.column.max(active.range.focus.column);
        let mut lines = Vec::new();
        for (row, cells) in self.selected_rows.range(start.row..=end.row) {
            let (first_column, last_column) = if rectangular {
                (first_rectangle_column, last_rectangle_column)
            } else {
                (
                    if *row == start.row { start.column } else { 0 },
                    if *row == end.row { end.column } else { usize::MAX },
                )
            };
            lines.push(cells.text(first_column, last_column));
        }
        let text = lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn begin(&mut self, position: Position, modifiers: KeyModifiers) -> SelectionOutcome<Id> {
        let Some(snapshot_index) =
            self.frame.iter().position(|snapshot| snapshot.region.area.contains(position))
        else {
            return SelectionOutcome::Unhandled;
        };
        let snapshot = &self.frame[snapshot_index];
        let Some(point) = snapshot.point_at(position) else {
            return SelectionOutcome::Unhandled;
        };
        let region = snapshot.region;

        if modifiers.contains(KeyModifiers::SHIFT)
            && self.active.is_some_and(|active| {
                active.surface == region.id && active.revision == region.revision
            })
        {
            if let Some(active) = self.active.as_mut() {
                active.range.focus = point;
            }
            self.dragging = true;
            self.capture_current_rows(region.id, region.revision);
            return SelectionOutcome::Changed;
        }

        let click_count = self.next_click_count(region, point);
        let mode = if modifiers.contains(KeyModifiers::ALT) {
            SelectionMode::Rectangular
        } else {
            match click_count {
                2 => SelectionMode::Word,
                3.. => SelectionMode::Line,
                _ => SelectionMode::Cell,
            }
        };
        let range = self.initial_range(snapshot_index, point, mode);
        self.active =
            Some(ActiveSelection { surface: region.id, revision: region.revision, range, mode });
        self.dragging = true;
        self.selected_rows.clear();
        self.capture_current_rows(region.id, region.revision);
        SelectionOutcome::Changed
    }

    fn extend(&mut self, position: Position) -> SelectionOutcome<Id> {
        let Some(active) = self.active else {
            return SelectionOutcome::Unhandled;
        };
        let Some(snapshot_index) = self.frame.iter().position(|snapshot| {
            snapshot.region.id == active.surface
                && snapshot.region.revision == active.revision
                && snapshot.region.area.contains(position)
        }) else {
            let Some(snapshot) = self.frame.iter().find(|snapshot| {
                snapshot.region.id == active.surface
                    && snapshot.region.revision == active.revision
                    && position.x >= snapshot.region.area.x
                    && position.x < snapshot.region.area.right()
            }) else {
                return SelectionOutcome::Captured;
            };
            let lines = if position.y < snapshot.region.area.y {
                -1
            } else if position.y >= snapshot.region.area.bottom() {
                1
            } else {
                return SelectionOutcome::Captured;
            };
            return SelectionOutcome::EdgeScroll { surface: active.surface, lines };
        };
        let snapshot = &self.frame[snapshot_index];
        let Some(point) = snapshot.point_at(position) else {
            return SelectionOutcome::Captured;
        };
        let focus = self.extended_focus(snapshot_index, point, active.mode);
        if let Some(selection) = self.active.as_mut() {
            selection.range.focus = focus;
        }
        self.capture_current_rows(active.surface, active.revision);
        SelectionOutcome::Changed
    }

    fn initial_range(
        &self,
        snapshot_index: usize,
        point: TextPoint,
        mode: SelectionMode,
    ) -> SelectionRange {
        let snapshot = &self.frame[snapshot_index];
        match mode {
            SelectionMode::Word => snapshot
                .rows
                .get(&point.row)
                .and_then(|row| row.word_range(point.column))
                .map(|(start, end)| SelectionRange {
                    anchor: TextPoint::new(point.row, start),
                    focus: TextPoint::new(point.row, end),
                })
                .unwrap_or(SelectionRange { anchor: point, focus: point }),
            SelectionMode::Line => snapshot
                .rows
                .get(&point.row)
                .and_then(CapturedRow::bounds)
                .map(|(start, end)| SelectionRange {
                    anchor: TextPoint::new(point.row, start),
                    focus: TextPoint::new(point.row, end),
                })
                .unwrap_or(SelectionRange { anchor: point, focus: point }),
            SelectionMode::Cell | SelectionMode::Rectangular => {
                SelectionRange { anchor: point, focus: point }
            }
        }
    }

    fn extended_focus(
        &self,
        snapshot_index: usize,
        point: TextPoint,
        mode: SelectionMode,
    ) -> TextPoint {
        let Some(active) = self.active else { return point };
        let snapshot = &self.frame[snapshot_index];
        match mode {
            SelectionMode::Word => snapshot
                .rows
                .get(&point.row)
                .and_then(|row| row.word_range(point.column))
                .map(|(start, end)| {
                    TextPoint::new(
                        point.row,
                        if point >= active.range.anchor { end } else { start },
                    )
                })
                .unwrap_or(point),
            SelectionMode::Line => snapshot
                .rows
                .get(&point.row)
                .and_then(CapturedRow::bounds)
                .map(|(start, end)| {
                    TextPoint::new(
                        point.row,
                        if point >= active.range.anchor { end } else { start },
                    )
                })
                .unwrap_or(point),
            SelectionMode::Cell | SelectionMode::Rectangular => point,
        }
    }

    fn next_click_count(&mut self, region: SelectableRegion<Id>, point: TextPoint) -> u8 {
        let now = Instant::now();
        let count = self
            .clicks
            .filter(|click| {
                click.surface == region.id
                    && click.revision == region.revision
                    && now.duration_since(click.at) <= MULTI_CLICK_INTERVAL
                    && click.point.row == point.row
                    && click.point.column.abs_diff(point.column) <= 1
            })
            .map_or(1, |click| click.count.saturating_add(1).min(3));
        self.clicks = Some(ClickState {
            surface: region.id,
            revision: region.revision,
            point,
            count,
            at: now,
        });
        count
    }

    fn capture_current_rows(&mut self, surface: Id, revision: u64) {
        for snapshot in &self.frame {
            if snapshot.region.id != surface || snapshot.region.revision != revision {
                continue;
            }
            for (row, cells) in &snapshot.rows {
                self.selected_rows.entry(*row).or_default().merge(cells);
            }
        }
    }
}

fn selection_is_visible<Id: Copy + Eq>(
    snapshots: &[&RegionSnapshot<Id>],
    active: ActiveSelection<Id>,
) -> bool {
    let (start, end) = active.range.ordered();
    snapshots.iter().any(|snapshot| snapshot.rows.range(start.row..=end.row).next().is_some())
}

fn selection_changed<Id: Copy + Eq>(
    snapshot: &RegionSnapshot<Id>,
    active: ActiveSelection<Id>,
    selected_rows: &BTreeMap<i64, CapturedRow>,
) -> bool {
    let rectangular = active.mode == SelectionMode::Rectangular;
    selected_rows.iter().any(|(row, previous)| {
        let Some(current) = snapshot.rows.get(row) else {
            return false;
        };
        previous.cells.iter().any(|(column, text)| {
            active.range.contains(TextPoint::new(*row, *column), rectangular)
                && current.cells.get(column) != Some(text)
        })
    })
}

fn capture_region<Id: Copy>(
    buffer: &Buffer,
    region: SelectableRegion<Id>,
) -> Option<RegionSnapshot<Id>> {
    let area = region.area.intersection(*buffer.area());
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let mut rows = BTreeMap::new();
    for screen_row in area.y..area.bottom() {
        let logical_row =
            region.row_origin.saturating_add(i64::from(screen_row.saturating_sub(region.area.y)));
        let row = rows.entry(logical_row).or_insert_with(CapturedRow::default);
        let mut screen_column = area.x;
        while screen_column < area.right() {
            let cell = &buffer[(screen_column, screen_row)];
            if cell.diff_option == CellDiffOption::Skip {
                screen_column = screen_column.saturating_add(1);
                continue;
            }
            let logical_column = region
                .column_origin
                .saturating_add(usize::from(screen_column.saturating_sub(region.area.x)));
            row.cells.insert(logical_column, cell.symbol().to_owned());
            let width = UnicodeWidthStr::width(cell.symbol()).max(1);
            screen_column = screen_column.saturating_add(u16::try_from(width).unwrap_or(u16::MAX));
        }
    }
    Some(RegionSnapshot { region: SelectableRegion { area, ..region }, rows })
}

fn highlight_snapshot<Id: Copy + Eq>(
    buffer: &mut Buffer,
    snapshot: &RegionSnapshot<Id>,
    active: ActiveSelection<Id>,
    style: Style,
) {
    let rectangular = active.mode == SelectionMode::Rectangular;
    for screen_row in snapshot.region.area.y..snapshot.region.area.bottom() {
        let logical_row = snapshot
            .region
            .row_origin
            .saturating_add(i64::from(screen_row.saturating_sub(snapshot.region.area.y)));
        let row = snapshot.rows.get(&logical_row);
        for screen_column in snapshot.region.area.x..snapshot.region.area.right() {
            let rendered_column = snapshot
                .region
                .column_origin
                .saturating_add(usize::from(screen_column.saturating_sub(snapshot.region.area.x)));
            let logical_column =
                row.map_or(rendered_column, |row| row.normalize_column(rendered_column));
            if active.range.contains(TextPoint::new(logical_row, logical_column), rectangular) {
                buffer[(screen_column, screen_row)].set_style(style);
            }
        }
    }
}
