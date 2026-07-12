//! MediaWiki API client. Per PRD §6.2: per-wiki endpoints only
//! (`{lang}.wikipedia.org`), never `api.wikimedia.org`. Per §6.5 (NF-NET-2)
//! every request carries a descriptive User-Agent.
//!
//! The language edition is a per-request parameter rather than client
//! state, so `:lang de` (FR-CS-2 / FR-ML-2's config) can switch editions
//! mid-session without rebuilding the HTTP client.
//!
//! The host template itself *is* client state (PRD §6.2 rule 2 / FR-ML-5):
//! `config::resolve` picks a `base_url_template` from `WIKITUI_BASE_URL` or
//! a `[wiki.<name>]` section, defaulting to `{lang}.wikipedia.org`. This is
//! the supported way to point wikitui at a mock server or another
//! MediaWiki site — no source edits required, unlike the old ad hoc
//! testing hack this replaces.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const USER_AGENT_BASE: &str = concat!(
    "wikitui/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/iamfoz/wikitui) reqwest"
);

/// Cloned to hand a copy to a spawned background task (PRD FR-SR-1's
/// typeahead can't block the UI thread on the loop's redraw/`event::poll`
/// cycle) — cheap, since `reqwest::Client` is itself an `Arc` internally
/// and `base_url_template` is a small `String`. Mirrors reqwest's own
/// documented pattern of cloning the client rather than wrapping it.
#[derive(Clone)]
pub struct WikiClient {
    http: reqwest::Client,
    /// The resolved `[wiki.*].base_url` / `WIKITUI_BASE_URL` template
    /// (config.rs's `DEFAULT_BASE_URL_TEMPLATE` absent an override).
    base_url_template: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub description: Option<String>,
    pub excerpt: Option<String>,
    /// Article size in bytes (PRD FR-SR-2's "size" line) — optional since
    /// it isn't part of every `search/page` deployment's response, only
    /// what this mock (and some real ones) additionally provide.
    pub size: Option<u64>,
    /// Word count, same optionality as `size`.
    pub wordcount: Option<u32>,
    /// Last-edit timestamp (ISO 8601), same optionality as `size`.
    pub timestamp: Option<String>,
}

/// A typeahead completion (PRD FR-SR-1): just enough to render one dropdown
/// row and open the article directly — no excerpt/size/wordcount, unlike a
/// full-text `SearchResult`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TitleSuggestion {
    pub title: String,
    /// The Wikidata-style one-line description `search/title` returns
    /// alongside each candidate (PRD FR-SR-1, Appendix A).
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct SearchPageResponse {
    pages: Vec<SearchResult>,
    /// Not part of the core REST `search/page` response — PRD Appendix A
    /// lists `srinfo=suggestion` under the Action API's `list=search`, the
    /// documented *fallback* for full-text search, not the REST endpoint
    /// this client calls as primary. Added here as a schema extension
    /// (`#[serde(default)]`, so a real deployment that omits it just leaves
    /// this `None`, not an error) so MVP can surface did-you-mean (FR-SR-4
    /// / §7's zero-results row) without a second round trip to the Action
    /// API. The mock server emits it the same way, documented in its own
    /// comment.
    #[serde(default)]
    suggestion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchTitleResponse {
    pages: Vec<TitleSuggestion>,
}

/// Full-text search results plus the did-you-mean suggestion, when the
/// server offered one (PRD FR-SR-4).
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub suggestion: Option<String>,
}

