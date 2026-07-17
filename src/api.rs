//! MediaWiki API client. Per PRD §6.2: per-wiki endpoints only
//! (`{lang}.wikipedia.org`), never `api.wikimedia.org`. Per §6.5 (NF-NET-2)
//! every request carries a descriptive User-Agent.
//!
//! **One stated exception** (PRD FR-DL-3 v2, §6.2 rule 1's own carve-out):
//! [`WikiClient::fetch_liftwing_quality`] addresses the Lift Wing ML
//! inference gateway at `api.wikimedia.org`, because the article-quality
//! model it calls has no per-wiki-hosted equivalent — Lift Wing has only
//! ever been gateway-hosted. It is opt-in (`liftwing` config, default off)
//! and its endpoint is config-overridable (§6.2 rule 2) precisely because
//! the gateway's post-deprecation survival is unverified (SP-4): if it's
//! gone, this simply degrades to the documented "non-PageAssessments wikis
//! show no badge" behavior. See that method's own doc comment for the
//! request/response shape and how unverified it is against a live endpoint.
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

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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

/// PRD FR-ML-5's per-wiki feature-degradation matrix: which optional
/// endpoints/parser the active wiki is declared to support. Wikipedia's own
/// defaults (`parser: Auto, wikifeeds/pageviews/pageassessments: true`) are
/// the unconditional pre-this-chunk behavior; every other wiki (a sister
/// project or a third-party `[wiki.<name>]` site) defaults the latter three
/// off (`config::resolve_wiki`), so degradation only ever engages away from
/// the default install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WikiCapabilities {
    pub parser: ParserMode,
    /// Wikifeeds (`feed/featured`, `feed/onthisday`) — gates the FR-DL-1
    /// start-page feed / FR-PF-2 trending prefetch (`execute_featured`).
    pub wikifeeds: bool,
    /// `prop=pageviews` — gates FR-PF-1's link-rank pageviews term
    /// (`execute_rank_links`); false degrades ranking to lead-position-only.
    pub pageviews: bool,
    /// `prop=pageassessments` — gates the FR-DL-3 quality badge
    /// (`enrich_article`); false means no badge is ever looked up.
    pub pageassessments: bool,
}

impl WikiCapabilities {
    /// Every feature on, Parsoid-first — the implicit behavior every wiki
    /// had before this chunk, and still the built-in `wikipedia` default.
    /// Only reachable today via [`WikiClient::new`]/[`WikiClient::
    /// with_contact`] (production always resolves real capabilities through
    /// [`WikiClient::with_wiki`] instead — see their own doc comments for
    /// why they're kept anyway).
    #[allow(dead_code)]
    pub fn full() -> Self {
        Self {
            parser: ParserMode::Auto,
            wikifeeds: true,
            pageviews: true,
            pageassessments: true,
        }
    }
}

/// PRD §6.2 rule 3's article-HTML source selector. `Auto` (the default) is
/// what makes third-party wikis (FR-ML-5) work with zero config: try Parsoid
/// REST first, and only pay for a second request when the first one comes
/// back in a shape that means "this wiki doesn't run Parsoid REST" rather
/// than "this article doesn't exist" (see [`parsoid_404_is_genuine_miss`]).
/// `Parsoid`/`Legacy` are the explicit per-wiki `[wiki.<name>] parser =`
/// overrides: `Parsoid` disables the fallback (surface the real error
/// instead of silently masking a misconfigured wiki as "unsupported");
/// `Legacy` skips the Parsoid attempt entirely, saving a doomed request on a
/// wiki already known to lack it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserMode {
    Auto,
    Parsoid,
    Legacy,
}

impl ParserMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "parsoid" => Some(Self::Parsoid),
            "legacy" => Some(Self::Legacy),
            _ => None,
        }
    }
}

/// The stable per-wiki key that scopes the page cache, the session-state
/// maps, and reading-position memory (PRD FR-ML-4/5). Derived from the
/// registry *name* the reader selected via `active_wiki`/`:wiki <name>`,
/// which is lang-independent and stable across a session and across runs.
///
/// The primary `"wikipedia"` entry maps to the **empty** scope on purpose:
/// its on-disk cache layout (`blob/{lang}/…`, `page/{lang}/…`) and every
/// session-map key then stay byte-identical to the pre-multi-wiki code, so
/// a normal Wikipedia session behaves exactly as before *and* every cache
/// entry written before wikis could be switched (all of which were, by
/// construction, Wikipedia's) is read back transparently as this wiki's —
/// no migration pass, no refetch storm. Every other wiki gets its own
/// registry name as a distinct scope, so a `:wiki`-switched article can
/// never be served another wiki's cached copy of the same `(lang, title)`.
///
/// The name — not the resolved host — is the key because it is the identity
/// the reader actually chose and it needs no host parsing or `{lang}`
/// substitution. Two differently-named `[wiki.<name>]` sections that happen
/// to point at the same host keep separate caches; that is a negligible
/// duplication, never a correctness problem, whereas the reverse (a
/// collision) is exactly the bug this scoping fixes. The mapping is a
/// bijection (`""`⇄`"wikipedia"`, every other name maps to itself), so the
/// registry name is always recoverable from a stored scope.
pub fn wiki_scope(name: &str) -> &str {
    if name == crate::sisters::WIKIPEDIA.name {
        ""
    } else {
        name
    }
}

/// One entry in the client's wiki registry (PRD FR-ML-4/5): everything
/// needed to point requests at a wiki by name — built once at startup from
/// `config::ResolvedWikiRegistry` (Wikipedia, the four sister projects, and
/// every `[wiki.<name>]` section) and consulted by `:wiki <name>`
/// (`main::switch_wiki`) without re-reading the config file.
#[derive(Debug, Clone)]
pub struct WikiRegistryEntry {
    pub base_url_template: String,
    pub capabilities: WikiCapabilities,
}

/// The wiki a [`WikiClient`] currently addresses: its registry name (for
/// `:wiki`'s "already on this wiki" check and `doctor`'s report), the host
/// template, and its capability matrix. Shared, mutable client state (see
/// `WikiClient`'s own doc comment) so `:wiki` can repoint every clone of the
/// client at once without rebuilding the `reqwest::Client` underneath it.
#[derive(Debug, Clone)]
struct ActiveWiki {
    name: String,
    base_url_template: String,
    capabilities: WikiCapabilities,
}

/// Cloned to hand a copy to a spawned background task (PRD FR-SR-1's
/// typeahead can't block the UI thread on the loop's redraw/`event::poll`
/// cycle) — cheap, since `reqwest::Client` is itself an `Arc` internally and
/// the active wiki is behind an `Arc<RwLock<_>>`. Mirrors reqwest's own
/// documented pattern of cloning the client rather than wrapping it.
///
/// The active wiki (host template + capability matrix) is `RwLock`-guarded
/// rather than a plain field because PRD FR-ML-4's `:wiki` switch must
/// repoint *every* clone of the client at once — the event loop threads
/// `&WikiClient`/cloned `WikiClient`s through many spawned tasks, and a
/// plain field would only ever update the one clone `:wiki` was called on.
#[derive(Clone)]
pub struct WikiClient {
    http: reqwest::Client,
    /// PRD §7 "Redirect" row's `:noredirect`: identical to `http` in every
    /// other respect (same User-Agent, gzip, timeout) except its redirect
    /// policy is `Policy::none()` — reqwest fixes a client's redirect
    /// behavior at build time, not per-request, so the one call site that
    /// needs the raw pre-redirect response (the redirect notice page
    /// itself) needs its own client rather than an option on `http`'s own
    /// requests.
    http_noredirect: reqwest::Client,
    active: std::sync::Arc<std::sync::RwLock<ActiveWiki>>,
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
    /// Same schema-extension story as `suggestion` just above — PRD Appendix
    /// A / FR-SR-4 also names the Action API's `srinfo=rewrittenquery`, the
    /// "showing results for X" auto-correction the search engine *already
    /// applied* to produce `pages` (distinct from `suggestion`'s "did you
    /// mean" invitation, which the reader must opt into via Enter). A
    /// rewritten query typically arrives *alongside* non-empty `pages`
    /// (that's the whole point — it's how those results were found), unlike
    /// `suggestion`, which the mock only ever sends on a genuine zero-result
    /// miss.
    #[serde(default)]
    rewrittenquery: Option<String>,
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
/// (PRD Appendix A): the plain-text extract that powers T2 link-peek plus the
/// display title, Wikidata one-line description, and thumbnail URL the
/// FR-NV-5 link-preview popup shows.
#[derive(Debug, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    extract: String,
    #[serde(default)]
    thumbnail: Option<SummaryThumbnail>,
}

#[derive(Debug, Deserialize)]
struct SummaryThumbnail {
    #[serde(default)]
    source: String,
}

/// PRD FR-NV-5's link-preview payload: the target article's display title,
/// Wikidata description, lead extract, and (optional) thumbnail URL, all
/// resolved from one page-summary call (§6.2 rule 4). Sanitized (SEC-1) since
/// every field is remote-derived, displayable text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummaryData {
    pub title: String,
    pub description: String,
    pub extract: String,
    pub thumbnail: Option<String>,
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

/// PRD FR-DL-5's `generator=links&prop=info` missing-flag response
/// (formatversion=2): the real MediaWiki technique for "which of this
/// page's links are broken" — a linked title that doesn't exist comes back
/// with `"missing": true` and no `pageid`, exactly like `action=query&
/// titles=` on a nonexistent title directly.
#[derive(Debug, Deserialize, Default)]
struct MissingLinksResponse {
    #[serde(default)]
    query: Option<MissingLinksQuery>,
}

#[derive(Debug, Deserialize)]
struct MissingLinksQuery {
    #[serde(default)]
    pages: Vec<MissingLinksPage>,
}

#[derive(Debug, Deserialize)]
struct MissingLinksPage {
    title: String,
    #[serde(default)]
    missing: bool,
}

/// PRD FR-PF-3's `prop=categories` response (formatversion=2). Each page
/// carries a `categories` array of `{title}` (in `Category:Foo` form); a page
/// with no non-hidden categories has an empty/absent array.
#[derive(Debug, Deserialize, Default)]
struct CategoriesResponse {
    #[serde(default)]
    query: Option<CategoriesQuery>,
}

#[derive(Debug, Deserialize)]
struct CategoriesQuery {
    #[serde(default)]
    pages: Vec<CategoriesPage>,
}

#[derive(Debug, Deserialize)]
struct CategoriesPage {
    title: String,
    #[serde(default)]
    categories: Vec<CategoryEntry>,
}

#[derive(Debug, Deserialize)]
struct CategoryEntry {
    #[serde(default)]
    title: String,
}

