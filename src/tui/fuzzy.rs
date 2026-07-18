//! Shared Nucleo-backed fuzzy matching for bounded candidate sets.
//!
//! Construct one [`Matcher`] per query and reuse it across every candidate. Nucleo's matcher owns
//! substantial scratch storage, so the old stateless helper API intentionally does not survive the
//! cutover. Lower scores are better to preserve Kit's existing ranking convention.

use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher as NucleoMatcher, Utf32Str,
};

pub struct Matcher {
    needle: String,
    case_sensitive: bool,
    pattern: Pattern,
    matcher: NucleoMatcher,
    chars: Vec<char>,
}

impl Matcher {
    pub fn new(needle: &str) -> Self {
        Self::with_config(needle, CaseMatching::Respect, Config::DEFAULT)
    }

    pub fn case_insensitive(needle: &str) -> Self {
        Self::with_config(needle, CaseMatching::Ignore, Config::DEFAULT)
    }

    pub fn paths(needle: &str) -> Self {
        Self::with_config(needle, CaseMatching::Ignore, Config::DEFAULT.match_paths())
    }

    pub fn score(&mut self, candidate: &str) -> Option<u64> {
        self.chars.clear();
        let comparable =
            if self.case_sensitive { candidate.to_owned() } else { candidate.to_lowercase() };
        let tier: u8 = if comparable == self.needle {
            0
        } else if comparable.starts_with(&self.needle) {
            1
        } else {
            2
        };
        let candidate = Utf32Str::new(candidate, &mut self.chars);
        self.pattern
            .score(candidate, &mut self.matcher)
            .map(|score| (u64::from(tier) << 32) | u64::from(u32::MAX - score))
    }

    fn with_config(needle: &str, case: CaseMatching, config: Config) -> Self {
        let case_sensitive = case == CaseMatching::Respect;
        Self {
            needle: if case_sensitive { needle.to_owned() } else { needle.to_lowercase() },
            case_sensitive,
            pattern: Pattern::parse(needle, case, Normalization::Smart),
            matcher: NucleoMatcher::new(config),
            chars: Vec::new(),
        }
    }
}

pub const fn tier(score: u64) -> u8 {
    (score >> 32) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_rank_exact_then_prefix_then_subsequence() {
        let mut matcher = Matcher::new("eval");
        let exact = matcher.score("eval").unwrap();
        let prefix = matcher.score("evaluate").unwrap();
        let scattered = matcher.score("retrieval").unwrap();
        assert!(exact < prefix);
        assert!(prefix < scattered);
        assert_eq!(matcher.score("snap"), None);
    }

    #[test]
    fn case_insensitive_matches_titles() {
        let mut matcher = Matcher::case_insensitive("work");
        assert!(matcher.score("Workspace · ari").is_some());
        let mut matcher = Matcher::case_insensitive("wsp");
        assert!(matcher.score("WORKSPACE").is_some());
    }

    #[test]
    fn path_config_supports_multiword_fzf_queries() {
        let mut matcher = Matcher::paths("render config");
        assert!(matcher.score("src/tools/render/config.rs").is_some());
        assert!(matcher.score("src/tools/search/index.rs").is_none());
    }
}
