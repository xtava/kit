//! Tab completion for the interactive prompt. Static candidates are derived from the same clap
//! grammar that parses the line — command words, subcommands, and flags can never go stale — and
//! value positions draw on live session data: element refs from the daemon's last snapshot, flow
//! and lens names from disk, key chords and flag vocabularies from the grammar's own tables.

use clap::CommandFactory;

use crate::cdp::TrackKind;
use crate::tui::fuzzy;

use super::protocol::NAMED_KEYS;

/// One offered completion: what replaces the word under the cursor, and a dimmed annotation
/// (a command's about-line, an element's role and name).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub insert: String,
    pub hint: String,
}

/// A completion result: candidates for the word starting at byte `start` of the line.
#[derive(Debug)]
pub struct Completion {
    pub start: usize,
    pub candidates: Vec<Candidate>,
}

/// Live values beyond the static grammar. All optional — an empty context still completes the
/// grammar itself.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub flows: Vec<String>,
    pub lenses: Vec<String>,
    pub refs: Vec<ElementRef>,
}

/// An element from the daemon's last accessibility snapshot, offered at locator positions.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ElementRef {
    pub reference: String,
    pub role: String,
    pub name: String,
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

/// Complete the word under the cursor (the line's trailing word). `None` means nothing sensible
/// to offer — the caller leaves the prompt alone.
pub fn complete(line: &str, ctx: &Context) -> Option<Completion> {
    let (start, word) = current_word(line);
    let prior = shell_words::split(&line[..start]).ok()?;

    let pool = if prior.is_empty() {
        first_words()
    } else if word.starts_with('-') {
        flag_candidates(&prior)?
    } else if let Some(values) = flag_value_candidates(&prior) {
        values
    } else {
        positional_candidates(&prior, ctx)?
    };

    let needle = word.trim_start_matches(['\'', '"']);
    // A match on the insert text always outranks a match that only hit the hint — hints exist so
    // an element can be found by its name, not to shadow the words being typed.
    let mut ranked: Vec<(u32, Candidate)> = pool
        .into_iter()
        .filter_map(|candidate| {
            if needle.is_empty() {
                return Some((0, candidate));
            }
            match fuzzy::score_ci(&candidate.insert, needle) {
                Some(score) => Some((u32::from(score) + 10_000, candidate)),
                None => fuzzy::score_ci(&candidate.hint, needle)
                    .map(|score| (u32::from(score), candidate)),
            }
        })
        .collect();
    if ranked.is_empty() {
        return None;
    }
    if !needle.is_empty() {
        ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    }
    Some(Completion { start, candidates: ranked.into_iter().map(|(_, c)| c).collect() })
}

/// The trailing word being typed: its byte offset and text. A line ending in whitespace starts a
/// fresh empty word.
fn current_word(line: &str) -> (usize, &str) {
    match line.rfind(char::is_whitespace) {
        Some(at) => (at + 1, &line[at + 1..]),
        None => (0, line),
    }
}

fn first_words() -> Vec<Candidate> {
    let mut out: Vec<Candidate> = session_commands()
        .map(|cmd| Candidate { insert: cmd.get_name().to_owned(), hint: about(cmd) })
        .collect();
    out.extend(
        META.iter()
            .map(|(word, hint)| Candidate { insert: (*word).to_owned(), hint: (*hint).to_owned() }),
    );
    out
}

fn session_commands() -> impl Iterator<Item = &'static clap::Command> {
    grammar()
        .get_subcommands()
        .filter(|cmd| !cmd.is_hide_set() && !NOT_IN_SESSION.contains(&cmd.get_name()))
}

/// The clap grammar, built once — completion walks the same tree parsing walks.
fn grammar() -> &'static clap::Command {
    use std::sync::OnceLock;
    static GRAMMAR: OnceLock<clap::Command> = OnceLock::new();
    GRAMMAR.get_or_init(super::CdpArgs::command)
}

/// Resolve the deepest subcommand the typed words have entered.
fn resolve<'a>(prior: &[String]) -> Option<&'a clap::Command> {
    let mut node = grammar();
    let mut entered = false;
    for word in prior {
        match node.get_subcommands().find(|cmd| cmd.get_name() == word) {
            Some(next) => {
                node = next;
                entered = true;
            }
            None => break,
        }
    }
    entered.then_some(node)
}

fn flag_candidates(prior: &[String]) -> Option<Vec<Candidate>> {
    let node = resolve(prior)?;
    let flags: Vec<Candidate> = node
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter_map(|arg| {
            let long = arg.get_long()?;
            Some(Candidate {
                insert: format!("--{long}"),
                hint: arg.get_help().map(ToString::to_string).unwrap_or_default(),
            })
        })
        .collect();
    (!flags.is_empty()).then_some(flags)
}

/// Values for a flag the previous token named — vocabularies the grammar itself implies.
fn flag_value_candidates(prior: &[String]) -> Option<Vec<Candidate>> {
    let flag = prior.last()?.strip_prefix("--")?;
    let plain = |values: &[&str]| {
        Some(
            values
                .iter()
                .map(|v| Candidate { insert: (*v).to_owned(), hint: String::new() })
                .collect(),
        )
    };
    match flag {
        "source" => plain(&["main", "renderer"]),
        "track" => Some(
            TrackKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .chain(["watch"])
                .map(|name| Candidate { insert: name.to_owned(), hint: String::new() })
                .collect(),
        ),
        "status" => plain(&["2xx", "3xx", "4xx", "5xx", "ok", "fail"]),
        "since-mark" => plain(&["last-action", "do-start", "launch"]),
        _ => None,
    }
}

