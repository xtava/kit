const MULTI_PANEL_MIN_WIDTH: u16 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PanelSlot {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionChange {
    Unchanged,
    Focus(PanelSlot),
    Replace(PanelSlot),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClosePanelChange {
    KeepPrimary,
    PromoteSecondary,
}

pub(super) fn select_change(
    primary: Option<usize>,
    secondary: Option<usize>,
    focused: PanelSlot,
    target: usize,
) -> SelectionChange {
    if match focused {
        PanelSlot::Primary => primary,
        PanelSlot::Secondary => secondary,
    } == Some(target)
    {
        SelectionChange::Unchanged
    } else if primary == Some(target) {
        SelectionChange::Focus(PanelSlot::Primary)
    } else if secondary == Some(target) {
        SelectionChange::Focus(PanelSlot::Secondary)
    } else {
        SelectionChange::Replace(focused)
    }
}

pub(super) fn next_split_session(
    primary: Option<usize>,
    secondary: Option<usize>,
    sessions: impl IntoIterator<Item = usize>,
) -> Option<usize> {
    secondary
        .is_none()
        .then(|| sessions.into_iter().find(|session_id| Some(*session_id) != primary))?
}

pub(super) fn close_panel_change(
    has_secondary: bool,
    focused: PanelSlot,
) -> Option<ClosePanelChange> {
    has_secondary.then_some(match focused {
        PanelSlot::Primary => ClosePanelChange::PromoteSecondary,
        PanelSlot::Secondary => ClosePanelChange::KeepPrimary,
    })
}

pub(super) fn visible_panel_slots(
    has_secondary: bool,
    width: u16,
    focused: PanelSlot,
) -> Vec<PanelSlot> {
    if !has_secondary {
        vec![PanelSlot::Primary]
    } else if width < MULTI_PANEL_MIN_WIDTH {
        vec![focused]
    } else {
        vec![PanelSlot::Primary, PanelSlot::Secondary]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_a_visible_session_focuses_its_existing_panel() {
        assert_eq!(
            select_change(Some(1), Some(2), PanelSlot::Primary, 2),
            SelectionChange::Focus(PanelSlot::Secondary)
        );
        assert_eq!(
            select_change(Some(1), Some(2), PanelSlot::Secondary, 1),
            SelectionChange::Focus(PanelSlot::Primary)
        );
    }

    #[test]
    fn selecting_an_unassigned_session_replaces_only_the_focused_panel() {
        assert_eq!(
            select_change(Some(1), Some(2), PanelSlot::Primary, 3),
            SelectionChange::Replace(PanelSlot::Primary)
        );
        assert_eq!(
            select_change(Some(1), Some(2), PanelSlot::Secondary, 3),
            SelectionChange::Replace(PanelSlot::Secondary)
        );
    }

    #[test]
    fn selecting_the_focused_session_is_unchanged() {
        assert_eq!(
            select_change(Some(1), Some(2), PanelSlot::Secondary, 2),
            SelectionChange::Unchanged
        );
    }

    #[test]
    fn split_uses_the_first_session_not_already_in_the_primary_panel() {
        assert_eq!(next_split_session(Some(1), None, [1, 2, 3]), Some(2));
        assert_eq!(next_split_session(Some(1), Some(2), [1, 2, 3]), None);
        assert_eq!(next_split_session(Some(1), None, [1]), None);
    }

    #[test]
    fn closing_the_primary_promotes_the_secondary_without_closing_a_session() {
        assert_eq!(
            close_panel_change(true, PanelSlot::Primary),
            Some(ClosePanelChange::PromoteSecondary)
        );
        assert_eq!(
            close_panel_change(true, PanelSlot::Secondary),
            Some(ClosePanelChange::KeepPrimary)
        );
        assert_eq!(close_panel_change(false, PanelSlot::Primary), None);
    }

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
