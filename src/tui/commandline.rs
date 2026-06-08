//! The slash-command line: a tool supplies its command set as a `const`, and this parses,
//! fuzzy-suggests, and tab-completes against it. Generalized from domain's original `command.rs`.

/// One slash command's spec. `name` is canonical; `aliases` also resolve to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub description: &'static str,
}

/// What a submitted input line means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedInput {
    Empty,
    /// Free text — the tool's primary action (a domain query, a search).
    Query(String),
    /// A recognized `/command`, resolved to its canonical `name` plus the trailing `args`.
    Command { name: &'static str, args: String },
    /// A `/word` that matched no command.
    Unknown(String),
}

/// A tool's command set, wrapping its `&'static [CommandSpec]`. Construct as a `const`.
#[derive(Clone, Copy)]
pub struct CommandSet {
    specs: &'static [CommandSpec],
}

impl CommandSet {
    pub const fn new(specs: &'static [CommandSpec]) -> Self {
        Self { specs }
    }

    pub fn all(&self) -> &'static [CommandSpec] {
        self.specs
    }

    pub fn parse(&self, input: &str) -> ParsedInput {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return ParsedInput::Empty;
        }

        let Some(command_text) = trimmed.strip_prefix('/') else {
            return ParsedInput::Query(trimmed.to_owned());
        };

        let (name, args) = split_command(command_text);
        let name = name.to_lowercase();

        match self.spec(&name) {
            Some(spec) => ParsedInput::Command {
                name: spec.name,
                args: args.to_owned(),
            },
            None => ParsedInput::Unknown(name),
        }
    }

    pub fn suggestions(&self, input: &str) -> Vec<&'static CommandSpec> {
        let Some(command_text) = input.trim_start().strip_prefix('/') else {
            return Vec::new();
        };
        let (prefix, _) = split_command(command_text);
        let prefix = prefix.to_lowercase();

        if prefix.is_empty() {
            return self.specs.iter().collect();
        }

        let mut matches = self
            .specs
            .iter()
            .filter_map(|spec| fuzzy_match(spec, &prefix).map(|score| (score, spec)))
            .collect::<Vec<_>>();

        if matches.iter().any(|(score, _)| *score < 100) {
            matches.retain(|(score, _)| *score < 100);
        }

        matches.sort_by_key(|(score, spec)| (*score, spec.name));
        matches.into_iter().map(|(_, spec)| spec).collect()
    }

    pub fn complete(&self, input: &str) -> Option<String> {
        let trimmed = input.trim_start();
        let command_text = trimmed.strip_prefix('/')?;
        if command_text.chars().any(char::is_whitespace) {
            return None;
        }
        if command_text.is_empty() {
            return None;
        }

        let mut matches = self
            .suggestions(trimmed)
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        matches.sort_unstable();
        matches.dedup();

        match matches.as_slice() {
            [command] => Some(format!("/{command} ")),
            _ => None,
        }
    }

    fn spec(&self, name: &str) -> Option<&'static CommandSpec> {
        self.specs
            .iter()
            .find(|spec| spec.name == name || spec.aliases.contains(&name))
    }
}

fn split_command(command_text: &str) -> (&str, &str) {
    let command_text = command_text.trim_start();
    match command_text.find(char::is_whitespace) {
        Some(index) => (command_text[..index].trim(), command_text[index..].trim()),
        None => (command_text, ""),
    }
}

fn fuzzy_match(spec: &'static CommandSpec, needle: &str) -> Option<u16> {
    std::iter::once(spec.name)
        .chain(spec.aliases.iter().copied())
        .filter_map(|candidate| fuzzy_score(candidate, needle))
        .min()
}

fn fuzzy_score(candidate: &str, needle: &str) -> Option<u16> {
    if candidate == needle {
        return Some(0);
    }
    if candidate.starts_with(needle) {
        return Some(10 + candidate.len().saturating_sub(needle.len()) as u16);
    }
    subsequence_score(candidate, needle).map(|score| 100 + score)
}

fn subsequence_score(candidate: &str, needle: &str) -> Option<u16> {
    let mut score = 0_u16;
    let mut last_match = None;
    let mut chars = candidate.char_indices();

    for needle_char in needle.chars() {
        let (index, _) = chars.find(|(_, candidate_char)| *candidate_char == needle_char)?;
        score = score.saturating_add(index as u16);

        if let Some(last_index) = last_match {
            score = score.saturating_add(index.saturating_sub(last_index + 1) as u16);
        }

        last_match = Some(index);
    }

    score = score.saturating_add(candidate.len().saturating_sub(needle.len()) as u16);
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: CommandSet = CommandSet::new(&[
        CommandSpec { name: "tlds", aliases: &[], usage: "/tlds", description: "set TLDs" },
        CommandSpec { name: "quit", aliases: &["q"], usage: "/quit", description: "exit" },
        CommandSpec { name: "help", aliases: &["?"], usage: "/help", description: "help" },
        CommandSpec { name: "clear", aliases: &[], usage: "/clear", description: "clear" },
    ]);

    #[test]
    fn parses_command_with_args() {
        assert_eq!(
            COMMANDS.parse("/tlds com,ai"),
            ParsedInput::Command { name: "tlds", args: "com,ai".to_owned() }
        );
    }

    #[test]
    fn resolves_aliases_to_canonical_name() {
        assert_eq!(
            COMMANDS.parse("/q"),
            ParsedInput::Command { name: "quit", args: String::new() }
        );
    }

    #[test]
    fn parses_unknown_and_query() {
        assert_eq!(COMMANDS.parse("/foo bar"), ParsedInput::Unknown("foo".to_owned()));
        assert_eq!(COMMANDS.parse("modkit"), ParsedInput::Query("modkit".to_owned()));
    }

    #[test]
    fn completes_and_fuzzy_matches() {
        assert_eq!(COMMANDS.complete("/t"), Some("/tlds ".to_owned()));
        assert_eq!(COMMANDS.complete("/"), None);
        assert_eq!(COMMANDS.suggestions("/ld").first().map(|spec| spec.name), Some("tlds"));
        assert_eq!(COMMANDS.suggestions("/?").first().map(|spec| spec.name), Some("help"));
        assert!(COMMANDS.suggestions("/zz").is_empty());
    }
}