/// PRD FR-DL-3's quality classes as `prop=pageassessments` reports them
/// (Appendix A "Quality"), ordered low→high so `Ord`/`>=` express "at least
/// as good as" directly — FR-SR-5's "random good article" filter is exactly
/// `>= Ga`. Only the six classes the PRD names are recognized; anything
/// else a real wiki reports (`FL`, `A`, `Disambig`, `List`, `NA`,
/// `Redirect`, …) parses to `None` rather than inventing a rank for it — a
/// wiki that only ever reports those never shows a badge at all, same as one
/// with no PageAssessments extension (§7-adjacent graceful degradation, not
/// a bug).
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

    /// PRD FR-DL-3's display glyph: `★FA`/`+GA`/`B`/`C`/`Start`/`Stub`, shown
    /// in the status bar and prefixed on search-result rows (`ui.rs`). A
    /// wiki with no PageAssessments data for a title (or no PageAssessments
    /// extension at all) simply has no `QualityClass` to call this on — see
    /// `App::quality_badge_for`, which is where "no badge" actually happens;
    /// this method itself always has something to show once a class parsed.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Fa => "★FA",
            Self::Ga => "+GA",
            Self::B => "B",
            Self::C => "C",
            Self::Start => "Start",
            Self::Stub => "Stub",
        }
    }

    /// PRD FR-DL-3 v2: maps a Lift Wing `articlequality` score to a
    /// `QualityClass`. `prediction` is the model's own already-decided top
    /// class (an argmax over `probability` performed server-side), so this
    /// simply parses that string with the same [`Self::parse`] the
    /// `pageassessments` path already uses for its `class` field — the two
    /// fallback paths converge on one class-name vocabulary. `probability`'s
    /// per-class map is consulted only when `prediction` is missing or
    /// unrecognized: an argmax computed locally instead, favoring the
    /// higher-ranked class (`QualityClass`'s own `Ord`) on an exact tie
    /// rather than picking arbitrarily. A weighted-average score (rather
    /// than either argmax) was considered and rejected: it can land on a
    /// class the distribution never actually favors most (e.g. FA-heavy and
    /// Stub-heavy mass on either side of B averaging to "B" even when B
    /// itself has near-zero probability), which reads as a worse-justified
    /// badge than either directly-reported "most likely" figure.
    fn from_liftwing_score(score: &LiftWingScore) -> Option<Self> {
        Self::parse(&score.prediction).or_else(|| {
            score
                .probability
                .iter()
                .filter_map(|(name, p)| Self::parse(name).map(|class| (class, *p)))
                .max_by(|(class_a, p_a), (class_b, p_b)| {
                    p_a.total_cmp(p_b).then(class_a.cmp(class_b))
                })
                .map(|(class, _)| class)
        })
    }
}

/// PRD FR-DL-3 v2's Lift Wing `articlequality` predict response. Modeled on
/// the ORES-legacy-compatible shape Lift Wing kept for backward
/// compatibility with ORES consumers — `{wiki_db: {scores: {rev_id:
/// {articlequality: {score: {prediction, probability}}}}}}` — the
/// best-documented public shape available for this model family. **This is
/// UNVERIFIED against a live endpoint** (PRD SP-4: the `api.wikimedia.org`
/// gateway's post-deprecation status, and by extension whether this exact
/// response shape still holds, is unconfirmed); `liftwing`'s config default
/// is off for exactly this reason. A response that doesn't parse into this
/// shape is treated as "no score" (`WikiClient::fetch_liftwing_quality`
/// returns `Ok(None)`), never a hard error that would take down the whole
/// article-open path over an optional badge.
type LiftWingResponse = HashMap<String, LiftWingWikiScores>;

#[derive(Debug, Deserialize, Default)]
struct LiftWingWikiScores {
    #[serde(default)]
    scores: HashMap<String, LiftWingRevScore>,
}

#[derive(Debug, Deserialize)]
struct LiftWingRevScore {
    articlequality: LiftWingModelScore,
}

#[derive(Debug, Deserialize)]
struct LiftWingModelScore {
    score: LiftWingScore,
}

#[derive(Debug, Deserialize, Default)]
struct LiftWingScore {
    #[serde(default)]
    prediction: String,
    #[serde(default)]
    probability: HashMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct BareResponse {
    latest: BareLatest,
}

/// `meta=userinfo` (formatversion=2) — the authenticated whoami (PRD FR-ACC-1,
/// Appendix A userinfo). Only the central account `name` is read here; the
/// rest of the payload (rights, options) is out of scope for the login label.
#[derive(Debug, Deserialize)]
struct UserInfoResponse {
    query: UserInfoQuery,
}

#[derive(Debug, Deserialize)]
struct UserInfoQuery {
    userinfo: UserInfo,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    name: String,
    /// An anonymous session reports `anon: true`; a real login never does. Its
    /// presence is how `fetch_userinfo` distinguishes "the token authenticated
    /// as someone" from "the token was ignored and this is an anon reply".
    #[serde(default)]
    anon: bool,
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
    /// PRD §7 "Redirect": `true` when this fetch's final response URL
    /// differed from the one requested — i.e. the server (or, for Parsoid
    /// REST, the HTTP client's own default redirect-following) resolved a
    /// redirect page to its target rather than serving the redirect notice
    /// itself. `false` for [`WikiClient::fetch_article_html_noredirect`] by
    /// construction (it never follows one to begin with) and for the legacy
    /// `action=parse` fallback (documented scope limit: real deployments do
    /// report a `redirects` array there too, but this module doesn't parse
    /// it — `fetch_article_html_noredirect` is Parsoid-only, so the legacy
    /// path never needs to answer "did I redirect?").
    pub redirected: bool,
}

/// PRD §7 "429 / maxlag on interactive request": a foreground fetch hit a
/// rate limit or lag signal. Unlike every other foreground failure (which
/// stays a plain `anyhow::Error`), this one is a distinct type so callers —
/// `main::fetch_page`/`open_title` — can `downcast_ref` to tell "Wikipedia
/// is busy, worth an automatic retry" apart from "this article genuinely
/// doesn't exist" or "the network is down" without parsing error strings.
#[derive(Debug)]
pub struct RateLimited {
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_after {
            Some(d) => write!(f, "rate-limited by the wiki; retry after {}s", d.as_secs()),
            None => write!(f, "rate-limited by the wiki"),
        }
    }
}

impl std::error::Error for RateLimited {}

/// PRD §7 "Bookmark/saved page whose target moved or was deleted": a
/// foreground fetch confirmed the article genuinely doesn't exist upstream
/// (Parsoid's own "nonexistent title" 404 shape, not a network failure or an
/// unsupported-wiki 404) — a distinct type, same reasoning as
/// [`RateLimited`], so `main::open_title` can tell "this bookmark's target
/// is gone" apart from "the network is down right now" without string-
/// matching the error text.
#[derive(Debug)]
pub struct ArticleMissing {
    pub title: String,
}

impl std::fmt::Display for ArticleMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no article named {:?} on this wiki", self.title)
    }
}

impl std::error::Error for ArticleMissing {}

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

/// PRD §6.2 rule 3's fallback trigger, shared by the foreground and
/// background Parsoid attempts: a 404 from a wiki that genuinely runs
/// Parsoid REST carries MediaWiki's own JSON error envelope (an `errorKey`
/// field, e.g. `rest-nonexistent-title`) — that shape means "this article
/// doesn't exist," the same answer as today, unchanged. Any other 404 shape
/// (no body at all, an HTML error page from a web server that never routed
/// to MediaWiki, or JSON missing that field) means "this wiki doesn't run
/// Parsoid REST" — §6.2 rule 3's documented cue to fall back to legacy
/// `action=parse` instead of reporting a false "doesn't exist."
fn parsoid_404_is_genuine_miss(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .is_some_and(|v| v.get("errorKey").is_some())
}

/// The outcome of one Parsoid REST attempt (fg/bg share this shape — see
/// `fetch_article_html_parsoid`/`fetch_article_html_parsoid_bg`). Only a
/// genuine transport/5xx failure propagates as an `Err` from those methods;
/// a plain 404 always resolves to `Missing` or `Unsupported` here so the
/// caller decides what it means without re-deriving the classification.
enum ParsoidOutcome<T> {
    Found(T),
    /// PRD §6.2 rule 3: a genuinely nonexistent title (unchanged Wikipedia
    /// behavior).
    Missing,
    /// This wiki doesn't run Parsoid REST at all — the FR-ML-5 fallback
    /// trigger.
    Unsupported,
}

/// Legacy `action=parse&prop=text` request URL (PRD §6.2 rule 3's
/// third-party fallback) — `page=` rather than `titles=` (the single-page
/// form `action=parse` actually accepts), `redirects=1` so a redirect
/// resolves the same way Parsoid's own REST endpoint already does.
fn legacy_parse_url(host: &str, title: &str) -> String {
    format!(
        "{host}/w/api.php?action=parse&format=json&formatversion=2&prop=text&redirects=1&page={}",
        urlencoding::encode(&title.replace(' ', "_"))
    )
}

/// `action=parse`'s response shape (formatversion=2): either a `parse`
/// object (`text` is a raw HTML string in this format version, not the
/// formatversion=1 `{"*": ...}` wrapper) or an `error` object for a missing
/// title — MediaWiki reports that as HTTP 200 with `error.code ==
/// "missingtitle"`, never a 404, unlike the core REST endpoint.
#[derive(Debug, Deserialize, Default)]
struct LegacyParseResponse {
    #[serde(default)]
    parse: Option<LegacyParse>,
    #[serde(default)]
    error: Option<LegacyParseError>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyParse {
    #[serde(default)]
    text: String,
    /// Present on a real deployment's `action=parse` response even without
    /// requesting `prop=revid` explicitly; `None` degrades to revid 0, the
    /// same documented fallback as a missing Parsoid `ETag`.
    #[serde(default)]
    revid: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct LegacyParseError {
    #[serde(default)]
    info: String,
}

/// Full-text search results plus the did-you-mean suggestion, when the
/// server offered one (PRD FR-SR-4).
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub suggestion: Option<String>,
    /// PRD FR-SR-4's other zero/poor-result signal: the query the search
    /// engine actually ran, when it differs from what the reader typed
    /// (`srinfo=rewrittenquery`) — a "showing results for X" notice, not a
    /// "did you mean" invitation (`suggestion`'s own job).
    pub rewritten_query: Option<String>,
}

impl WikiClient {
    /// `base_url_template` is the config-resolved host (PRD §6.2 rule 2):
    /// `{lang}` is substituted per-request when present, else used
    /// verbatim — arbitrary MediaWiki sites (FR-ML-5) aren't per-language
    /// subdomains, so a template without `{lang}` addresses one fixed host.
    /// Defaults to [`WikiCapabilities::full`] — every existing caller (tests
    /// included) that only ever addressed Wikipedia keeps that unconditional
    /// behavior; a real per-wiki matrix comes from [`Self::with_wiki`].
    /// Production (`main::run`) always has a config-resolved matrix in hand
    /// and calls [`Self::with_wiki`] directly, so this simpler constructor is
    /// test-support surface today — kept public rather than `#[cfg(test)]`
    /// since it's a legitimate, documented API a caller with no capability
    /// data of its own can still reach for.
    #[allow(dead_code)]
    pub fn new(base_url_template: String) -> Result<Self> {
        Self::with_contact(base_url_template, DEFAULT_CONTACT)
    }

