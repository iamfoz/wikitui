//! Shared fuzzy-subsequence matching (the fzf-lite idiom): every character
//! of a query must appear in a haystack, in that order, case-insensitively —
//! not necessarily contiguous. First written for the bookmark picker's `/`
//! filter (PRD FR-BM-1); the history picker's `/` filter and ranked search
//! (PRD FR-HS-1) need the identical subsequence rule plus a relevance score,
//! so both live here instead of wikitui growing a second, subtly different
//! matcher as more pickers pick up the same need.

/// A minimal subsequence "fuzzy" match: every character of `query` must
/// appear in `text`, in that order, case-insensitively — not necessarily
/// contiguous. An empty query matches anything.
pub fn fuzzy_matches(text: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let haystack = text.to_lowercase();
    let mut chars = haystack.chars();
    query
        .to_lowercase()
        .chars()
        .all(|qc| chars.any(|tc| tc == qc))
}

/// A relevance score for a subsequence match, or `None` if `query` doesn't
/// match `text` at all (mirrors `fuzzy_matches`'s pass/fail, scored instead
/// of boolean). Positions are found leftmost-greedy — the same walk
/// `fuzzy_matches` does — then scored as `query.len() / span`, where `span`
/// is the distance from the first matched character to the last: `1.0` for
/// a fully contiguous run (an exact substring match), shrinking toward `0`
/// as the same characters scatter further apart in `text`. Case-insensitive;
/// an empty query always scores a perfect `1.0` (mirrors `fuzzy_matches`'s
/// "empty query matches everything").
pub fn fuzzy_score(text: &str, query: &str) -> Option<f64> {
    if query.is_empty() {
        return Some(1.0);
    }
    let haystack: Vec<char> = text.to_lowercase().chars().collect();
    let mut positions = Vec::with_capacity(query.len());
    let mut cursor = 0usize;
    for qc in query.to_lowercase().chars() {
        let found = haystack[cursor..].iter().position(|&hc| hc == qc)?;
        let pos = cursor + found;
        positions.push(pos);
        cursor = pos + 1;
    }
    // `positions` is built in strictly increasing order by construction, so
    // first/last are the span's endpoints without needing a sort.
    let span = positions.last().unwrap() - positions.first().unwrap() + 1;
    Some(positions.len() as f64 / span as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_is_a_true_subsequence_not_a_substring_match() {
        assert!(fuzzy_matches("Alan Turing", "atn"));
        assert!(
            !fuzzy_matches("Alan Turing", "nat"),
            "wrong order must not match"
        );
        assert!(fuzzy_matches("Alan Turing", ""));
    }

    #[test]
    fn fuzzy_score_is_none_for_a_non_match() {
        assert_eq!(fuzzy_score("Alan Turing", "xyz"), None);
        assert_eq!(fuzzy_score("Alan Turing", "nat"), None, "wrong order");
    }

    #[test]
    fn fuzzy_score_is_perfect_for_a_contiguous_substring() {
        let score = fuzzy_score("Alan Turing", "turing").unwrap();
        assert!((score - 1.0).abs() < 1e-9, "got {score}");
    }

    #[test]
    fn fuzzy_score_is_lower_the_more_scattered_the_match() {
        // "atn" as a subsequence of "Alan Turing": leftmost-greedy positions
        // are 'A'(0) 't'(6, "Turing") 'n'(9) — span 10, query len 3.
        let scattered = fuzzy_score("Alan Turing", "atn").unwrap();
        let tight = fuzzy_score("Alan Turing", "turing").unwrap();
        assert!(
            scattered < tight,
            "scattered {scattered} must score below a contiguous match {tight}"
        );
        assert!(scattered > 0.0 && scattered < 1.0);
    }

    #[test]
    fn empty_query_scores_a_perfect_match() {
        assert_eq!(fuzzy_score("anything", ""), Some(1.0));
    }
}
