//! Shared terminal themes and user theme-file parsing.

use std::{fs, path::Path};

use anyhow::{bail, Context, Result};
use ratatui::style::Color;
use serde::Deserialize;

/// The exact Nord roles from the user's active btop theme.
///
/// Normal surfaces intentionally preserve the terminal's transparent background.
#[derive(Clone, Copy, Debug)]
pub struct TuiTheme {
    pub background: Color,
    pub surface: Color,
    pub border: Color,
    pub text: Color,
    pub text_strong: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub accent_alt: Color,
    pub info: Color,
    pub focus: Color,
    pub warning: Color,
    pub selection: Color,
    pub danger: Color,
    pub attention: Color,
    pub success: Color,
    pub special: Color,
    pub code_background: Color,
}

impl TuiTheme {
    /// Quantize a syntax color to the nearest active palette role.
    pub fn nearest_syntax_color(self, red: u8, green: u8, blue: u8) -> Color {
        let candidates = [
            self.text,
            self.text_strong,
            self.text_muted,
            self.accent,
            self.accent_alt,
            self.info,
            self.focus,
            self.warning,
            self.danger,
            self.attention,
            self.success,
            self.special,
        ];
        candidates
            .into_iter()
            .filter_map(|color| rgb(color).map(|rgb| (color, distance(rgb, (red, green, blue)))))
            .min_by_key(|(_, distance)| *distance)
            .map(|(color, _)| color)
            .unwrap_or(self.text)
    }
}

pub const NORD: TuiTheme = TuiTheme {
    background: Color::Reset,
    surface: Color::Reset,
    border: Color::Rgb(76, 86, 106),
    text: Color::Rgb(216, 222, 233),
    text_strong: Color::Rgb(236, 239, 244),
    text_muted: Color::Rgb(76, 86, 106),
    accent: Color::Rgb(143, 188, 187),
    accent_alt: Color::Rgb(136, 192, 208),
    info: Color::Rgb(129, 161, 193),
    focus: Color::Rgb(94, 129, 172),
    warning: Color::Rgb(235, 203, 139),
    selection: Color::Rgb(76, 86, 106),
    danger: Color::Rgb(191, 97, 106),
    attention: Color::Rgb(208, 135, 112),
    success: Color::Rgb(163, 190, 140),
    special: Color::Rgb(180, 142, 173),
    code_background: Color::Rgb(59, 66, 82),
};

pub const TERMINAL: TuiTheme = TuiTheme {
    background: Color::Reset,
    surface: Color::Reset,
    border: Color::DarkGray,
    text: Color::White,
    text_strong: Color::White,
    text_muted: Color::DarkGray,
    accent: Color::Cyan,
    accent_alt: Color::LightCyan,
    info: Color::LightBlue,
    focus: Color::Blue,
    warning: Color::Yellow,
    selection: Color::DarkGray,
    danger: Color::Red,
    attention: Color::LightRed,
    success: Color::Green,
    special: Color::Magenta,
    code_background: Color::DarkGray,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    #[serde(default = "default_base")]
    base: String,
    #[serde(default)]
    colors: ThemeColors,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeColors {
    background: Option<String>,
    surface: Option<String>,
    border: Option<String>,
    text: Option<String>,
    text_strong: Option<String>,
    text_muted: Option<String>,
    accent: Option<String>,
    accent_alt: Option<String>,
    info: Option<String>,
    focus: Option<String>,
    warning: Option<String>,
    selection: Option<String>,
    danger: Option<String>,
    attention: Option<String>,
    success: Option<String>,
    special: Option<String>,
    code_background: Option<String>,
}

pub fn resolve(spec: &str) -> Result<(String, TuiTheme)> {
    if let Some(theme) = built_in(spec) {
        let name = if matches!(spec.trim().to_ascii_lowercase().as_str(), "ansi" | "terminal") {
            "terminal"
        } else {
            "nord"
        };
        return Ok((name.to_owned(), theme));
    }
    let path =
        Path::new(spec).canonicalize().with_context(|| format!("resolve theme path {spec:?}"))?;
    let source = fs::read_to_string(&path)
        .with_context(|| format!("read terminal theme {}", path.display()))?;
    let theme =
        parse(&source).with_context(|| format!("parse terminal theme {}", path.display()))?;
    Ok((path.to_string_lossy().into_owned(), theme))
}

pub fn built_in(spec: &str) -> Option<TuiTheme> {
    match spec.trim().to_ascii_lowercase().as_str() {
        "nord" => Some(NORD),
        "terminal" | "ansi" => Some(TERMINAL),
        _ => None,
    }
}

pub fn next_built_in(spec: &str) -> &'static str {
    match spec.trim().to_ascii_lowercase().as_str() {
        "nord" => "terminal",
        _ => "nord",
    }
}

