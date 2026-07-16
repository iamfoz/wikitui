//! CLI deep-link parsing (PRD FR-CS-6, MVP slice): the TITLE argument
//! accepts a plain title ("Alan Turing"), a full wikipedia.org URL
//! (`https://de.wikipedia.org/wiki/Alan_Turing#Leben`), or a lang-prefixed
//! title (`de:Alan Turing`) — each resolving to (language, title).
//!
//! PRD FR-ML-4 extends both forms to the four sister projects
//! ([`crate::sisters`]): a full sister-project URL
//! (`https://en.wiktionary.org/wiki/Word`) resolves exactly like a
//! wikipedia.org one, plus `project`; and the small, fixed set of real
//! MediaWiki interwiki-map prefixes these four projects are addressed by —
//! `wikt:` (Wiktionary), `voy:` (Wikivoyage), `q:` (Wikiquote), `n:`
//! (Wikinews) — resolve the same way a lang prefix does, just naming a
//! project instead of a language. Checked *before* the lang-prefix branch
//! below: "wikt" and "voy" are themselves shaped like plausible (if
//! nonsensical) language codes per [`is_lang_code`]'s loose ASCII-letters
//! check, so the interwiki match must run first or it would never fire.
//!
//! ## `wiki://`/`wiki:` protocol scheme (PRD FR-CS-7)
//!
//! A v2 addition, completing the groundwork `packaging/wikitui.desktop`
//! shipped (its `x-scheme-handler/wiki` MimeType route): a desktop
//! environment dispatching a `wiki://` link hands it to `wikitui` as an
//! ordinary CLI argument (`Exec=wikitui %u`), which lands here exactly like
//! any other TITLE argument. Grammar:
//!
//! - `wiki://{host}/{title}` — `host` resolves exactly like the `https://`
//!   branch above: `{lang}.wikipedia.org` or one of the four sister-project
//!   domains, including `.m.` mobile hosts. `{title}` may optionally carry
//!   the same `wiki/` path segment a real URL does (`wiki://en.wikipedia.org
//!   /wiki/Alan_Turing`) or omit it (`wiki://en.wikipedia.org/Alan_Turing`)
//!   — both resolve identically, since there is no real HTTP path to stay
//!   compatible with, only the convention this scheme borrows from it.
//! - `wiki:{title}` (no host) — opens `{title}` on the default wiki, the
//!   same "no language" convention a bare plain-title argument already uses.
//!   There is no lang-prefix form inside a bare `wiki:` URI (`wiki:de:Alan
//!   Turing` is one literal title, `"de:Alan Turing"`, not a language
//!   selector) — a reader who wants a specific language uses the full
//!   `wiki://{lang}.wikipedia.org/{title}` form instead.
//!
//! Checked *before* the interwiki-prefix branch below: "wiki" itself
//! satisfies [`is_lang_code`]'s loose shape check (it's all lowercase ASCII
//! letters), so without this branch running first, `wiki:Alan Turing` would
//! be misparsed as language code "wiki" by the lang-prefix branch further
//! down — the same ordering hazard "wikt"/"voy" already forced onto the
//! interwiki-prefix branch, one level earlier.
//!
//! An unrecognized host, or a `wiki://`/`wiki:` with nothing left to call a
//! title, degrades gracefully (PRD's protocol-handler contract has no
//! "reject the link" option) rather than erroring: the best title-shaped
//! remainder found is used, falling back to the raw input verbatim as a
//! last resort — the same "let the API report it missing" posture the
//! `https://` branch's own unrecognized-URL fallthrough already takes.

use crate::sisters;

/// What a TITLE argument resolved to. `lang` is `None` when the argument
/// didn't carry its own language (plain titles), in which case the
/// `--lang` flag (or its default) applies. `project` is `Some(name)` only
/// when the argument named a sister project explicitly (FR-ML-4); `None`
/// means "the currently active wiki," matching a plain title's own "no
/// language" convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub lang: Option<String>,
    pub title: String,
    pub project: Option<String>,
}

