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
}

#[derive(Debug, Deserialize)]
struct SearchPageResponse {
    pages: Vec<SearchResult>,
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
    pub async fn search(&self, lang: &str, query: &str, limit: u32) -> Result<Vec<SearchResult>> {
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
}
