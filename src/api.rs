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
use std::collections::HashMap;
use std::time::Duration;

/// The canonical project URL and HTTP-library token for the User-Agent
/// (PRD §6.5 NF-NET-2). `HTTP_LIB` tracks the `reqwest` version pinned in
/// `Cargo.toml` — bump it there and here together.
const REPO_URL: &str = "https://github.com/iamfoz/wikitui";
const HTTP_LIB: &str = "reqwest/0.13";

/// Default contact channel embedded in the User-Agent when none is configured
/// — the project's issue tracker, so a WMF operator hitting a policy problem
/// can reach a human (the exact failure mode that 403'd wiki-tui, #267).
/// Overridable via `[network] contact` / `WIKITUI_CONTACT` (PRD §6.2 rule 2).
pub const DEFAULT_CONTACT: &str = "https://github.com/iamfoz/wikitui/issues";

/// PRD §6.5 NF-NET-2: `wikitui/{ver} (repo; {contact}) {lib}/{ver}` on 100% of
/// requests. Built once and handed to reqwest's `user_agent`, which stamps it
/// on every request the client makes — foreground and background alike — so
/// there is no code path that can omit it (the wiki-tui #267 regression).
pub fn build_user_agent(contact: &str) -> String {
    format!(
        "wikitui/{} ({REPO_URL}; {contact}) {HTTP_LIB}",
        env!("CARGO_PKG_VERSION")
    )
}

/// PRD §6.5 NF-NET-3: `maxlag=5` on every non-interactive (background)
/// request. Interactive foreground fetches (article open, search, typeahead)
/// deliberately do not carry it — the reader is waiting on those.
const MAXLAG: &str = "5";

/// Why a *background* request failed, classified so the substrate's circuit
/// breaker and Retry-After handling (NF-NET-4) can react correctly. Foreground
/// methods keep returning `anyhow::Error`; only the background variants below
/// need this finer split.
#[derive(Debug)]
pub enum BgFailure {
    /// HTTP 429, or a 503/maxlag lag signal — honor `Retry-After` and trip the
    /// breaker (NF-NET-3/4).
    RateLimited { retry_after: Option<Duration> },
    /// Any other 5xx — trips the breaker; the worker backs off.
    ServerError,
    /// Timeout, connection error, or a body that wouldn't parse — backoff,
    /// but not a breaker trip (NF-NET-4 reserves that for 429/5xx).
    Network,
}

/// A background article fetch result, carrying the byte size so the queue can
/// charge it against the daily prefetch byte budget (FR-PF-5 / NF-NET-7).
#[derive(Debug, Clone)]
pub struct BgArticle {
    pub html: String,
    pub revid: u64,
    pub etag: Option<String>,
    pub bytes: u64,
}

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

/// The subset of `/api/rest_v1/page/summary/{title}` this client consumes
/// (PRD Appendix A): just the plain-text extract that powers T2 link-peek.
#[derive(Debug, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    extract: String,
}

/// One interwiki language edition of an article (PRD FR-ML-1/2, Appendix A
/// "Langlinks"): the language code, its own name for itself (`autonym`,
/// e.g. "Deutsch"), the English name (`langname`, e.g. "German") FR-ML-1's
/// picker rows show alongside it ("Deutsch (German)"), the article's title
/// in that edition, and — when the server sends it — the interwiki URL.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LangLink {
    #[serde(rename = "lang")]
    pub code: String,
    #[serde(default)]
    pub autonym: String,
    #[serde(default)]
    pub langname: String,
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
}

/// `prop=langlinks`'s response shape (formatversion=2): a single queried
/// page carrying its own `langlinks` array, empty or absent for an article
/// with none (a stub, or a title this wiki has no interwiki record for).
#[derive(Debug, Deserialize, Default)]
struct LangLinksResponse {
    #[serde(default)]
    query: Option<LangLinksQuery>,
}

#[derive(Debug, Deserialize)]
struct LangLinksQuery {
    #[serde(default)]
    pages: Vec<LangLinksPage>,
}

#[derive(Debug, Deserialize)]
struct LangLinksPage {
    #[serde(default)]
    langlinks: Vec<LangLink>,
}

/// The subset of `list=categorymembers` (formatversion=2) this client reads.
#[derive(Debug, Deserialize)]
struct CategoryMembersResponse {
    query: CategoryMembersQuery,
}

#[derive(Debug, Deserialize)]
struct CategoryMembersQuery {
    #[serde(default)]
    categorymembers: Vec<CategoryMember>,
}

#[derive(Debug, Deserialize)]
struct CategoryMember {
    title: String,
}

/// `list=random` (formatversion=2), PRD FR-SR-5 / Appendix A "Random".
#[derive(Debug, Deserialize, Default)]
struct RandomResponse {
    #[serde(default)]
    query: Option<RandomQuery>,
}

#[derive(Debug, Deserialize)]
struct RandomQuery {
    #[serde(default)]
    random: Vec<RandomPage>,
}

#[derive(Debug, Deserialize)]
struct RandomPage {
    title: String,
}