/// Valid Wikipedia language subdomains are lowercase ASCII letters plus
/// '-' (e.g. "en", "de", "zh-yue", "roa-rup"), and at least two chars —
/// anything else ("C:", "Template:...") is not a lang prefix.
pub fn is_lang_code(s: &str) -> bool {
    s.len() >= 2 && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// PRD SEC-1: `parse`'s single sanitization choke point. `parse_raw` below
/// has three different resolution paths (URL decode, lang-prefixed title,
/// plain title) and three early returns; rather than sanitize at each one,
/// every path funnels through this wrapper before the title can reach the
/// fetch/open path — and, eventually, the terminal — same rationale as
/// `doc::parse_article_html`'s `sanitize_document`.
pub fn parse(input: &str) -> Target {
    let mut target = parse_raw(input);
    target.title = crate::sanitize::sanitize_single_line(&target.title).into_owned();
    target
}

/// Matches `host` against Wikipedia's own domain or one of the four sister
/// projects' (PRD FR-ML-4), returning the sister project's registry name
/// (`None` for a plain `wikipedia.org` host — the default wiki needs no
/// override) and the leading language subdomain.
fn match_known_host(host: &str) -> Option<(Option<&'static str>, &str)> {
    if let Some(lang) = host.strip_suffix(".wikipedia.org") {
        return Some((None, lang));
    }
    for project in sisters::SISTER_PROJECTS {
        if let Some(lang) = host.strip_suffix(&format!(".{}", project.domain_suffix)) {
            return Some((Some(project.name), lang));
        }
    }
    None
}

fn parse_raw(input: &str) -> Target {
    let input = input.trim();

    // Full URL: https://{lang}.wikipedia.org/wiki/{Title}[#fragment], or the
    // same shape on one of the four sister-project domains (FR-ML-4).
    if let Some(rest) = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        && let Some((host, path)) = rest.split_once('/')
        && let Some((project, lang)) = match_known_host(host)
    {
        // "m.": mobile hosts are {lang}.m.<domain> on every one of these
        // projects, not just Wikipedia.
        let lang = lang.strip_suffix(".m").unwrap_or(lang);
        if let Some(encoded_title) = path.strip_prefix("wiki/") {
            let encoded_title = encoded_title
                .split(['#', '?'])
                .next()
                .unwrap_or(encoded_title);
            let title = urlencoding::decode(encoded_title)
                .map(|t| t.into_owned())
                .unwrap_or_else(|_| encoded_title.to_string())
                .replace('_', " ");
            if !title.is_empty() && is_lang_code(lang) {
                return Target {
                    lang: Some(lang.to_string()),
                    title,
                    project: project.map(str::to_string),
                };
            }
        }
        // A URL we don't understand: fall through and treat the whole
        // string as a title — the API will report it missing, which is a
        // clearer failure than silently mangling it.
    }

    // `wiki://`/`wiki:` protocol-handler scheme (PRD FR-CS-7) — see the
    // module doc's own section for the full grammar and why this must run
    // before the interwiki/lang-prefix branches below.
    if let Some(rest) = input.strip_prefix("wiki://") {
        if let Some((host, path)) = rest.split_once('/')
            && let Some((project, lang)) = match_known_host(host)
        {
            let lang = lang.strip_suffix(".m").unwrap_or(lang);
            // Symmetry with the `https://` branch's own `wiki/`-prefixed
            // path — accepted here too, but optional, since this scheme has
            // no real HTTP path to stay compatible with.
            let path = path.strip_prefix("wiki/").unwrap_or(path);
            let encoded_title = path.split(['#', '?']).next().unwrap_or(path);
            let title = urlencoding::decode(encoded_title)
                .map(|t| t.into_owned())
                .unwrap_or_else(|_| encoded_title.to_string())
                .replace('_', " ");
            if !title.is_empty() && is_lang_code(lang) {
                return Target {
                    lang: Some(lang.to_string()),
                    title,
                    project: project.map(str::to_string),
                };
            }
        }
        // An unrecognized host, a non-language-shaped one, or no title left
        // at all: graceful fallback, not an error (see the module doc) —
        // recover the last path segment as a plain title on the default
        // wiki when there is one, else fall back to the raw input verbatim.
        let recovered = rest.rsplit('/').next().unwrap_or("").replace('_', " ");
        return Target {
            lang: None,
            title: if recovered.is_empty() {
                input.to_string()
            } else {
                recovered
            },
            project: None,
        };
    }
    if let Some(rest) = input.strip_prefix("wiki:") {
        // Bare `wiki:Title` (no host): default wiki + title, same "no
        // language" convention a plain title argument already carries.
        let title = urlencoding::decode(rest)
            .map(|t| t.into_owned())
            .unwrap_or_else(|_| rest.to_string())
            .trim()
            .replace('_', " ");
        return Target {
            lang: None,
            title: if title.is_empty() {
                input.to_string()
            } else {
                title
            },
            project: None,
        };
    }

    // Interwiki-prefixed title: "wikt:Word", "voy:Place", "q:Quote",
    // "n:Headline" (PRD FR-ML-4) — checked before the lang-prefix branch
    // below since "wikt"/"voy" would otherwise also satisfy
    // `is_lang_code`'s loose shape check. No language segment (matching how
    // real wikitext interwiki links carry none): the sister project's
    // *current* language edition applies, same as a plain title's `lang:
    // None`.
    if let Some((prefix, rest)) = input.split_once(':')
        && let Some(project) = sisters::by_interwiki_prefix(prefix)
        && !rest.is_empty()
        && !rest.starts_with("//")
    {
        return Target {
            lang: None,
            title: rest.trim().replace('_', " "),
            project: Some(project.name.to_string()),
        };
    }

    // Lang-prefixed title: "de:Alan Turing". Real titles can contain ':'
    // ("Star Trek: First Contact", "Template:Infobox"), so only treat the
    // prefix as a language when it actually looks like one — and never
    // when the remainder starts with "//" (that's a URL scheme like
    // "https:", which is all lowercase letters and would otherwise pass).
    if let Some((prefix, rest)) = input.split_once(':')
        && is_lang_code(prefix)
        && !rest.is_empty()
        && !rest.starts_with("//")
    {
        return Target {
            lang: Some(prefix.to_string()),
            title: rest.trim().replace('_', " "),
            project: None,
        };
    }

    Target {
        lang: None,
        title: input.replace('_', " "),
        project: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(lang: Option<&str>, title: &str) -> Target {
        Target {
            lang: lang.map(str::to_string),
            title: title.to_string(),
            project: None,
        }
    }

    /// Like [`t`] but with a sister project set (PRD FR-ML-4).
    fn tp(lang: Option<&str>, title: &str, project: &str) -> Target {
        Target {
            lang: lang.map(str::to_string),
            title: title.to_string(),
            project: Some(project.to_string()),
        }
    }

    #[test]
    fn plain_titles_pass_through_with_underscores_normalized() {
        assert_eq!(parse("Alan Turing"), t(None, "Alan Turing"));
        assert_eq!(parse("Alan_Turing"), t(None, "Alan Turing"));
    }

    #[test]
    fn full_urls_resolve_language_and_title() {
        assert_eq!(
            parse("https://en.wikipedia.org/wiki/Alan_Turing"),
            t(Some("en"), "Alan Turing")
        );
        assert_eq!(
            parse("http://de.wikipedia.org/wiki/Kurt_G%C3%B6del"),
            t(Some("de"), "Kurt Gödel")
        );
    }

    #[test]
    fn url_fragments_and_queries_are_stripped() {
        assert_eq!(
            parse("https://en.wikipedia.org/wiki/Alan_Turing#Legacy"),
            t(Some("en"), "Alan Turing")
        );
        assert_eq!(
            parse("https://en.wikipedia.org/wiki/Alan_Turing?oldid=12345"),
            t(Some("en"), "Alan Turing")
        );
    }

    #[test]
    fn mobile_hosts_resolve_to_the_same_wiki() {
        assert_eq!(
            parse("https://en.m.wikipedia.org/wiki/Alan_Turing"),
            t(Some("en"), "Alan Turing")
        );
    }

    #[test]
    fn lang_prefixes_apply_only_when_they_look_like_languages() {
        assert_eq!(parse("de:Alan Turing"), t(Some("de"), "Alan Turing"));
        assert_eq!(parse("zh-yue:香港"), t(Some("zh-yue"), "香港"));
        // Real titles containing ':' must NOT be split:
        assert_eq!(
            parse("Star Trek: First Contact"),
            t(None, "Star Trek: First Contact")
        );
        assert_eq!(parse("Template:Infobox"), t(None, "Template:Infobox"));
        // Single letters aren't languages ("C:" is a drive, not a wiki).
        assert_eq!(parse("C: drive"), t(None, "C: drive"));
    }

    #[test]
    fn non_wikipedia_urls_fall_through_as_titles() {
        let target = parse("https://example.com/wiki/Whatever");
        assert_eq!(target.lang, None);
        assert!(target.title.contains("example.com"));
    }

    /// PRD FR-ML-4: a full sister-project URL resolves language, title, AND
    /// project — the same shape a wikipedia.org URL already resolves, plus
    /// which project.
    #[test]
    fn sister_project_urls_resolve_language_title_and_project() {
        assert_eq!(
            parse("https://en.wiktionary.org/wiki/computer"),
            tp(Some("en"), "computer", "wiktionary")
        );
        assert_eq!(
            parse("https://en.wikivoyage.org/wiki/London"),
            tp(Some("en"), "London", "wikivoyage")
        );
        assert_eq!(
            parse("https://en.wikiquote.org/wiki/Alan_Turing"),
            tp(Some("en"), "Alan Turing", "wikiquote")
        );
        assert_eq!(
            parse("https://en.wikinews.org/wiki/Test_story"),
            tp(Some("en"), "Test story", "wikinews")
        );
    }

    #[test]
    fn sister_project_mobile_hosts_resolve_to_the_same_project() {
        assert_eq!(
            parse("https://en.m.wiktionary.org/wiki/computer"),
            tp(Some("en"), "computer", "wiktionary")
        );
    }

    /// PRD FR-ML-4: the documented interwiki-prefix set — `wikt:`, `voy:`,
    /// `q:`, `n:` — resolves to (no language override, the title, the
    /// project), the same way a lang prefix resolves to (language, title).
    /// `wikt`/`voy` are exactly the case this must win over the plain
    /// lang-prefix branch: both are shaped like a plausible language code.
    #[test]
    fn interwiki_prefixes_resolve_to_their_sister_project() {
        assert_eq!(parse("wikt:computer"), tp(None, "computer", "wiktionary"));
        assert_eq!(parse("voy:London"), tp(None, "London", "wikivoyage"));
        assert_eq!(parse("q:Alan Turing"), tp(None, "Alan Turing", "wikiquote"));
        assert_eq!(parse("n:Test story"), tp(None, "Test story", "wikinews"));
        // Underscores normalize the same way a plain title's do.
        assert_eq!(
            parse("wikt:Alan_Turing"),
            tp(None, "Alan Turing", "wiktionary")
        );
    }

    // ---- `wiki://`/`wiki:` protocol scheme (PRD FR-CS-7) -------------------

    #[test]
    fn wiki_scheme_url_resolves_host_and_title_like_https() {
        assert_eq!(
            parse("wiki://en.wikipedia.org/Alan_Turing"),
            t(Some("en"), "Alan Turing")
        );
        assert_eq!(
            parse("wiki://de.wikipedia.org/Kurt_G%C3%B6del"),
            t(Some("de"), "Kurt Gödel")
        );
    }

    #[test]
    fn wiki_scheme_url_accepts_an_optional_wiki_path_segment() {
        // Both forms — with and without the real URL's "wiki/" path
        // segment — resolve identically (module doc: no real HTTP path to
        // stay compatible with here).
        assert_eq!(
            parse("wiki://en.wikipedia.org/wiki/Alan_Turing"),
            parse("wiki://en.wikipedia.org/Alan_Turing")
        );
    }

    #[test]
    fn wiki_scheme_url_resolves_sister_project_hosts() {
        assert_eq!(
            parse("wiki://en.wiktionary.org/computer"),
            tp(Some("en"), "computer", "wiktionary")
        );
    }

    #[test]
    fn wiki_scheme_url_mobile_host_resolves_to_the_same_wiki() {
        assert_eq!(
            parse("wiki://en.m.wikipedia.org/Alan_Turing"),
            t(Some("en"), "Alan Turing")
        );
    }

    #[test]
    fn bare_wiki_scheme_opens_the_default_wiki() {
        assert_eq!(parse("wiki:Alan Turing"), t(None, "Alan Turing"));
        assert_eq!(parse("wiki:Alan_Turing"), t(None, "Alan Turing"));
    }

    #[test]
    fn malformed_wiki_scheme_urls_degrade_gracefully_instead_of_erroring() {
        // An unrecognized host still recovers a title from the path rather
        // than surfacing an error — the "let the API report it missing"
        // posture the https:// branch's own fallthrough already takes.
        let target = parse("wiki://not-a-wiki-host.example/Some_Title");
        assert_eq!(target.lang, None);
        assert_eq!(target.title, "Some Title");

        // Nothing at all to recover a title from: falls back to the raw
        // input rather than panicking or looping.
        assert_eq!(parse("wiki://").lang, None);
        assert!(!parse("wiki://").title.is_empty());
        assert_eq!(parse("wiki:").lang, None);
        assert!(!parse("wiki:").title.is_empty());
    }

    #[test]
    fn bare_wiki_scheme_does_not_chain_a_lang_prefix() {
        // Module doc: a bare `wiki:` URI has no lang-prefix form of its
        // own — the whole remainder is one literal title.
        assert_eq!(parse("wiki:de:Alan Turing"), t(None, "de:Alan Turing"));
    }

    #[test]
    fn unrecognized_prefixes_are_not_mistaken_for_interwiki_links() {
        // A real language code takes the plain lang-prefix path, not the
        // interwiki one (no sister project matches "de").
        assert_eq!(parse("de:Wörterbuch"), t(Some("de"), "Wörterbuch"));
        // Text that merely resembles "q:" or "n:" with an empty/URL-shaped
        // remainder must not be swallowed as an interwiki link.
        assert_eq!(parse("q:"), t(None, "q:"));
    }
}
