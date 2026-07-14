//! Completion for the interactive prompt, in two layers.
//!
//! **Slot resolution** walks the same clap grammar that parses the line and answers "what does
//! the word under the cursor mean?" — a command word, a flag name, a flag's value, or the Nth
//! positional of the resolved command. Because clap argument ids are semantic names (`locator`,
//! `key`, `mark`, `expr`), the slot maps to a [`ValueKind`] by id — one match arm per meaning,
//! and a new command gets completion by naming its fields well.
//!
//! **A [`ValueKind`]** is the single vocabulary both surfaces speak: it yields candidates for
//! the Tab popup (live element refs, flow and lens names, key chords, flag vocabularies) and a
//! placeholder for the inline ghost hint, so the prompt always shows what the next word means
//! and whether Tab has something to offer.

use clap::CommandFactory;

use crate::cdp::TrackKind;
use crate::tui::{fuzzy, Suggestion};

use super::protocol::NAMED_KEYS;

/// A completion result: candidates for the word starting at byte `start` of the line.
#[derive(Debug)]
pub struct Completion {
    pub start: usize,
    pub candidates: Vec<Suggestion>,
}

/// The inline ghost rendered after the cursor.
#[derive(Debug, PartialEq)]
pub struct Ghost {
    pub text: String,
    /// Literal completion text the Right arrow accepts; `false` is an informational placeholder.
    pub acceptable: bool,
}

/// Live values beyond the static grammar. All optional — an empty context still completes the
/// grammar itself.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub flows: Vec<String>,
    pub lenses: Vec<String>,
    pub targets: Vec<String>,
    pub refs: Vec<ElementRef>,
}

/// An element from the daemon's last accessibility snapshot, offered at locator positions.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ElementRef {
    pub reference: String,
    pub role: String,
    pub name: String,
}

/// What a slot accepts — every argument position and value-taking flag resolves to one of these.
/// Each kind knows its candidates and how its ghost placeholder reads.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ValueKind {
    /// The first word: session commands and meta words.
    Command,
    /// The next word picks a subcommand of the resolved command.
    Subcommand,
    /// An element to interact with: live refs as `@eN` and quoted `role:name`.
    Locator,
    Flow,
    Lens,
    Key,
    Mark,
    Track,
    Source,
    Status,
    Target,
    /// A screenshot encoding: png, jpeg, or webp.
    Format,
    /// Free-form input with no candidates — the ghost still names what belongs here.
    Free(&'static str),
}

impl ValueKind {
    fn candidates(self, line: &Line<'_>, ctx: &Context) -> Vec<Suggestion> {
        match self {
            Self::Command => first_words(),
            Self::Subcommand => subcommands(line.node),
            Self::Locator => ref_candidates(ctx),
            Self::Flow => named(&ctx.flows),
            Self::Lens => named(&ctx.lenses),
            Self::Target => named(&ctx.targets),
            Self::Key => plain(NAMED_KEYS),
            Self::Mark => plain(&["last-action", "do-start", "launch"]),
            Self::Source => plain(&["main", "renderer"]),
            Self::Status => plain(&["2xx", "3xx", "4xx", "5xx", "ok", "fail"]),
            Self::Format => plain(&["png", "jpeg", "webp"]),
            Self::Track => TrackKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .chain(["watch"])
                .map(|name| Suggestion { insert: name.to_owned(), hint: String::new() })
                .collect(),
            Self::Free(_) => Vec::new(),
        }
    }

