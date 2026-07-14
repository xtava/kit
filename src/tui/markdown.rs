//! Pure, reusable Markdown-to-Ratatui presentation rendering.
//!
//! The renderer owns Markdown semantics and width-aware terminal layout. It performs no I/O and
//! deliberately emits presentation text rather than Markdown source delimiters.

use std::sync::LazyLock;

use pulldown_cmark::{
    Alignment as MarkdownAlignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options,
    Parser, Tag, TagEnd,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span, Text},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::{TuiTheme, NORD};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static CODE_THEME: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults()
        .themes
        .get("base16-ocean.dark")
        .cloned()
        .expect("syntect ships the base16-ocean.dark theme")
});

#[derive(Clone, Debug)]
struct MarkdownTheme {
    syntax_palette: TuiTheme,
    text: Style,
    headings: [Style; 6],
    strong: Style,
    emphasis: Style,
    strikethrough: Style,
    inline_code: Style,
    code: Style,
    code_border: Style,
    code_language: Style,
    link: Style,
    image: Style,
    quote: Style,
    quote_marker: Style,
    list_marker: Style,
    task_complete: Style,
    task_pending: Style,
    table_border: Style,
    table_heading: Style,
    rule: Style,
    metadata: Style,
    footnote: Style,
    math: Style,
    html: Style,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        NORD.into()
    }
}

impl From<TuiTheme> for MarkdownTheme {
    fn from(palette: TuiTheme) -> Self {
        Self {
            syntax_palette: palette,
            text: Style::default().fg(palette.text).bg(palette.surface),
            headings: [
                Style::default()
                    .fg(palette.text_strong)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
                Style::default().fg(palette.accent_alt).add_modifier(Modifier::BOLD),
                Style::default().fg(palette.info).add_modifier(Modifier::BOLD),
                Style::default().fg(palette.focus).add_modifier(Modifier::ITALIC),
                Style::default().fg(palette.text_muted).add_modifier(Modifier::ITALIC),
            ],
            strong: Style::default().fg(palette.text_strong).add_modifier(Modifier::BOLD),
            emphasis: Style::default().fg(palette.accent_alt).add_modifier(Modifier::ITALIC),
            strikethrough: Style::default()
                .fg(palette.text_muted)
                .add_modifier(Modifier::CROSSED_OUT),
            inline_code: Style::default().fg(palette.warning).bg(palette.code_background),
            code: Style::default().fg(palette.text),
            code_border: Style::default().fg(palette.border),
            code_language: Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
            link: Style::default().fg(palette.accent_alt).add_modifier(Modifier::UNDERLINED),
            image: Style::default().fg(palette.special),
            quote: Style::default().fg(palette.text).add_modifier(Modifier::ITALIC),
            quote_marker: Style::default().fg(palette.accent),
            list_marker: Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
            task_complete: Style::default().fg(palette.success).add_modifier(Modifier::BOLD),
            task_pending: Style::default().fg(palette.warning).add_modifier(Modifier::BOLD),
            table_border: Style::default().fg(palette.border),
            table_heading: Style::default().fg(palette.accent).add_modifier(Modifier::BOLD),
            rule: Style::default().fg(palette.border),
            metadata: Style::default().fg(palette.text_muted),
            footnote: Style::default().fg(palette.info),
            math: Style::default().fg(palette.special),
            html: Style::default().fg(palette.text_muted),
        }
    }
}

/// Converts CommonMark/GFM source into width-aware Ratatui text.
#[derive(Clone, Debug, Default)]
pub struct MarkdownRenderer {
    theme: MarkdownTheme,
}

impl MarkdownRenderer {
    pub fn new(theme: TuiTheme) -> Self {
        Self { theme: theme.into() }
    }