impl WikiClient {
    /// `base_url_template` is the config-resolved host (PRD §6.2 rule 2):
    /// `{lang}` is substituted per-request when present, else used
    /// verbatim — arbitrary MediaWiki sites (FR-ML-5) aren't per-language
    /// subdomains, so a template without `{lang}` addresses one fixed host.
    pub fn new(base_url_template: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT_BASE)
            .gzip(true)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base_url_template,
        })
    }

    fn host(&self, lang: &str) -> String {
        if self.base_url_template.contains("{lang}") {
            self.base_url_template.replace("{lang}", lang)
        } else {
            self.base_url_template.clone()
        }
    }

    fn title_path(title: &str) -> String {
        urlencoding::encode(&title.replace(' ', "_")).into_owned()
    }

    /// Fetch Parsoid HTML for an article (PRD §6.2 rule 3: core REST
    /// `/w/rest.php/v1/page/{title}/html` is the primary content source).
    /// Returns `(canonical_title, html)`.
    pub async fn fetch_article_html(&self, lang: &str, title: &str) -> Result<(String, String)> {
        let url = format!(
            "{}/w/rest.php/v1/page/{}/html",
            self.host(lang),
            Self::title_path(title)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting article HTML for {title:?}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("no article named {title:?} on {lang}.wikipedia.org");
        }
        let resp = resp.error_for_status().context("fetching article HTML")?;
        let html = resp.text().await.context("reading article HTML body")?;
        Ok((title.to_string(), html))
    }

    /// Full-text search (PRD Appendix A: `GET /w/rest.php/v1/search/page`).
    pub async fn search(&self, lang: &str, query: &str, limit: u32) -> Result<SearchOutcome> {
        let url = format!(
            "{}/w/rest.php/v1/search/page?q={}&limit={}",
            self.host(lang),
            urlencoding::encode(query),
            limit
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting search results")?
            .error_for_status()
            .context("search request failed")?;
        let parsed: SearchPageResponse = resp.json().await.context("parsing search response")?;
        Ok(SearchOutcome {
            results: parsed.pages,
            suggestion: parsed.suggestion,
        })
    }

    /// Typeahead title completion (PRD FR-SR-1, Appendix A: `GET
    /// /w/rest.php/v1/search/title`). Called on a debounce timer, not on
    /// every keystroke, from a spawned task — see `main::fire_typeahead`.
    pub async fn search_title(
        &self,
        lang: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<TitleSuggestion>> {
        let url = format!(
            "{}/w/rest.php/v1/search/title?q={}&limit={}",
            self.host(lang),
            urlencoding::encode(query),
            limit
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting typeahead suggestions")?
            .error_for_status()
            .context("typeahead request failed")?;
        let parsed: SearchTitleResponse =
            resp.json().await.context("parsing typeahead response")?;
        Ok(parsed.pages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_substitutes_lang_in_the_default_template() {
        let client = WikiClient::new(crate::config::DEFAULT_BASE_URL_TEMPLATE.to_string()).unwrap();
        assert_eq!(client.host("de"), "https://de.wikipedia.org");
        assert_eq!(client.host("en"), "https://en.wikipedia.org");
    }

    /// The supported override this module documents: a config/env base URL
    /// with `{lang}` still gets per-request substitution (e.g. a mock
    /// server standing in for multiple language editions).
    #[test]
    fn host_substitutes_lang_in_a_configured_template() {
        let client = WikiClient::new("http://127.0.0.1:8943/{lang}".to_string()).unwrap();
        assert_eq!(client.host("ja"), "http://127.0.0.1:8943/ja");
    }

    /// A template with no `{lang}` placeholder — arbitrary MediaWiki sites
    /// (FR-ML-5, e.g. ArchWiki) aren't per-language subdomains — is used
    /// verbatim, ignoring whatever `lang` the caller passes.
    #[test]
    fn host_is_verbatim_when_template_has_no_lang_placeholder() {
        let client = WikiClient::new("http://127.0.0.1:8943".to_string()).unwrap();
        assert_eq!(client.host("en"), "http://127.0.0.1:8943");
        assert_eq!(client.host("de"), "http://127.0.0.1:8943");
    }

    /// FR-SR-4's did-you-mean: the `suggestion` field this module's own
    /// schema extension adds (see `SearchPageResponse`'s doc comment) must
    /// round-trip, and its absence must deserialize to `None`, not an
    /// error — a real deployment that never sends it must still work.
    #[test]
    fn search_page_response_parses_the_suggestion_extension() {
        let with_suggestion = r#"{"pages": [], "suggestion": "Alan Turing"}"#;
        let parsed: SearchPageResponse = serde_json::from_str(with_suggestion).unwrap();
        assert!(parsed.pages.is_empty());
        assert_eq!(parsed.suggestion.as_deref(), Some("Alan Turing"));

        let without_suggestion = r#"{"pages": []}"#;
        let parsed: SearchPageResponse = serde_json::from_str(without_suggestion).unwrap();
        assert_eq!(parsed.suggestion, None);
    }

    /// FR-SR-2's size/wordcount/timestamp are all optional per-result — a
    /// response that omits them (today's fixture-free REST schema) must
    /// still parse, not error, and a response that includes them must
    /// carry them through.
    #[test]
    fn search_result_size_wordcount_timestamp_are_optional() {
        let minimal = r#"{"pages": [{"title": "Alan Turing"}]}"#;
        let parsed: SearchPageResponse = serde_json::from_str(minimal).unwrap();
        assert_eq!(parsed.pages[0].size, None);
        assert_eq!(parsed.pages[0].wordcount, None);
        assert_eq!(parsed.pages[0].timestamp, None);

        let full = r#"{"pages": [{"title": "Alan Turing", "size": 4820, "wordcount": 812, "timestamp": "2026-06-30T10:15:00Z"}]}"#;
        let parsed: SearchPageResponse = serde_json::from_str(full).unwrap();
        assert_eq!(parsed.pages[0].size, Some(4820));
        assert_eq!(parsed.pages[0].wordcount, Some(812));
        assert_eq!(
            parsed.pages[0].timestamp.as_deref(),
            Some("2026-06-30T10:15:00Z")
        );
    }

    /// The typeahead schema: title + optional Wikidata-style description.
    #[test]
    fn search_title_response_parses_titles_and_descriptions() {
        let json = r#"{"pages": [
            {"title": "Alan Turing", "description": "British mathematician"},
            {"title": "Enigma machine"}
        ]}"#;
        let parsed: SearchTitleResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.pages.len(), 2);
        assert_eq!(parsed.pages[0].title, "Alan Turing");
        assert_eq!(
            parsed.pages[0].description.as_deref(),
            Some("British mathematician")
        );
        assert_eq!(parsed.pages[1].description, None);
    }
}