    /// The ghost shown when the slot is still empty: what belongs here, and whether Tab has a
    /// list to offer.
    fn placeholder(self, line: &Line<'_>, ctx: &Context) -> String {
        let listed = |name: &str, count: usize| match count {
            0 => format!("‹{name}›"),
            _ => format!("‹{name} · ⇥ {count}›"),
        };
        match self {
            Self::Command => "‹command · ⇥ list›".to_owned(),
            Self::Subcommand => {
                let names: Vec<&str> = subcommand_names(line.node);
                if names.len() <= 4 {
                    format!("‹{}›", names.join(" · "))
                } else {
                    listed("subcommand", names.len())
                }
            }
            Self::Locator => listed("element", ctx.refs.len()),
            Self::Flow => listed("flow", ctx.flows.len()),
            Self::Lens => listed("lens", ctx.lenses.len()),
            Self::Target => listed("target", ctx.targets.len()),
            Self::Key => listed("key", NAMED_KEYS.len()),
            Self::Mark => "‹mark · ⇥ list›".to_owned(),
            Self::Track => "‹track · ⇥ list›".to_owned(),
            Self::Source => "‹main · renderer›".to_owned(),
            Self::Status => "‹2xx · 404 · ok · fail›".to_owned(),
            Self::Format => "‹png · jpeg · webp›".to_owned(),
            Self::Free(name) => format!("‹{name}›"),
        }
    }
}

/// The proper case for each thing: a slot's clap id (qualified by the command that owns it when
/// one id means different things in different commands) names its meaning. Extending completion
/// to a new argument is one arm here — or zero, when an existing id fits.
fn kind_for(owner: &str, id: &str) -> ValueKind {
    match (owner, id) {
        (_, "locator") => ValueKind::Locator,
        (_, "key") => ValueKind::Key,
        (_, "mark" | "since_mark") => ValueKind::Mark,
        (_, "since") => ValueKind::Free("window · 10s 5m"),
        (_, "track") => ValueKind::Track,
        (_, "source") => ValueKind::Source,
        (_, "status") => ValueKind::Status,
        (_, "target") => ValueKind::Target,
        (_, "format") => ValueKind::Format,
        (_, "out") => ValueKind::Free("file path · /tmp/shot.png"),
        (_, "quality") => ValueKind::Free("0-100"),
        ("run" | "show", "name") => ValueKind::Flow,
        ("lens", "name") => ValueKind::Lens,
        ("rm", "name") => ValueKind::Free("watch/trace name"),
        ("add", "name") => ValueKind::Free("watch/trace name"),
        ("mark", "name") => ValueKind::Free("new mark name"),
        (_, "location") => ValueKind::Free("file.js:line"),
        (_, "path") => ValueKind::Free("object.path · app.api.save"),
        (_, "when") => ValueKind::Free("js condition"),
        (_, "rate") => ValueKind::Free("hits/sec cap"),
        (_, "expr") => ValueKind::Free("js expr"),
        (_, "needle") => ValueKind::Free("text to find"),
        ("find", "text") => ValueKind::Free("literal to search parsed scripts for"),
        (_, "text") => ValueKind::Free("text"),
        (_, "option") => ValueKind::Free("option label"),
        (_, "pattern") => ValueKind::Free("url substring"),
        (_, "steps") => ValueKind::Free("step; step; …"),
        (_, "params") => ValueKind::Free("key=value"),
        (_, "idle" | "timeout" | "within" | "interval") => ValueKind::Free("duration · 500ms 5s"),
        (_, "equals") => ValueKind::Free("expected value"),
        (_, "contains") => ValueKind::Free("expected text"),
        (_, "grep") => ValueKind::Free("text filter"),
        (_, "limit") => ValueKind::Free("row count"),
        (_, "request_id") => ValueKind::Free("request id"),
        (_, "extension") | (_, "extension_id") => ValueKind::Free("extension id"),
        _ => ValueKind::Free(""),
    }
}

/// Where the cursor sits, resolved against the grammar.
struct Slot<'a> {
    line: Line<'a>,
    kind: SlotKind,
}

enum SlotKind {
    Value(ValueKind),
    /// The word starts with `-`: completing a flag name of the resolved command.
    FlagName,
}

/// The parsed shape of everything before the word under the cursor.
struct Line<'a> {
    /// Deepest command the typed words entered; the grammar root before any command word.
    node: &'a clap::Command,
    /// Tokens after the command path, in order.
    rest: Vec<String>,
}

