use crate::tui::{SplitMinimums, SplitRatio};

use super::{
    client::{SessionId, TerminalView},
    scroll::{ScrollMetrics, ScrollState},
};

pub(super) const TERMINAL_PANEL_MIN_WIDTH: u16 = 32;
pub(super) const TERMINAL_SPLIT_MINIMUMS: SplitMinimums =
    SplitMinimums::new(TERMINAL_PANEL_MIN_WIDTH, TERMINAL_PANEL_MIN_WIDTH);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PanelSlot {
    Primary,
    Secondary,
}

impl PanelSlot {
    pub(super) const ALL: [Self; 2] = [Self::Primary, Self::Secondary];
}

pub(super) struct TerminalPanel {
    session_id: SessionId,
    terminal: Option<TerminalView>,
    scroll: ScrollState,
    last_terminal_size: Option<(usize, u16, u16)>,
    viewport_visible: bool,
}

impl TerminalPanel {
    fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            terminal: None,
            scroll: ScrollState::default(),
            last_terminal_size: None,
            viewport_visible: false,
        }
    }

    pub(super) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(super) fn terminal(&self) -> Option<&TerminalView> {
        self.terminal.as_ref()
    }

    pub(super) const fn scroll(&self) -> ScrollState {
        self.scroll
    }

    pub(super) fn scroll_mut(&mut self) -> &mut ScrollState {
        &mut self.scroll
    }

    pub(super) fn last_terminal_size_mut(&mut self) -> &mut Option<(usize, u16, u16)> {
        &mut self.last_terminal_size
    }

    pub(super) fn observe_viewport_visibility(&mut self, visible: bool) -> bool {
        let became_visible = visible && !self.viewport_visible;
        self.viewport_visible = visible;
        became_visible
    }

    pub(super) fn reset_viewport_announcement(&mut self) {
        self.last_terminal_size = None;
    }

    fn set_terminal(&mut self, terminal: Option<TerminalView>) -> bool {
        let changed = self.terminal != terminal;
        self.terminal = terminal;
        self.normalize_scroll();
        changed
    }

    fn normalize_scroll(&mut self) {
        if let Some(terminal) = self.terminal.as_ref() {
            self.scroll.normalize(ScrollMetrics::new(
                terminal.first_row,
                terminal.lines.len(),
                terminal.rows,
            ));
        } else {
            self.scroll.reset();
        }
    }
}

