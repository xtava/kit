//! Pure viewport policy for Console terminal projections.
//!
//! Console retains only a bounded window of each pane's stable rows. A historical viewport is
//! therefore anchored by its stable top row rather than by an offset from the moving live tail.

use std::ops::Range;

use wezterm_term::StableRowIndex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ScrollMetrics {
    first_row: StableRowIndex,
    line_count: usize,
    visible_rows: usize,
}

impl ScrollMetrics {
    pub(super) const fn new(
        first_row: StableRowIndex,
        line_count: usize,
        visible_rows: usize,
    ) -> Self {
        Self { first_row, line_count, visible_rows }
    }

    fn projected_end(self) -> StableRowIndex {
        self.first_row.saturating_add(count_as_stable_row(self.line_count))
    }

    fn visible_line_count(self) -> usize {
        self.visible_rows.min(self.line_count)
    }

    fn live_top(self) -> StableRowIndex {
        self.projected_end().saturating_sub(count_as_stable_row(self.visible_line_count()))
    }

    fn clamp_top(self, top: StableRowIndex) -> StableRowIndex {
        top.clamp(self.first_row, self.live_top())
    }

    fn line_index(self, row: StableRowIndex) -> usize {
        usize::try_from(row.saturating_sub(self.first_row)).unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ScrollState {
    anchored_top: Option<StableRowIndex>,
}

impl ScrollState {
    pub(super) const fn is_live(self) -> bool {
        self.anchored_top.is_none()
    }

    pub(super) fn reset(&mut self) {
        self.anchored_top = None;
    }

    pub(super) fn normalize(&mut self, metrics: ScrollMetrics) {
        if let Some(top) = self.anchored_top {
            let top = metrics.clamp_top(top);
            self.anchored_top = (top < metrics.live_top()).then_some(top);
        }
    }

    pub(super) fn can_scroll_up(self, metrics: ScrollMetrics) -> bool {
        self.top_row(metrics) > metrics.first_row
    }

    pub(super) fn can_scroll_down(self) -> bool {
        !self.is_live()
    }

    pub(super) fn scroll_up(&mut self, rows: usize, metrics: ScrollMetrics) {
        let top =
            self.top_row(metrics).saturating_sub(count_as_stable_row(rows)).max(metrics.first_row);
        self.anchored_top = (top < metrics.live_top()).then_some(top);
    }

    pub(super) fn scroll_down(&mut self, rows: usize, metrics: ScrollMetrics) {
        let top =
            self.top_row(metrics).saturating_add(count_as_stable_row(rows)).min(metrics.live_top());
        self.anchored_top = (top < metrics.live_top()).then_some(top);
    }

    pub(super) fn scroll_to_row(&mut self, row: StableRowIndex, metrics: ScrollMetrics) {
        let top = metrics.clamp_top(row);
        self.anchored_top = (top < metrics.live_top()).then_some(top);
    }

    pub(super) fn visible_range(self, metrics: ScrollMetrics) -> Range<usize> {
        let start = metrics.line_index(self.top_row(metrics)).min(metrics.line_count);
        start..start.saturating_add(metrics.visible_line_count()).min(metrics.line_count)
    }

    fn top_row(self, metrics: ScrollMetrics) -> StableRowIndex {
        self.anchored_top.map_or_else(|| metrics.live_top(), |top| metrics.clamp_top(top))
    }
}

fn count_as_stable_row(count: usize) -> StableRowIndex {
    StableRowIndex::try_from(count).unwrap_or(StableRowIndex::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: ScrollMetrics = ScrollMetrics::new(100, 20, 5);

    #[test]
    fn line_and_page_scrolling_are_distinct_and_bounded() {
        let mut scroll = ScrollState::default();
        assert_eq!(scroll.visible_range(METRICS), 15..20);

        scroll.scroll_up(3, METRICS);
        assert_eq!(scroll.visible_range(METRICS), 12..17);
        assert!(!scroll.is_live());

        scroll.scroll_up(4, METRICS);
        assert_eq!(scroll.visible_range(METRICS), 8..13);

        scroll.scroll_up(usize::MAX, METRICS);
        assert_eq!(scroll.visible_range(METRICS), 0..5);
        assert!(!scroll.can_scroll_up(METRICS));
    }

    #[test]
    fn scrolling_down_to_the_tail_restores_live_follow() {
        let mut scroll = ScrollState::default();
        scroll.scroll_up(3, METRICS);
        scroll.scroll_down(2, METRICS);
        assert_eq!(scroll.visible_range(METRICS), 14..19);
        assert!(scroll.can_scroll_down());

        scroll.scroll_down(1, METRICS);
        assert_eq!(scroll.visible_range(METRICS), 15..20);
        assert!(scroll.is_live());
    }

    #[test]
    fn historical_anchor_survives_appended_output() {
        let mut scroll = ScrollState::default();
        scroll.scroll_up(3, METRICS);
        assert_eq!(scroll.visible_range(METRICS), 12..17);

        let appended = ScrollMetrics::new(100, 24, 5);
        scroll.normalize(appended);
        assert_eq!(scroll.visible_range(appended), 12..17);
    }

    #[test]
    fn retention_clamps_an_anchor_to_the_oldest_projected_row() {
        let mut scroll = ScrollState::default();
        scroll.scroll_up(10, METRICS);
        assert_eq!(scroll.visible_range(METRICS), 5..10);

        let retained = ScrollMetrics::new(108, 20, 5);
        scroll.normalize(retained);
        assert_eq!(scroll.visible_range(retained), 0..5);
        assert!(!scroll.is_live());
    }

    #[test]
    fn a_viewport_larger_than_the_projection_has_one_stable_range() {
        let metrics = ScrollMetrics::new(42, 3, 8);
        let mut scroll = ScrollState::default();
        scroll.scroll_up(usize::MAX, metrics);
        assert_eq!(scroll.visible_range(metrics), 0..3);
        assert!(scroll.is_live());
        assert!(!scroll.can_scroll_up(metrics));
    }
}