/// Complete the word under the cursor. `None` means nothing sensible to offer.
pub fn complete(text: &str, ctx: &Context) -> Option<Completion> {
    let (start, word) = current_word(text);
    let slot = resolve_slot(&text[..start], word)?;
    let pool = match slot.kind {
        SlotKind::FlagName => flag_candidates(slot.line.node)?,
        SlotKind::Value(kind) => {
            let pool = kind.candidates(&slot.line, ctx);
            if pool.is_empty() {
                return None;
            }
            pool
        }
    };
    let ranked = rank(pool, word.trim_start_matches(['\'', '"']));
    (!ranked.is_empty()).then_some(Completion { start, candidates: ranked })
}

/// The inline ghost for the current line: the top completion's remainder when the typed word
/// prefixes one (acceptable), otherwise the slot's placeholder (informational).
pub fn ghost(text: &str, ctx: &Context) -> Option<Ghost> {
    if text.is_empty() {
        return None;
    }
    let (start, word) = current_word(text);
    let slot = resolve_slot(&text[..start], word)?;

    if word.is_empty() {
        if let SlotKind::Value(kind) = slot.kind {
            let placeholder = kind.placeholder(&slot.line, ctx);
            return (!placeholder.is_empty() && placeholder != "‹›")
                .then_some(Ghost { text: placeholder, acceptable: false });
        }
        return None;
    }

    // A ghost must literally extend what's typed — fuzzy matches belong in the popup.
    let completion = complete(text, ctx)?;
    let needle = word.trim_start_matches(['\'', '"']);
    let top =
        completion.candidates.iter().find(|candidate| starts_with_ci(&candidate.insert, needle))?;
    let remainder = &top.insert[needle.len()..];
    (!remainder.is_empty()).then_some(Ghost { text: remainder.to_owned(), acceptable: true })
}

fn starts_with_ci(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Resolve what the word under the cursor means: walk the command path, then classify the
/// position as a flag name, a flag's value, or the command's Nth positional.
fn resolve_slot<'a>(prior_text: &str, word: &str) -> Option<Slot<'a>> {
    let prior = shell_words::split(prior_text).ok()?;
    let (node, rest) = descend(&prior);
    let line = Line { node, rest };

    if line.node.get_name() == "cdp" {
        // Still at the root: an empty prior means the command word is being typed; leftover
        // tokens mean an unknown first word — nothing to anchor on.
        let typing_command = line.rest.is_empty();
        return typing_command.then_some(Slot { line, kind: SlotKind::Value(ValueKind::Command) });
    }
    if word.starts_with('-') {
        return Some(Slot { line, kind: SlotKind::FlagName });
    }

    // A value-taking flag immediately before the cursor claims this word.
    if let Some(flag) = line.rest.last().and_then(|token| token.strip_prefix("--")) {
        if let Some(arg) = value_flag(line.node, flag) {
            let kind = kind_for(line.node.get_name(), arg.get_id().as_str());
            return Some(Slot { line, kind: SlotKind::Value(kind) });
        }
    }

    // Otherwise this is the Nth positional: count the prior positionals, skipping flags and
    // the values they consumed.
    let mut index = 0;
    let mut tokens = line.rest.iter().peekable();
    while let Some(token) = tokens.next() {
        match token.strip_prefix("--") {
            Some(flag) => {
                if value_flag(line.node, flag).is_some() {
                    tokens.next();
                }
            }
            None => index += 1,
        }
    }

    if line.node.has_subcommands() {
        // Position 0 of a command with subcommands picks the subcommand (descend() already moved
        // past any typed one, so reaching here means it hasn't been typed yet).
        return Some(Slot { line, kind: SlotKind::Value(ValueKind::Subcommand) });
    }
    let positionals: Vec<&clap::Arg> = line.node.get_positionals().collect();
    // Past the declared positionals, a trailing multi-value positional keeps absorbing words.
    let arg = positionals.get(index).copied().or_else(|| {
        positionals.last().copied().filter(|arg| {
            index >= positionals.len()
                && arg.get_num_args().is_some_and(|range| range.max_values() > 1)
        })
    })?;
    let kind = kind_for(line.node.get_name(), arg.get_id().as_str());
    Some(Slot { line, kind: SlotKind::Value(kind) })
}

/// Follow typed words down the subcommand tree; returns the deepest node and the tokens after it.
fn descend(prior: &[String]) -> (&'static clap::Command, Vec<String>) {
    let mut node = grammar();
    let mut at = 0;
    for (index, word) in prior.iter().enumerate() {
        match node.get_subcommands().find(|cmd| cmd.get_name() == word) {
            Some(next) => {
                node = next;
                at = index + 1;
            }
            None => break,
        }
    }
    (node, prior[at..].to_vec())
}

/// The flag named `long` on `node`, if it takes a value.
fn value_flag<'a>(node: &'a clap::Command, long: &str) -> Option<&'a clap::Arg> {
    node.get_arguments().find(|arg| {
        arg.get_long() == Some(long)
            && !matches!(
                arg.get_action(),
                clap::ArgAction::SetTrue | clap::ArgAction::SetFalse | clap::ArgAction::Count
            )
    })
}