enum PanelLayout {
    Empty,
    Single { panel: TerminalPanel, focus: FocusSurface },
    Split { primary: TerminalPanel, secondary: TerminalPanel, focus: SplitFocus },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FocusSurface {
    Sessions,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitFocus {
    Sessions(PanelSlot),
    Terminal(PanelSlot),
}

impl SplitFocus {
    const fn slot(self) -> PanelSlot {
        match self {
            Self::Sessions(slot) | Self::Terminal(slot) => slot,
        }
    }

    const fn surface(self) -> FocusSurface {
        match self {
            Self::Sessions(_) => FocusSurface::Sessions,
            Self::Terminal(_) => FocusSurface::Terminal,
        }
    }

    const fn with_slot(self, slot: PanelSlot) -> Self {
        match self {
            Self::Sessions(_) => Self::Sessions(slot),
            Self::Terminal(_) => Self::Terminal(slot),
        }
    }
}

pub(super) struct PanelWorkspace {
    layout: PanelLayout,
    split_ratio: SplitRatio,
}

impl PanelWorkspace {
    pub(super) fn new(session_id: Option<SessionId>, split_ratio: SplitRatio) -> Self {
        let layout = session_id.map_or(PanelLayout::Empty, |session_id| PanelLayout::Single {
            panel: TerminalPanel::new(session_id),
            focus: FocusSurface::Sessions,
        });
        Self { layout, split_ratio }
    }

    pub(super) const fn split_ratio(&self) -> SplitRatio {
        self.split_ratio
    }

    pub(super) fn set_split_ratio(&mut self, ratio: SplitRatio) {
        self.split_ratio = ratio;
    }

    pub(super) const fn is_split(&self) -> bool {
        matches!(&self.layout, PanelLayout::Split { .. })
    }

    pub(super) const fn focused_slot(&self) -> PanelSlot {
        match &self.layout {
            PanelLayout::Split { focus, .. } => focus.slot(),
            PanelLayout::Empty | PanelLayout::Single { .. } => PanelSlot::Primary,
        }
    }

    pub(super) const fn focus_surface(&self) -> FocusSurface {
        match &self.layout {
            PanelLayout::Empty => FocusSurface::Sessions,
            PanelLayout::Single { focus, .. } => *focus,
            PanelLayout::Split { focus, .. } => focus.surface(),
        }
    }

    pub(super) fn focused_session_id(&self) -> Option<SessionId> {
        self.session_id(self.focused_slot())
    }

    pub(super) fn session_id(&self, slot: PanelSlot) -> Option<SessionId> {
        self.panel(slot).map(TerminalPanel::session_id)
    }

    pub(super) fn panel(&self, slot: PanelSlot) -> Option<&TerminalPanel> {
        match (&self.layout, slot) {
            (PanelLayout::Single { panel, .. }, PanelSlot::Primary)
            | (PanelLayout::Split { primary: panel, .. }, PanelSlot::Primary) => Some(panel),
            (PanelLayout::Split { secondary: panel, .. }, PanelSlot::Secondary) => Some(panel),
            (PanelLayout::Empty | PanelLayout::Single { .. }, PanelSlot::Secondary)
            | (PanelLayout::Empty, PanelSlot::Primary) => None,
        }
    }

    pub(super) fn panel_mut(&mut self, slot: PanelSlot) -> Option<&mut TerminalPanel> {
        match (&mut self.layout, slot) {
            (PanelLayout::Single { panel, .. }, PanelSlot::Primary)
            | (PanelLayout::Split { primary: panel, .. }, PanelSlot::Primary) => Some(panel),
            (PanelLayout::Split { secondary: panel, .. }, PanelSlot::Secondary) => Some(panel),
            (PanelLayout::Empty | PanelLayout::Single { .. }, PanelSlot::Secondary)
            | (PanelLayout::Empty, PanelSlot::Primary) => None,
        }
    }

    pub(super) fn terminal(&self, slot: PanelSlot) -> Option<&TerminalView> {
        self.panel(slot).and_then(TerminalPanel::terminal)
    }

    pub(super) fn scroll(&self, slot: PanelSlot) -> ScrollState {
        self.panel(slot).map_or(ScrollState::default(), TerminalPanel::scroll)
    }

    pub(super) fn scroll_mut(&mut self, slot: PanelSlot) -> Option<&mut ScrollState> {
        self.panel_mut(slot).map(TerminalPanel::scroll_mut)
    }

    pub(super) fn set_terminal(&mut self, slot: PanelSlot, terminal: Option<TerminalView>) -> bool {
        self.panel_mut(slot).is_some_and(|panel| panel.set_terminal(terminal))
    }

    pub(super) fn normalize_terminals(&mut self) {
        for slot in PanelSlot::ALL {
            if let Some(panel) = self.panel_mut(slot) {
                panel.normalize_scroll();
            }
        }
    }

    pub(super) fn reset_viewport_announcements(&mut self) {
        for slot in PanelSlot::ALL {
            if let Some(panel) = self.panel_mut(slot) {
                panel.reset_viewport_announcement();
            }
        }
    }

    pub(super) fn focus_terminal(&mut self, slot: PanelSlot) -> bool {
        match &mut self.layout {
            PanelLayout::Empty => false,
            PanelLayout::Single { focus, .. } if slot == PanelSlot::Primary => {
                let changed = *focus != FocusSurface::Terminal;
                *focus = FocusSurface::Terminal;
                changed
            }
            PanelLayout::Single { .. } => false,
            PanelLayout::Split { focus, .. } => {
                let next = SplitFocus::Terminal(slot);
                let changed = *focus != next;
                *focus = next;
                changed
            }
        }
    }

    pub(super) fn focus_selected_terminal(&mut self) -> bool {
        self.focus_terminal(self.focused_slot())
    }

    pub(super) fn focus_sessions(&mut self) -> bool {
        match &mut self.layout {
            PanelLayout::Empty => false,
            PanelLayout::Single { focus, .. } => {
                let changed = *focus != FocusSurface::Sessions;
                *focus = FocusSurface::Sessions;
                changed
            }
            PanelLayout::Split { focus, .. } => {
                let next = SplitFocus::Sessions(focus.slot());
                let changed = *focus != next;
                *focus = next;
                changed
            }
        }
    }

    pub(super) fn select(&mut self, session_id: SessionId) -> SelectionChange {
        match &mut self.layout {
            PanelLayout::Empty => {
                self.layout = PanelLayout::Single {
                    panel: TerminalPanel::new(session_id),
                    focus: FocusSurface::Sessions,
                };
                SelectionChange::Replace(PanelSlot::Primary)
            }
            PanelLayout::Single { panel, .. } if panel.session_id == session_id => {
                SelectionChange::Unchanged
            }
            PanelLayout::Single { panel, .. } => {
                *panel = TerminalPanel::new(session_id);
                SelectionChange::Replace(PanelSlot::Primary)
            }
            PanelLayout::Split { primary, secondary, focus }
                if match focus.slot() {
                    PanelSlot::Primary => primary.session_id,
                    PanelSlot::Secondary => secondary.session_id,
                } == session_id =>
            {
                SelectionChange::Unchanged
            }
            PanelLayout::Split { primary, focus, .. } if primary.session_id == session_id => {
                *focus = focus.with_slot(PanelSlot::Primary);
                SelectionChange::Focus(PanelSlot::Primary)
            }
            PanelLayout::Split { secondary, focus, .. } if secondary.session_id == session_id => {
                *focus = focus.with_slot(PanelSlot::Secondary);
                SelectionChange::Focus(PanelSlot::Secondary)
            }
            PanelLayout::Split { primary, secondary, focus } => {
                let slot = focus.slot();
                *match slot {
                    PanelSlot::Primary => primary,
                    PanelSlot::Secondary => secondary,
                } = TerminalPanel::new(session_id);
                SelectionChange::Replace(slot)
            }
        }
    }

    pub(super) fn split(&mut self, session_id: SessionId) -> bool {
        let layout = std::mem::replace(&mut self.layout, PanelLayout::Empty);
        match layout {
            PanelLayout::Single { panel, .. } if panel.session_id != session_id => {
                self.layout = PanelLayout::Split {
                    primary: panel,
                    secondary: TerminalPanel::new(session_id),
                    focus: SplitFocus::Terminal(PanelSlot::Secondary),
                };
                true
            }
            layout => {
                self.layout = layout;
                false
            }
        }
    }

    pub(super) fn close_focused(&mut self) -> bool {
        let layout = std::mem::replace(&mut self.layout, PanelLayout::Empty);
        let PanelLayout::Split { primary, secondary, focus } = layout else {
            self.layout = layout;
            return false;
        };
        self.layout = PanelLayout::Single {
            panel: match focus.slot() {
                PanelSlot::Primary => secondary,
                PanelSlot::Secondary => primary,
            },
            focus: FocusSurface::Terminal,
        };
        true
    }

    pub(super) fn next_split_session(
        &self,
        sessions: impl IntoIterator<Item = SessionId>,
    ) -> Option<SessionId> {
        if self.is_split() {
            return None;
        }
        let primary = self.session_id(PanelSlot::Primary);
        sessions.into_iter().find(|session_id| Some(*session_id) != primary)
    }

    pub(super) fn reconcile(&mut self, live_sessions: &[SessionId]) -> bool {
        let previous_primary = self.session_id(PanelSlot::Primary);
        let previous_secondary = self.session_id(PanelSlot::Secondary);
        let previous_focus = self.focused_slot();
        let previous_surface = self.focus_surface();
        let is_live = |panel: &TerminalPanel| live_sessions.contains(&panel.session_id);
        let layout = std::mem::replace(&mut self.layout, PanelLayout::Empty);
        self.layout = match layout {
            PanelLayout::Empty => live_sessions.first().copied().map_or(PanelLayout::Empty, |id| {
                PanelLayout::Single { panel: TerminalPanel::new(id), focus: FocusSurface::Sessions }
            }),
            PanelLayout::Single { panel, focus } if is_live(&panel) => {
                PanelLayout::Single { panel, focus }
            }
            PanelLayout::Single { focus, .. } => {
                live_sessions.first().copied().map_or(PanelLayout::Empty, |id| {
                    PanelLayout::Single { panel: TerminalPanel::new(id), focus }
                })
            }
            PanelLayout::Split { primary, secondary, focus }
                if is_live(&primary)
                    && is_live(&secondary)
                    && primary.session_id != secondary.session_id =>
            {
                PanelLayout::Split { primary, secondary, focus }
            }
            PanelLayout::Split { primary, secondary: _, focus } if is_live(&primary) => {
                PanelLayout::Single { panel: primary, focus: focus.surface() }
            }
            PanelLayout::Split { primary: _, secondary, focus } if is_live(&secondary) => {
                PanelLayout::Single { panel: secondary, focus: focus.surface() }
            }
            PanelLayout::Split { focus, .. } => {
                live_sessions.first().copied().map_or(PanelLayout::Empty, |id| {
                    PanelLayout::Single { panel: TerminalPanel::new(id), focus: focus.surface() }
                })
            }
        };
        previous_primary != self.session_id(PanelSlot::Primary)
            || previous_secondary != self.session_id(PanelSlot::Secondary)
            || previous_focus != self.focused_slot()
            || previous_surface != self.focus_surface()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionChange {
    Unchanged,
    Focus(PanelSlot),
    Replace(PanelSlot),
}

pub(super) fn visible_panel_slots(
    has_secondary: bool,
    width: u16,
    focused: PanelSlot,
) -> Vec<PanelSlot> {
    if !has_secondary {
        vec![PanelSlot::Primary]
    } else if !TERMINAL_SPLIT_MINIMUMS.fits(width) {
        vec![focused]
    } else {
        vec![PanelSlot::Primary, PanelSlot::Secondary]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_layout_keeps_only_the_focused_assignment_visible() {
        assert_eq!(visible_panel_slots(false, 120, PanelSlot::Secondary), vec![PanelSlot::Primary]);
        assert_eq!(
            visible_panel_slots(true, 120, PanelSlot::Secondary),
            vec![PanelSlot::Primary, PanelSlot::Secondary]
        );
        assert_eq!(visible_panel_slots(true, 50, PanelSlot::Secondary), vec![PanelSlot::Secondary]);
    }
}
