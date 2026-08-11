//! Reusable finite and follow-live viewport mechanics plus authoritative scrollbar geometry.
//!
//! Tools retain content policy and event loops. This module owns only bounded offset transitions
//! and the exact vertical scrollbar layout shared by rendering and pointer hit-testing.

use std::ops::Range;

use ratatui::{
    layout::{Position, Rect},
    style::{Color, Style},
    Frame,
};

/// Content and visible lengths for one one-dimensional viewport.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ViewportMetrics {
    content_len: usize,
    visible_len: usize,
}

impl ViewportMetrics {
    pub const fn new(content_len: usize, visible_len: usize) -> Self {
        Self { content_len, visible_len }
    }

    pub const fn content_len(self) -> usize {
        self.content_len
    }

    pub const fn visible_len(self) -> usize {
        self.visible_len
    }

    pub const fn max_top(self) -> usize {
        self.content_len.saturating_sub(self.visible_len)
    }

    pub const fn has_overflow(self) -> bool {
        self.content_len > self.visible_len && self.visible_len > 0
    }

    fn clamp_top(self, top: usize) -> usize {
        top.min(self.max_top())
    }

    fn visible_range(self, top: usize) -> Range<usize> {
        let start = self.clamp_top(top).min(self.content_len);
        start..start.saturating_add(self.visible_len).min(self.content_len)
    }
}

/// A bounded finite viewport whose top offset remains the source of truth.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    top: usize,
}

impl Viewport {
    pub const fn new(top: usize) -> Self {
        Self { top }
    }

    pub fn top(self, metrics: ViewportMetrics) -> usize {
        metrics.clamp_top(self.top)
    }

    pub fn visible_range(self, metrics: ViewportMetrics) -> Range<usize> {
        metrics.visible_range(self.top)
    }

    pub fn normalize(&mut self, metrics: ViewportMetrics) {
        self.top = metrics.clamp_top(self.top);
    }

    pub fn scroll_by(&mut self, delta: isize, metrics: ViewportMetrics) {
        let next = self.top(metrics).saturating_add_signed(delta);
        self.top = metrics.clamp_top(next);
    }

    pub fn page_by(&mut self, pages: isize, metrics: ViewportMetrics) {
        let rows = metrics.visible_len.max(1);
        self.scroll_by(pages.saturating_mul(rows as isize), metrics);
    }

    pub fn home(&mut self) {
        self.top = 0;
    }

    pub fn end(&mut self, metrics: ViewportMetrics) {
        self.top = metrics.max_top();
    }

    pub fn set_top(&mut self, top: usize, metrics: ViewportMetrics) {
        self.top = metrics.clamp_top(top);
    }

    /// Moves only enough to make `index` visible, preserving the current top when possible.
    pub fn ensure_visible(&mut self, index: usize, metrics: ViewportMetrics) {
        if metrics.visible_len == 0 || metrics.content_len == 0 {
            self.normalize(metrics);
            return;
        }
        let index = index.min(metrics.content_len - 1);
        let top = self.top(metrics);
        if index < top {
            self.top = index;
        } else if index >= top.saturating_add(metrics.visible_len) {
            self.top =
                metrics.clamp_top(index.saturating_add(1).saturating_sub(metrics.visible_len));
        }
    }
}

/// A viewport that distinguishes an intentional historical position from following the live tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FollowViewport {
    Live,
    Historical(usize),
}

impl Default for FollowViewport {
    fn default() -> Self {
        Self::Live
    }
}

impl FollowViewport {
    pub const fn at_top() -> Self {
        Self::Historical(0)
    }

    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }

    pub fn top(self, metrics: ViewportMetrics) -> usize {
        match self {
            Self::Live => metrics.max_top(),
            Self::Historical(top) => metrics.clamp_top(top),
        }
    }

    pub fn visible_range(self, metrics: ViewportMetrics) -> Range<usize> {
        metrics.visible_range(self.top(metrics))
    }

    pub fn normalize(&mut self, metrics: ViewportMetrics) {
        if let Self::Historical(top) = self {
            *top = metrics.clamp_top(*top);
        }
    }

    pub fn scroll_by(&mut self, delta: isize, metrics: ViewportMetrics) {
        let next = self.top(metrics).saturating_add_signed(delta).min(metrics.max_top());
        *self = if delta > 0 && next == metrics.max_top() {
            Self::Live
        } else {
            Self::Historical(next)
        };
    }

    pub fn page_by(&mut self, pages: isize, metrics: ViewportMetrics) {
        let rows = metrics.visible_len.max(1);
        self.scroll_by(pages.saturating_mul(rows as isize), metrics);
    }

    pub fn home(&mut self) {
        *self = Self::Historical(0);
    }

    pub fn end(&mut self) {
        *self = Self::Live;
    }

    pub fn set_historical_top(&mut self, top: usize, metrics: ViewportMetrics) {
        *self = Self::Historical(metrics.clamp_top(top));
    }

    /// Sets an absolute top and resumes live following when that position is the current tail.
    pub fn set_top(&mut self, top: usize, metrics: ViewportMetrics) {
        let top = metrics.clamp_top(top);
        *self = if top == metrics.max_top() { Self::Live } else { Self::Historical(top) };
    }
}