/// Rank a candidate pool against the typed word. Fuzzy scores are lower-is-better; a match on
/// the insert text always outranks a match that only hit the hint — hints exist so an element
/// can be found by its name, not to shadow the words being typed. A leading `@` is the element
/// operator, not part of the name: it is stripped before hint matching, and a bare `@` therefore
/// narrows to the `@ref` inserts alone.
fn rank(pool: Vec<Suggestion>, needle: &str) -> Vec<Suggestion> {
    let by_name = needle.trim_start_matches('@');
    let mut ranked: Vec<(u32, Suggestion)> = pool
        .into_iter()
        .filter_map(|candidate| {
            if needle.is_empty() {
                return Some((0, candidate));
            }
            match fuzzy::score_ci(&candidate.insert, needle) {
                Some(score) => Some((u32::from(score), candidate)),
                None if by_name.is_empty() => None,
                None => fuzzy::score_ci(&candidate.hint, by_name)
                    .map(|score| (10_000 + u32::from(score), candidate)),
            }
        })
        .collect();
    if !needle.is_empty() {
        ranked.sort_by_key(|(score, _)| *score);
    }
    ranked.into_iter().map(|(_, candidate)| candidate).collect()
}

/// The trailing word being typed: its byte offset and text. A line ending in whitespace starts a
/// fresh empty word.
fn current_word(line: &str) -> (usize, &str) {
    match line.rfind(char::is_whitespace) {
        Some(at) => (at + 1, &line[at + 1..]),
        None => (0, line),
    }
}

/// Subcommands the interactive session refuses — completing them would advertise dead ends.
const NOT_IN_SESSION: &[&str] = &[
    "launch",
    "launch-electron",
    "launched",
    "close",
    "attach",
    "detach",
    "ls",
    "gc",
    "bundle",
    "profile",
];

/// Interactive-only meta words, offered alongside the session grammar at the first position.
const META: &[(&str, &str)] = &[
    ("target", "focus a target (bare opens the picker; `target main` clears)"),
    ("track", "filter the live pane by track, e.g. `track net,ws` / `track all`"),
    ("source", "filter the live pane by side: main / renderer / all"),
    ("clear", "clear the feed"),
    ("help", "the key and command reference"),
    ("quit", "leave the session"),
];

fn first_words() -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = grammar()
        .get_subcommands()
        .filter(|cmd| !cmd.is_hide_set() && !NOT_IN_SESSION.contains(&cmd.get_name()))
        .map(|cmd| Suggestion { insert: cmd.get_name().to_owned(), hint: about(cmd) })
        .collect();
    out.extend(
        META.iter().map(|(word, hint)| Suggestion {
            insert: (*word).to_owned(),
            hint: (*hint).to_owned(),
        }),
    );
    out
}

/// The clap grammar, built once — completion walks the same tree parsing walks.
fn grammar() -> &'static clap::Command {
    use std::sync::OnceLock;
    static GRAMMAR: OnceLock<clap::Command> = OnceLock::new();
    GRAMMAR.get_or_init(super::CdpArgs::command)
}

