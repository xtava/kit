//! `settings` — the shared editor for tool-owned operator preferences.

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};
use crossterm::event::Event;

use crate::framework::{Context, SettingsSection, Tool, ToolMeta};
use crate::tui::{EventReader, Session, SessionOptions, SettingsEditor, SettingsFlow};

pub fn tool(sections: Vec<SettingsSection>) -> SettingsTool {
    SettingsTool { sections }
}

pub struct SettingsTool {
    sections: Vec<SettingsSection>,
}

#[derive(Parser)]
#[command(
    name = "settings",
    about = "Edit Kit operator preferences",
    long_about = "Opens Kit's shared TUI for editing tool-owned Settings in their XDG TOML files."
)]
struct SettingsArgs {
    /// Theme name (nord or terminal) or a custom theme TOML path.
    #[arg(long, value_name = "THEME", default_value = "nord")]
    theme: String,
}

#[async_trait]
impl Tool for SettingsTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "settings",
            about: "Edit Kit operator preferences",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        SettingsArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = SettingsArgs::from_arg_matches(matches)?;
        if !cx.term.interactive() {
            bail!("kit settings requires an interactive terminal");
        }
        let (_, theme) = crate::tui::theme::resolve(&args.theme)
            .with_context(|| format!("load Settings theme {:?}", args.theme))?;
        let mut editor = SettingsEditor::open(cx.config.clone(), self.sections.clone(), theme);
        let mut session =
            Session::open(SessionOptions { mouse_capture: false, bracketed_paste: false })?;
        let mut events = EventReader::start();
        loop {
            session.draw(|frame| editor.render(frame, frame.area()))?;
            match events.recv().await {
                Some(Event::Key(key))
                    if key.is_press() && editor.on_key(key) == SettingsFlow::Exit =>
                {
                    break;
                }
                Some(Event::Resize(_, _)) => {}
                None => break,
                _ => {}
            }
        }
        Ok(())
    }
}