    pub fn render(&self, source: &str, width: u16) -> Text<'static> {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_FOOTNOTES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_SMART_PUNCTUATION);
        options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
        options.insert(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
        options.insert(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS);
        options.insert(Options::ENABLE_MATH);
        options.insert(Options::ENABLE_GFM);
        options.insert(Options::ENABLE_DEFINITION_LIST);
        options.insert(Options::ENABLE_SUPERSCRIPT);
        options.insert(Options::ENABLE_SUBSCRIPT);
        options.insert(Options::ENABLE_WIKILINKS);

        let mut writer = Writer::new(self.theme.clone(), width.max(1) as usize);
        for event in Parser::new_ext(source, options) {
            writer.event(event);
        }
        writer.finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WrapMode {
    Words,
    Exact,
    None,
}

struct LogicalLine {
    spans: Vec<Span<'static>>,
    continuation: Vec<Span<'static>>,
    wrap: WrapMode,
}

struct ListState {
    next: Option<u64>,
}

struct ItemState {
    marker: String,
    marker_style: Style,
    indent: usize,
    started: bool,
}

struct CodeBlock {
    language: String,
    source: String,
}

struct ImageState {
    destination: String,
    title: String,
    alt: String,
}

struct TableState {
    alignments: Vec<MarkdownAlignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    cell: Vec<Span<'static>>,
    heading_rows: usize,
}

struct Writer {
    theme: MarkdownTheme,
    width: usize,
    lines: Vec<LogicalLine>,
    current: Option<LogicalLine>,
    inline_styles: Vec<Style>,
    block_styles: Vec<Style>,
    quotes: Vec<Option<BlockQuoteKind>>,
    lists: Vec<ListState>,
    items: Vec<ItemState>,
    code: Option<CodeBlock>,
    image: Option<ImageState>,
    links: Vec<String>,
    table: Option<TableState>,
    footnote_depth: usize,
    needs_gap: bool,
    last_item_marker: Option<usize>,
}

impl Writer {
    fn new(theme: MarkdownTheme, width: usize) -> Self {
        Self {
            theme,
            width,
            lines: Vec::new(),
            current: None,
            inline_styles: Vec::new(),
            block_styles: Vec::new(),
            quotes: Vec::new(),
            lists: Vec::new(),
            items: Vec::new(),
            code: None,
            image: None,
            links: Vec::new(),
            table: None,
            footnote_depth: 0,
            needs_gap: false,
            last_item_marker: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(text.as_ref()),
            Event::Code(code) => self.push_span(code.to_string(), self.theme.inline_code),
            Event::Html(html) | Event::InlineHtml(html) => {
                self.push_span(html.to_string(), self.theme.html)
            }
            Event::FootnoteReference(label) => {
                self.push_span(superscript_reference(label.as_ref()), self.theme.footnote)
            }
            Event::SoftBreak => self.push_span(" ".to_owned(), Style::default()),
            Event::HardBreak => self.finish_line(),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => self.task_marker(checked),
            Event::InlineMath(math) => self.push_span(math.to_string(), self.theme.math),
            Event::DisplayMath(math) => {
                self.separate_block();
                self.push_span(math.to_string(), self.theme.math);
                self.finish_line();
                self.needs_gap = true;
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.items.is_empty() && self.footnote_depth == 0 {
                    self.separate_block();
                }
            }
            Tag::Heading { level, .. } => {
                self.separate_block();
                self.block_styles.push(self.theme.headings[heading_index(level)]);
            }
            Tag::BlockQuote(kind) => {
                if self.quotes.is_empty() {
                    self.separate_block();
                }
                self.quotes.push(kind);
                self.block_styles.push(self.theme.quote);
                if let Some(kind) = kind {
                    self.push_span(alert_name(kind).to_owned(), self.theme.strong);
                    self.finish_line();
                }
            }
            Tag::CodeBlock(kind) => {
                self.separate_block();
                let language = match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(language) => {
                        language.split_whitespace().next().unwrap_or_default().to_owned()
                    }
                };
                self.code = Some(CodeBlock { language, source: String::new() });
            }
            Tag::HtmlBlock => self.separate_block(),
            Tag::List(first) => {
                if self.lists.is_empty() {
                    self.separate_block();
                }
                self.lists.push(ListState { next: first });
            }
            Tag::Item => self.start_item(),
            Tag::FootnoteDefinition(label) => {
                self.separate_block();
                self.footnote_depth += 1;
                self.push_span(format!("{}. ", label), self.theme.footnote);
            }
            Tag::Table(alignments) => {
                self.separate_block();
                self.table = Some(TableState {
                    alignments,
                    rows: Vec::new(),
                    row: Vec::new(),
                    cell: Vec::new(),
                    heading_rows: 0,
                });
            }
            Tag::TableHead => {}
            Tag::TableRow => {}
            Tag::TableCell => {}
            Tag::Emphasis => self.inline_styles.push(self.theme.emphasis),
            Tag::Strong => self.inline_styles.push(self.theme.strong),
            Tag::Strikethrough => self.inline_styles.push(self.theme.strikethrough),
            Tag::Subscript | Tag::Superscript => self
                .inline_styles
                .push(Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC)),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                self.inline_styles.push(self.theme.link);
            }
            Tag::Image { dest_url, title, .. } => {
                self.image = Some(ImageState {
                    destination: dest_url.to_string(),
                    title: title.to_string(),
                    alt: String::new(),
                });
            }
            Tag::MetadataBlock(_) => {
                self.separate_block();
                self.block_styles.push(self.theme.metadata);
            }
            Tag::DefinitionList => self.separate_block(),
            Tag::DefinitionListTitle => self.block_styles.push(self.theme.strong),
            Tag::DefinitionListDefinition => self.push_span("  ".to_owned(), Style::default()),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.finish_line();
                self.needs_gap = self.items.is_empty() && self.footnote_depth == 0;
            }
            TagEnd::Heading(_) => {
                self.finish_line();
                self.block_styles.pop();
                self.needs_gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.finish_line();
                self.quotes.pop();
                self.block_styles.pop();
                self.needs_gap = self.quotes.is_empty();
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::HtmlBlock => {
                self.finish_line();
                self.needs_gap = true;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.needs_gap = self.lists.is_empty();
            }
            TagEnd::Item => {
                self.finish_line();
                self.items.pop();
            }
            TagEnd::FootnoteDefinition => {
                self.finish_line();
                self.footnote_depth = self.footnote_depth.saturating_sub(1);
                self.needs_gap = true;
            }
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    if !table.row.is_empty() {
                        table.rows.push(std::mem::take(&mut table.row));
                    }
                    table.heading_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(std::mem::take(&mut table.row));
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.row.push(std::mem::take(&mut table.cell));
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Subscript
            | TagEnd::Superscript => {
                self.inline_styles.pop();
            }
            TagEnd::Link => {
                self.inline_styles.pop();
                if let Some(destination) = self.links.pop() {
                    self.push_span(format!(" ‹{destination}›"), self.theme.link);
                }
            }
            TagEnd::Image => self.end_image(),
            TagEnd::MetadataBlock(_) => {
                self.finish_line();
                self.block_styles.pop();
                self.needs_gap = true;
            }
            TagEnd::DefinitionList => self.needs_gap = true,
            TagEnd::DefinitionListTitle => {
                self.finish_line();
                self.block_styles.pop();
            }
            TagEnd::DefinitionListDefinition => self.finish_line(),
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = &mut self.code {
            code.source.push_str(text);
            return;
        }
        if let Some(image) = &mut self.image {
            image.alt.push_str(text);
            return;
        }

        let style = self.effective_style();
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_line();
            }
            if !line.is_empty() {
                self.push_span(line.to_owned(), style);
            }
        }
    }

    fn effective_style(&self) -> Style {
        self.block_styles
            .iter()
            .chain(&self.inline_styles)
            .fold(self.theme.text, |style, next| style.patch(*next))
    }

    fn start_item(&mut self) {
        self.finish_line();
        let indent = self.lists.len().saturating_sub(1) * 2;
        let list = self.lists.last_mut().expect("pulldown emits items inside lists");
        let marker = match &mut list.next {
            Some(next) => {
                let marker = format!("{next}. ");
                *next += 1;
                marker
            }
            None => "• ".to_owned(),
        };
        self.items.push(ItemState {
            marker,
            marker_style: self.theme.list_marker,
            indent,
            started: false,
        });
    }

    fn task_marker(&mut self, checked: bool) {
        self.ensure_line();
        let (symbol, style) = if checked {
            ("☑ ", self.theme.task_complete)
        } else {
            ("☐ ", self.theme.task_pending)
        };
        if let (Some(line), Some(index)) = (&mut self.current, self.last_item_marker) {
            if index < line.spans.len() {
                line.spans[index] = Span::styled(symbol.to_owned(), style);
                return;
            }
        }
        self.push_span(symbol.to_owned(), style);
    }

    fn rule(&mut self) {
        self.separate_block();
        let rule = "─".repeat(self.width);
        self.lines.push(LogicalLine {
            spans: vec![Span::styled(rule, self.theme.rule)],
            continuation: Vec::new(),
            wrap: WrapMode::None,
        });
        self.needs_gap = true;
    }

    fn end_image(&mut self) {
        let Some(image) = self.image.take() else {
            return;
        };
        let alt = if image.alt.trim().is_empty() { "image" } else { image.alt.trim() };
        let title = if image.title.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", image.title.trim())
        };
        self.push_span(format!("▧ {alt}{title} ‹{}›", image.destination), self.theme.image);
    }

    fn end_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        if !code.language.is_empty() {
            self.lines.push(LogicalLine {
                spans: vec![Span::styled(code.language.clone(), self.theme.code_language)],
                continuation: Vec::new(),
                wrap: WrapMode::Words,
            });
        }

        let source = code.source.strip_suffix('\n').unwrap_or(&code.source);
        let source_lines = if source.is_empty() { vec![""] } else { source.split('\n').collect() };
        let highlighted = highlight_code(
            source_lines,
            &code.language,
            self.theme.code,
            self.theme.syntax_palette,
        );
        for mut spans in highlighted {
            spans.insert(0, Span::styled("│ ", self.theme.code_border));
            self.lines.push(LogicalLine {
                spans,
                continuation: vec![Span::styled("│ ", self.theme.code_border)],
                wrap: WrapMode::Exact,
            });
        }
        self.needs_gap = true;
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        render_table(&mut self.lines, table, self.width, &self.theme);
        self.needs_gap = true;
    }

    fn push_span(&mut self, content: String, style: Style) {
        let style = self.effective_style().patch(style);
        if let Some(table) = &mut self.table {
            table.cell.push(Span::styled(content, style));
            return;
        }
        self.ensure_line();
        if let Some(line) = &mut self.current {
            line.spans.push(Span::styled(content, style));
        }
    }

    fn ensure_line(&mut self) {
        if self.current.is_some() {
            return;
        }

        let mut spans = Vec::new();
        let mut continuation = Vec::new();
        for _ in &self.quotes {
            spans.push(Span::styled("│ ", self.theme.quote_marker));
            continuation.push(Span::styled("│ ", self.theme.quote_marker));
        }

        self.last_item_marker = None;
        if let Some(item) = self.items.last_mut() {
            let indentation = " ".repeat(item.indent);
            spans.push(Span::raw(indentation.clone()));
            continuation.push(Span::raw(indentation));
            if item.started {
                continuation.push(Span::raw(" ".repeat(item.marker.width())));
                spans.push(Span::raw(" ".repeat(item.marker.width())));
            } else {
                self.last_item_marker = Some(spans.len());
                spans.push(Span::styled(item.marker.clone(), item.marker_style));
                continuation.push(Span::raw(" ".repeat(item.marker.width())));
                item.started = true;
            }
        }

        self.current = Some(LogicalLine { spans, continuation, wrap: WrapMode::Words });
    }

    fn finish_line(&mut self) {
        if let Some(line) = self.current.take() {
            self.lines.push(line);
        }
        self.last_item_marker = None;
    }

    fn separate_block(&mut self) {
        self.finish_line();
        if self.needs_gap && self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(LogicalLine {
                spans: Vec::new(),
                continuation: Vec::new(),
                wrap: WrapMode::None,
            });
        }
        self.needs_gap = false;
    }

    fn finish(mut self) -> Text<'static> {
        self.finish_line();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(
            self.lines.into_iter().flat_map(|line| wrap_line(line, self.width)).collect::<Vec<_>>(),
        )
    }
}

