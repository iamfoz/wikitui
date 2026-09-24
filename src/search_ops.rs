//! PRD FR-SR-3's CirrusSearch operator support: `intitle:`, `incategory:`,
//! `insource:/regex/`, `hastemplate:`, `deepcat:`, `articletopic:`,
//! `morelike:`, `prefix:` are just query *string* syntax the search server
//! itself interprets — `api::WikiClient::search` already forwards
//! `search_input` to the endpoint's `q` param verbatim (nothing rewrites
//! `:`/`/`), so passthrough needed no new code. What FR-SR-3 asks for
//! *beyond* passthrough are the two things this module holds: a static
//! cheat-sheet (shown on `?` inside the search prompt) and operator-*name*
//! completion (Tab). Argument completion — category names, template names,
//! the inside of an `insource:/regex/` — is an explicit non-goal: this
//! module only ever looks at the last bare word being typed, never at what
//! follows a `:`.

/// One entry in the cheat-sheet: the bare operator name (no trailing colon)
/// and a one-line description.
pub struct Operator {
    pub name: &'static str,
    pub about: &'static str,
}

/// PRD FR-SR-3's named operator list, in the order the PRD lists them.
pub const OPERATORS: &[Operator] = &[
    Operator {
        name: "intitle",
        about: "match only if the term appears in the title",
    },
    Operator {
        name: "incategory",
        about: "pages that belong to the given category",
    },
    Operator {
        name: "insource",
        about: "match the wikitext source; /regex/ form supported",
    },
    Operator {
        name: "hastemplate",
        about: "pages that transclude the given template",
    },
    Operator {
        name: "deepcat",
        about: "pages in the category or any of its subcategories",
    },
    Operator {
        name: "articletopic",
        about: "pages classified under a predicted topic",
    },
    Operator {
        name: "morelike",
        about: "pages whose text resembles the given title (powers the Related panel)",
    },
    Operator {
        name: "prefix",
        about: "titles starting with the given prefix",
    },
];

/// Tab-completion for the *last* whitespace-delimited word of `input` (PRD
/// FR-SR-3c): if that word is a case-insensitive prefix of exactly one
/// operator name, returns `input` with the word replaced by `name:` —
/// unchanged up to that word, so completion mid-query (`"turing morel"`)
/// still works. Returns `None` (the caller's Tab keeps its other meaning,
/// full-text search) when:
///  - the word is empty,
///  - the word already has a `:` in it (an operator, argument and all, is
///    already typed — argument completion is out of scope, so there is
///    nothing left for this function to add), or
///  - the prefix is ambiguous (`"in"` matches `intitle`/`incategory`/
///    `insource`) or matches no operator at all.
pub fn complete_operator_name(input: &str) -> Option<String> {
    let word_start = input.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let (prefix, word) = input.split_at(word_start);
    if word.is_empty() || word.contains(':') {
        return None;
    }
    let word_lower = word.to_lowercase();
    let mut matches = OPERATORS
        .iter()
        .filter(|op| op.name.starts_with(word_lower.as_str()));
    let only = matches.next()?;
    if matches.next().is_some() {
        return None; // ambiguous prefix — nothing to commit to
    }
    Some(format!("{prefix}{}:", only.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unambiguous_prefix_completes_to_the_full_operator_name() {
        assert_eq!(
            complete_operator_name("morel"),
            Some("morelike:".to_string())
        );
        assert_eq!(
            complete_operator_name("hastempl"),
            Some("hastemplate:".to_string())
        );
    }

    /// A prefix typed in full (no more letters to add) still gets its
    /// colon appended — Tab's job is "commit to this operator", not just
    /// "fill in missing letters".
    #[test]
    fn a_fully_typed_operator_name_still_gets_the_colon_appended() {
        assert_eq!(
            complete_operator_name("morelike"),
            Some("morelike:".to_string())
        );
    }

    #[test]
    fn earlier_words_in_the_query_are_preserved() {
        assert_eq!(
            complete_operator_name("Turing morel"),
            Some("Turing morelike:".to_string())
        );
    }

    /// `in` prefixes three operators (`intitle`, `incategory`, `insource`) —
    /// ambiguous, so nothing is committed.
    #[test]
    fn an_ambiguous_prefix_completes_to_nothing() {
        assert_eq!(complete_operator_name("in"), None);
    }

    /// A word that already carries an operator *and* its argument
    /// (`intitle:Turing`) is left alone — argument completion is out of
    /// scope, and re-completing the operator name would be destructive.
    #[test]
    fn a_word_that_already_has_a_colon_is_left_untouched() {
        assert_eq!(complete_operator_name("intitle:Turing"), None);
    }

    #[test]
    fn a_non_operator_word_completes_to_nothing() {
        assert_eq!(complete_operator_name("hello"), None);
        assert_eq!(complete_operator_name("Alan Turing"), None);
    }

    #[test]
    fn empty_input_completes_to_nothing() {
        assert_eq!(complete_operator_name(""), None);
        assert_eq!(complete_operator_name("Alan "), None);
    }

    /// Sanity on the cheat-sheet data itself: every operator has a
    /// non-empty description and the PRD's exact eight names are present.
    #[test]
    fn every_documented_operator_is_listed_with_a_description() {
        let names: Vec<&str> = OPERATORS.iter().map(|op| op.name).collect();
        for expected in [
            "intitle",
            "incategory",
            "insource",
            "hastemplate",
            "deepcat",
            "articletopic",
            "morelike",
            "prefix",
        ] {
            assert!(names.contains(&expected), "missing operator {expected:?}");
        }
        for op in OPERATORS {
            assert!(!op.about.is_empty(), "{} has no description", op.name);
        }
    }
}
