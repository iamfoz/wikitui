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