fn heading_index(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 0,
        HeadingLevel::H2 => 1,
        HeadingLevel::H3 => 2,
        HeadingLevel::H4 => 3,
        HeadingLevel::H5 => 4,
        HeadingLevel::H6 => 5,
    }
}

fn alert_name(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "NOTE",
        BlockQuoteKind::Tip => "TIP",
        BlockQuoteKind::Important => "IMPORTANT",
        BlockQuoteKind::Warning => "WARNING",
        BlockQuoteKind::Caution => "CAUTION",
    }
}

fn superscript_reference(label: &str) -> String {
    if label.chars().all(|character| character.is_ascii_digit()) {
        label
            .chars()
            .map(|character| match character {
                '0' => '⁰',
                '1' => '¹',
                '2' => '²',
                '3' => '³',
                '4' => '⁴',
                '5' => '⁵',
                '6' => '⁶',
                '7' => '⁷',
                '8' => '⁸',
                '9' => '⁹',
                _ => unreachable!(),
            })
            .collect()
    } else {
        format!("⁽{label}⁾")
    }
}

fn highlight_code(
    lines: Vec<&str>,
    language: &str,
    fallback: Style,
    palette: TuiTheme,
) -> Vec<Vec<Span<'static>>> {
    let syntax = SYNTAXES
        .find_syntax_by_token(language)
        .or_else(|| SYNTAXES.find_syntax_by_extension(language));
    let Some(syntax) = syntax else {
        return lines
            .into_iter()
            .map(|line| vec![Span::styled(line.to_owned(), fallback)])
            .collect();
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
                            Span::styled(text.to_owned(), syntect_style(style, palette))
                        })
                        .collect()
                })
                .unwrap_or_else(|_| vec![Span::styled(line.to_owned(), fallback)])
        })
        .collect()
}

