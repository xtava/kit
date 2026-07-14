use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::Paragraph,
    Frame,
};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

const RATIO_SCALE: u16 = 1_000;
const DIVIDER_WIDTH: u16 = 1;

/// A terminal split ratio stored as thousandths of the available width.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SplitRatio(u16);

impl SplitRatio {
    pub const fn new(value: u16) -> Self {
        assert!(value > 0 && value < RATIO_SCALE, "split ratio must be between 1 and 999");
        Self(value)
    }

    pub fn adjusted(self, delta: i16) -> Self {
        Self(self.0.saturating_add_signed(delta).clamp(1, RATIO_SCALE - 1))
    }

    fn from_divider(divider: u16, available: u16) -> Self {
        if available == 0 {
            return Self::new(RATIO_SCALE / 2);
        }
        let scaled = (u32::from(divider) * u32::from(RATIO_SCALE) / u32::from(available))
            .clamp(1, u32::from(RATIO_SCALE - 1));
        Self::new(scaled as u16)
    }

    fn desired_cells(self, available: u16) -> u16 {
        ((u32::from(available) * u32::from(self.0) + u32::from(RATIO_SCALE / 2))
            / u32::from(RATIO_SCALE)) as u16
    }
}

/// One in-progress divider drag, including the owning surface and rollback ratio.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitDrag<Context> {
    context: Context,
    start_ratio: SplitRatio,
}

impl<Context: Copy + Eq> SplitDrag<Context> {
    pub fn begin(
        context: Context,
        frame: SplitFrame,
        ratio: SplitRatio,
        column: u16,
        row: u16,
    ) -> Option<Self> {
        frame.contains_separator(column, row).then_some(Self { context, start_ratio: ratio })
    }

    pub fn ratio_for_column(
        self,
        context: Context,
        frame: SplitFrame,
        column: u16,
    ) -> Option<SplitRatio> {
        (self.context == context).then(|| frame.ratio_for_column(column)).flatten()
    }

    pub fn changed(self, ratio: SplitRatio) -> bool {
        ratio != self.start_ratio
    }

    pub fn applies_to(self, context: Context) -> bool {
        self.context == context
    }

    pub const fn cancel(self) -> (Context, SplitRatio) {
        (self.context, self.start_ratio)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitDividerStyle {
    pub idle_color: Color,
    pub active_color: Color,
    pub idle_line: &'static str,
    pub idle_grip: &'static str,
    pub active_line: &'static str,
}

pub fn render_split_divider(
    frame: &mut Frame<'_>,
    split: SplitFrame,
    dragging: bool,
    style: SplitDividerStyle,
) {
    if split.separator.width == 0 || split.separator.height == 0 {
        return;
    }
    let color = if dragging { style.active_color } else { style.idle_color };
    let midpoint = split.separator.height / 2;
    let lines = (0..split.separator.height)
        .map(|row| {
            let symbol = if dragging {
                style.active_line
            } else if row.abs_diff(midpoint) <= 1 {
                style.idle_grip
            } else {
                style.idle_line
            };
            Line::styled(symbol, Style::default().fg(color))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), split.separator);
}

impl<'de> Deserialize<'de> for SplitRatio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        if !(1..RATIO_SCALE).contains(&value) {
            return Err(D::Error::custom(format!(
                "split ratio must be between 1 and {}",
                RATIO_SCALE - 1
            )));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SplitMinimums {
    first: u16,
    second: u16,
}

impl SplitMinimums {
    pub const fn new(first: u16, second: u16) -> Self {
        Self { first, second }
    }
}

/// The exact rendered rectangles and hit target for a two-pane horizontal split.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SplitFrame {
    pub content: Rect,
    pub first: Rect,
    pub second: Rect,
    pub separator: Rect,
    pub separator_hit_region: Rect,
    minimums: SplitMinimums,
}

impl SplitFrame {
    pub fn horizontal(content: Rect, ratio: SplitRatio, minimums: SplitMinimums) -> Self {
        let available = content.width.saturating_sub(DIVIDER_WIDTH);
        let first_width = effective_divider(ratio.desired_cells(available), available, minimums);
        let separator_width = u16::from(content.width > 0);
        let separator_x = content.x.saturating_add(first_width);
        let second_x = separator_x.saturating_add(separator_width);
        let second_width = available.saturating_sub(first_width);
        let hit_start = separator_x.saturating_sub(1).max(content.x);
        let content_end = content.x.saturating_add(content.width);
        let hit_end =
            separator_x.saturating_add(separator_width).saturating_add(1).min(content_end);

        Self {
            content,
            first: Rect::new(content.x, content.y, first_width, content.height),
            second: Rect::new(second_x, content.y, second_width, content.height),
            separator: Rect::new(separator_x, content.y, separator_width, content.height),
            separator_hit_region: Rect::new(
                hit_start,
                content.y,
                hit_end.saturating_sub(hit_start),
                content.height,
            ),
            minimums,
        }
    }

    pub fn contains_separator(self, column: u16, row: u16) -> bool {
        contains(self.separator_hit_region, column, row)
    }

    pub fn ratio_for_column(self, column: u16) -> Option<SplitRatio> {
        let available = self.content.width.saturating_sub(DIVIDER_WIDTH);
        if available == 0 {
            return None;
        }
        let requested = column.saturating_sub(self.content.x).min(available);
        let divider = effective_divider(requested, available, self.minimums);
        Some(SplitRatio::from_divider(divider, available))
    }
}

fn effective_divider(requested: u16, available: u16, minimums: SplitMinimums) -> u16 {
    if available >= minimums.first.saturating_add(minimums.second) {
        requested.clamp(minimums.first, available.saturating_sub(minimums.second))
    } else if available >= 2 {
        requested.clamp(1, available - 1)
    } else {
        requested.min(available)
    }
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_terminal_clamps_without_changing_preference() {
        let ratio = SplitRatio::new(900);
        let frame =
            SplitFrame::horizontal(Rect::new(0, 0, 30, 10), ratio, SplitMinimums::new(24, 32));

        assert!(frame.first.width > 0);
        assert!(frame.second.width > 0);
        assert_eq!(frame.first.width + frame.separator.width + frame.second.width, 30);
        assert_eq!(ratio, SplitRatio::new(900));
    }

    #[test]
    fn separator_hit_region_tracks_rendered_divider() {
        let frame = SplitFrame::horizontal(
            Rect::new(10, 5, 101, 20),
            SplitRatio::new(430),
            SplitMinimums::new(28, 36),
        );

        assert!(frame.contains_separator(frame.separator.x, 10));
        assert!(frame.contains_separator(frame.separator.x.saturating_sub(1), 10));
        assert!(!frame.contains_separator(10, 10));
        assert_eq!(frame.ratio_for_column(60), Some(SplitRatio::new(500)));
    }

    #[test]
    fn drag_rejects_stale_surfaces_and_restores_its_start_ratio() {
        let frame = SplitFrame::horizontal(
            Rect::new(0, 0, 100, 10),
            SplitRatio::new(400),
            SplitMinimums::new(20, 20),
        );
        let drag = SplitDrag::begin("browse", frame, SplitRatio::new(400), frame.separator.x, 5)
            .expect("separator starts a drag");

        assert!(drag.ratio_for_column("versions", frame, 60).is_none());
        assert_eq!(drag.ratio_for_column("browse", frame, 60), Some(SplitRatio::new(606)));
        assert_eq!(drag.cancel(), ("browse", SplitRatio::new(400)));
    }
}