/// Candidates for a positional value, decided by which command (and subcommand) the line is in.
fn positional_candidates(prior: &[String], ctx: &Context) -> Option<Vec<Candidate>> {
    let mut words = prior.iter().map(String::as_str).filter(|word| !word.starts_with('-'));
    let command = words.next()?;
    let sub = words.next();
    match (command, sub) {
        ("click" | "fill" | "select", None) => Some(ref_candidates(ctx)),
        ("flow", None) => Some(subcommand_candidates("flow")),
        ("flow", Some("run" | "show")) => Some(named(&ctx.flows)),
        ("lens", None) => Some(named(&ctx.lenses)),
        ("press", None) => Some(
            NAMED_KEYS
                .iter()
                .map(|key| Candidate { insert: (*key).to_owned(), hint: String::new() })
                .collect(),
        ),
        ("expect" | "watch" | "net" | "ext", None) => Some(subcommand_candidates(command)),
        _ => None,
    }
}

fn subcommand_candidates(name: &str) -> Vec<Candidate> {
    grammar()
        .get_subcommands()
        .find(|cmd| cmd.get_name() == name)
        .map(|cmd| {
            cmd.get_subcommands()
                .filter(|sub| !sub.is_hide_set())
                .map(|sub| Candidate { insert: sub.get_name().to_owned(), hint: about(sub) })
                .collect()
        })
        .unwrap_or_default()
}

/// Each element twice: the short `@ref` and the navigation-proof quoted `role:name` — fuzzy
/// filtering narrows to whichever form the user started typing.
fn ref_candidates(ctx: &Context) -> Vec<Candidate> {
    let mut out = Vec::with_capacity(ctx.refs.len() * 2);
    for element in &ctx.refs {
        let identity = format!("{} \"{}\"", element.role, element.name.trim());
        out.push(Candidate { insert: format!("@{}", element.reference), hint: identity });
        if !element.name.trim().is_empty() {
            out.push(Candidate {
                insert: quote(&format!("{}:{}", element.role, element.name.trim())),
                hint: format!("@{}", element.reference),
            });
        }
    }
    out
}

fn named(names: &[String]) -> Vec<Candidate> {
    names.iter().map(|name| Candidate { insert: name.clone(), hint: String::new() }).collect()
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
    fn typing_narrows_by_fuzzy_match() {
        let some = complete("ver", &Context::default()).unwrap();
        assert_eq!(names(&some)[0], "verify");
        assert_eq!(some.start, 0);
    }

    #[test]
    fn subcommands_and_flags_come_from_the_grammar() {
        let subs = complete("expect ", &Context::default()).unwrap();
        for expected in ["text", "eval", "net", "no-errors"] {
            assert!(names(&subs).contains(&expected), "{expected} missing: {:?}", names(&subs));
        }
        let flags = complete("click --", &Context::default()).unwrap();
        for expected in ["--no-settle", "--idle", "--timeout", "--target"] {
            assert!(names(&flags).contains(&expected), "{expected} missing: {:?}", names(&flags));
        }
    }

    /// Locator positions offer live elements in both spellings, quoted when names carry spaces.
    #[test]
    fn locator_positions_offer_live_refs() {
        let ctx = Context {
            refs: vec![ElementRef {
                reference: "e23".into(),
                role: "button".into(),
                name: "Save settings".into(),
            }],
            ..Context::default()
        };
        let offered = complete("click ", &ctx).unwrap();
        let listed = names(&offered);
        assert!(listed.contains(&"@e23"), "{listed:?}");
        assert!(listed.contains(&"'button:Save settings'"), "{listed:?}");
        // Fuzzy narrowing by the element's *name* still finds the @ref via its hint.
        let narrowed = complete("click sav", &ctx).unwrap();
        assert!(!narrowed.candidates.is_empty());
    }

    #[test]
    fn flag_values_and_named_keys_complete() {
        let status = complete("expect net /api --status ", &Context::default()).unwrap();
        assert!(names(&status).contains(&"2xx"));
        let keys = complete("press ", &Context::default()).unwrap();
        assert!(names(&keys).contains(&"Enter"));
        let tracks = complete("tail --track ", &Context::default()).unwrap();
        assert!(names(&tracks).contains(&"watch"));
    }

    #[test]
    fn flow_names_come_from_the_context() {
        let ctx = Context { flows: vec!["save-smoke".into()], ..Context::default() };
        let flows = complete("flow run ", &ctx).unwrap();
        assert_eq!(names(&flows), vec!["save-smoke"]);
    }

    /// Mid-line completion replaces only the trailing word.
    #[test]
    fn start_points_at_the_trailing_word() {
        let completion = complete("expect te", &Context::default()).unwrap();
        assert_eq!(completion.start, 7);
        assert_eq!(names(&completion)[0], "text");
    }
}