fn syntect_style(style: syntect::highlighting::Style, palette: TuiTheme) -> Style {
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

fn render_table(
    lines: &mut Vec<LogicalLine>,
    table: TableState,
    width: usize,
    theme: &MarkdownTheme,
) {
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return;
    }

    if width < columns.saturating_mul(4).saturating_add(1) {
        for row in table.rows {
            let mut spans = Vec::new();
            for (index, cell) in row.into_iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" · ", theme.table_border));
                }
                spans.extend(cell);
            }
            lines.push(LogicalLine { spans, continuation: Vec::new(), wrap: WrapMode::Words });
        }
        return;
    }

    let mut widths = vec![1usize; columns];
    for row in &table.rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(spans_width(cell).min(width / 2).max(1));
        }
    }
    let available = width.saturating_sub(columns * 3 + 1).max(columns);
    while widths.iter().sum::<usize>() > available {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, value)| **value > 1)
            .max_by_key(|(_, value)| **value)
        else {
            break;
        };
        widths[index] -= 1;
    }

    lines.push(table_border('┌', '┬', '┐', &widths, theme.table_border));
    for (row_index, row) in table.rows.into_iter().enumerate() {
        let mut spans = vec![Span::styled("│", theme.table_border)];
        for (column, column_width) in widths.iter().copied().enumerate().take(columns) {
            let alignment =
                table.alignments.get(column).copied().unwrap_or(MarkdownAlignment::None);
            let cell = row.get(column).cloned().unwrap_or_default();
            let cell_style =
                if row_index < table.heading_rows { theme.table_heading } else { Style::default() };
            spans.push(Span::raw(" "));
            spans.extend(fit_cell(cell, column_width, alignment, cell_style));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("│", theme.table_border));
        }
        lines.push(LogicalLine { spans, continuation: Vec::new(), wrap: WrapMode::None });
        if row_index + 1 == table.heading_rows {
            lines.push(table_border('├', '┼', '┤', &widths, theme.table_border));
        }
    }
    lines.push(table_border('└', '┴', '┘', &widths, theme.table_border));
}