fn subcommands(node: &clap::Command) -> Vec<Suggestion> {
    node.get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| Suggestion { insert: sub.get_name().to_owned(), hint: about(sub) })
        .collect()
}

fn subcommand_names(node: &clap::Command) -> Vec<&str> {
    node.get_subcommands().filter(|sub| !sub.is_hide_set()).map(|sub| sub.get_name()).collect()
}

fn flag_candidates(node: &clap::Command) -> Option<Vec<Suggestion>> {
    let flags: Vec<Suggestion> = node
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter_map(|arg| {
            let long = arg.get_long()?;
            Some(Suggestion {
                insert: format!("--{long}"),
                hint: arg.get_help().map(ToString::to_string).unwrap_or_default(),
            })
        })
        .collect();
    (!flags.is_empty()).then_some(flags)
}

/// Each element twice: the short `@ref` and the navigation-proof quoted `role:name` — fuzzy
/// filtering narrows to whichever form the user started typing.
fn ref_candidates(ctx: &Context) -> Vec<Suggestion> {
    let mut out = Vec::with_capacity(ctx.refs.len() * 2);
    for element in &ctx.refs {
        let identity = format!("{} \"{}\"", element.role, element.name.trim());
        out.push(Suggestion { insert: format!("@{}", element.reference), hint: identity });
        if !element.name.trim().is_empty() {
            out.push(Suggestion {
                insert: quote(&format!("{}:{}", element.role, element.name.trim())),
                hint: format!("@{}", element.reference),
            });
        }
    }
    out
}

fn named(names: &[String]) -> Vec<Suggestion> {
    names.iter().map(|name| Suggestion { insert: name.clone(), hint: String::new() }).collect()
}

fn plain(values: &[&str]) -> Vec<Suggestion> {
    values.iter().map(|v| Suggestion { insert: (*v).to_owned(), hint: String::new() }).collect()
}

fn about(cmd: &clap::Command) -> String {
    cmd.get_about().map(ToString::to_string).unwrap_or_default()
}

