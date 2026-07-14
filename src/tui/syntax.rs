//! Shared whole-source syntax highlighting for terminal surfaces.

use std::sync::LazyLock;

use ratatui::{
    style::{Modifier, Style},
    text::Span,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};

use super::theme::TuiTheme;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static CODE_THEME: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults()
        .themes
        .get("base16-ocean.dark")
        .cloned()
        .expect("syntect ships the base16-ocean.dark theme")
});

/// Highlight lines in source order so multiline parser state is retained across hidden ranges.
pub fn highlight_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    syntax_hint: &str,
    fallback: Style,
    palette: TuiTheme,
) -> Vec<Vec<Span<'static>>> {
    let lines: Vec<_> = lines.into_iter().collect();
    let syntax = SYNTAXES
        .find_syntax_by_token(syntax_hint)
        .or_else(|| SYNTAXES.find_syntax_by_extension(syntax_hint));
    let Some(syntax) = syntax else {
        return fallback_lines(lines, fallback);
    };

    let mut highlighter = HighlightLines::new(syntax, &CODE_THEME);
    lines
        .into_iter()
        .map(|line| {
            highlighter
                .highlight_line(line, &SYNTAXES)
                .map(|ranges| {
                    ranges
                        .into_iter()
                        .map(|(style, text)| {
                            Span::styled(text.to_owned(), to_ratatui(style, palette))
                        })
                        .collect()
                })
                .unwrap_or_else(|_| vec![Span::styled(line.to_owned(), fallback)])
        })
        .collect()
}

fn fallback_lines(lines: Vec<&str>, fallback: Style) -> Vec<Vec<Span<'static>>> {
    lines.into_iter().map(|line| vec![Span::styled(line.to_owned(), fallback)]).collect()
}

fn to_ratatui(style: syntect::highlighting::Style, palette: TuiTheme) -> Style {
    let mut output = Style::default().fg(palette.nearest_syntax_color(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        output = output.add_modifier(Modifier::UNDERLINED);
    }
    output
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;
    use crate::tui::theme::NORD;

    #[test]
    fn retains_multiline_state_across_source_lines() {
        let lines = ["/* begin\n", "still a comment\n", "end */ let value = 1;\n"];
        let highlighted = highlight_lines(lines, "rs", Style::default(), NORD);

        let comment_color = highlighted[0][0].style.fg;
        assert_ne!(comment_color, Some(Color::Reset));
        assert_eq!(highlighted[1][0].style.fg, comment_color);
        assert!(highlighted[2].iter().any(|span| span.style.fg != comment_color));
    }
}