fn table_border(
    left: char,
    junction: char,
    right: char,
    widths: &[usize],
    style: Style,
) -> LogicalLine {
    let mut border = String::new();
    border.push(left);
    for (index, width) in widths.iter().enumerate() {
        border.push_str(&"─".repeat(width + 2));
        border.push(if index + 1 == widths.len() { right } else { junction });
    }
    LogicalLine {
        spans: vec![Span::styled(border, style)],
        continuation: Vec::new(),
        wrap: WrapMode::None,
    }
}

fn fit_cell(
    spans: Vec<Span<'static>>,
    width: usize,
    alignment: MarkdownAlignment,
    base_style: Style,
) -> Vec<Span<'static>> {
    let original_width = spans_width(&spans);
    let truncated = original_width > width;
    let content_width = if truncated { width.saturating_sub(1) } else { original_width };
    let remaining = width.saturating_sub(if truncated { content_width + 1 } else { content_width });
    let (left, right) = match alignment {
        MarkdownAlignment::Center => (remaining / 2, remaining - remaining / 2),
        MarkdownAlignment::Right => (remaining, 0),
        MarkdownAlignment::None | MarkdownAlignment::Left => (0, remaining),
    };

    let mut output = vec![Span::raw(" ".repeat(left))];
    let mut used = 0;
    'spans: for span in spans {
        let style = base_style.patch(span.style);
        let mut segment = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if used + character_width > content_width {
                if !segment.is_empty() {
                    output.push(Span::styled(segment, style));
                }
                break 'spans;
            }
            segment.push(character);
            used += character_width;
        }
        if !segment.is_empty() {
            output.push(Span::styled(segment, style));
        }
    }
    if truncated {
        output.push(Span::styled("…", base_style));
    }
    output.push(Span::raw(" ".repeat(right)));
    output
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn wrap_line(line: LogicalLine, width: usize) -> Vec<Line<'static>> {
    if line.spans.is_empty() {
        return vec![Line::default()];
    }
    if line.wrap == WrapMode::None || spans_width(&line.spans) <= width {
        return vec![Line::from(line.spans)];
    }

    let continuation_width = spans_width(&line.continuation);
    let mut output = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;

    for span in line.spans {
        let style = span.style;
        let tokens = match line.wrap {
            WrapMode::Words => word_tokens(span.content.as_ref()),
            WrapMode::Exact => {
                span.content.chars().map(|character| character.to_string()).collect()
            }
            WrapMode::None => unreachable!(),
        };
        for token in tokens {
            let token_width = token.width();
            let whitespace = token.chars().all(char::is_whitespace);
            if line.wrap == WrapMode::Words
                && !whitespace
                && current_width > continuation_width
                && current_width + token_width > width
            {
                output.push(Line::from(std::mem::take(&mut current)));
                current = line.continuation.clone();
                current_width = continuation_width;
            }
            if line.wrap == WrapMode::Words && whitespace && current_width == continuation_width {
                continue;
            }

            for character in token.chars() {
                let character_width = character.width().unwrap_or(0);
                if current_width + character_width > width && current_width > continuation_width {
                    output.push(Line::from(std::mem::take(&mut current)));
                    current = line.continuation.clone();
                    current_width = continuation_width;
                    if character.is_whitespace() && line.wrap == WrapMode::Words {
                        continue;
                    }
                }
                push_character(&mut current, character, style);
                current_width += character_width;
            }
        }
    }
    if !current.is_empty() {
        output.push(Line::from(current));
    }
    output
}

