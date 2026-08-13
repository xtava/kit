//! Shared whole-source syntax highlighting for terminal surfaces.

use std::{path::Path, sync::LazyLock};

use ratatui::{
    style::{Modifier, Style},
    text::Span,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

use super::theme::TuiTheme;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);
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
    let lines = lines.into_iter().map(without_line_ending).collect::<Vec<_>>();
    let syntax = SYNTAXES
        .find_syntax_by_token(syntax_hint)
        .or_else(|| SYNTAXES.find_syntax_by_extension(syntax_hint));
    highlight_with_syntax(lines, syntax, fallback, palette)
}

/// Highlight a source file using its full filename, extension, or first-line mode marker.
pub fn highlight_file_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    path: &Path,
    fallback: Style,
    palette: TuiTheme,
) -> Vec<Vec<Span<'static>>> {
    let lines = lines.into_iter().map(without_line_ending).collect::<Vec<_>>();
    let first_line = lines.first().copied().unwrap_or_default();
    let syntax = syntax_for_file(path, first_line);
    highlight_with_syntax(lines, syntax, fallback, palette)
}

/// Return the display name of the syntax selected for a source file.
pub fn file_syntax_name(path: &Path, first_line: &str) -> Option<String> {
    syntax_for_file(path, first_line).map(|syntax| syntax.name.clone())
}

/// Whether a filename or extension has a bundled syntax definition.
pub fn supports_file(path: &Path) -> bool {
    syntax_for_path(path).is_some_and(|syntax| syntax.name != "Plain Text")
}

fn syntax_for_file(path: &Path, first_line: &str) -> Option<&'static SyntaxReference> {
    syntax_for_path(path).or_else(|| SYNTAXES.find_syntax_by_first_line(first_line))
}

fn syntax_for_path(path: &Path) -> Option<&'static SyntaxReference> {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or_default();
    SYNTAXES
        .find_syntax_by_extension(file_name)
        .or_else(|| SYNTAXES.find_syntax_by_extension(extension))
}

fn highlight_with_syntax(
    lines: Vec<&str>,
    syntax: Option<&SyntaxReference>,
    fallback: Style,
    palette: TuiTheme,
) -> Vec<Vec<Span<'static>>> {
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

fn without_line_ending(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
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

    #[test]
    fn highlights_typescript_and_tsx_fence_labels() {
        let source = ["const greeting: string = \"hello\"; // typed\n"];
        for language in ["typescript", "ts", "tsx"] {
            let fallback = Style::default().fg(Color::Black);
            let highlighted = highlight_lines(source, language, fallback, NORD);
            let spans = &highlighted[0];

            assert!(spans.len() > 1, "{language} should produce syntax spans");
            assert!(spans.iter().all(|span| span.style.fg != Some(Color::Black)));
            assert!(spans.iter().all(|span| span.style.bg.is_none()));
        }
    }
}