    /// Like [`new`](Self::new) but with a configured contact channel for the
    /// User-Agent (PRD NF-NET-2 / §6.2 rule 2's config-overridable
    /// networking) — same test-support scope as `new`.
    #[allow(dead_code)]
    pub fn with_contact(base_url_template: String, contact: &str) -> Result<Self> {
        Self::with_wiki(
            "wikipedia".to_string(),
            base_url_template,
            WikiCapabilities::full(),
            contact,
        )
    }

    /// The real startup path (PRD FR-ML-4/5): `name` and `capabilities` come
    /// from `config::ResolvedConfig`'s active-wiki resolution, so the client
    /// starts already knowing whether this wiki has Wikifeeds/pageviews/
    /// pageassessments and which parser to try first — no separate "probe
    /// the wiki" round trip before the first real request.
    pub fn with_wiki(
        name: String,
        base_url_template: String,
        capabilities: WikiCapabilities,
        contact: &str,
    ) -> Result<Self> {
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
        let http_noredirect = reqwest::Client::builder()
            .user_agent(build_user_agent(contact))
            .gzip(true)
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building no-redirect HTTP client")?;
        Ok(Self {
            http,
            http_noredirect,
            active: std::sync::Arc::new(std::sync::RwLock::new(ActiveWiki {
                name,
                base_url_template,
                capabilities,
            })),
        })
    }

    /// PRD FR-ML-4's `:wiki <name>` switch: repoints every clone of this
    /// client at a different wiki's host + capability matrix at once (see
    /// the struct doc comment for why this is `RwLock`-guarded rather than a
    /// plain field). Takes effect on the very next request — nothing needs
    /// to be rebuilt or re-cloned.
    pub fn switch_wiki(
        &self,
        name: String,
        base_url_template: String,
        capabilities: WikiCapabilities,
    ) {
        let mut guard = self.active.write().unwrap_or_else(|e| e.into_inner());
        *guard = ActiveWiki {
            name,
            base_url_template,
            capabilities,
        };
    }

    /// The registry name of the wiki this client currently addresses
    /// (`"wikipedia"`, `"wiktionary"`, or a custom `[wiki.<name>]` name).
    /// `App::active_wiki_name` mirrors this for code that only has `&App`
    /// (every current caller); kept as a client accessor too since it's the
    /// authoritative value `switch_wiki` actually set, exercised directly by
    /// this module's own tests.
    #[allow(dead_code)]
    pub fn active_wiki_name(&self) -> String {
        self.active
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .name
            .clone()
    }

    /// The wiki-scope key for the wiki this client currently addresses — see
    /// the free function [`wiki_scope`]. Threaded into every cache read/write
    /// and session-state key so a `:wiki`-switched article never collides
    /// with another wiki's cached copy of the same `(lang, title)`.
    pub fn wiki_scope(&self) -> String {
        wiki_scope(&self.active.read().unwrap_or_else(|e| e.into_inner()).name).to_string()
    }

    /// The current wiki's feature-degradation matrix (PRD FR-ML-5) — cheap
    /// to call per-request since `WikiCapabilities` is `Copy`.
    pub fn capabilities(&self) -> WikiCapabilities {
        self.active
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .capabilities
    }

    fn host(&self, lang: &str) -> String {
        let template = self
            .active
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .base_url_template
            .clone();
        if template.contains("{lang}") {
            template.replace("{lang}", lang)
        } else {
            template
        }
    }

    /// PRD §6.2 rule 2 / FR-BM-5: exposes the per-lang resolved wiki origin
    /// for the Reading List sync's `project` field and same-wiki entry
    /// filter (`account`'s ReadingLists module doc) — the one external need
    /// for what was, until this feature, a private helper.
    pub fn wiki_origin(&self, lang: &str) -> String {
        self.host(lang)
    }

    fn title_path(title: &str) -> String {
        urlencoding::encode(&title.replace(' ', "_")).into_owned()
    }

    /// Fetch an article's HTML (PRD §6.2 rule 3). Tries Parsoid REST core
    /// REST `/w/rest.php/v1/page/{title}/html` — the primary content source
    /// — first unless this wiki's `parser` capability is `Legacy`; on an
    /// unsupported-shaped 404 (see [`parsoid_404_is_genuine_miss`]) falls
    /// back to legacy `action=parse&prop=text`, the documented FR-ML-5
    /// fallback for third-party wikis that don't run Parsoid REST at all.
    /// Wikipedia (and any wiki that does answer Parsoid) never pays for the
    /// second request — this is the "keep working exactly as before"
    /// path (PRD deliverable: no regression).
    pub async fn fetch_article_html(&self, lang: &str, title: &str) -> Result<FetchedArticle> {
        let caps = self.capabilities();
        if caps.parser != ParserMode::Legacy {
            match self.fetch_article_html_parsoid(lang, title).await? {
                ParsoidOutcome::Found(article) => return Ok(article),
                ParsoidOutcome::Missing => {
                    return Err(ArticleMissing {
                        title: title.to_string(),
                    }
                    .into());
                }
                ParsoidOutcome::Unsupported => {
                    if caps.parser == ParserMode::Parsoid {
                        bail!(
                            "{lang} wiki has no Parsoid REST HTML for {title:?}, and \
                             parser=parsoid disables the legacy fallback"
                        );
                    }
                    // Auto mode: PRD §6.2 rule 3's documented third-party
                    // fallback — fall through to legacy below.
                }
            }
        }
        self.fetch_article_html_legacy(lang, title).await
    }