fn word_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut whitespace = None;
    for character in text.chars() {
        let is_whitespace = character.is_whitespace();
        if whitespace.is_some_and(|previous| previous != is_whitespace) {
            tokens.push(std::mem::take(&mut current));
        }
        current.push(character);
        whitespace = Some(is_whitespace);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn push_character(spans: &mut Vec<Span<'static>>, character: char, style: Style) {
    if let Some(last) = spans.last_mut().filter(|span| span.style == style) {
        last.content.to_mut().push(character);
    } else {
        spans.push(Span::styled(character.to_string(), style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str, width: u16) -> Text<'static> {
        MarkdownRenderer::default().render(source, width)
    }

    fn plain(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_presentation_without_source_delimiters() {
        let text = render(
            "# Heading\n\n**bold** *italic* ~~gone~~ and `code`.\n\n```rust\nfn main() {}\n```",
            80,
        );
        let output = plain(&text);

        assert!(output.contains("Heading"), "{output}");
        assert!(output.contains("bold italic gone and code."), "{output}");
        assert!(output.contains("fn main() {}"), "{output}");
        assert!(!output.contains("# Heading"), "{output}");
        assert!(!output.contains("**"), "{output}");
        assert!(!output.contains("```"), "{output}");
        assert!(text.lines[0].spans.iter().any(|span| {
            span.style.add_modifier.contains(Modifier::BOLD) && span.content.contains("Heading")
        }));
    }

    #[test]
    fn injected_palette_controls_markdown_and_syntax_colors() {
        let palette = TuiTheme {
            accent: ratatui::style::Color::Rgb(1, 2, 3),
            text: ratatui::style::Color::Rgb(4, 5, 6),
            ..NORD
        };
        let text =
            MarkdownRenderer::new(palette).render("## Heading\n\n```rust\nfn main() {}\n```", 80);

        assert_eq!(text.lines[0].spans[0].style.fg, Some(palette.accent));
        assert!(text.lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.fg.is_none()
                || [
                    palette.text,
                    palette.text_strong,
                    palette.border,
                    palette.text_muted,
                    palette.accent,
                    palette.accent_alt,
                    palette.info,
                    palette.focus,
                    palette.warning,
                    palette.danger,
                    palette.attention,
                    palette.success,
                    palette.special,
                ]
                .contains(&span.style.fg.expect("checked as present"))
        }));
    }

    #[test]
    fn renders_nested_ordered_unordered_and_task_lists() {
        let output = plain(&render(
            "1. first\n2. second\n   - nested\n   - [x] shipped\n   - [ ] pending",
            80,
        ));

        assert!(output.contains("1. first"), "{output}");
        assert!(output.contains("2. second"), "{output}");
        assert!(output.contains("• nested"), "{output}");
        assert!(output.contains("☑ shipped"), "{output}");
        assert!(output.contains("☐ pending"), "{output}");
        assert!(!output.contains("[x]"), "{output}");
    }

    #[test]
    fn renders_links_images_quotes_alerts_and_footnotes() {
        let source = concat!(
            "> [!NOTE]\n> Read [the guide](https://example.test).\n\n",
            "![diagram](diagram.png \"Architecture\")\n\n",
            "Fact.[^1]\n\n[^1]: Source"
        );
        let output = plain(&render(source, 80));

        assert!(output.contains("NOTE"), "{output}");
        assert!(output.contains("│ Read the guide ‹https://example.test›"), "{output}");
        assert!(output.contains("▧ diagram — Architecture ‹diagram.png›"), "{output}");
        assert!(output.contains("Fact.¹"), "{output}");
        assert!(output.contains("1. Source"), "{output}");
    }

    #[test]
    fn renders_aligned_tables_within_width() {
        let text = render(
            "| Name | State |\n| :--- | ---: |\n| renderer | complete |\n| very long value | yes |",
            32,
        );
        let output = plain(&text);

        assert!(output.contains("┌"), "{output}");
        assert!(output.contains("renderer"), "{output}");
        assert!(output.contains("└"), "{output}");
        assert!(text.lines.iter().all(|line| line.width() <= 32), "{output}");
        assert!(!output.contains("| :---"), "{output}");
    }

    #[test]
    fn wraps_words_and_preserves_hanging_list_indent() {
        let text = render("- This list item wraps onto another terminal line cleanly.", 24);
        let output = plain(&text);

        assert!(text.lines.len() >= 3, "{output}");
        assert!(output.lines().next().unwrap().starts_with("• "), "{output}");
        assert!(output.lines().skip(1).all(|line| line.starts_with("  ")), "{output}");
        assert!(text.lines.iter().all(|line| line.width() <= 24), "{output}");
    }

    #[test]
    fn renders_metadata_math_definitions_and_html_fallback() {
        let source = concat!(
            "---\ntitle: Demo\n---\n\n",
            "Term\n: Definition\n\n",
            "Math $x + y$ and <kbd>Enter</kbd>."
        );
        let output = plain(&render(source, 80));

        assert!(output.contains("title: Demo"), "{output}");
        assert!(!output.contains("---"), "{output}");
        assert!(output.contains("Term\n  Definition"), "{output}");
        assert!(output.contains("Math x + y and <kbd>Enter</kbd>."), "{output}");
    }
}
