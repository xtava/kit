use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::TuiTheme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub insert: String,
    pub hint: String,
}

impl Suggestion {
    pub fn new(insert: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { insert: insert.into(), hint: hint.into() }
    }
}

/// A passive-until-engaged suggestion list shared by Kit's bottom-prompt TUIs.
///
/// Typing replaces the menu with a fresh value. Tab or an arrow calls [`Self::cycle`] to engage a
/// row, Enter accepts [`Self::selected`], and Escape calls [`Self::disengage`]. The caller owns what
/// accepting a suggestion means and how candidates are produced.
pub struct SuggestionMenu {
    candidates: Vec<Suggestion>,
    start: usize,
    selected: Option<usize>,
}

impl SuggestionMenu {
    pub fn new(candidates: Vec<Suggestion>, start: usize) -> Self {
        Self { candidates, start, selected: None }
    }

    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn is_engaged(&self) -> bool {
        self.selected.is_some()
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected(&self) -> Option<&Suggestion> {
        self.selected.and_then(|index| self.candidates.get(index))
    }

    pub fn first(&self) -> Option<&Suggestion> {
        self.candidates.first()
    }

    pub fn cycle(&mut self, step: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len() as isize;
        self.selected = Some(match self.selected {
            None if step > 0 => 0,
            None => (len - 1) as usize,
            Some(index) => ((index as isize + step).rem_euclid(len)) as usize,
        });
    }

    pub fn disengage(&mut self) {
        self.selected = None;
    }

    /// Number of candidate rows the menu can claim while leaving `reserved_rows` for the rest of
    /// the screen. The rule line is not included.
    pub fn visible_rows(&self, area: Rect, max_rows: usize, reserved_rows: usize) -> usize {
        let room = (area.height as usize).saturating_sub(reserved_rows);
        self.candidates.len().min(max_rows).min(room)
    }

    /// Render the common suggestion band. `shown` is produced by [`Self::visible_rows`], and
    /// `insert_width` controls the stable first column allocated to replacement text.
    pub fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        shown: usize,
        insert_width: usize,
        theme: TuiTheme,
    ) {
        if shown == 0 {
            return;
        }
        let offset = self.selected.unwrap_or(0).saturating_sub(shown - 1);
        let hint_width = (area.width as usize).saturating_sub(insert_width.saturating_add(8));
        let mut lines = vec![self.rule(offset, shown, area.width as usize, theme)];

        for (index, candidate) in self.candidates.iter().enumerate().skip(offset).take(shown) {
            let engaged = self.selected == Some(index);
            let marker = if engaged { "▌ " } else { "  " };
            let insert_style = if engaged {
                Style::default()
                    .fg(theme.text_strong)
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let hint = if candidate.hint.is_empty() {
                String::new()
            } else {
                format!("  {}", truncate(&candidate.hint, hint_width))
            };
            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(theme.accent)),
                Span::styled(fit(&candidate.insert, insert_width), insert_style),
                Span::styled(hint, Style::default().fg(theme.text_muted)),
            ]));
        }

        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(theme.text).bg(theme.background)),
            area,
        );
    }

    fn rule(&self, offset: usize, shown: usize, width: usize, theme: TuiTheme) -> Line<'static> {
        let mut label = format!("─ suggestions · {} ", self.candidates.len());
        if offset > 0 {
            label.push_str(&format!("· ↑ {offset} "));
        }
        let below = self.candidates.len().saturating_sub(offset + shown);
        if below > 0 {
            label.push_str(&format!("· ↓ {below} "));
        }
        let keys =
            if self.is_engaged() { " ⏎ accept · esc back ─" } else { " ⇥ select ─" };
        let fill = width.saturating_sub(label.width() + keys.width());
        Line::from(Span::styled(
            format!("{label}{}{keys}", "─".repeat(fill)),
            Style::default().fg(theme.border),
        ))
    }
}

fn fit(text: &str, width: usize) -> String {
    let mut fitted = truncate(text, width);
    let padding = width.saturating_sub(fitted.width());
    fitted.push_str(&" ".repeat(padding));
    fitted
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }

    let content_width = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(count: usize) -> SuggestionMenu {
        SuggestionMenu::new(
            (0..count).map(|index| Suggestion::new(format!("c{index}"), "")).collect(),
            0,
        )
    }

    fn rule_text(menu: &SuggestionMenu, offset: usize, shown: usize) -> String {
        menu.rule(offset, shown, 80, super::super::theme::NORD)
            .spans
            .iter()
            .map(|span| span.content.clone())
            .collect()
    }

    #[test]
    fn selection_cycles_and_disengages() {
        let mut menu = menu(3);
        assert!(!menu.is_engaged());
        menu.cycle(1);
        assert_eq!(menu.selected_index(), Some(0));
        menu.cycle(-1);
        assert_eq!(menu.selected_index(), Some(2));
        menu.disengage();
        assert!(!menu.is_engaged());
    }

    #[test]
    fn rule_reports_window_and_keys() {
        let passive = menu(12);
        let rule = rule_text(&passive, 0, 8);
        assert!(rule.contains("suggestions · 12"), "{rule}");
        assert!(rule.contains("↓ 4"), "{rule}");
        assert!(!rule.contains('↑'), "{rule}");
        assert!(rule.contains("⇥ select"), "{rule}");

        let mut engaged = menu(12);
        for _ in 0..10 {
            engaged.cycle(1);
        }
        let rule = rule_text(&engaged, 2, 8);
        assert!(rule.contains("↑ 2"), "{rule}");
        assert!(rule.contains("↓ 2"), "{rule}");
        assert!(rule.contains("⏎ accept"), "{rule}");
    }

    #[test]
    fn visible_rows_never_starves_the_main_view() {
        let area = |height| Rect { x: 0, y: 0, width: 80, height };
        let menu = menu(20);
        assert_eq!(menu.visible_rows(area(40), 8, 8), 8);
        assert_eq!(menu.visible_rows(area(12), 8, 8), 4);
        assert_eq!(menu.visible_rows(area(8), 8, 8), 0);
    }

    #[test]
    fn fit_uses_terminal_column_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("文档-name", 6), "文档-…");
    }
}