    /// The Parsoid REST attempt shared by [`Self::fetch_article_html`] and
    /// [`Self::fetch_article_html_bg`]. Only a genuine network/5xx failure is
    /// an `Err`; a 404 always resolves to one of the two [`ParsoidOutcome`]
    /// variants so the caller can decide what it means (missing article vs.
    /// unsupported wiki) without re-deriving the classification itself.
    async fn fetch_article_html_parsoid(
        &self,
        lang: &str,
        title: &str,
    ) -> Result<ParsoidOutcome<FetchedArticle>> {
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

        // PRD §7 "429 / maxlag on interactive request": checked before the
        // 404 branch below (429/503 are never MediaWiki's "missing title"
        // shape) so a foreground caller can distinguish "busy, worth an
        // automatic retry" from every other outcome via `downcast_ref`,
        // exactly like the background substrate's `bg_status_failure`
        // already does for its own callers.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            return Err(RateLimited {
                retry_after: parse_retry_after(resp.headers()),
            }
            .into());
        }

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            let body = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
                .await
                .unwrap_or_default();
            return Ok(if parsoid_404_is_genuine_miss(&body) {
                ParsoidOutcome::Missing
            } else {
                ParsoidOutcome::Unsupported
            });
        }
        let resp = resp.error_for_status().context("fetching article HTML")?;
        // PRD §7 "Redirect": the real core REST API answers a redirect
        // source title with a 3xx to its target's own REST URL, which this
        // client's default redirect policy follows transparently — so by
        // the time `resp` reaches here, its own `url()` already differs from
        // what was requested exactly when a redirect was followed. Captured
        // before the body read below (which doesn't change `resp.url()` but
        // does consume `resp` itself).
        let redirected = resp.url().as_str() != url;
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
        Ok(ParsoidOutcome::Found(FetchedArticle {
            html,
            revid,
            etag,
            redirected,
        }))
    }

    /// PRD §7 "Redirect" row's `:noredirect`: fetches `title`'s own Parsoid
    /// HTML *without* following a redirect it might be — the raw "Redirect
    /// to: X" notice page, the exact content [`Self::fetch_article_html`]
    /// transparently hops past via `http_noredirect`'s build-time redirect
    /// policy (`Policy::none()`). A literal 3xx response therefore reaches
    /// here directly rather than being auto-followed; reqwest's
    /// `error_for_status` only ever flags 4xx/5xx, so it's read exactly like
    /// an ordinary 200 — the redirect notice page's body rides the 3xx
    /// response the same way a real deployment's redirect page does.
    /// Parsoid-only (no legacy `action=parse` fallback): `:noredirect` is a
    /// deliberately rare diagnostic command, not a first-class reading path,
    /// so FR-ML-5's third-party-wiki degradation isn't worth duplicating
    /// here.
    pub async fn fetch_article_html_noredirect(
        &self,
        lang: &str,
        title: &str,
    ) -> Result<FetchedArticle> {
        let url = format!(
            "{}/w/rest.php/v1/page/{}/html",
            self.host(lang),
            Self::title_path(title)
        );
        let resp = self
            .http_noredirect
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting non-redirected article HTML for {title:?}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("no article named {title:?} on {lang} wiki");
        }
        let resp = resp
            .error_for_status()
            .context("fetching non-redirected article HTML")?;
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let revid = etag.as_deref().and_then(parse_revid_from_etag).unwrap_or(0);
        let bytes = read_capped(resp, crate::doc::MAX_ARTICLE_HTML_BYTES)
            .await
            .context("reading non-redirected article HTML body")?;
        let html = decode_lossy_utf8(bytes);
        Ok(FetchedArticle {
            html,
            revid,
            etag,
            redirected: false,
        })
    }

    /// PRD §6.2 rule 3's fallback: legacy `action=parse&prop=text`, the same
    /// article-HTML request shape MediaWiki has answered since long before
    /// Parsoid REST existed. `doc::parse_article_html` reads whatever HTML
    /// comes back the same way regardless of source — legacy HTML just
    /// carries none of Parsoid's RDFa/`data-mw` annotations, so
    /// template-name-based features (infobox/citation-needed detection)
    /// degrade gracefully rather than erroring (§7-adjacent, same posture as
    /// `QualityClass`'s "no data, no badge").
    async fn fetch_article_html_legacy(&self, lang: &str, title: &str) -> Result<FetchedArticle> {
        let url = legacy_parse_url(&self.host(lang), title);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting legacy parse for {title:?}"))?
            .error_for_status()
            .context("legacy parse request failed")?;
        let bytes = read_capped(resp, crate::doc::MAX_ARTICLE_HTML_BYTES)
            .await
            .context("reading legacy parse response body")?;
        let parsed: LegacyParseResponse =
            serde_json::from_slice(&bytes).context("parsing legacy parse response")?;
        if let Some(err) = parsed.error {
            let info = if err.info.is_empty() {
                "legacy parser".to_string()
            } else {
                err.info
            };
            bail!("no article named {title:?} on {lang} ({info})");
        }
        let parse = parsed.parse.ok_or_else(|| {
            anyhow!("legacy parse response for {title:?} had neither 'parse' nor 'error'")
        })?;
        Ok(FetchedArticle {
            html: parse.text,
            revid: parse.revid.unwrap_or(0),
            // Legacy `action=parse` carries no `ETag` — the cache degrades
            // to title-keyed, always-revalidated storage for this wiki
            // (`FetchedArticle`'s own doc comment covers this fallback).
            etag: None,
            // PRD §7 "Redirect": `redirects=1` on this request (see
            // `legacy_parse_url`'s own doc comment) does resolve a redirect
            // server-side, same as Parsoid's 3xx-follow — but this module
            // doesn't parse the response's `redirects` array back out, a
            // documented scope limit (`FetchedArticle::redirected`'s doc
            // comment): the legacy fallback path is Wikipedia-secondary
            // (FR-ML-5 third-party wikis), so its own redirect notice never
            // reaches `main::open_title`'s "Redirected from X" banner today.
            redirected: false,
        })
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
        Ok(self.fetch_summary_full(lang, title).await?.extract)
    }

    /// The full page-summary payload (PRD FR-NV-5 link preview): title,
    /// Wikidata description, lead extract, and thumbnail URL. Same endpoint and
    /// SEC-1 sanitization as [`fetch_summary`] (which is now this method's
    /// extract-only projection), so the T2 offline path and the interactive
    /// preview share one request shape and one parser.
    pub async fn fetch_summary_full(&self, lang: &str, title: &str) -> Result<SummaryData> {
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
        let clean = |s: &str| crate::sanitize::sanitize_single_line(s).into_owned();
        Ok(SummaryData {
            title: clean(&parsed.title),
            description: clean(&parsed.description),
            extract: clean(&parsed.extract),
            thumbnail: parsed
                .thumbnail
                .map(|t| clean(&t.source))
                .filter(|s| !s.is_empty()),
        })
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

    /// PRD FR-DL-3 v2 (§6.2 rule 1's stated exception): the Lift Wing
    /// `articlequality` model, queried when `prop=pageassessments` isn't
    /// available for the active wiki at all (see `main::enrich_article`'s
    /// own gate — never called when the wiki *has* PageAssessments, only
    /// when it doesn't). `liftwing_base_url` is a caller-supplied parameter
    /// (config-resolved, §6.2 rule 2), not `self.host()`: Lift Wing is
    /// gateway-hosted, the one endpoint in this whole client that isn't
    /// per-wiki, so it can't be derived from the active wiki's own template.
    ///
    /// Keyed by revision id, not title — the model scores one specific
    /// revision's text, not "whatever the latest revision happens to be",
    /// so the caller passes the revid the article view already fetched
    /// (`Tab::current_revid`) rather than this method spending a second
    /// "what's the latest revid" round trip to look one up (PRD's "sparing"
    /// framing for this fallback). `rev_id == 0` (the "unknown/degraded"
    /// convention `Tab::current_revid` documents) is the caller's problem to
    /// gate on; this method does not special-case it and will simply ask
    /// Lift Wing about revision `0`, getting back no score.
    ///
    /// Returns `Ok(None)` — not an error — for "no score for this revision"
    /// and for a response that doesn't parse into the expected shape: an
    /// optional quality badge going missing must never surface as a fetch
    /// failure. **UNVERIFIED against a live endpoint** — see
    /// [`LiftWingResponse`]'s own doc comment for why.
    pub async fn fetch_liftwing_quality(
        &self,
        liftwing_base_url: &str,
        lang: &str,
        rev_id: u64,
    ) -> Result<Option<QualityClass>> {
        let url = format!("{liftwing_base_url}/models/{lang}wiki-articlequality:predict");
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "rev_id": rev_id, "extended_output": true }))
            .send()
            .await
            .context("requesting Lift Wing article quality")?
            .error_for_status()
            .context("Lift Wing article quality request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading Lift Wing response body")?;
        let parsed: LiftWingResponse =
            serde_json::from_slice(&bytes).context("parsing Lift Wing response")?;
        let wiki_key = format!("{lang}wiki");
        Ok(parsed
            .get(&wiki_key)
            .and_then(|w| w.scores.get(&rev_id.to_string()))
            .and_then(|s| QualityClass::from_liftwing_score(&s.articlequality.score)))
    }

    /// PRD FR-PF-3's per-article categories: `prop=categories` for up to 50
    /// titles in **one** batched call (§6.2 rule 10 — never a per-article
    /// fanout), used to build the interest-affinity model. `clshow=!hidden`
    /// asks the API to drop its own hidden/tracking categories server-side;
    /// the remaining maintenance categories the API doesn't flag hidden are
    /// dropped client-side by `interest::is_maintenance_category`. Returns
    /// `title -> category names` (in `Category:Foo` API form — the interest
    /// model normalizes them). Category titles are sanitized (SEC-1) as they
    /// become model keys and, via `:interests`, displayed text.
    pub async fn fetch_categories(
        &self,
        lang: &str,
        titles: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        let joined = titles.join("|");
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&prop=categories&clshow=!hidden&cllimit=500&titles={}",
            self.host(lang),
            urlencoding::encode(&joined)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting page categories")?
            .error_for_status()
            .context("page categories request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading page categories response body")?;
        let parsed: CategoriesResponse =
            serde_json::from_slice(&bytes).context("parsing page categories response")?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for page in parsed.query.map(|q| q.pages).unwrap_or_default() {
            let title = crate::sanitize::sanitize_single_line(&page.title).into_owned();
            let cats = page
                .categories
                .into_iter()
                .map(|c| crate::sanitize::sanitize_single_line(&c.title).into_owned())
                .collect();
            out.insert(title, cats);
        }
        Ok(out)
    }

    /// PRD FR-DL-5's batched redlink check: `source_title`'s own outgoing
    /// links (`generator=links`, same `gpllimit=50` cap `fetch_link_
    /// pageviews` uses) joined with `prop=info` in **one** request — never a
    /// per-link fanout (§6.2 rule 10 / NF-NET-5) — returning the subset
    /// `info` reports `missing: true` for. This is the *fallback* path
    /// (PRD: "otherwise"): it runs after — and independently of — Parsoid's
    /// own cheaper `class="new"` pre-marking (`doc::SpanStyle::RedLink`,
    /// checked at parse time with no network at all), so it only ever needs
    /// to catch what that cheaper signal missed. Callers are expected to
    /// gate this on `App::prefetch_active()` (PRD's "skippable on budget") —
    /// this method itself has no budget awareness, matching `page_
    /// assessments`'s own scope (a plain batched call, not a netqueue job;
    /// see `main::open_title`'s doc comment for why this chunk chose that
    /// over full substrate integration).
    pub async fn fetch_missing_links(
        &self,
        lang: &str,
        source_title: &str,
    ) -> Result<HashSet<String>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&generator=links&titles={}&gpllimit=50&gplnamespace=0&prop=info",
            self.host(lang),
            urlencoding::encode(&source_title.replace(' ', "_"))
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting link info")?
            .error_for_status()
            .context("link info request failed")?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading link info response body")?;
        let parsed: MissingLinksResponse =
            serde_json::from_slice(&bytes).context("parsing link info response")?;
        Ok(parsed
            .query
            .map(|q| q.pages)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.missing)
            .map(|p| crate::sanitize::sanitize_single_line(&p.title).into_owned())
            .collect())
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

    /// PRD NF-NET-9 / FR-ACC-1: an authenticated GET carrying `Authorization:
    /// Bearer {token}`. This is the integration seam the watchlist (FR-ACC-2)
    /// and notifications (FR-ACC-3) consumers — the next Phase C chunk — build
    /// on; authenticated requests also identify the client for the elevated
    /// (5,000/h) rate limits (§6.5 NF-NET-9). The URL is built by the caller
    /// (an Action API query, typically) so this stays a thin bearer-attaching
    /// wrapper over the shared client.
    pub async fn authed_get(&self, url: &str, access_token: &str) -> Result<reqwest::Response> {
        self.http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?
            .error_for_status()
            .context("authenticated request failed")
    }

    /// PRD FR-ACC-1: the logged-in username via authenticated `meta=userinfo`,
    /// fetched right after token exchange to label the session (and re-checked
    /// on demand to confirm the token still authenticates). Rejects an
    /// `anon: true` reply — a token the server ignored comes back anonymous,
    /// which must not be mistaken for a successful login. The name is
    /// sanitized (SEC-1) since it becomes displayable status-bar text.
    pub async fn fetch_userinfo(&self, lang: &str, access_token: &str) -> Result<String> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&meta=userinfo",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading userinfo response body")?;
        let parsed: UserInfoResponse =
            serde_json::from_slice(&bytes).context("parsing userinfo response")?;
        if parsed.query.userinfo.anon || parsed.query.userinfo.name.is_empty() {
            bail!("the access token did not authenticate (userinfo returned an anonymous session)");
        }
        Ok(crate::sanitize::sanitize_single_line(&parsed.query.userinfo.name).into_owned())
    }

    /// PRD §6.2 rule 8: `meta=tokens&type=csrf|watch` — the token every
    /// write action (`watch`, `thank`, `echomarkread`) needs. `kind` is
    /// `"csrf"` or `"watch"`; the response's own key is `{kind}token`, read
    /// back generically rather than one struct per kind since the shape is
    /// otherwise identical. Cached by `account::TokenCache`, not here — this
    /// method always hits the network, matching every other `fetch_*` in
    /// this module.
    pub async fn fetch_token(&self, lang: &str, access_token: &str, kind: &str) -> Result<String> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&meta=tokens&type={kind}",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading tokens response body")?;
        #[derive(Deserialize)]
        struct TokensResponse {
            query: TokensQuery,
        }
        #[derive(Deserialize)]
        struct TokensQuery {
            tokens: HashMap<String, String>,
        }
        let parsed: TokensResponse =
            serde_json::from_slice(&bytes).context("parsing tokens response")?;
        parsed
            .query
            .tokens
            .get(&format!("{kind}token"))
            .cloned()
            .ok_or_else(|| anyhow!("tokens response carried no {kind}token"))
    }

    /// PRD FR-ACC-2's `w` toggle: whether the current session already
    /// watches `title` (`prop=info&inprop=watched`), read fresh immediately
    /// before every toggle so the watch/unwatch decision is never made from
    /// a stale local guess. Defaults to not-watched on any unparseable
    /// response — the safer of the two guesses (worst case `w` watches an
    /// already-watched page again, a harmless no-op on the real API).
    pub async fn fetch_watched_status(
        &self,
        lang: &str,
        access_token: &str,
        title: &str,
    ) -> Result<bool> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&prop=info&inprop=watched&titles={}",
            self.host(lang),
            urlencoding::encode(&title.replace(' ', "_"))
        );
        let resp = self.authed_get(&url, access_token).await?;
        let bytes = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading watched-status response body")?;
        Ok(crate::account::parse_watched_status(&bytes))
    }

    /// A `POST` carrying `Authorization: Bearer` plus a form body, for the
    /// write actions (PRD §6.2 rule 8: `watch`, `thank`, `echomarkread`).
    /// Deliberately does **not** call `error_for_status`: the classic Action
    /// API reports its own errors (including `badtoken`) as HTTP 200 with an
    /// `{"error":{...}}` body, so the caller must be able to read that body
    /// — an early `error_for_status` would only fire on a genuine transport-
    /// level failure (connection reset, 5xx from a proxy), which is exactly
    /// when propagating an error, rather than a body, is correct.
    async fn authed_post_form(
        &self,
        url: &str,
        access_token: &str,
        form: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        let body = form
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let resp = self
            .http
            .post(url)
            .bearer_auth(access_token)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .with_context(|| format!("posting to {url}"))?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading write-action response body")
    }

    /// PRD FR-ACC-2: `action=watch` (`unwatch=1` to remove instead of add).
    /// Returns the raw response body — a badtoken error and a success both
    /// parse from it (see `account::is_badtoken_response`/
    /// `account::parse_watch_outcome`).
    pub async fn watch_raw(
        &self,
        lang: &str,
        access_token: &str,
        title: &str,
        token: &str,
        unwatch: bool,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let mut form = vec![("action", "watch"), ("title", title), ("token", token)];
        if unwatch {
            form.push(("unwatch", "1"));
        }
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-BM-6: the watch-mirror's batched `action=watch`/unwatch —
    /// `titles=A|B|C` in one request (real MediaWiki's own batching for this
    /// module), distinct from [`watch_raw`](Self::watch_raw)'s single-`title`
    /// form the `w` keybinding uses. Same response shape either way (see
    /// `account::parse_watch_batch_outcome`).
    pub async fn watch_batch_raw(
        &self,
        lang: &str,
        access_token: &str,
        titles: &[String],
        token: &str,
        unwatch: bool,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let joined = titles.join("|");
        let mut form = vec![
            ("action", "watch"),
            ("titles", joined.as_str()),
            ("token", token),
        ];
        if unwatch {
            form.push(("unwatch", "1"));
        }
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-BM-5 / SP-7 (uncertain — see `account.rs`'s ReadingLists module
    /// doc for every assumption this build makes): `action=readinglists&
    /// command=setup`, a CSRF-token'd write. Idempotent on a real account
    /// (calling it again once already set up is a documented no-op there);
    /// the test mock always answers success.
    pub async fn readinglists_setup_raw(
        &self,
        lang: &str,
        access_token: &str,
        token: &str,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let form = [
            ("action", "readinglists"),
            ("command", "setup"),
            ("token", token),
        ];
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-BM-5 / SP-7: `command=list` — the account's Reading Lists
    /// (this build only ever uses the default one, `account::default_list_id`).
    pub async fn fetch_readinglists(&self, lang: &str, access_token: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?format=json&formatversion=2&action=readinglists&command=list",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading readinglists-list response body")
    }

    /// PRD FR-BM-5 / SP-7: `command=listentries` — every entry in one list.
    pub async fn fetch_readinglist_entries(
        &self,
        lang: &str,
        access_token: &str,
        list_id: u64,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?format=json&formatversion=2&action=readinglists&command=listentries&list={list_id}",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading readinglists-listentries response body")
    }

    /// PRD FR-BM-5 / SP-7: `command=createentry` — adds `title` (on
    /// `project`, this build's own resolved wiki origin, see
    /// [`wiki_origin`](Self::wiki_origin)) to `list_id`.
    pub async fn readinglists_createentry_raw(
        &self,
        lang: &str,
        access_token: &str,
        list_id: u64,
        project: &str,
        title: &str,
        token: &str,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let list_id_str = list_id.to_string();
        let form = [
            ("action", "readinglists"),
            ("command", "createentry"),
            ("list", list_id_str.as_str()),
            ("project", project),
            ("title", title),
            ("token", token),
        ];
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-BM-5 / SP-7: `command=deleteentry`.
    pub async fn readinglists_deleteentry_raw(
        &self,
        lang: &str,
        access_token: &str,
        entry_id: u64,
        token: &str,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let entry_id_str = entry_id.to_string();
        let form = [
            ("action", "readinglists"),
            ("command", "deleteentry"),
            ("entry", entry_id_str.as_str()),
            ("token", token),
        ];
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-ACC-2: the raw watched-pages list (`list=watchlistraw`).
    pub async fn fetch_watchlistraw(&self, lang: &str, access_token: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&list=watchlistraw",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading watchlistraw response body")
    }

    /// PRD FR-ACC-2: the "what changed" activity feed (`list=watchlist`),
    /// most-recent changes to every currently-watched page.
    pub async fn fetch_watchlist_changes(
        &self,
        lang: &str,
        access_token: &str,
        limit: u32,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&list=watchlist&wlprop=title|timestamp|user|comment|ids&wllimit={limit}",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading watchlist response body")
    }

    /// PRD FR-ACC-3: the unread-count badge (`meta=notifications&
    /// notprop=count`) — fetched at login and on opening the pane only, per
    /// this module's poll-cadence contract (see `account.rs`'s doc comment).
    pub async fn fetch_notifications_count(
        &self,
        lang: &str,
        access_token: &str,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&meta=notifications&notprop=count",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading notifications-count response body")
    }

    /// PRD FR-ACC-3: the alerts/messages list (`notprop=list`).
    pub async fn fetch_notifications_list(
        &self,
        lang: &str,
        access_token: &str,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&meta=notifications&notprop=list",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading notifications-list response body")
    }

    /// PRD FR-ACC-3: `action=echomarkread`. `ids` is ignored when `all` is
    /// set (mark-all-read); otherwise it's the `|`-joined id list to mark.
    pub async fn echomarkread_raw(
        &self,
        lang: &str,
        access_token: &str,
        token: &str,
        all: bool,
        ids: &[String],
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let joined_ids = ids.join("|");
        let mut form = vec![("action", "echomarkread"), ("token", token)];
        if all {
            form.push(("all", "1"));
        } else {
            form.push(("list", joined_ids.as_str()));
        }
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-ACC-4: `list=usercontribs` — public, no auth token needed;
    /// works for any username, logged in or not.
    pub async fn fetch_usercontribs(
        &self,
        lang: &str,
        username: &str,
        limit: u32,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&list=usercontribs&ucuser={}&uclimit={limit}&ucprop=title|timestamp|comment|ids|sizediff",
            self.host(lang),
            urlencoding::encode(username)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting contributions for {username:?}"))?
            .error_for_status()
            .context("usercontribs request failed")?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading usercontribs response body")
    }

    /// PRD FR-ACC-6: `action=thank&rev={revid}`, a purely positive one-way
    /// gesture (never a reciprocal notification back to this client).
    pub async fn thank_raw(
        &self,
        lang: &str,
        access_token: &str,
        revid: u64,
        token: &str,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let revid_str = revid.to_string();
        let form = [
            ("action", "thank"),
            ("rev", revid_str.as_str()),
            ("token", token),
        ];
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-ACC-8: fetches an article's current **wikitext** plus the
    /// revision identity an edit needs for conflict detection —
    /// `action=query&prop=revisions&rvprop=content|ids|timestamp&rvslots=main`.
    /// This is deliberately the *source* wikitext (what the editor fixes),
    /// **not** the Parsoid HTML `fetch_article_html` renders. Authenticated
    /// (the whole edit flow requires a login) so it identifies the client for
    /// the elevated rate limits (NF-NET-9). Parsed by
    /// `editing::parse_wikitext_response`.
    pub async fn fetch_wikitext(
        &self,
        lang: &str,
        title: &str,
        access_token: &str,
    ) -> Result<crate::editing::FetchedWikitext> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&prop=revisions&rvprop=content%7Cids%7Ctimestamp&rvslots=main&titles={}",
            self.host(lang),
            urlencoding::encode(&title.replace(' ', "_"))
        );
        let resp = self.authed_get(&url, access_token).await?;
        let bytes = read_capped(resp, crate::doc::MAX_ARTICLE_HTML_BYTES)
            .await
            .context("reading wikitext response body")?;
        crate::editing::parse_wikitext_response(&bytes).ok_or_else(|| {
            anyhow!("no wikitext for {title:?} on {lang} (missing page or empty revision)")
        })
    }

    /// PRD FR-ACC-8: the ONLY article-write in the product — `action=edit`
    /// with the changed wikitext, an auto summary + optional user note, the
    /// **minor** flag (always set — a typo fix is a minor edit), and
    /// `baserevid`/`basetimestamp` for **conflict detection** (a stale base
    /// makes the API return an `editconflict` error, surfaced not forced —
    /// see `editing::parse_edit_response`). `nocreate=1` is an extra
    /// structural guard: this can only ever *edit an existing* main-namespace
    /// article, never create a page. Returns the raw body — a success, an
    /// edit conflict, and a badtoken all parse from it. There is deliberately
    /// **no** `action=move`/`upload`/`rollback` counterpart anywhere in this
    /// module (FR-ACC-8's hard prohibitions, enforced by absence).
    #[allow(clippy::too_many_arguments)]
    pub async fn edit_raw(
        &self,
        lang: &str,
        access_token: &str,
        title: &str,
        text: &str,
        summary: &str,
        baserevid: u64,
        basetimestamp: &str,
        token: &str,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/w/api.php?format=json&formatversion=2", self.host(lang));
        let baserevid_str = baserevid.to_string();
        let form = [
            ("action", "edit"),
            ("title", title),
            ("text", text),
            ("summary", summary),
            ("minor", "1"),
            ("nocreate", "1"),
            ("baserevid", baserevid_str.as_str()),
            ("basetimestamp", basetimestamp),
            ("token", token),
        ];
        self.authed_post_form(&url, access_token, &form).await
    }

    /// PRD FR-ACC-7: `meta=userinfo&uiprop=options` — the read-only prefs
    /// surface. Never paired with a write; this build has no `action=
    /// options` call anywhere.
    pub async fn fetch_userinfo_options(&self, lang: &str, access_token: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{}/w/api.php?action=query&format=json&formatversion=2&meta=userinfo&uiprop=options",
            self.host(lang)
        );
        let resp = self.authed_get(&url, access_token).await?;
        read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
            .await
            .context("reading userinfo-options response body")
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
        let rewritten_query = parsed
            .rewrittenquery
            .map(|s| crate::sanitize::sanitize_single_line(&s).into_owned());
        Ok(SearchOutcome {
            results: parsed.pages,
            suggestion,
            rewritten_query,
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
    /// Falls back to legacy `action=parse` the same way the foreground
    /// [`Self::fetch_article_html`] does (PRD §6.2 rule 3 / FR-ML-5) — a
    /// wiki without Parsoid REST still prefetches, it just costs a little
    /// more (the failed Parsoid attempt) the first time on `Auto`.
    pub async fn fetch_article_html_bg(
        &self,
        lang: &str,
        title: &str,
    ) -> std::result::Result<BgArticle, BgFailure> {
        let caps = self.capabilities();
        if caps.parser != ParserMode::Legacy {
            match self.fetch_article_html_parsoid_bg(lang, title).await? {
                ParsoidOutcome::Found(article) => return Ok(article),
                // A background prefetch has no user waiting on a specific
                // "doesn't exist" message — both classify as a plain network
                // failure, same as any other non-2xx (`bg_status_failure`'s
                // own posture for a 404).
                ParsoidOutcome::Missing => return Err(BgFailure::Network),
                ParsoidOutcome::Unsupported => {
                    if caps.parser == ParserMode::Parsoid {
                        return Err(BgFailure::Network);
                    }
                }
            }
        }
        self.fetch_article_html_legacy_bg(lang, title).await
    }

    /// The background counterpart of [`Self::fetch_article_html_parsoid`] —
    /// same 404 classification, `BgFailure` instead of `anyhow::Error`.
    async fn fetch_article_html_parsoid_bg(
        &self,
        lang: &str,
        title: &str,
    ) -> std::result::Result<ParsoidOutcome<BgArticle>, BgFailure> {
        let url = format!(
            "{}/w/rest.php/v1/page/{}/html?maxlag={MAXLAG}",
            self.host(lang),
            Self::title_path(title)
        );
        let resp = self.http.get(&url).send().await.map_err(bg_send_error)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            let body = read_capped(resp, MAX_SEARCH_RESPONSE_BYTES)
                .await
                .unwrap_or_default();
            return Ok(if parsoid_404_is_genuine_miss(&body) {
                ParsoidOutcome::Missing
            } else {
                ParsoidOutcome::Unsupported
            });
        }
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
        Ok(ParsoidOutcome::Found(BgArticle {
            html,
            revid,
            etag,
            bytes: len,
        }))
    }

    /// The background counterpart of [`Self::fetch_article_html_legacy`].
    async fn fetch_article_html_legacy_bg(
        &self,
        lang: &str,
        title: &str,
    ) -> std::result::Result<BgArticle, BgFailure> {
        let url = format!(
            "{}&maxlag={MAXLAG}",
            legacy_parse_url(&self.host(lang), title)
        );
        let resp = self.http.get(&url).send().await.map_err(bg_send_error)?;
        if let Some(fail) = bg_status_failure(&resp) {
            return Err(fail);
        }
        let bytes = read_capped(resp, crate::doc::MAX_ARTICLE_HTML_BYTES)
            .await
            .map_err(|_| BgFailure::Network)?;
        let len = bytes.len() as u64;
        let parsed: LegacyParseResponse =
            serde_json::from_slice(&bytes).map_err(|_| BgFailure::Network)?;
        if parsed.error.is_some() {
            return Err(BgFailure::Network);
        }
        let parse = parsed.parse.ok_or(BgFailure::Network)?;
        Ok(BgArticle {
            html: parse.text,
            revid: parse.revid.unwrap_or(0),
            etag: None,
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

    /// FR-SR-4's other zero/poor-result signal: `rewrittenquery`, this
    /// module's own schema extension alongside `suggestion` — same
    /// round-trip/absence contract, and distinct from `suggestion` (a
    /// rewrite typically arrives with non-empty `pages`, since it's what
    /// the engine actually searched for).
    #[test]
    fn search_page_response_parses_the_rewrittenquery_extension() {
        let with_rewrite =
            r#"{"pages": [{"title": "Alan Turing"}], "rewrittenquery": "Alan Turing"}"#;
        let parsed: SearchPageResponse = serde_json::from_str(with_rewrite).unwrap();
        assert_eq!(parsed.pages.len(), 1);
        assert_eq!(parsed.rewrittenquery.as_deref(), Some("Alan Turing"));

        let without_rewrite = r#"{"pages": []}"#;
        let parsed: SearchPageResponse = serde_json::from_str(without_rewrite).unwrap();
        assert_eq!(parsed.rewrittenquery, None);
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

    /// [`parsoid_404_is_genuine_miss`]'s classification table: a real core
    /// REST error envelope (`errorKey` present) is a genuine miss; anything
    /// else — no body, an unrelated JSON shape, an HTML error page — means
    /// "this wiki doesn't run Parsoid REST" (PRD §6.2 rule 3 / FR-ML-5).
    #[test]
    fn parsoid_404_classification_table() {
        assert!(parsoid_404_is_genuine_miss(
            br#"{"httpCode":404,"httpReason":"Not Found","errorKey":"rest-nonexistent-title"}"#
        ));
        assert!(!parsoid_404_is_genuine_miss(b""));
        assert!(!parsoid_404_is_genuine_miss(b"<html>404 Not Found</html>"));
        assert!(!parsoid_404_is_genuine_miss(br#"{"error":"not found"}"#));
    }

    /// PRD §6.2 rule 3: a genuinely missing title on a Parsoid-capable wiki
    /// still bails the way it always has — the mock's 404 carries the real
    /// core-REST error envelope (`errorKey`), so no legacy fallback engages
    /// and only one request is ever made.
    #[tokio::test]
    async fn fetch_article_html_genuine_miss_bails_without_a_legacy_fallback() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body =
                    br#"{"httpCode":404,"httpReason":"Not Found","errorKey":"rest-nonexistent-title"}"#;
                let header = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let err = client
            .fetch_article_html("en", "Nonexistent")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no article named"),
            "unexpected error: {err}"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// PRD §6.2 rule 3 / FR-ML-5's core degradation: a wiki whose Parsoid
    /// REST 404s in a shape that doesn't look like a genuine miss (here, a
    /// bare body — exactly what a route that doesn't exist at all returns)
    /// falls back to legacy `action=parse`, in `Auto` mode, and still
    /// renders — the fixture a third-party wiki without Parsoid REST hits.
    #[tokio::test]
    async fn fetch_article_html_falls_back_to_legacy_parse_when_parsoid_is_unsupported() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let requests2 = requests.clone();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let first_line = head.lines().next().unwrap_or_default().to_string();
                requests2.lock().unwrap().push(first_line.clone());
                if first_line.contains("/rest.php/v1/page/") {
                    // No body at all — the "route doesn't exist" shape.
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    let body = br#"{"parse":{"title":"Test","revid":55,"text":"<p>Legacy body with a <a href=\"./Other\">link</a>.</p><h2>Section</h2>"}}"#;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                }
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let fetched = client
            .fetch_article_html("en", "Test")
            .await
            .expect("auto mode must fall back to legacy, not error");
        assert_eq!(fetched.revid, 55);
        assert_eq!(fetched.etag, None);
        assert!(fetched.html.contains("Legacy body"));
        let seen = requests.lock().unwrap().clone();
        assert_eq!(
            seen.len(),
            2,
            "expected one Parsoid attempt then one legacy fallback: {seen:?}"
        );
        assert!(seen[0].contains("/rest.php/v1/page/"));
        assert!(seen[1].contains("action=parse"));
    }

    /// The legacy-HTML fixture above, run through the same `doc.rs` pipeline
    /// every Parsoid response goes through (PRD §6.2 rule 3: "parse the
    /// legacy HTML through the same doc.rs pipeline") — links and sections
    /// still resolve even without Parsoid's RDFa/`data-mw` annotations.
    #[tokio::test]
    async fn legacy_parse_html_parses_to_blocks_links_and_sections_via_doc_rs() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                if head.contains("/rest.php/v1/page/") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    let body = br#"{"parse":{"title":"Legacy Article","revid":7,"text":"<div class=\"mw-parser-output\"><p>Intro paragraph linking to <a href=\"/wiki/Other_Page\" title=\"Other Page\">Other Page</a>.</p><h2><span class=\"mw-headline\" id=\"A_section\">A section</span></h2><p>More text.</p></div>"}}"#;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                }
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let fetched = client
            .fetch_article_html("en", "Legacy Article")
            .await
            .unwrap();
        let doc = crate::doc::parse_article_html("Legacy Article", &fetched.html);
        assert!(
            doc.blocks
                .iter()
                .any(|b| format!("{b:?}").contains("Intro paragraph")),
            "lead paragraph text must survive: {:?}",
            doc.blocks
        );
        let links = crate::doc::collect_links(&doc);
        assert!(
            links
                .iter()
                .any(|l| l.internal_title.as_deref() == Some("Other Page")),
            "internal link must still resolve without Parsoid annotations: {links:?}"
        );
        let sections = crate::doc::section_outline(&doc);
        assert!(
            !sections.is_empty(),
            "a legacy <h2> must still produce a section: {sections:?}"
        );
    }

    /// PRD §6.2 rule 3's per-wiki override: `parser = "legacy"` skips the
    /// Parsoid attempt entirely — only one request is ever made, straight to
    /// `action=parse`, saving the doomed round trip on a wiki already known
    /// to lack Parsoid REST.
    #[tokio::test]
    async fn fetch_article_html_with_parser_legacy_never_attempts_parsoid() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits2 = hits.clone();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let requests2 = requests.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                *requests2.lock().unwrap() = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let body = br#"{"parse":{"title":"Test","revid":9,"text":"<p>Legacy only.</p>"}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::with_wiki(
            "legacywiki".to_string(),
            format!("http://127.0.0.1:{port}"),
            WikiCapabilities {
                parser: ParserMode::Legacy,
                wikifeeds: false,
                pageviews: false,
                pageassessments: false,
            },
            DEFAULT_CONTACT,
        )
        .unwrap();
        let fetched = client.fetch_article_html("en", "Test").await.unwrap();
        assert_eq!(fetched.revid, 9);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(requests.lock().unwrap().contains("action=parse"));
    }

    /// PRD §6.2 rule 3's opposite override: `parser = "parsoid"` disables
    /// the fallback — an unsupported-shaped 404 surfaces as an error instead
    /// of silently trying legacy, and only one request is made.
    #[tokio::test]
    async fn fetch_article_html_with_parser_parsoid_never_falls_back() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let client = WikiClient::with_wiki(
            "forced-parsoid".to_string(),
            format!("http://127.0.0.1:{port}"),
            WikiCapabilities {
                parser: ParserMode::Parsoid,
                wikifeeds: false,
                pageviews: false,
                pageassessments: false,
            },
            DEFAULT_CONTACT,
        )
        .unwrap();
        let err = client.fetch_article_html("en", "Test").await.unwrap_err();
        assert!(err.to_string().contains("parser=parsoid"), "{err}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Background prefetch (PRD FR-PF-1/2) gets the same Parsoid→legacy
    /// fallback as the foreground fetch, so a wiki without Parsoid REST can
    /// still be prefetched, not just opened interactively.
    #[tokio::test]
    async fn fetch_article_html_bg_falls_back_to_legacy_parse_when_parsoid_is_unsupported() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                if head.contains("/rest.php/v1/page/") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    let body =
                        br#"{"parse":{"title":"Test","revid":3,"text":"<p>bg legacy body</p>"}}"#;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                }
            }
        });
        let client = WikiClient::new(format!("http://127.0.0.1:{port}")).unwrap();
        let fetched = client
            .fetch_article_html_bg("en", "Test")
            .await
            .expect("bg fetch must fall back to legacy too");
        assert_eq!(fetched.revid, 3);
        assert!(fetched.html.contains("bg legacy body"));
    }

    /// PRD FR-ML-4's `:wiki` switch: repointing the client's host and
    /// capabilities is visible on the very next request, through every clone
    /// (the `Arc<RwLock<_>>` this is the whole reason for).
    #[test]
    fn switch_wiki_repoints_every_clone() {
        let client = WikiClient::new("https://{lang}.wikipedia.org".to_string()).unwrap();
        let clone = client.clone();
        assert_eq!(client.active_wiki_name(), "wikipedia");
        assert!(client.capabilities().wikifeeds);

        client.switch_wiki(
            "wiktionary".to_string(),
            "https://{lang}.wiktionary.org".to_string(),
            WikiCapabilities {
                parser: ParserMode::Auto,
                wikifeeds: false,
                pageviews: false,
                pageassessments: false,
            },
        );

        assert_eq!(clone.active_wiki_name(), "wiktionary");
        assert_eq!(clone.host("en"), "https://en.wiktionary.org");
        assert!(!clone.capabilities().wikifeeds);
    }

    #[test]
    fn parser_mode_parses_the_three_documented_values() {
        assert_eq!(ParserMode::parse("auto"), Some(ParserMode::Auto));
        assert_eq!(ParserMode::parse("parsoid"), Some(ParserMode::Parsoid));
        assert_eq!(ParserMode::parse("legacy"), Some(ParserMode::Legacy));
        assert_eq!(ParserMode::parse("bogus"), None);
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

    // ---- Lift Wing (PRD FR-DL-3 v2, §6.2 rule 1's stated exception) -------

    /// The documented request shape: `POST` to `{base}/models/{lang}wiki-
    /// articlequality:predict` with a JSON body naming the revision — and
    /// the `prediction` field of a successful response mapped straight to a
    /// `QualityClass`.
    #[tokio::test]
    async fn fetch_liftwing_quality_requests_the_documented_endpoint_shape() {
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
                let body = br#"{"enwiki":{"scores":{"1001":{"articlequality":{"score":{
                    "prediction":"GA",
                    "probability":{"FA":0.1,"GA":0.6,"B":0.2,"C":0.05,"Start":0.03,"Stub":0.02}
                }}}}}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new("http://unused/{lang}".to_string()).unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let class = client
            .fetch_liftwing_quality(&base, "en", 1001)
            .await
            .unwrap();
        assert_eq!(class, Some(QualityClass::Ga));
        let head = captured.lock().unwrap().clone();
        let request_line = head.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("POST /models/enwiki-articlequality:predict"),
            "{request_line:?}"
        );
        assert!(head.contains("\"rev_id\":1001"), "{head:?}");
    }

    /// A response for a wiki/revid this call didn't ask about (or no score
    /// at all) is "no badge," not an error — the caller (`enrich_article`)
    /// must be free to treat a missing Lift Wing score exactly like a wiki
    /// with no PageAssessments data for a title.
    #[tokio::test]
    async fn fetch_liftwing_quality_with_no_matching_score_is_none_not_an_error() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let body = br#"{"enwiki":{"scores":{}}}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        let client = WikiClient::new("http://unused/{lang}".to_string()).unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let class = client
            .fetch_liftwing_quality(&base, "en", 9999)
            .await
            .unwrap();
        assert_eq!(class, None);
    }

    /// `from_liftwing_score`'s documented fallback: an unrecognized/missing
    /// `prediction` string still resolves via an argmax over `probability`,
    /// rather than reporting no badge when the distribution clearly favors
    /// one recognized class.
    #[test]
    fn liftwing_score_falls_back_to_probability_argmax_when_prediction_is_unusable() {
        let score = LiftWingScore {
            prediction: String::new(),
            probability: HashMap::from([
                ("FA".to_string(), 0.05),
                ("GA".to_string(), 0.05),
                ("B".to_string(), 0.75),
                ("C".to_string(), 0.10),
                ("Start".to_string(), 0.03),
                ("Stub".to_string(), 0.02),
            ]),
        };
        assert_eq!(
            QualityClass::from_liftwing_score(&score),
            Some(QualityClass::B)
        );
    }

    /// `prediction` wins over `probability` even when they'd disagree —
    /// documented precedence (the model's own reported top class, not a
    /// locally recomputed one).
    #[test]
    fn liftwing_score_prefers_prediction_over_recomputing_from_probability() {
        let score = LiftWingScore {
            prediction: "Stub".to_string(),
            probability: HashMap::from([("FA".to_string(), 0.99), ("Stub".to_string(), 0.01)]),
        };
        assert_eq!(
            QualityClass::from_liftwing_score(&score),
            Some(QualityClass::Stub)
        );
    }

    /// A score with neither a usable `prediction` nor any recognized
    /// `probability` entry parses to no class at all — a Lift Wing response
    /// this build can't make sense of is "no badge," never invented.
    #[test]
    fn liftwing_score_with_nothing_recognizable_is_none() {
        let score = LiftWingScore {
            prediction: "unknown-model-output".to_string(),
            probability: HashMap::from([("FL".to_string(), 0.9)]),
        };
        assert_eq!(QualityClass::from_liftwing_score(&score), None);
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

    /// PRD FR-DL-3's exact display glyphs — locks the mapping the status bar
    /// and search-result rows both render.
    #[test]
    fn quality_class_badge_glyphs() {
        assert_eq!(QualityClass::Fa.badge(), "★FA");
        assert_eq!(QualityClass::Ga.badge(), "+GA");
        assert_eq!(QualityClass::B.badge(), "B");
        assert_eq!(QualityClass::C.badge(), "C");
        assert_eq!(QualityClass::Start.badge(), "Start");
        assert_eq!(QualityClass::Stub.badge(), "Stub");
    }

    /// PRD FR-DL-5 / §6.2 rule 10: `fetch_missing_links` sends the source
    /// article's own outgoing links in **one** `generator=links&prop=info`
    /// request (never one per link) and returns only the titles the response
    /// marked `missing: true`.
    #[tokio::test]
    async fn fetch_missing_links_batches_one_source_into_one_request() {
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
                    {"title":"Real Article","pageid":1},
                    {"title":"Nonexistent Concept X","missing":true},
                    {"title":"Uncharted Topic Y","missing":true}
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
        let missing = client
            .fetch_missing_links("en", "Redlink Showcase")
            .await
            .unwrap();
        assert_eq!(missing.len(), 2);
        assert!(missing.contains("Nonexistent Concept X"));
        assert!(missing.contains("Uncharted Topic Y"));
        assert!(
            !missing.contains("Real Article"),
            "a page present without `missing: true` must not be reported as a redlink"
        );
        let head = captured.lock().unwrap().clone();
        let request_line = head.lines().next().unwrap_or_default();
        assert!(request_line.contains("generator=links"), "{request_line:?}");
        assert!(request_line.contains("prop=info"), "{request_line:?}");
        assert!(
            request_line.contains(&urlencoding::encode("Redlink_Showcase").into_owned()),
            "{request_line:?}"
        );
    }

    /// A page absent from the response entirely (as opposed to present with
    /// `missing: false`) must not be treated as missing — only an explicit
    /// `missing: true` counts.
    #[test]
    fn missing_links_response_only_flags_explicit_missing_true() {
        let json = br#"{"query":{"pages":[
            {"title":"A","pageid":1},
            {"title":"B","missing":true},
            {"title":"C"}
        ]}}"#;
        let parsed: MissingLinksResponse = serde_json::from_slice(json).unwrap();
        let pages = parsed.query.unwrap().pages;
        let missing: Vec<&str> = pages
            .iter()
            .filter(|p| p.missing)
            .map(|p| p.title.as_str())
            .collect();
        assert_eq!(missing, vec!["B"]);
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

    // ---- PRD FR-ACC-2/3/4/6/7: watchlist/notifications/contribs/thank/prefs

    /// Captures one HTTP request's path+query (for a GET) or body (for a
    /// POST), plus its `Authorization` header — the request-shape assertion
    /// every write-action test below needs. Mirrors `auth.rs`'s own
    /// `read_http_request`, kept local here rather than shared across
    /// modules since each is a small, throwaway test fixture.
    struct CapturedRequest {
        request_line: String,
        body: String,
        authorization: String,
    }

    fn spawn_one_shot_server(
        response_json: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<CapturedRequest>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(hdr_end) = text.find("\r\n\r\n") {
                        let content_len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= hdr_end + 4 + content_len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                let request_line = headers.lines().next().unwrap_or("").to_string();
                let authorization = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("authorization:")
                            .map(|_| {
                                l.split_once(':')
                                    .map(|x| x.1)
                                    .unwrap_or("")
                                    .trim()
                                    .to_string()
                            })
                    })
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_json}",
                    response_json.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = tx.send(CapturedRequest {
                    request_line,
                    body: body.to_string(),
                    authorization,
                });
            }
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    #[tokio::test]
    async fn fetch_token_reads_the_named_token_and_carries_the_bearer_header() {
        let (base, rx) =
            spawn_one_shot_server(r#"{"query":{"tokens":{"csrftoken":"CSRF123+\\"}}}"#);
        let client = WikiClient::new(base).unwrap();
        let token = client
            .fetch_token("en", "access-tok", "csrf")
            .await
            .unwrap();
        assert_eq!(token, "CSRF123+\\");
        let req = rx.recv().unwrap();
        assert!(
            req.request_line.contains("meta=tokens"),
            "{}",
            req.request_line
        );
        assert!(
            req.request_line.contains("type=csrf"),
            "{}",
            req.request_line
        );
        assert_eq!(req.authorization, "Bearer access-tok");
    }

    #[tokio::test]
    async fn watch_raw_request_carries_action_title_and_token() {
        let (base, rx) =
            spawn_one_shot_server(r#"{"watch":[{"ns":0,"title":"Alan Turing","watched":true}]}"#);
        let client = WikiClient::new(base).unwrap();
        let body = client
            .watch_raw("en", "access-tok", "Alan Turing", "WATCHTOK", false)
            .await
            .unwrap();
        assert!(crate::account::parse_watch_outcome(&body).is_some());
        let req = rx.recv().unwrap();
        assert!(req.request_line.starts_with("POST"), "{}", req.request_line);
        assert!(req.body.contains("action=watch"), "{}", req.body);
        assert!(
            req.body.contains("title=Alan%20Turing") || req.body.contains("title=Alan+Turing"),
            "{}",
            req.body
        );
        assert!(req.body.contains("token=WATCHTOK"), "{}", req.body);
        assert!(
            !req.body.contains("unwatch"),
            "a watch (not unwatch) must not send the flag"
        );
        assert_eq!(req.authorization, "Bearer access-tok");
    }

    #[tokio::test]
    async fn watch_raw_sends_unwatch_when_requested() {
        let (base, rx) =
            spawn_one_shot_server(r#"{"watch":[{"ns":0,"title":"X","unwatched":true}]}"#);
        let client = WikiClient::new(base).unwrap();
        let _ = client
            .watch_raw("en", "access-tok", "X", "WATCHTOK", true)
            .await
            .unwrap();
        let req = rx.recv().unwrap();
        assert!(req.body.contains("unwatch=1"), "{}", req.body);
    }

    #[tokio::test]
    async fn echomarkread_raw_request_carries_the_id_list_and_token() {
        let (base, rx) =
            spawn_one_shot_server(r#"{"query":{"echomarkread":{"result":"success"}}}"#);
        let client = WikiClient::new(base).unwrap();
        let ids = vec!["101".to_string(), "201".to_string()];
        let _ = client
            .echomarkread_raw("en", "access-tok", "CSRFTOK", false, &ids)
            .await
            .unwrap();
        let req = rx.recv().unwrap();
        assert!(req.body.contains("action=echomarkread"), "{}", req.body);
        assert!(req.body.contains("token=CSRFTOK"), "{}", req.body);
        assert!(req.body.contains("list=101%7C201"), "{}", req.body);
        assert!(!req.body.contains("all="), "{}", req.body);
    }

    #[tokio::test]
    async fn echomarkread_raw_sends_all_when_marking_everything_read() {
        let (base, rx) =
            spawn_one_shot_server(r#"{"query":{"echomarkread":{"result":"success"}}}"#);
        let client = WikiClient::new(base).unwrap();
        let _ = client
            .echomarkread_raw("en", "access-tok", "CSRFTOK", true, &[])
            .await
            .unwrap();
        let req = rx.recv().unwrap();
        assert!(req.body.contains("all=1"), "{}", req.body);
    }

    #[tokio::test]
    async fn thank_raw_request_carries_the_revid_and_csrf_token() {
        let (base, rx) = spawn_one_shot_server(r#"{"result":{"success":1}}"#);
        let client = WikiClient::new(base).unwrap();
        let body = client
            .thank_raw("en", "access-tok", 5103, "CSRFTOK")
            .await
            .unwrap();
        assert!(crate::account::thank_succeeded(&body));
        let req = rx.recv().unwrap();
        assert!(req.body.contains("action=thank"), "{}", req.body);
        assert!(req.body.contains("rev=5103"), "{}", req.body);
        assert!(req.body.contains("token=CSRFTOK"), "{}", req.body);
    }

    #[tokio::test]
    async fn fetch_wikitext_requests_source_content_and_carries_the_bearer() {
        // PRD FR-ACC-8: the edit flow fetches SOURCE wikitext (revisions),
        // never the Parsoid HTML, authenticated.
        let (base, rx) = spawn_one_shot_server(
            r#"{"query":{"pages":[{"title":"Alan Turing","revisions":[{"revid":1001,"timestamp":"2026-07-15T08:00:00Z","slots":{"main":{"content":"Alan Turing was a mathematician."}}}]}]}}"#,
        );
        let client = WikiClient::new(base).unwrap();
        let got = client
            .fetch_wikitext("en", "Alan Turing", "access-tok")
            .await
            .unwrap();
        assert_eq!(got.text, "Alan Turing was a mathematician.");
        assert_eq!(got.revid, 1001);
        let req = rx.recv().unwrap();
        assert!(
            req.request_line.contains("prop=revisions"),
            "{}",
            req.request_line
        );
        assert!(
            req.request_line.contains("rvprop=content"),
            "{}",
            req.request_line
        );
        assert_eq!(req.authorization, "Bearer access-tok");
    }

    #[tokio::test]
    async fn edit_raw_request_carries_summary_minor_baserevid_and_token() {
        // PRD FR-ACC-8: the save request shape — summary, minor flag,
        // baserevid/basetimestamp (conflict detection), nocreate guard, CSRF
        // token, all under the Bearer header.
        let (base, rx) = spawn_one_shot_server(
            r#"{"edit":{"result":"Success","newrevid":1002,"oldrevid":1001}}"#,
        );
        let client = WikiClient::new(base).unwrap();
        let body = client
            .edit_raw(
                "en",
                "access-tok",
                "Alan Turing",
                "Alan Turing was a brilliant mathematician.",
                "Typo fix via wikitui",
                1001,
                "2026-07-15T08:00:00Z",
                "CSRFTOK",
            )
            .await
            .unwrap();
        assert_eq!(
            crate::editing::parse_edit_response(&body),
            crate::editing::EditOutcome::Success { newrevid: 1002 }
        );
        let req = rx.recv().unwrap();
        assert!(req.request_line.starts_with("POST"), "{}", req.request_line);
        assert!(req.body.contains("action=edit"), "{}", req.body);
        assert!(req.body.contains("summary=Typo%20fix"), "{}", req.body);
        assert!(req.body.contains("minor=1"), "{}", req.body);
        assert!(req.body.contains("nocreate=1"), "{}", req.body);
        assert!(req.body.contains("baserevid=1001"), "{}", req.body);
        assert!(req.body.contains("basetimestamp="), "{}", req.body);
        assert!(req.body.contains("token=CSRFTOK"), "{}", req.body);
        assert_eq!(req.authorization, "Bearer access-tok");
    }

    #[tokio::test]
    async fn edit_raw_surfaces_an_edit_conflict_body() {
        // PRD FR-ACC-8: a stale base revid comes back as an editconflict the
        // caller reads off the response (never forced).
        let (base, _rx) = spawn_one_shot_server(
            r#"{"error":{"code":"editconflict","info":"Edit conflict detected"}}"#,
        );
        let client = WikiClient::new(base).unwrap();
        let body = client
            .edit_raw(
                "en",
                "access-tok",
                "Alan Turing",
                "t",
                "s",
                999,
                "ts",
                "TOK",
            )
            .await
            .unwrap();
        assert_eq!(
            crate::editing::parse_edit_response(&body),
            crate::editing::EditOutcome::Conflict
        );
    }

    #[tokio::test]
    async fn fetch_usercontribs_encodes_the_requested_username() {
        let (base, rx) = spawn_one_shot_server(r#"{"query":{"usercontribs":[]}}"#);
        let client = WikiClient::new(base).unwrap();
        let _ = client
            .fetch_usercontribs("en", "Jane Q. Editor", 10)
            .await
            .unwrap();
        let req = rx.recv().unwrap();
        assert!(
            req.request_line.contains("list=usercontribs"),
            "{}",
            req.request_line
        );
        assert!(
            req.request_line.contains("ucuser=Jane") && req.request_line.contains("Editor"),
            "{}",
            req.request_line
        );
        // Public endpoint — no Bearer header at all (works logged out).
        assert!(req.authorization.is_empty(), "{}", req.authorization);
    }

    #[tokio::test]
    async fn fetch_watchlistraw_and_watchlist_changes_send_the_bearer_token() {
        let (base, rx) =
            spawn_one_shot_server(r#"{"watchlistraw":[{"ns":0,"title":"Alan Turing"}]}"#);
        let client = WikiClient::new(base).unwrap();
        let body = client.fetch_watchlistraw("en", "access-tok").await.unwrap();
        assert_eq!(
            crate::account::parse_watchlistraw(&body).unwrap(),
            vec!["Alan Turing".to_string()]
        );
        let req = rx.recv().unwrap();
        assert!(
            req.request_line.contains("list=watchlistraw"),
            "{}",
            req.request_line
        );
        assert_eq!(req.authorization, "Bearer access-tok");
    }

    #[tokio::test]
    async fn fetch_watched_status_parses_the_authed_response() {
        let (base, _rx) = spawn_one_shot_server(
            r#"{"query":{"pages":[{"title":"Alan Turing","watched":true}]}}"#,
        );
        let client = WikiClient::new(base).unwrap();
        let watched = client
            .fetch_watched_status("en", "access-tok", "Alan Turing")
            .await
            .unwrap();
        assert!(watched);
    }

    #[tokio::test]
    async fn fetch_userinfo_options_round_trips_prefs() {
        let (base, rx) = spawn_one_shot_server(
            r#"{"query":{"userinfo":{"id":42,"name":"MockWikipedian","options":{"skin":"vector-2022","language":"en"},"editcount":7,"emailauthenticated":"2020-01-01T00:00:00Z"}}}"#,
        );
        let client = WikiClient::new(base).unwrap();
        let body = client
            .fetch_userinfo_options("en", "access-tok")
            .await
            .unwrap();
        let prefs = crate::account::parse_userinfo_options(&body).unwrap();
        assert_eq!(prefs.skin.as_deref(), Some("vector-2022"));
        assert_eq!(prefs.editcount, Some(7));
        let req = rx.recv().unwrap();
        assert!(
            req.request_line.contains("uiprop=options"),
            "{}",
            req.request_line
        );
    }
}
