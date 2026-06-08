//! The shared fuzzy matcher: subsequence scoring with exact/prefix tiers. One implementation behind
//! every "type to narrow a list" surface — the slash-command line and the cdp target picker.
//!
//! Lower is better, `None` means no match. Sort ascending and exact beats prefix beats a scattered
//! subsequence; within a tier, earlier and tighter matches win.

/// Score `candidate` against `needle`, case-sensitive. `None` if `needle` is not a subsequence of
/// `candidate`. Exact = 0, a prefix scores in the tens, a scattered subsequence in the hundreds.
pub fn score(candidate: &str, needle: &str) -> Option<u16> {
    if candidate == needle {
        return Some(0);
    }
    if candidate.starts_with(needle) {
        return Some(10 + candidate.len().saturating_sub(needle.len()) as u16);
    }
    subsequence(candidate, needle).map(|score| 100 + score)
}

/// Case-insensitive [`score`] — for human-facing text like target titles and urls.
pub fn score_ci(candidate: &str, needle: &str) -> Option<u16> {
    score(&candidate.to_lowercase(), &needle.to_lowercase())
}

fn subsequence(candidate: &str, needle: &str) -> Option<u16> {
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

    #[test]
    fn tiers_rank_exact_then_prefix_then_subsequence() {
        assert_eq!(score("eval", "eval"), Some(0));
        assert!(score("evaluate", "eval").unwrap() < score("retrieval", "eval").unwrap());
        assert_eq!(score("snap", "xyz"), None);
    }

    #[test]
    fn case_insensitive_matches_titles() {
        assert!(score_ci("Workspace · ari", "work").is_some());
        assert!(score_ci("WORKSPACE", "wsp").is_some());
    }
}
