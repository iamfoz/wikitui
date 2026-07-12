//! CLI deep-link parsing (PRD FR-CS-6, MVP slice): the TITLE argument
//! accepts a plain title ("Alan Turing"), a full wikipedia.org URL
//! (`https://de.wikipedia.org/wiki/Alan_Turing#Leben`), or a lang-prefixed
//! title (`de:Alan Turing`) — each resolving to (language, title).

/// What a TITLE argument resolved to. `lang` is `None` when the argument
/// didn't carry its own language (plain titles), in which case the
/// `--lang` flag (or its default) applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub lang: Option<String>,
    pub title: String,
}

/// Valid Wikipedia language subdomains are lowercase ASCII letters plus
/// '-' (e.g. "en", "de", "zh-yue", "roa-rup"), and at least two chars —
/// anything else ("C:", "Template:...") is not a lang prefix.
pub fn is_lang_code(s: &str) -> bool {
    s.len() >= 2 && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

pub fn parse(input: &str) -> Target {
    let input = input.trim();

    // Full URL: https://{lang}.wikipedia.org/wiki/{Title}[#fragment]
    if let Some(rest) = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
    {
        if let Some((host, path)) = rest.split_once('/') {
            if let Some(lang) = host.strip_suffix(".wikipedia.org") {
                // "m.": Wikipedia's mobile hosts are {lang}.m.wikipedia.org.
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
                        };
                    }
                }
            }
        }
        // A URL we don't understand: fall through and treat the whole
        // string as a title — the API will report it missing, which is a
        // clearer failure than silently mangling it.
    }

    // Lang-prefixed title: "de:Alan Turing". Real titles can contain ':'
    // ("Star Trek: First Contact", "Template:Infobox"), so only treat the
    // prefix as a language when it actually looks like one — and never
    // when the remainder starts with "//" (that's a URL scheme like
    // "https:", which is all lowercase letters and would otherwise pass).
    if let Some((prefix, rest)) = input.split_once(':') {
        if is_lang_code(prefix) && !rest.is_empty() && !rest.starts_with("//") {
            return Target {
                lang: Some(prefix.to_string()),
                title: rest.trim().replace('_', " "),
            };
        }
    }

    Target {
        lang: None,
        title: input.replace('_', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(lang: Option<&str>, title: &str) -> Target {
        Target {
            lang: lang.map(str::to_string),
            title: title.to_string(),
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
}
