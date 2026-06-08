use anyhow::Result;
use clap::{Arg, ArgAction, Command};

use super::{ConfigStore, Context, Output, OutputFormat, Terminal, Tool};

/// The set of registered tools, and the dispatcher that turns argv into one tool's `run`.
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }

    pub async fn dispatch(self) -> Result<()> {
        let mut command = Command::new("kit")
            .about("A personal toolbelt — one binary, many sharp tools.")
            .version(env!("CARGO_PKG_VERSION"))
            .arg(
                Arg::new("json")
                    .long("json")
                    .global(true)
                    .action(ArgAction::SetTrue)
                    .help("Emit JSON instead of formatted text"),
            );
        for tool in &self.tools {
            command = command.subcommand(tool.command());
        }

        let matches = command.get_matches();
        let terminal = Terminal::detect();
        let config = ConfigStore::bootstrap()?;

        match matches.subcommand() {
            Some((name, sub)) => {
                let json = matches.get_flag("json") || sub.get_flag("json");
                let format = if json { OutputFormat::Json } else { OutputFormat::Text };
                let out = Output::new(format, terminal.stdout_tty);
                let cx = Context { config, out, term: terminal };
                let tool = self
                    .tools
                    .iter()
                    .find(|tool| tool.meta().name == name)
                    .expect("clap matched a registered subcommand");
                tool.run(&cx, sub).await
            }
            None => {
                self.print_tool_list();
                Ok(())
            }
        }
    }

    fn print_tool_list(&self) {
        println!("kit — a personal toolbelt\n");
        if self.tools.is_empty() {
            println!("no tools registered");
            return;
        }
        println!("tools:");
        for tool in &self.tools {
            let meta = tool.meta();
            println!("  {:<10} {}", meta.name, meta.about);
        }
        println!("\nrun `kit <tool> --help` for details");
    }
}
