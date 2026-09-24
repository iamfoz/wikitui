//! Known Wikimedia sister projects (PRD FR-ML-4). Each is the *same*
//! MediaWiki software as Wikipedia, deployed on its own `{lang}.<project>.org`
//! domain — so the same `api::WikiClient` endpoints work against it, just
//! with a different `base_url_template` (§6.2 rule 1/2). This module is the
//! one place that enumerates which projects wikitui knows about by name
//! (`active_wiki = "wiktionary"`, `:wiki wiktionary`) and by real MediaWiki
//! interwiki-map prefix (`wikt:Word`, `target::parse`).
//!
//! Capability defaults (FR-ML-5's degradation matrix, applied by
//! `config::resolve_wiki`): only the built-in `wikipedia` entry is assumed to
//! have Wikifeeds and PageAssessments — both are close to Wikipedia-only in
//! practice (§6.2 rule 6 lists feed availability as "varies per language";
//! PageAssessments is a WikiProject-tagging convention Wiktionary/Wikivoyage/
//! Wikiquote/Wikinews don't use). PageViewInfo (`prop=pageviews`) *is*
//! actually enabled wiki-wide, but this module still defaults it off for
//! sister projects, matching the simpler rule "only `wikipedia` gets every
//! feature by default" — a reader who knows their sister project supports it
//! opts back in with one `[wiki.<name>]` line (`config::resolve_wiki`'s own
//! doc comment covers the override).

/// One entry in the sister-project table: its `:wiki`/`active_wiki` name,
/// the domain suffix `{lang}.` is prepended to, its real MediaWiki
/// interwiki-map prefix (`None` for Wikipedia itself, which this client
/// never needs a prefix to address — a plain title already means "the
/// active wiki"), and the display name the `:wiki` picker shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownProject {
    pub name: &'static str,
    pub domain_suffix: &'static str,
    pub interwiki_prefix: Option<&'static str>,
    pub display_name: &'static str,
}

impl KnownProject {
    /// The `{lang}`-templated base URL `config::resolve_wiki` seeds for this
    /// project absent an overriding `[wiki.<name>] base_url`.
    pub fn base_url_template(&self) -> String {
        format!("https://{{lang}}.{}", self.domain_suffix)
    }
}

/// The built-in default wiki — not itself part of [`SISTER_PROJECTS`] since
/// it carries no interwiki prefix (a bare title always means "this wiki").
pub const WIKIPEDIA: KnownProject = KnownProject {
    name: "wikipedia",
    domain_suffix: "wikipedia.org",
    interwiki_prefix: None,
    display_name: "Wikipedia",
};

/// PRD FR-ML-4's four sister projects, in the `:wiki` picker's display
/// order. Interwiki prefixes are en.wikipedia.org's own interwiki-map short
/// forms for these four projects — documented here since they're the
/// contract `target::parse` promises callers.
pub const SISTER_PROJECTS: &[KnownProject] = &[
    KnownProject {
        name: "wiktionary",
        domain_suffix: "wiktionary.org",
        interwiki_prefix: Some("wikt"),
        display_name: "Wiktionary",
    },
    KnownProject {
        name: "wikivoyage",
        domain_suffix: "wikivoyage.org",
        interwiki_prefix: Some("voy"),
        display_name: "Wikivoyage",
    },
    KnownProject {
        name: "wikiquote",
        domain_suffix: "wikiquote.org",
        interwiki_prefix: Some("q"),
        display_name: "Wikiquote",
    },
    KnownProject {
        name: "wikinews",
        domain_suffix: "wikinews.org",
        interwiki_prefix: Some("n"),
        display_name: "Wikinews",
    },
];

/// Every built-in project the `:wiki` picker lists, Wikipedia first.
pub fn all_known_projects() -> Vec<KnownProject> {
    std::iter::once(WIKIPEDIA)
        .chain(SISTER_PROJECTS.iter().copied())
        .collect()
}

/// Looks a project up by its `:wiki`/`active_wiki` name — `"wikipedia"` or
/// one of the four sister names. `None` for anything else (a custom
/// `[wiki.<name>]` site has no built-in entry here).
pub fn by_name(name: &str) -> Option<KnownProject> {
    if name == WIKIPEDIA.name {
        return Some(WIKIPEDIA);
    }
    SISTER_PROJECTS.iter().copied().find(|p| p.name == name)
}

/// Looks a project up by its real MediaWiki interwiki-map prefix (`wikt`,
/// `voy`, `q`, `n`) — `target::parse`'s `wikt:Word` deep-link form.
pub fn by_interwiki_prefix(prefix: &str) -> Option<KnownProject> {
    SISTER_PROJECTS
        .iter()
        .copied()
        .find(|p| p.interwiki_prefix == Some(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sister_project_has_a_distinct_name_and_prefix() {
        let names: std::collections::HashSet<_> = SISTER_PROJECTS.iter().map(|p| p.name).collect();
        assert_eq!(names.len(), SISTER_PROJECTS.len(), "duplicate project name");
        let prefixes: std::collections::HashSet<_> = SISTER_PROJECTS
            .iter()
            .filter_map(|p| p.interwiki_prefix)
            .collect();
        assert_eq!(
            prefixes.len(),
            SISTER_PROJECTS.len(),
            "duplicate interwiki prefix"
        );
    }

    #[test]
    fn by_name_finds_wikipedia_and_every_sister() {
        assert_eq!(by_name("wikipedia"), Some(WIKIPEDIA));
        assert_eq!(by_name("wiktionary").map(|p| p.name), Some("wiktionary"));
        assert_eq!(by_name("wikivoyage").map(|p| p.name), Some("wikivoyage"));
        assert_eq!(by_name("wikiquote").map(|p| p.name), Some("wikiquote"));
        assert_eq!(by_name("wikinews").map(|p| p.name), Some("wikinews"));
        assert_eq!(by_name("archwiki"), None);
    }

    #[test]
    fn by_interwiki_prefix_matches_the_documented_set() {
        assert_eq!(
            by_interwiki_prefix("wikt").map(|p| p.name),
            Some("wiktionary")
        );
        assert_eq!(
            by_interwiki_prefix("voy").map(|p| p.name),
            Some("wikivoyage")
        );
        assert_eq!(by_interwiki_prefix("q").map(|p| p.name), Some("wikiquote"));
        assert_eq!(by_interwiki_prefix("n").map(|p| p.name), Some("wikinews"));
        assert_eq!(by_interwiki_prefix("wikipedia"), None);
        assert_eq!(by_interwiki_prefix("xx"), None);
    }

    #[test]
    fn base_url_template_substitutes_the_project_domain() {
        assert_eq!(
            WIKIPEDIA.base_url_template(),
            "https://{lang}.wikipedia.org"
        );
        let wikt = by_name("wiktionary").unwrap();
        assert_eq!(wikt.base_url_template(), "https://{lang}.wiktionary.org");
    }
}
