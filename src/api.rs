//! MediaWiki API client. Per PRD §6.2: per-wiki endpoints only
//! (`{lang}.wikipedia.org`), never `api.wikimedia.org`. Per §6.5 (NF-NET-2)
//! every request carries a descriptive User-Agent.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const USER_AGENT_BASE: &str = concat!(
    "wikitui/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/iamfoz/wikitui) reqwest"
);

pub struct WikiClient {
    http: reqwest::Client,
    lang: String,
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
    pub fn new(lang: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT_BASE)
            .gzip(true)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            lang: lang.into(),
        })
    }

    fn host(&self) -> String {
        format!("https://{}.wikipedia.org", self.lang)
    }

    fn title_path(title: &str) -> String {
        urlencoding::encode(&title.replace(' ', "_")).into_owned()
    }

    /// Fetch Parsoid HTML for an article (PRD §6.2 rule 3: core REST
    /// `/w/rest.php/v1/page/{title}/html` is the primary content source).
    /// Returns `(canonical_title, html)`.
    pub async fn fetch_article_html(&self, title: &str) -> Result<(String, String)> {
        let url = format!(
            "{}/w/rest.php/v1/page/{}/html",
            self.host(),
            Self::title_path(title)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting article HTML for {title:?}"))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("no article named {title:?} on {}.wikipedia.org", self.lang);
        }
        let resp = resp.error_for_status().context("fetching article HTML")?;
        let html = resp.text().await.context("reading article HTML body")?;
        Ok((title.to_string(), html))
    }

    /// Full-text search (PRD Appendix A: `GET /w/rest.php/v1/search/page`).
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchResult>> {
        let url = format!(
            "{}/w/rest.php/v1/search/page?q={}&limit={}",
            self.host(),
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