fn parse(source: &str) -> Result<TuiTheme> {
    let file: ThemeFile = toml::from_str(source)?;
    let mut theme = built_in(&file.base)
        .with_context(|| format!("unknown base theme {:?}; use nord or terminal", file.base))?;
    let colors = file.colors;

    macro_rules! apply {
        ($field:ident) => {
            if let Some(value) = colors.$field {
                theme.$field = parse_color(&value)
                    .with_context(|| format!("invalid color for {}", stringify!($field)))?;
            }
        };
    }

    apply!(background);
    apply!(surface);
    apply!(border);
    apply!(text);
    apply!(text_strong);
    apply!(text_muted);
    apply!(accent);
    apply!(accent_alt);
    apply!(info);
    apply!(focus);
    apply!(warning);
    apply!(selection);
    apply!(danger);
    apply!(attention);
    apply!(success);
    apply!(special);
    apply!(code_background);
    Ok(theme)
}

fn parse_color(value: &str) -> Result<Color> {
    let normalized = value.trim().to_ascii_lowercase();
    let named = match normalized.as_str() {
        "reset" | "none" | "transparent" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "dark-gray" | "dark-grey" => Some(Color::DarkGray),
        "white" => Some(Color::White),
        _ => None,
    };
    if let Some(color) = named {
        return Ok(color);
    }

    let hex = normalized.strip_prefix('#').unwrap_or(&normalized);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("expected #RRGGBB or a named terminal color, got {value:?}");
    }
    let red = u8::from_str_radix(&hex[0..2], 16)?;
    let green = u8::from_str_radix(&hex[2..4], 16)?;
    let blue = u8::from_str_radix(&hex[4..6], 16)?;
    Ok(Color::Rgb(red, green, blue))
}

fn default_base() -> String {
    "nord".to_owned()
}

fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(red, green, blue) => Some((red, green, blue)),
        _ => None,
    }
}

fn distance(left: (u8, u8, u8), right: (u8, u8, u8)) -> u32 {
    let red = left.0 as i32 - right.0 as i32;
    let green = left.1 as i32 - right.1 as i32;
    let blue = left.2 as i32 - right.2 as i32;
    (red * red + green * green + blue * blue) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_colors_are_quantized_to_active_palette() {
        assert_eq!(NORD.nearest_syntax_color(190, 96, 105), NORD.danger);
        assert_eq!(NORD.nearest_syntax_color(235, 202, 140), NORD.warning);
        assert_eq!(NORD.nearest_syntax_color(215, 221, 232), NORD.text);
    }

    #[test]
    fn custom_theme_inherits_nord_and_overrides_selected_roles() -> Result<()> {
        let theme = parse(
            r##"
                base = "nord"

                [colors]
                accent = "#010203"
                background = "transparent"
            "##,
        )?;
        assert_eq!(theme.accent, Color::Rgb(1, 2, 3));
        assert_eq!(theme.background, Color::Reset);
        assert_eq!(theme.text, NORD.text);
        Ok(())
    }

    #[test]
    fn invalid_theme_colors_fail_loudly() {
        let error = parse("[colors]\naccent = 'oops'").unwrap_err();
        assert!(error.to_string().contains("invalid color for accent"));
    }

    #[test]
    fn unknown_theme_roles_fail_loudly() {
        let error = parse("[colors]\nacccent = '#010203'").unwrap_err();
        assert!(error.to_string().contains("unknown field `acccent`"));
    }
}
