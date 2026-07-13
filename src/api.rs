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
//!
//! PRD SEC-3: every response body read in this module goes through
//! [`read_capped`] rather than reqwest's own unbounded `.text()`/`.json()`
//! (both buffer the whole body regardless of size) — see its doc comment.
//! PRD SEC-1: every field this module hands back that the UI displays
//! (search/typeahead titles, descriptions, excerpts) is sanitized once,
//! right after parsing, by `sanitize_search_result`/`sanitize_title_suggestion`
//! — the single choke point for this module, mirroring `doc.rs`'s
//! `sanitize_document`. Article HTML itself is deliberately NOT sanitized
//! here: it is still raw markup at this point, and `doc::parse_article_html`
//! is the module responsible for turning it into sanitized `Document` text.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;

const USER_AGENT_BASE: &str = concat!(
    "wikitui/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/iamfoz/wikitui) reqwest"
);

/// PRD SEC-3: search/typeahead responses are bounded by a `limit` query
/// param (≤100 results) and are never expected to approach this size in
/// practice; it exists purely as a hard ceiling against a broken or hostile
/// server, so search can't be made to buffer unbounded memory the way
/// article HTML could without its own cap.
const MAX_SEARCH_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// PRD SEC-3: reads a response body in chunks, stopping once more than
/// `cap` bytes have been buffered, instead of trusting `Content-Length` (a
/// hostile server can lie about it) or calling reqwest's own unbounded
/// `.text()`/`.bytes()`/`.json()`, all of which buffer the entire body
/// before this code ever sees it. A body larger than `cap` is returned
/// anyway, truncated to at most one chunk over the cap (reqwest/hyper
/// chunks are small, so the overshoot is bounded, not unbounded) — deciding
/// exactly where to cut and what to do about it (parse-anyway-and-flag vs.
/// error) is the caller's job; this function only bounds memory during the
/// read itself.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while buf.len() <= cap {
        match resp.chunk().await.context("reading response body")? {
            Some(chunk) => buf.extend_from_slice(&chunk),
            None => break,
        }
    }
    Ok(buf)
}

