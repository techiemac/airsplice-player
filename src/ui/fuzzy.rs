//! Subsequence fuzzy matching for the command palette and file finder.

/// A match: a score (higher is better) and the byte offsets of matched
/// characters in the candidate, for highlighting.
#[derive(Debug, PartialEq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

/// Case-insensitive subsequence match. Consecutive runs and matches at word
/// starts score higher; an empty query matches everything with score 0.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<Match> {
    let mut positions = Vec::new();
    let mut score = 0;
    let mut chars = candidate.char_indices().peekable();
    let mut prev_matched_at: Option<usize> = None;
    let mut prev_char: Option<char> = None;
    for q in query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase) {
        loop {
            let (i, c) = chars.next()?;
            let before = prev_char.replace(c);
            if c.to_lowercase().eq(std::iter::once(q)) {
                let word_start = before.is_none_or(|b| !b.is_alphanumeric() || (b.is_lowercase() && c.is_uppercase()));
                score += 1;
                if word_start {
                    score += 8;
                }
                if prev_matched_at.is_some_and(|p| candidate[p..i].chars().count() == 1) {
                    score += 5;
                }
                prev_matched_at = Some(i);
                positions.push(i);
                break;
            }
        }
    }
    // Prefer shorter candidates among equal matches.
    score -= (candidate.chars().count() / 8) as i32;
    Some(Match { score, positions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matches_case_insensitively() {
        let m = fuzzy_match("plps", "Play / Pause").unwrap();
        assert_eq!(m.positions, vec![0, 1, 7, 10]);
        assert!(fuzzy_match("xyz", "Play / Pause").is_none());
        assert_eq!(fuzzy_match("", "anything").unwrap().positions, Vec::<usize>::new());
    }

    #[test]
    fn word_starts_and_runs_rank_higher() {
        let word = fuzzy_match("tr", "Toggle Repeat").unwrap().score;
        let mid = fuzzy_match("tr", "Stretch").unwrap().score;
        assert!(word > mid, "{word} vs {mid}");
        let run = fuzzy_match("next", "Next Track").unwrap().score;
        let spread = fuzzy_match("next", "New Text").unwrap().score;
        assert!(run > spread, "{run} vs {spread}");
    }

    #[test]
    fn positions_are_byte_offsets_for_highlighting() {
        let m = fuzzy_match("é", "café").unwrap();
        assert_eq!(m.positions, vec![3]);
        assert!("café".is_char_boundary(m.positions[0]));
    }
}