/// Shell-quote an insert the prompt's tokenizer would otherwise split.
fn quote(text: &str) -> String {
    if text.contains(char::is_whitespace) || text.contains('\'') {
        format!("'{}'", text.replace('\'', "\\'"))
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(completion: &Completion) -> Vec<&str> {
        completion.candidates.iter().map(|c| c.insert.as_str()).collect()
    }

    fn ctx() -> Context {
        Context {
            flows: vec!["save-smoke".into()],
            lenses: vec!["workbench".into(), "extensions".into()],
            targets: vec!["workspace".into()],
            refs: vec![ElementRef {
                reference: "e23".into(),
                role: "button".into(),
                name: "Save settings".into(),
            }],
        }
    }

    /// The first word offers the session grammar plus meta words — and never the lifecycle
    /// commands the session would refuse.
    #[test]
    fn first_word_offers_session_and_meta_but_not_lifecycle() {
        let all = complete("", &Context::default()).unwrap();
        let listed = names(&all);
        for expected in ["verify", "do", "watch", "expect", "click", "flow", "target", "help"] {
            assert!(listed.contains(&expected), "missing {expected}: {listed:?}");
        }
        for excluded in ["launch", "attach", "detach", "gc", "__serve"] {
            assert!(!listed.contains(&excluded), "{excluded} should not be offered");
        }
    }

    #[test]
    fn typing_narrows_by_fuzzy_match_with_insert_priority() {
        let some = complete("ver", &Context::default()).unwrap();
        assert_eq!(names(&some)[0], "verify");
        assert_eq!(some.start, 0);
    }

    /// Slots resolve through the grammar: positionals by index, flag values by the flag's id,
    /// and flags consumed with their values don't shift positional counting.
    #[test]
    fn slots_resolve_positionals_flags_and_flag_values() {
        let locators = complete("click ", &ctx()).unwrap();
        assert!(names(&locators).contains(&"@e23"), "{:?}", names(&locators));
        assert!(names(&locators).contains(&"'button:Save settings'"));

        // A consumed flag+value pair doesn't shift the positional index.
        let still_locator = complete("click --target workspace ", &ctx()).unwrap();
        assert!(names(&still_locator).contains(&"@e23"));

        let flag_value = complete("click --target ", &ctx()).unwrap();
        assert_eq!(names(&flag_value), vec!["workspace"]);

        let flags = complete("click --", &ctx()).unwrap();
        for expected in ["--no-settle", "--idle", "--timeout", "--target"] {
            assert!(names(&flags).contains(&expected), "{expected}: {:?}", names(&flags));
        }
    }

    #[test]
    fn subcommand_slots_and_their_arguments() {
        let subs = complete("expect ", &Context::default()).unwrap();
        for expected in ["text", "eval", "net", "no-errors"] {
            assert!(names(&subs).contains(&expected), "{expected}: {:?}", names(&subs));
        }
        let status = complete("expect net /api --status ", &Context::default()).unwrap();
        assert!(names(&status).contains(&"2xx"));
        let flows = complete("flow run ", &ctx()).unwrap();
        assert_eq!(names(&flows), vec!["save-smoke"]);
        let lenses = complete("lens ", &ctx()).unwrap();
        assert!(names(&lenses).contains(&"workbench"));
        let keys = complete("press ", &Context::default()).unwrap();
        assert!(names(&keys).contains(&"Enter"));
        let tracks = complete("tail --track ", &Context::default()).unwrap();
        assert!(names(&tracks).contains(&"watch"));
        let marks = complete("after ", &Context::default()).unwrap();
        assert!(names(&marks).contains(&"last-action"));
    }

    /// The ghost: a typed prefix shows the top candidate's remainder (acceptable); an empty slot
    /// shows what belongs there (informational); free slots still name themselves.
    #[test]
    fn ghosts_extend_prefixes_and_name_empty_slots() {
        let typed = ghost("ver", &Context::default()).unwrap();
        assert_eq!(typed, Ghost { text: "ify".into(), acceptable: true });

        let slot = ghost("click ", &ctx()).unwrap();
        assert!(!slot.acceptable);
        assert!(slot.text.contains("element") && slot.text.contains('1'), "{}", slot.text);

        let subs = ghost("expect ", &Context::default()).unwrap();
        assert!(subs.text.contains("text") && subs.text.contains("no-errors"), "{}", subs.text);

        let free = ghost("wait ", &Context::default()).unwrap();
        assert_eq!(free, Ghost { text: "‹js expr›".into(), acceptable: false });

        // Fuzzy-but-not-prefix matches must not ghost — they'd render as garbage mid-word.
        assert!(ghost("click sv", &ctx()).is_none());
        // A fully typed word has no remainder to show.
        assert!(ghost("verify", &Context::default()).is_none());
    }

    /// `@` is the ref trigger by construction: it matches only `@eN` inserts, so the menu
    /// narrows to elements; `@` plus letters keeps narrowing by element *name* via the hint.
    #[test]
    fn at_sign_narrows_to_refs_then_by_name() {
        let at = complete("click @", &ctx()).unwrap();
        assert_eq!(names(&at), vec!["@e23"], "bare @ shows refs only");
        let by_name = complete("click @sav", &ctx()).unwrap();
        assert_eq!(names(&by_name), vec!["@e23"], "name narrows through the hint");
        assert!(complete("click @zzz", &ctx()).is_none(), "no element matches");
    }

    /// Multi-value trailing positionals keep their meaning for every following word.
    #[test]
    fn trailing_multi_positionals_keep_their_kind() {
        let second_word = ghost("fill @e23 some ", &ctx());
        assert!(second_word.is_some_and(|g| g.text.contains("text")), "fill text continues");
    }

    #[test]
    fn unknown_first_words_offer_nothing() {
        assert!(complete("frobnicate ", &Context::default()).is_none());
        assert!(ghost("frobnicate ", &Context::default()).is_none());
    }
}