/// `prop=pageassessments` (formatversion2), PRD FR-DL-3 / Appendix A
/// "Quality". A page can carry a different class per WikiProject; only the
/// per-project `class` string is read.
#[derive(Debug, Deserialize, Default)]
struct PageAssessmentsResponse {
    #[serde(default)]
    query: Option<PageAssessmentsQuery>,
}

#[derive(Debug, Deserialize)]
struct PageAssessmentsQuery {
    #[serde(default)]
    pages: Vec<PageAssessmentsPage>,
}

#[derive(Debug, Deserialize)]
struct PageAssessmentsPage {
    title: String,
    #[serde(default)]
    pageassessments: HashMap<String, ProjectAssessment>,
}

#[derive(Debug, Deserialize, Default)]
struct ProjectAssessment {
    #[serde(default)]
    class: String,
}

/// PRD FR-DL-3's quality classes as `prop=pageassessments` reports them
/// (Appendix A "Quality"), ordered low→high so `Ord`/`>=` express "at least
/// as good as" directly — FR-SR-5's "random good article" filter is exactly
/// `>= Ga`. Only the six classes the PRD names are recognized; anything
/// else a real wiki reports (`FL`, `A`, `Disambig`, `List`, `NA`,
/// `Redirect`, …) parses to `None` rather than inventing a rank for it —
/// full quality-badge classification (FR-DL-3's `★FA/+GA/B/C/Start/Stub`
/// display) is a separate, unshipped feature; this enum only needs to
/// answer "good or better" for the random-good filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QualityClass {
    Stub,
    Start,
    C,
    B,
    Ga,
    Fa,
}

impl QualityClass {
    /// Parses one WikiProject's `class` string. Case-sensitive (the real
    /// API's values are consistently these exact tokens); an unrecognized
    /// or empty string is `None`, not a guess.
    pub fn parse(class: &str) -> Option<Self> {
        match class.trim() {
            "FA" => Some(Self::Fa),
            "GA" => Some(Self::Ga),
            "B" => Some(Self::B),
            "C" => Some(Self::C),
            "Start" => Some(Self::Start),
            "Stub" => Some(Self::Stub),
            _ => None,
        }
    }

    /// PRD FR-SR-5's "random good article" filter: assessment ≥ GA.
    pub fn is_good_or_better(self) -> bool {
        self >= Self::Ga
    }
}

#[derive(Debug, Deserialize)]
struct BareResponse {
    latest: BareLatest,
}

/// FR-PF-1's `generator=links` + `prop=pageviews` response (formatversion=2).
/// Each page carries a per-day `pageviews` map (values may be null); the
/// client sums the non-null days into a single total per title.
#[derive(Debug, Deserialize)]
struct PageviewsResponse {
    #[serde(default)]
    query: Option<PageviewsQuery>,
}

#[derive(Debug, Deserialize)]
struct PageviewsQuery {
    #[serde(default)]
    pages: Vec<PageviewsPage>,
}

