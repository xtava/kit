use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellAlignment {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellOverflow {
    Clip,
    Ellipsis,
}

pub fn terminal_text_width(value: &str) -> usize {
    value.width()
}

pub fn truncate_terminal_text(value: &str, width: usize, overflow: CellOverflow) -> String {
    if terminal_text_width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }

    let content_width = match overflow {
        CellOverflow::Clip => width,
        CellOverflow::Ellipsis => width.saturating_sub(1),
    };
    let mut used: usize = 0;
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let character_width = character.width().unwrap_or_default();
        if used.saturating_add(character_width) > content_width {
            break;
        }
        used = used.saturating_add(character_width);
        end = index + character.len_utf8();
    }
    let mut output = value[..end].to_owned();
    if overflow == CellOverflow::Ellipsis {
        output.push('…');
    }
    output
}

pub fn fit_terminal_text(
    value: &str,
    width: usize,
    alignment: CellAlignment,
    overflow: CellOverflow,
) -> String {
    let fitted = truncate_terminal_text(value, width, overflow);
    let padding = " ".repeat(width.saturating_sub(terminal_text_width(&fitted)));
    match alignment {
        CellAlignment::Left => fitted + padding.as_str(),
        CellAlignment::Right => padding + fitted.as_str(),
    }
}