/// Exact rendered geometry for one vertical scrollbar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollbarLayout {
    pub track: Rect,
    pub thumb: Rect,
    max_top: usize,
}

impl ScrollbarLayout {
    /// Places a scrollbar in the rightmost column of `area` when content overflows.
    pub fn vertical_right(area: Rect, metrics: ViewportMetrics, top: usize) -> Option<Self> {
        if area.width == 0 || area.height == 0 || !metrics.has_overflow() {
            return None;
        }
        let track = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
        let track_len = usize::from(track.height);
        let thumb_len =
            div_ceil(metrics.visible_len.saturating_mul(track_len), metrics.content_len)
                .clamp(1, track_len);
        let travel = track_len.saturating_sub(thumb_len);
        let top = metrics.clamp_top(top);
        let thumb_start = if metrics.max_top() == 0 {
            0
        } else {
            div_round(top.saturating_mul(travel), metrics.max_top()).min(travel)
        };
        Some(Self {
            track,
            thumb: Rect::new(
                track.x,
                track.y.saturating_add(u16::try_from(thumb_start).unwrap_or(u16::MAX)),
                1,
                u16::try_from(thumb_len).unwrap_or(track.height),
            ),
            max_top: metrics.max_top(),
        })
    }

    pub fn contains(self, position: Position) -> bool {
        self.track.contains(position)
    }

    pub fn thumb_contains(self, position: Position) -> bool {
        self.thumb.contains(position)
    }

    /// Maps a track click to a top offset with the thumb centered on the pointer.
    pub fn top_for_track_row(self, row: u16) -> usize {
        let thumb_len = usize::from(self.thumb.height);
        let track_len = usize::from(self.track.height);
        let travel = track_len.saturating_sub(thumb_len);
        if travel == 0 || self.max_top == 0 {
            return 0;
        }
        let relative = usize::from(row.saturating_sub(self.track.y));
        let start = relative.saturating_sub(thumb_len / 2).min(travel);
        div_round(start.saturating_mul(self.max_top), travel).min(self.max_top)
    }
}

/// Pointer capture for dragging a published scrollbar thumb.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollbarDrag {
    grab_offset: u16,
}

impl ScrollbarDrag {
    pub fn begin(layout: ScrollbarLayout, position: Position) -> Option<Self> {
        layout
            .thumb_contains(position)
            .then_some(Self { grab_offset: position.y.saturating_sub(layout.thumb.y) })
    }

    pub fn top_for_row(self, layout: ScrollbarLayout, row: u16) -> usize {
        let track_len = usize::from(layout.track.height);
        let thumb_len = usize::from(layout.thumb.height);
        let travel = track_len.saturating_sub(thumb_len);
        if travel == 0 || layout.max_top == 0 {
            return 0;
        }
        let relative =
            usize::from(row.saturating_sub(layout.track.y).saturating_sub(self.grab_offset))
                .min(travel);
        div_round(relative.saturating_mul(layout.max_top), travel).min(layout.max_top)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollbarStyle {
    pub track_color: Color,
    pub thumb_color: Color,
    pub active_thumb_color: Color,
    pub track_symbol: &'static str,
    pub thumb_symbol: &'static str,
}

/// Renders exactly the geometry used by [`ScrollbarLayout`] pointer hit-testing.
pub fn render_vertical_scrollbar(
    frame: &mut Frame<'_>,
    layout: ScrollbarLayout,
    dragging: bool,
    style: ScrollbarStyle,
) {
    for row in layout.track.rows() {
        frame.buffer_mut()[(row.x, row.y)]
            .set_symbol(style.track_symbol)
            .set_style(Style::default().fg(style.track_color));
    }
    let thumb_color = if dragging { style.active_thumb_color } else { style.thumb_color };
    for row in layout.thumb.rows() {
        frame.buffer_mut()[(row.x, row.y)]
            .set_symbol(style.thumb_symbol)
            .set_style(Style::default().fg(thumb_color));
    }
}

fn div_ceil(numerator: usize, denominator: usize) -> usize {
    if denominator == 0 {
        0
    } else {
        numerator / denominator + usize::from(numerator % denominator != 0)
    }
}

fn div_round(numerator: usize, denominator: usize) -> usize {
    if denominator == 0 {
        0
    } else {
        numerator.saturating_add(denominator / 2) / denominator
    }
}