#[derive(Debug, Deserialize)]
struct PageviewsPage {
    #[serde(default)]
    title: String,
    #[serde(default)]
    pageviews: Option<HashMap<String, Option<u64>>>,
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
        Self::with_contact(base_url_template, DEFAULT_CONTACT)
    }

    /// Like [`new`](Self::new) but with a configured contact channel for the
    /// User-Agent (PRD NF-NET-2 / §6.2 rule 2's config-overridable networking).
    pub fn with_contact(base_url_template: String, contact: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(build_user_agent(contact))
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

    /// Fetch a raw image (thumbnail) by absolute URL for inline rendering
    /// (PRD FR-RD-8). The URL is a sanitized http(s) media URL from the
    /// document model (`doc::sanitize_image_src` already rejected other
    /// schemes); this refuses anything else defensively. The body is read
    /// through the same size-capping reader as article HTML (PRD SEC-3), at a
    /// thumbnail-appropriate cap, so a hostile/huge asset can't exhaust
    /// memory.
    pub async fn fetch_image(&self, url: &str) -> Result<Vec<u8>> {
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            bail!("refusing to fetch non-http(s) image URL {url:?}");
        }
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("requesting image {url:?}"))?
            .error_for_status()
            .context("fetching image")?;
        const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
        read_capped(resp, MAX_IMAGE_BYTES)
            .await
            .context("reading image body")
    }

    /// A link-target summary/extract (PRD Appendix A's "Summary" row: `GET
    /// /api/rest_v1/page/summary/{title}`), for T2 saved pages (FR-OFF-4) so
    /// link-peek resolves offline. Returns the plain-text extract, sanitized
    /// (SEC-1) since it becomes stored, displayable text. Batching the
    /// extracts (`action=query&prop=extracts`, `exlimit≤20`) is Appendix A's
    /// documented optimization and a future seam; this per-title call keeps
    /// the T2 path simple.
    pub async fn fetch_summary(&self, lang: &str, title: &str) -> Result<String> {
        let url = format!(
            "{}/api/rest_v1/page/summary/{}",
            self.host(lang),
            Self::title_path(title)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting summary for {title:?}"))?
            .error_for_status()
            .context("summary request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading summary response body")?;
        let parsed: SummaryResponse =
            serde_json::from_slice(&bytes).context("parsing summary response")?;
        Ok(crate::sanitize::sanitize_single_line(&parsed.extract).into_owned())
    }

    /// PRD FR-ML-1/2 (Appendix A "Langlinks"): an article's interwiki
    /// language editions. Built against the Action API's `prop=langlinks&
    /// llprop=autonym|langname|url` rather than Appendix A's other listed
    /// primary, the core REST `/page/{title}/links/language` endpoint: that
    /// REST shape carries only `name` (the autonym), never an English
    /// `langname` — and FR-ML-1's picker rows need both ("Deutsch
    /// (German)") — so the Action API call is the one that actually
    /// satisfies the requirement here, not a fallback-of-convenience.
    /// One title per call (never a fanout): unlike `page_assessments`/
    /// `fetch_link_pageviews`, there is exactly one article's langlinks to
    /// ask for at a time — the article on screen — so there is nothing to
    /// batch across titles; `lllimit=500` only bounds the *number of
    /// editions* one article can return, per §6.2 rule 10's batching cap.
    /// A foreground, interactive fetch — the reader pressed `:lang`, or an
    /// article just opened and the app wants FR-ML-2's "available in your
    /// preferred language" hint for it — so, like `search`/
    /// `fetch_onthisday_feed`, it carries no `maxlag` (NF-NET-3 reserves
    /// that for the non-interactive `_bg` methods); NF-NET-2's User-Agent
    /// still rides every request via the client's own `reqwest::Client`.
    pub async fn fetch_langlinks(&self, lang: &str, title: &str) -> Result<Vec<LangLink>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&prop=langlinks&llprop=autonym|langname|url&lllimit=500&titles={}",
            self.host(lang),
            urlencoding::encode(&title.replace(' ', "_"))
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting language links for {title:?}"))?
            .error_for_status()
            .context("language links request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading language links response body")?;
        let parsed: LangLinksResponse =
            serde_json::from_slice(&bytes).context("parsing language links response")?;
        let mut links: Vec<LangLink> = parsed
            .query
            .map(|q| q.pages)
            .unwrap_or_default()
            .into_iter()
            .flat_map(|p| p.langlinks)
            .collect();
        // PRD SEC-1: this module's choke point for langlinks — every field
        // the picker displays (autonym/langname/title/code) is sanitized
        // here, once, mirroring `sanitize_search_result`.
        for link in &mut links {
            sanitize_langlink(link);
        }
        Ok(links)
    }

    /// FR-DL-2's `:today` panel: one Wikifeeds `feed/onthisday/{type}/{m}/{d}`
    /// call per event type (events/births/deaths/holidays/selected). This is
    /// a *foreground* fetch — the reader explicitly asked to see today in
    /// history, the same category as `search`/`fetch_summary` — so unlike
    /// [`Self::fetch_featured_feed`] (the background FR-PF-2 daily seed) it
    /// does not carry `maxlag` (§6.5 NF-NET-3 reserves that for
    /// non-interactive/background traffic) and returns a plain
    /// `anyhow::Result` rather than the background failure enum. Returns the
    /// raw body for [`crate::prefetch::parse_onthisday`] to parse.
    pub async fn fetch_onthisday_feed(
        &self,
        lang: &str,
        event_type: &str,
        month: u32,
        day: u32,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/api/rest_v1/feed/onthisday/{event_type}/{month:02}/{day:02}",
            self.host(lang)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting on-this-day feed ({event_type})"))?
            .error_for_status()
            .context("on-this-day feed request failed")?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading on-this-day feed response body")
    }

    /// The members of a category (PRD FR-OFF-5's bulk-save-by-category:
    /// Appendix A's `list=categorymembers`, depth 1). Namespace-0 article
    /// titles only, capped at `limit`. `cat` may be given with or without the
    /// `Category:` prefix.
    pub async fn fetch_category_members(
        &self,
        lang: &str,
        cat: &str,
        limit: u32,
    ) -> Result<Vec<String>> {
        let cmtitle = if cat.to_ascii_lowercase().starts_with("category:") {
            cat.to_string()
        } else {
            format!("Category:{cat}")
        };
        let url = format!(
            "{}/w/api.php?action=query&list=categorymembers&cmtitle={}&cmtype=page&cmlimit={}&format=json&formatversion=2",
            self.host(lang),
            urlencoding::encode(&cmtitle),
            limit
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting category members for {cat:?}"))?
            .error_for_status()
            .context("categorymembers request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading categorymembers response body")?;
        let parsed: CategoryMembersResponse =
            serde_json::from_slice(&bytes).context("parsing categorymembers response")?;
        Ok(parsed
            .query
            .categorymembers
            .into_iter()
            .map(|m| crate::sanitize::sanitize_single_line(&m.title).into_owned())
            .collect())
    }

    /// PRD FR-SR-5 / Appendix A "Random": `list=random` restricted to the
    /// main namespace (`rnnamespace=0` — never talk/category/user pages).
    /// `limit` is 1 for the plain `gr`/`:random` binding and
    /// [`crate::random::GOOD_BATCH`] for `:random good`'s batched pick
    /// (§6.2 rule 10: one call for the whole batch, never per-title
    /// fanout). Titles are sanitized (SEC-1): they become both a display
    /// title and a fetch target.
    pub async fn random_titles(&self, lang: &str, limit: u32) -> Result<Vec<String>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&list=random&rnnamespace=0&rnlimit={limit}",
            self.host(lang)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting a random article")?
            .error_for_status()
            .context("random article request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading random article response body")?;
        let parsed: RandomResponse =
            serde_json::from_slice(&bytes).context("parsing random article response")?;
        Ok(parsed
            .query
            .map(|q| q.random)
            .unwrap_or_default()
            .into_iter()
            .map(|p| crate::sanitize::sanitize_single_line(&p.title).into_owned())
            .collect())
    }

    /// PRD FR-DL-3/FR-SR-5 (Appendix A "Quality"): the batched
    /// `prop=pageassessments` call — every title from one `:random good`
    /// batch in a single request (NF-NET-5), never per-article fanout.
    /// Returns the *best* class per title (max across whichever WikiProjects
    /// assessed it); a title absent from the response, or present with no
    /// class this client recognizes, is simply absent from the map — the
    /// caller (`random::pick_first_good`) treats "no entry" as "not good
    /// enough" either way.
    pub async fn page_assessments(
        &self,
        lang: &str,
        titles: &[String],
    ) -> Result<HashMap<String, QualityClass>> {
        let joined = titles.join("|");
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&prop=pageassessments&titles={}",
            self.host(lang),
            urlencoding::encode(&joined)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting page assessments")?
            .error_for_status()
            .context("page assessments request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading page assessments response body")?;
        let parsed: PageAssessmentsResponse =
            serde_json::from_slice(&bytes).context("parsing page assessments response")?;
        let mut best = HashMap::new();
        for page in parsed.query.map(|q| q.pages).unwrap_or_default() {
            let title = crate::sanitize::sanitize_single_line(&page.title).into_owned();
            let top = page
                .pageassessments
                .values()
                .filter_map(|p| QualityClass::parse(&p.class))
                .max();
            if let Some(class) = top {
                best.insert(title, class);
            }
        }
        Ok(best)
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

    /// Background article fetch (PRD FR-PF-1/2 prefetch, FR-OFF-2 revalidation
    /// re-fetch): Parsoid HTML with `maxlag=5` (NF-NET-3) and failures
    /// classified for the substrate breaker (NF-NET-4). Returns the byte size
    /// so the queue can charge it against the daily prefetch budget (FR-PF-5).
    pub async fn fetch_article_html_bg(
        &self,
        lang: &str,
        title: &str,
    ) -> std::result::Result<BgArticle, BgFailure> {
        let url = format!(
            "{}/w/rest.php/v1/page/{}/html?maxlag={MAXLAG}",
            self.host(lang),
            Self::title_path(title)
        );
        let resp = self.http.get(&url).send().await.map_err(bg_send_error)?;
        if let Some(fail) = bg_status_failure(&resp) {
            return Err(fail);
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let revid = etag.as_deref().and_then(parse_revid_from_etag).unwrap_or(0);
        let bytes = read_capped(resp, crate::doc::MAX_ARTICLE_HTML_BYTES)
            .await
            .map_err(|_| BgFailure::Network)?;
        let len = bytes.len() as u64;
        let html = decode_lossy_utf8(bytes);
        Ok(BgArticle {
            html,
            revid,
            etag,
            bytes: len,
        })
    }

    /// Background revalidation metadata (PRD FR-OFF-2): the cheap `bare` call
    /// with `maxlag=5`, classified for the breaker. Mirrors
    /// [`fetch_bare_metadata`](Self::fetch_bare_metadata) but on the
    /// background failure model.
    pub async fn fetch_bare_metadata_bg(
        &self,
        lang: &str,
        title: &str,
    ) -> std::result::Result<u64, BgFailure> {
        let url = format!(
            "{}/w/rest.php/v1/page/{}/bare?maxlag={MAXLAG}",
            self.host(lang),
            Self::title_path(title)
        );
        let resp = self.http.get(&url).send().await.map_err(bg_send_error)?;
        if let Some(fail) = bg_status_failure(&resp) {
            return Err(fail);
        }
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .map_err(|_| BgFailure::Network)?;
        let parsed: BareResponse =
            serde_json::from_slice(&bytes).map_err(|_| BgFailure::Network)?;
        Ok(parsed.latest.id)
    }

    /// FR-PF-1's ranking primitive (§6.2 rule 7 / Appendix A): the outgoing
    /// links of `source_title` joined with their pageviews in **one** batched
    /// call (`generator=links` + `prop=pageviews`, ≤50 links) — never a
    /// per-article fanout. Returns `title -> total views over the window` plus
    /// the response byte size. Titles are sanitized (SEC-1) since they become
    /// displayed `:prefetch-log` rows and fetch targets.
    pub async fn fetch_link_pageviews(
        &self,
        lang: &str,
        source_title: &str,
    ) -> std::result::Result<(HashMap<String, u64>, u64), BgFailure> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&generator=links&titles={}&gpllimit=50&gplnamespace=0&prop=pageviews&pvipdays=1&maxlag={MAXLAG}",
            self.host(lang),
            urlencoding::encode(&source_title.replace(' ', "_"))
        );
        let resp = self.http.get(&url).send().await.map_err(bg_send_error)?;
        if let Some(fail) = bg_status_failure(&resp) {
            return Err(fail);
        }
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .map_err(|_| BgFailure::Network)?;
        let len = bytes.len() as u64;
        let parsed: PageviewsResponse =
            serde_json::from_slice(&bytes).map_err(|_| BgFailure::Network)?;
        let mut map = HashMap::new();
        for page in parsed.query.map(|q| q.pages).unwrap_or_default() {
            let total: u64 = page
                .pageviews
                .unwrap_or_default()
                .values()
                .flatten()
                .copied()
                .sum();
            let title = crate::sanitize::sanitize_single_line(&page.title).into_owned();
            map.insert(title, total);
        }
        Ok((map, len))
    }

    /// FR-PF-2 / FR-DL-1: the one daily Wikifeeds featured-content call
    /// (`feed/featured/{y}/{m}/{d}`), with `maxlag=5`. Returns the raw body
    /// (parsed by [`crate::prefetch::FeaturedFeed`]) and its byte size.
    pub async fn fetch_featured_feed(
        &self,
        lang: &str,
        year: i32,
        month: u32,
        day: u32,
    ) -> std::result::Result<(Vec<u8>, u64), BgFailure> {
        let url = format!(
            "{}/api/rest_v1/feed/featured/{year:04}/{month:02}/{day:02}?maxlag={MAXLAG}",
            self.host(lang)
        );
        let resp = self.http.get(&url).send().await.map_err(bg_send_error)?;
        if let Some(fail) = bg_status_failure(&resp) {
            return Err(fail);
        }
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .map_err(|_| BgFailure::Network)?;
        let len = bytes.len() as u64;
        Ok((bytes, len))
    }
}