/// Decodes response bytes as UTF-8 with lossy replacement of malformed
/// sequences (`char::REPLACEMENT_CHARACTER`) — the same guarantee
/// `reqwest::Response::text()` documents (BOM-stripped, malformed-sequence
/// replacement; charset is UTF-8-only here since this crate doesn't enable
/// reqwest's `charset` feature, and Parsoid/MediaWiki REST responses are
/// always UTF-8 regardless). Replacing `.text()` with `read_capped` +
/// this function keeps that guarantee while adding the SEC-3 size cap
/// `.text()` doesn't have.
fn decode_lossy_utf8(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

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

#[derive(Debug, Deserialize)]
struct BareLatest {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct BareResponse {
    latest: BareLatest,
}

/// One freshly fetched article (PRD FR-OFF-1/2): Parsoid HTML plus whatever
/// revision identity the REST response exposed. `revid` is `0` when the
/// `ETag` was missing or unparseable (older mock endpoints, some
/// third-party wikis) — the two-layer cache degrades to title-keyed,
/// always-revalidated storage in that case (see `cache`'s module doc
/// comment) rather than refusing to open the article.
#[derive(Debug, Clone)]
pub struct FetchedArticle {
    pub html: String,
    pub revid: u64,
    pub etag: Option<String>,
}

/// Extracts the leading numeric revid from a Parsoid-shaped `ETag`
/// (`W/"1234567/uuid"`: weak-validator prefix and uuid suffix both
/// optional). PRD Appendix A's "Page metadata / latest revid" identity,
/// captured here so L2 storage can key content by revid instead of just a
/// title. Anything that doesn't parse to a leading integer — garbage, an
/// empty tag, a strong-validator quote with no slash-separated id — yields
/// `None` rather than an error: an unfamiliar `ETag` shape must still let
/// the article open, just without the immutability/SWR wins a real revid
/// enables (the caller's `unwrap_or(0)` is the documented fallback).
fn parse_revid_from_etag(etag: &str) -> Option<u64> {
    let s = etag.trim();
    let s = s.strip_prefix("W/").unwrap_or(s).trim();
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    let (first, _) = s.split_once('/').unwrap_or((s, ""));
    first.parse::<u64>().ok()
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
            // NF-NET-8: short timeouts with immediate cache fallback — a
            // hung request must not stall the reader (foreground fetches)
            // or leave a background revalidation (PRD FR-OFF-2) occupying
            // the "revalidation in flight" state indefinitely.
            .timeout(Duration::from_secs(5))
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
    pub async fn fetch_article_html(&self, lang: &str, title: &str) -> Result<FetchedArticle> {
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
        // Captured before the body read below consumes `resp` — PRD
        // FR-OFF-1/Appendix A's revid identity rides the ETag on this same
        // response, so there is no second round trip for it.
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let revid = etag.as_deref().and_then(parse_revid_from_etag).unwrap_or(0);
        // PRD SEC-3: bounds memory during the read itself; the authoritative
        // truncate-and-flag decision for oversized article HTML is
        // `doc::parse_article_html`'s own cap, run against whatever string
        // this returns (fresh fetch, on-disk cache, or a test fixture all go
        // through that same check).
        let bytes = read_capped(resp, crate::doc::MAX_ARTICLE_HTML_BYTES)
            .await
            .context("reading article HTML body")?;
        let html = decode_lossy_utf8(bytes);
        Ok(FetchedArticle { html, revid, etag })
    }

    /// The cheap revalidation call (PRD FR-OFF-2 / Appendix A's "Page
    /// metadata / latest revid" row): `GET /w/rest.php/v1/page/{title}/bare`
    /// returns just `{"latest": {"id": revid}}`, so a background staleness
    /// check costs a fraction of a full article re-fetch. Returns the
    /// latest revid only — the caller (`main::fire_revalidation`) compares
    /// it against what's cached via `cache::revalidate_action`.
    pub async fn fetch_bare_metadata(&self, lang: &str, title: &str) -> Result<u64> {
        let url = format!(
            "{}/w/rest.php/v1/page/{}/bare",
            self.host(lang),
            Self::title_path(title)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting bare metadata for {title:?}"))?
            .error_for_status()
            .context("bare metadata request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading bare metadata response body")?;
        let parsed: BareResponse =
            serde_json::from_slice(&bytes).context("parsing bare metadata response")?;
        Ok(parsed.latest.id)
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
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading search response body")?;
        let mut parsed: SearchPageResponse =
            serde_json::from_slice(&bytes).context("parsing search response")?;
        // PRD SEC-1: this module's choke point — every display field a
        // search response carries is sanitized here, once, before it can
        // reach `ui.rs`'s results list or its `searchmatch` excerpt parser.
        for result in &mut parsed.pages {
            sanitize_search_result(result);
        }
        let suggestion = parsed
            .suggestion
            .map(|s| crate::sanitize::sanitize_single_line(&s).into_owned());
        Ok(SearchOutcome {
            results: parsed.pages,
            suggestion,
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
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading typeahead response body")?;
        let mut parsed: SearchTitleResponse =
            serde_json::from_slice(&bytes).context("parsing typeahead response")?;
        // PRD SEC-1: same choke point as `search`, for the typeahead dropdown.
        for suggestion in &mut parsed.pages {
            sanitize_title_suggestion(suggestion);
        }
        Ok(parsed.pages)
    }
}

/// PRD SEC-1: sanitizes every field of one full-text search result that the
/// UI displays as a single line (title, description). `excerpt` carries
/// deliberate `<span class="searchmatch">...</span>` markup that
/// `ui::parse_searchmatch` parses back out — sanitizing doesn't touch `<`/
/// `>`/ordinary ASCII, only control/bidi/zero-width characters, so the
/// markup survives untouched while any hostile bytes inside the excerpt
/// text don't.
fn sanitize_search_result(result: &mut SearchResult) {
    result.title = crate::sanitize::sanitize_single_line(&result.title).into_owned();
    if let Some(description) = &mut result.description {
        *description = crate::sanitize::sanitize_single_line(description).into_owned();
    }
    if let Some(excerpt) = &mut result.excerpt {
        *excerpt = crate::sanitize::sanitize_single_line(excerpt).into_owned();
    }
}

/// PRD SEC-1: the typeahead-dropdown counterpart of `sanitize_search_result`.
fn sanitize_title_suggestion(suggestion: &mut TitleSuggestion) {
    suggestion.title = crate::sanitize::sanitize_single_line(&suggestion.title).into_owned();
    if let Some(description) = &mut suggestion.description {
        *description = crate::sanitize::sanitize_single_line(description).into_owned();
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

    /// PRD SEC-1: `sanitize_search_result` is the module's choke point for
    /// full-text search fields — a hostile title/description/excerpt must
    /// come out free of control bytes while the `searchmatch` markup
    /// `ui::parse_searchmatch` depends on survives untouched.
    #[test]
    fn sanitize_search_result_strips_control_bytes_but_keeps_searchmatch_markup() {
        let mut result = SearchResult {
            title: "Evil\x1b[31mTitle".to_string(),
            description: Some("desc\x07ription".to_string()),
            excerpt: Some(
                r#"before <span class="searchmatch">hit\x1b</span> after"#.replace("\\x1b", "\x1b"),
            ),
            size: None,
            wordcount: None,
            timestamp: None,
        };
        sanitize_search_result(&mut result);
        assert!(!result.title.contains('\x1b'));
        assert!(!result.description.unwrap().contains('\x07'));
        let excerpt = result.excerpt.unwrap();
        assert!(!excerpt.contains('\x1b'));
        assert!(
            excerpt.contains(r#"<span class="searchmatch">"#),
            "searchmatch markup must survive sanitization: {excerpt:?}"
        );
    }

    #[test]
    fn sanitize_title_suggestion_strips_control_bytes() {
        let mut suggestion = TitleSuggestion {
            title: "Al\x1ban Turing".to_string(),
            description: Some("bri\x07tish".to_string()),
        };
        sanitize_title_suggestion(&mut suggestion);
        assert_eq!(suggestion.title, "Alan Turing");
        assert_eq!(suggestion.description.as_deref(), Some("british"));
    }

    /// PRD SEC-3: the network-level read cap bounds memory for an oversized
    /// (or hostile) response body instead of buffering it in full the way
    /// reqwest's own `.text()`/`.bytes()`/`.json()` would. A tiny raw
    /// std-socket server (no test-only HTTP framework dependency needed)
    /// serves a body well over `doc::MAX_ARTICLE_HTML_BYTES`; the client
    /// must come back with something bounded near the cap, not the full
    /// body.
    #[tokio::test]
    async fn fetch_article_html_bounds_the_network_read_for_an_oversized_body() {
        use std::io::{Read, Write};

        let oversized = crate::doc::MAX_ARTICLE_HTML_BYTES + 5_000_000;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body = vec![b'A'; oversized];
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });

        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let fetched = client
            .fetch_article_html("en", "Test")
            .await
            .expect("capped read must still succeed, not error, on an oversized body");
        assert!(
            fetched.html.len() < oversized,
            "must not buffer the full oversized body, got {} of {oversized}",
            fetched.html.len()
        );
        assert!(
            fetched.html.len() <= crate::doc::MAX_ARTICLE_HTML_BYTES + 4_000_000,
            "overshoot past the cap must be bounded (a few chunks at most), got {}",
            fetched.html.len()
        );
    }

    /// PRD FR-OFF-1/2: the `ETag` header on a Parsoid HTML response is
    /// captured verbatim and its leading revid parsed out, so L2 storage
    /// can key content by revision instead of just title.
    #[tokio::test]
    async fn fetch_article_html_captures_etag_and_parses_its_revid() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body = b"<html><body>hi</body></html>";
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nETag: W/\"4242/mock-uuid\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let fetched = client.fetch_article_html("en", "Test").await.unwrap();
        assert_eq!(fetched.etag.as_deref(), Some("W/\"4242/mock-uuid\""));
        assert_eq!(fetched.revid, 4242);
    }

    /// An article response with no `ETag` at all degrades to `revid = 0`
    /// rather than failing the fetch — the documented fallback for older
    /// mock endpoints and third-party wikis.
    #[tokio::test]
    async fn fetch_article_html_with_no_etag_degrades_to_revid_zero() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body = b"<html><body>hi</body></html>";
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let fetched = client.fetch_article_html("en", "Test").await.unwrap();
        assert_eq!(fetched.etag, None);
        assert_eq!(fetched.revid, 0);
    }

    /// PRD Appendix A's cheap "Page metadata / latest revid" call.
    #[tokio::test]
    async fn fetch_bare_metadata_parses_the_latest_revid() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body = br#"{"latest": {"id": 99}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let revid = client.fetch_bare_metadata("en", "Test").await.unwrap();
        assert_eq!(revid, 99);
    }

    /// `parse_revid_from_etag`'s full behavior table: well-formed weak
    /// validators, a bare quoted number, a strong validator, and garbage —
    /// only the first two categories carry a parseable revid.
    #[test]
    fn parse_revid_from_etag_table() {
        assert_eq!(
            parse_revid_from_etag(r#"W/"1234567/some-uuid""#),
            Some(1_234_567),
            "well-formed weak validator with a uuid suffix"
        );
        assert_eq!(
            parse_revid_from_etag(r#"W/"555""#),
            Some(555),
            "weak validator with no slash suffix"
        );
        assert_eq!(
            parse_revid_from_etag(r#""777/uuid""#),
            Some(777),
            "strong validator (no W/ prefix) still parses"
        );
        assert_eq!(
            parse_revid_from_etag(r#""not-a-number""#),
            None,
            "garbage payload must not panic or fake a revid"
        );
        assert_eq!(parse_revid_from_etag(""), None, "empty tag");
        assert_eq!(
            parse_revid_from_etag(r#"W/"/no-leading-number""#),
            None,
            "missing the leading integer entirely"
        );
    }
}