/// A reqwest send error (timeout, DNS, connection reset) is always a plain
/// network failure — not a rate-limit — so it backs off without tripping the
/// breaker (NF-NET-4).
fn bg_send_error(_e: reqwest::Error) -> BgFailure {
    BgFailure::Network
}

/// Classifies a background response's status (NF-NET-4): 429/503 are
/// rate-limit/lag signals that honor `Retry-After` and trip the breaker; other
/// 5xx trip the breaker on backoff; any other non-2xx (404 for a deleted link
/// target, say) is a terminal network-class failure. `None` = success.
fn bg_status_failure(resp: &reqwest::Response) -> Option<BgFailure> {
    let status = resp.status();
    if status.is_success() {
        return None;
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        return Some(BgFailure::RateLimited {
            retry_after: parse_retry_after(resp.headers()),
        });
    }
    if status.is_server_error() {
        return Some(BgFailure::ServerError);
    }
    Some(BgFailure::Network)
}

/// PRD NF-NET-4: the `Retry-After` header as a `Duration`. Only the
/// delta-seconds form is parsed (the HTTP-date form falls back to the
/// substrate's default pause) — Wikimedia emits seconds.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
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

/// PRD SEC-1: the langlinks counterpart of `sanitize_search_result` — every
/// field FR-ML-1's picker can put on screen (autonym, English langname,
/// translated title, and the short code shown in `[brackets]`) comes out
/// free of control/bidi/zero-width bytes. `url` isn't rendered by this
/// chunk's UI, but is sanitized too so a later "open in browser" doesn't
/// have to remember to add it here.
fn sanitize_langlink(link: &mut LangLink) {
    link.code = crate::sanitize::sanitize_single_line(&link.code).into_owned();
    link.autonym = crate::sanitize::sanitize_single_line(&link.autonym).into_owned();
    link.langname = crate::sanitize::sanitize_single_line(&link.langname).into_owned();
    link.title = crate::sanitize::sanitize_single_line(&link.title).into_owned();
    if let Some(url) = &mut link.url {
        *url = crate::sanitize::sanitize_single_line(url).into_owned();
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

    /// PRD Appendix A "Summary" (FR-OFF-4 T2): the extract field powers
    /// offline link-peek; a response missing it degrades to empty, not error.
    #[test]
    fn summary_response_parses_the_extract() {
        let json = r#"{"title": "Alan Turing", "extract": "A mathematician."}"#;
        let parsed: SummaryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.extract, "A mathematician.");
        let missing = r#"{"title": "X"}"#;
        let parsed: SummaryResponse = serde_json::from_str(missing).unwrap();
        assert_eq!(parsed.extract, "");
    }

    /// PRD FR-OFF-5's `list=categorymembers`: member titles are extracted, and
    /// an empty/absent list is not an error.
    #[test]
    fn categorymembers_response_parses_member_titles() {
        let json = r#"{"query": {"categorymembers": [
            {"title": "Alan Turing"}, {"title": "Computer science"}
        ]}}"#;
        let parsed: CategoryMembersResponse = serde_json::from_str(json).unwrap();
        let titles: Vec<_> = parsed
            .query
            .categorymembers
            .iter()
            .map(|m| m.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Alan Turing", "Computer science"]);

        let empty = r#"{"query": {}}"#;
        let parsed: CategoryMembersResponse = serde_json::from_str(empty).unwrap();
        assert!(parsed.query.categorymembers.is_empty());
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

    /// PRD NF-NET-2: the User-Agent matches the mandated
    /// `wikitui/{ver} (repo; contact) lib/ver` shape — the fix for wiki-tui's
    /// #267 403 breakage.
    #[test]
    fn user_agent_matches_the_nf_net_2_shape() {
        let ua = build_user_agent(DEFAULT_CONTACT);
        assert!(ua.starts_with("wikitui/"));
        assert!(ua.contains("(https://github.com/iamfoz/wikitui; "));
        assert!(ua.contains("reqwest/"));
        assert!(ua.contains("iamfoz/wikitui/issues"));
    }

    /// PRD NF-NET-2/NF-NET-3: a background request carries the descriptive
    /// User-Agent and `maxlag=5`. A raw std-socket server captures the request
    /// head so both are asserted on the wire.
    #[tokio::test]
    async fn background_request_carries_user_agent_and_maxlag() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                *cap2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = b"<html><body>hi</body></html>";
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nETag: W/\"1/x\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let _ = client.fetch_article_html_bg("en", "Test").await;
        let head = captured.lock().unwrap().clone();
        assert!(
            head.to_lowercase().contains("user-agent: wikitui/"),
            "UA present on the wire: {head:?}"
        );
        assert!(
            head.contains("github.com/iamfoz/wikitui"),
            "UA carries the repo URL"
        );
        assert!(
            head.contains("maxlag=5"),
            "maxlag=5 on the request: {head:?}"
        );
    }

    /// PRD §6.5 NF-NET-3: `fetch_onthisday_feed` (FR-DL-2's `:today`) is a
    /// *foreground* fetch, so — unlike `fetch_featured_feed`'s background
    /// counterpart above — it must NOT carry `maxlag`, while still carrying
    /// the mandated User-Agent (NF-NET-2 applies to every request, fore- and
    /// background alike).
    #[tokio::test]
    async fn onthisday_fetch_carries_user_agent_but_not_maxlag() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                *cap2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = br#"{"events": []}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let result = client.fetch_onthisday_feed("en", "events", 7, 14).await;
        assert!(result.is_ok());
        let head = captured.lock().unwrap().clone();
        assert!(
            head.contains("GET /api/rest_v1/feed/onthisday/events/07/14"),
            "path shape: {head:?}"
        );
        assert!(
            head.to_lowercase().contains("user-agent: wikitui/"),
            "UA present on the wire: {head:?}"
        );
        assert!(
            !head.contains("maxlag"),
            "foreground fetch must not carry maxlag: {head:?}"
        );
    }

    /// PRD NF-NET-4: a 429 becomes `RateLimited` carrying the parsed
    /// `Retry-After` so the substrate can honor it.
    #[tokio::test]
    async fn background_429_is_rate_limited_with_retry_after() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let header = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(header.as_bytes());
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let err = client
            .fetch_article_html_bg("en", "Test")
            .await
            .unwrap_err();
        match err {
            BgFailure::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(7)))
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// FR-PF-1: the batched pageviews response sums each title's per-day counts
    /// (nulls skipped) into one total — the join primitive for link ranking.
    #[test]
    fn pageviews_response_sums_daily_counts_per_title() {
        let json = br#"{"query":{"pages":[
            {"title":"Enigma machine","pageviews":{"2026-07-13":100,"2026-07-12":null,"2026-07-11":50}},
            {"title":"Computer science","pageviews":{"2026-07-13":10}}
        ]}}"#;
        let parsed: PageviewsResponse = serde_json::from_slice(json).unwrap();
        let mut totals = std::collections::HashMap::new();
        for p in parsed.query.unwrap().pages {
            let total: u64 = p
                .pageviews
                .unwrap_or_default()
                .values()
                .flatten()
                .copied()
                .sum();
            totals.insert(p.title, total);
        }
        assert_eq!(totals.get("Enigma machine"), Some(&150));
        assert_eq!(totals.get("Computer science"), Some(&10));
    }

    /// PRD FR-SR-3: CirrusSearch operator syntax is just query *string*
    /// syntax the server interprets — this client must never rewrite or
    /// strip it. A raw socket captures the request line so the assertion is
    /// on the actual bytes sent, not on a client-side round trip through its
    /// own encoder: the query arrives on the wire percent-encoded but
    /// otherwise byte-for-byte the operator string the caller passed in.
    #[tokio::test]
    async fn search_passes_cirrus_operators_through_unmangled() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                *cap2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = br#"{"pages": []}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let query = "intitle:Turing morelike:Enigma_machine";
        let _ = client.search("en", query, 20).await;
        let head = captured.lock().unwrap().clone();
        let request_line = head.lines().next().unwrap_or_default();
        let encoded_query = urlencoding::encode(query);
        assert!(
            request_line.contains(encoded_query.as_ref()),
            "operator query must reach the wire unmangled: {request_line:?}"
        );
        // Round-trip through the same decoder a server would use, proving
        // the bytes on the wire decode back to exactly what was typed —
        // not just that some substring survived.
        let q_param = request_line
            .split_once("q=")
            .and_then(|(_, rest)| rest.split(['&', ' ']).next())
            .unwrap_or_default();
        assert_eq!(
            urlencoding::decode(q_param).unwrap().as_ref(),
            query,
            "decoded query must equal the original operator string exactly"
        );
    }

    /// PRD FR-SR-5 / Appendix A "Random": the endpoint shape — main
    /// namespace only, the requested limit, and the response's titles
    /// parsed out.
    #[tokio::test]
    async fn random_titles_requests_the_documented_endpoint_shape() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                *cap2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body =
                    br#"{"query":{"random":[{"title":"Alan Turing"},{"title":"Enigma machine"}]}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let titles = client.random_titles("en", 10).await.unwrap();
        assert_eq!(titles, vec!["Alan Turing", "Enigma machine"]);
        let head = captured.lock().unwrap().clone();
        assert!(head.contains("list=random"), "{head:?}");
        assert!(head.contains("rnnamespace=0"), "{head:?}");
        assert!(head.contains("rnlimit=10"), "{head:?}");
    }

    /// PRD §6.2 rule 10 / NF-NET-5: `page_assessments` sends every title in
    /// **one** request, not one per title — a raw listener that only ever
    /// `accept()`s once would hang (not fail) if the client made a second
    /// connection, so the test additionally proves the batching by checking
    /// all titles landed in that single captured request.
    #[tokio::test]
    async fn page_assessments_batches_every_title_into_one_request() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap2 = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                *cap2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = br#"{"query":{"pages":[
                    {"title":"Alan Turing","pageassessments":{"Biography":{"class":"FA"}}}
                ]}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let titles: Vec<String> = (0..10).map(|i| format!("Title {i}")).collect();
        let result = client.page_assessments("en", &titles).await.unwrap();
        assert_eq!(result.get("Alan Turing"), Some(&QualityClass::Fa));
        let head = captured.lock().unwrap().clone();
        let request_line = head.lines().next().unwrap_or_default();
        for title in &titles {
            let encoded = urlencoding::encode(title);
            assert!(
                request_line.contains(encoded.as_ref()),
                "every batched title must appear in the one request: {title:?} missing from {request_line:?}"
            );
        }
    }

    /// A page can carry a different class per WikiProject; `page_assessments`
    /// must keep the best (highest-ranked) one, not the first or an
    /// arbitrary one — parsed directly against the private response shape,
    /// mirroring `pageviews_response_sums_daily_counts_per_title`'s style.
    #[test]
    fn page_assessments_response_keeps_the_best_class_across_projects() {
        let json = br#"{"query":{"pages":[
            {"title":"Alan Turing","pageassessments":{"Biography":{"class":"B"},"Computing":{"class":"FA"}}},
            {"title":"Some Stub","pageassessments":{"WikiProject X":{"class":"Stub"}}},
            {"title":"Unassessed","pageassessments":{}}
        ]}}"#;
        let parsed: PageAssessmentsResponse = serde_json::from_slice(json).unwrap();
        let mut best = HashMap::new();
        for page in parsed.query.unwrap().pages {
            let top = page
                .pageassessments
                .values()
                .filter_map(|p| QualityClass::parse(&p.class))
                .max();
            if let Some(class) = top {
                best.insert(page.title, class);
            }
        }
        assert_eq!(best.get("Alan Turing"), Some(&QualityClass::Fa));
        assert_eq!(best.get("Some Stub"), Some(&QualityClass::Stub));
        assert_eq!(
            best.get("Unassessed"),
            None,
            "no recognized assessment at all must leave the title absent, not a default rank"
        );
    }

    /// PRD FR-SR-5's "assessment ≥ GA" filter is exactly `QualityClass`'s
    /// ordering; this locks the parse table and the ordering it implies.
    #[test]
    fn quality_class_parse_and_ordering() {
        assert_eq!(QualityClass::parse("FA"), Some(QualityClass::Fa));
        assert_eq!(QualityClass::parse("GA"), Some(QualityClass::Ga));
        assert_eq!(QualityClass::parse("B"), Some(QualityClass::B));
        assert_eq!(QualityClass::parse("C"), Some(QualityClass::C));
        assert_eq!(QualityClass::parse("Start"), Some(QualityClass::Start));
        assert_eq!(QualityClass::parse("Stub"), Some(QualityClass::Stub));
        assert_eq!(
            QualityClass::parse("FL"),
            None,
            "unrecognized classes parse to None"
        );
        assert_eq!(QualityClass::parse(""), None);

        assert!(QualityClass::Fa.is_good_or_better());
        assert!(QualityClass::Ga.is_good_or_better());
        assert!(!QualityClass::B.is_good_or_better());
        assert!(!QualityClass::C.is_good_or_better());
        assert!(!QualityClass::Start.is_good_or_better());
        assert!(!QualityClass::Stub.is_good_or_better());
        assert!(QualityClass::Fa > QualityClass::Ga, "FA outranks GA");
    }

    /// PRD FR-ML-1/2: `LangLinksResponse`'s full parse table — autonym,
    /// English langname, translated title, and url all round-trip; a
    /// page with no `langlinks` key at all (never invented as an empty
    /// array explicitly by every deployment) degrades to an empty `Vec`,
    /// not an error.
    #[test]
    fn langlinks_response_parses_autonym_langname_title_and_url() {
        let json = r#"{"query":{"pages":[{"title":"Alan Turing","langlinks":[
            {"lang":"de","autonym":"Deutsch","langname":"German","title":"Alan Turing","url":"https://de.wikipedia.org/wiki/Alan_Turing"},
            {"lang":"ja","autonym":"日本語","langname":"Japanese","title":"アラン・チューリング"}
        ]}]}}"#;
        let parsed: LangLinksResponse = serde_json::from_slice(json.as_bytes()).unwrap();
        let pages = parsed.query.unwrap().pages;
        assert_eq!(pages[0].langlinks.len(), 2);
        assert_eq!(pages[0].langlinks[0].code, "de");
        assert_eq!(pages[0].langlinks[0].autonym, "Deutsch");
        assert_eq!(pages[0].langlinks[0].langname, "German");
        assert_eq!(pages[0].langlinks[0].title, "Alan Turing");
        assert_eq!(
            pages[0].langlinks[0].url.as_deref(),
            Some("https://de.wikipedia.org/wiki/Alan_Turing")
        );
        assert_eq!(pages[0].langlinks[1].code, "ja");
        assert_eq!(pages[0].langlinks[1].autonym, "日本語");
        assert_eq!(pages[0].langlinks[1].title, "アラン・チューリング");
        assert_eq!(
            pages[0].langlinks[1].url, None,
            "a langlink with no url field degrades to None, not an error"
        );

        let empty = br#"{"query":{"pages":[{"title":"Stub"}]}}"#;
        let parsed: LangLinksResponse = serde_json::from_slice(empty).unwrap();
        assert!(parsed.query.unwrap().pages[0].langlinks.is_empty());

        let no_query = br#"{}"#;
        let parsed: LangLinksResponse = serde_json::from_slice(no_query).unwrap();
        assert!(parsed.query.is_none());
    }

    /// End-to-end `fetch_langlinks`: parses a real HTTP response, including
    /// ordinary CJK autonym/title text, which must survive sanitization
    /// untouched (only control/bidi/zero-width bytes are ever stripped —
    /// see `sanitize_langlink_strips_control_bytes_but_keeps_plain_text`
    /// below for the hostile-input half of that claim).
    #[tokio::test]
    async fn fetch_langlinks_parses_a_real_response() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body = r#"{"query":{"pages":[{"title":"Alan Turing","langlinks":[
                    {"lang":"de","autonym":"Deutsch","langname":"German","title":"Alan Turing","url":"https://de.wikipedia.org/wiki/Alan_Turing"},
                    {"lang":"ja","autonym":"日本語","langname":"Japanese","title":"アラン・チューリング"}
                ]}]}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });

        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let links = client.fetch_langlinks("en", "Alan Turing").await.unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].code, "de");
        assert_eq!(links[0].autonym, "Deutsch");
        assert_eq!(links[0].langname, "German");
        assert_eq!(
            links[0].url.as_deref(),
            Some("https://de.wikipedia.org/wiki/Alan_Turing")
        );
        assert_eq!(links[1].code, "ja");
        assert_eq!(links[1].autonym, "日本語");
        assert_eq!(links[1].title, "アラン・チューリング");
    }

    /// PRD SEC-1: `sanitize_langlink`'s choke point — a hostile autonym/
    /// langname/title/url comes out free of control bytes, mirroring
    /// `sanitize_search_result_strips_control_bytes_but_keeps_searchmatch_markup`.
    #[test]
    fn sanitize_langlink_strips_control_bytes() {
        let mut link = LangLink {
            code: "de".to_string(),
            autonym: "Deu\x1btsch".to_string(),
            langname: "Ger\x07man".to_string(),
            title: "Evil\x1b[31mTitle".to_string(),
            url: Some("https://de.wikipedia.org/wiki/Evil\x1bTitle".to_string()),
        };
        sanitize_langlink(&mut link);
        assert!(!link.autonym.contains('\x1b'));
        assert!(!link.langname.contains('\x07'));
        assert!(!link.title.contains('\x1b'));
        assert!(!link.url.unwrap().contains('\x1b'));
    }

    /// An article with no langlinks at all (a stub, or a title this wiki
    /// has no interwiki record for) is an empty list, not an error — the
    /// picker's own "no language editions found" empty state depends on
    /// being able to tell this apart from a network failure.
    #[tokio::test]
    async fn fetch_langlinks_with_no_langlinks_is_empty_not_error() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body = br#"{"query":{"pages":[{"title":"Some Stub"}]}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let links = client.fetch_langlinks("en", "Some Stub").await.unwrap();
        assert!(links.is_empty());
    }
}
