//! The two v1.0 prefetch sources (PRD FR-PF-1 link-rank, FR-PF-2 trending)
//! and the transparency reasons (FR-PF-4) that ride the background substrate
//! ([`crate::netqueue`]). Everything here is pure data + parsing so it is
//! unit-testable without the network: the ranking join, the reason strings,
//! the Wikifeeds JSON shape, and the once-per-day feed cache.
//!
//! The ranking primitive (§6.2 rule 7 / Appendix A): there is no API that
//! ranks a page's outgoing links, so wikitui joins `links × pageviews` itself
//! in **one** batched call (`generator=links` + `prop=pageviews`) — never a
//! per-article fanout. This module does the join and scoring; `api` does the
//! single request; `main`'s executor wires them onto the queue.

use serde::Deserialize;
use std::collections::HashMap;

use crate::netqueue::{LinkCandidate, RankWeights};

/// A link a page could be prefetched from, once ranked (FR-PF-1). `reason` is
/// the FR-PF-4 transparency string stored with the eventual cache entry.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedLink {
    pub title: String,
    pub score: f64,
    pub reason: String,
}

/// A link at or before this document position is treated as a "lead" link for
/// the reason string (it appears in or near the lead section). A positional
/// heuristic, not a section parse — documented so the reason text ("lead") is
/// honest about what it means.
const LEAD_CUTOFF: usize = 5;

/// FR-PF-1 ranking: `w1·lead_position + w2·log(pageviews) + w3·interest`.
/// The `affinity` map supplies the FR-PF-3 w3 term per candidate title — the
/// summed topic affinity of a link *target* the reader already read this
/// session (0, i.e. absent from the map, for an unseen target: the interest
/// model never fabricates affinity for a target it has no evidence about, see
/// [`crate::interest`]'s w3 resolution). So this term only ever *raises* a
/// link to a demonstrably-preferred topic; it never reshuffles ranking for a
/// reader with no interest data (the map is empty), nor when interest learning
/// is off/incognito (the caller passes an empty map).
///
/// Returns up to `top_n` links, highest score first, **plus** the cursor link
/// if the reader has one focused and it didn't already make the cut — "the
/// link under the cursor is always a candidate" (FR-PF-1).
pub fn rank_links(
    article_title: &str,
    candidates: &[LinkCandidate],
    pageviews: &HashMap<String, u64>,
    affinity: &HashMap<String, f64>,
    weights: RankWeights,
    top_n: usize,
) -> Vec<RankedLink> {
    let mut scored: Vec<(f64, &LinkCandidate, u64)> = candidates
        .iter()
        .map(|c| {
            let views = pageviews.get(&c.title).copied().unwrap_or(0);
            let lead_term = weights.lead * lead_score(c.lead_position);
            let views_term = weights.pageviews * ((views as f64) + 1.0).ln();
            // FR-PF-3 w3 term: the target's topic affinity (0 if unseen).
            let affinity_term = weights.affinity * affinity.get(&c.title).copied().unwrap_or(0.0);
            (lead_term + views_term + affinity_term, c, views)
        })
        .collect();

    // Highest score first; ties broken by earlier document position so the
    // ordering is deterministic (tested).
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.lead_position.cmp(&b.1.lead_position))
    });

    let mut out: Vec<RankedLink> = Vec::new();
    let mut included: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (score, c, views) in scored.iter().take(top_n) {
        included.insert(c.title.as_str());
        out.push(RankedLink {
            title: c.title.clone(),
            score: *score,
            reason: link_reason(article_title, c, *views),
        });
    }
    // Guarantee the cursor link is present even if it ranked below the cut.
    if let Some((score, c, views)) = scored
        .iter()
        .find(|(_, c, _)| c.is_cursor && !included.contains(c.title.as_str()))
    {
        out.push(RankedLink {
            title: c.title.clone(),
            score: *score,
            reason: link_reason(article_title, c, *views),
        });
    }
    out
}

fn lead_score(position: usize) -> f64 {
    1.0 / (1.0 + position as f64)
}

/// FR-PF-4 reason string for a ranked link, e.g.
/// `linked from Fourier transform (lead, 12k views/day)`.
fn link_reason(article_title: &str, c: &LinkCandidate, views: u64) -> String {
    let mut quals: Vec<String> = Vec::new();
    if c.is_cursor {
        quals.push("under cursor".to_string());
    }
    if c.lead_position < LEAD_CUTOFF {
        quals.push("lead".to_string());
    }
    if views > 0 {
        quals.push(format!("{} views/day", format_views(views)));
    }
    if quals.is_empty() {
        format!("linked from {article_title}")
    } else {
        format!("linked from {article_title} ({})", quals.join(", "))
    }
}

/// FR-PF-4 reason for a trending body: TFA or the most-read rank.
pub fn trending_reason(rank: Option<usize>, views: u64) -> String {
    match rank {
        None => "today's featured article".to_string(),
        Some(r) if views > 0 => format!("trending #{r} ({} views/day)", format_views(views)),
        Some(r) => format!("trending #{r}"),
    }
}

/// FR-PF-4 reason for an interest-driven `morelike:` candidate (FR-PF-3), e.g.
/// `morelike your Cryptography reading, affinity 0.82` — the exact phrasing the
/// PRD gives. `affinity` is the seed article's summed topic affinity; `category`
/// is the dominant topic that made it a seed.
pub fn morelike_reason(category: &str, affinity: f64) -> String {
    format!("morelike your {category} reading, affinity {affinity:.2}")
}

/// Compact view counts for reason strings: `12345 -> "12k"`, `1234567 -> "1.2M"`.
pub fn format_views(v: u64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1_000_000.0)
    } else if v >= 1_000 {
        format!("{}k", v / 1_000)
    } else {
        v.to_string()
    }
}

// -- Wikifeeds featured-content parsing (FR-PF-2 / FR-DL-1/2/7 seam) -------

/// The parsed slice of the daily featured feed prefetch needs, plus the
/// fields the start page (FR-DL-1) and TIL widget (FR-DL-7) consume —
/// exposed so `startpage` can call `FeedCache::get` instead of making a
/// second feed request (that is the "one daily Wikifeeds call" the PRD
/// insists on).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeaturedFeed {
    /// Today's featured article title (fetchable form).
    pub tfa: Option<String>,
    /// The TFA's lead extract (FR-DL-1's "badge + extract"), sanitized and
    /// length-capped display text — see [`FEED_EXTRACT_CAP`].
    pub extract: Option<String>,
    /// Most-read articles with their view counts, in rank order.
    pub mostread: Vec<MostRead>,
    /// Picture-of-the-day title (caption/alt text).
    pub potd: Option<String>,
    /// Picture-of-the-day thumbnail URL, fed into the same half-block image
    /// pipeline (`image::decode_image`, `App::image_store`) inline article
    /// images already use (PRD FR-RD-8) — `None` when the feed carried no
    /// thumbnail, in which case the start page shows caption-only.
    pub potd_thumb_url: Option<String>,
    /// "In the news" headlines (FR-DL-1), each optionally linking an article.
    pub news: Vec<NewsItem>,
    /// A handful of "on this day" entries bundled into the daily feed —
    /// Wikifeeds' own mixed/selected set. Good enough for the start page's
    /// condensed strip (FR-DL-1) and the TIL widget's rotation (FR-DL-7's
    /// documented "random Good Article" seam: that needs `list=random` +
    /// `prop=pageassessments`, heavier than this one-call budget allows, so
    /// v1 rotates through these instead — see `startpage::pick_til`). The
    /// full per-type breakdown `:today` shows (events/births/deaths/
    /// holidays/selected, FR-DL-2) is a *separate*, on-demand fetch — see
    /// [`crate::api::WikiClient::fetch_onthisday_feed`]'s doc comment for why.
    pub onthisday: Vec<OtdEntry>,
    /// `onthisday.len()`, kept as its own field for callers that only ever
    /// wanted the count (prefetch's own log line, and this field's original
    /// tests).
    pub onthisday_events: usize,
}

/// One on-this-day entry — events/births/deaths/holidays/selected all share
/// this shape in the Wikifeeds response: the blurb, its year if the API gave
/// one, and the first linked article's title (if any) as the Enter target.
/// Shared between the bundled `feed/featured` entries and the dedicated
/// `feed/onthisday/{type}` panel (FR-DL-2) so both render identically.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OtdEntry {
    pub year: Option<i32>,
    pub text: String,
    pub page_title: Option<String>,
}

/// One "in the news" headline (FR-DL-1), with its first linked article (if
/// any) as the Enter target.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewsItem {
    pub headline: String,
    pub page_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MostRead {
    pub title: String,
    pub views: u64,
}

/// Display-text caps for feed content (SEC-1 spirit: a hostile/garbled feed
/// can't paint an unbounded status line or start-page row) — generous enough
/// for a real headline/blurb/extract, small enough to stay one screen's
/// worth of text.
const FEED_TEXT_CAP: usize = 220;
const FEED_EXTRACT_CAP: usize = 600;

impl FeaturedFeed {
    /// Parse a Wikifeeds `feed/featured/{y}/{m}/{d}` response body.
    pub fn parse(body: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: RawFeed = serde_json::from_slice(body)?;
        let extract = raw
            .tfa
            .as_ref()
            .and_then(|p| p.extract.as_deref())
            .map(|s| crate::sanitize::sanitize_and_cap_multiline(s, FEED_EXTRACT_CAP));
        let tfa = raw.tfa.and_then(RawPage::best_title);
        let mostread = raw
            .mostread
            .map(|m| {
                m.articles
                    .into_iter()
                    .filter_map(|a| {
                        let views = a.views;
                        a.best_title().map(|title| MostRead { title, views })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let potd_thumb_url = raw
            .image
            .as_ref()
            .and_then(|p| p.thumbnail.as_ref())
            .and_then(|t| t.source.clone());
        let potd = raw.image.and_then(RawPage::best_title);
        let news = raw
            .news
            .into_iter()
            .filter_map(RawNewsItem::into_item)
            .collect();
        let onthisday: Vec<OtdEntry> = raw
            .onthisday
            .into_iter()
            .map(RawOnThisDayItem::into_entry)
            .collect();
        Ok(Self {
            tfa,
            extract,
            mostread,
            potd,
            potd_thumb_url,
            news,
            onthisday_events: onthisday.len(),
            onthisday,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawFeed {
    #[serde(default)]
    tfa: Option<RawPage>,
    #[serde(default)]
    mostread: Option<RawMostRead>,
    #[serde(default)]
    image: Option<RawPage>,
    #[serde(default)]
    news: Vec<RawNewsItem>,
    #[serde(default)]
    onthisday: Vec<RawOnThisDayItem>,
}

#[derive(Debug, Deserialize)]
struct RawMostRead {
    #[serde(default)]
    articles: Vec<RawArticle>,
}

#[derive(Debug, Deserialize)]
struct RawThumbnail {
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPage {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    normalizedtitle: Option<String>,
    #[serde(default)]
    extract: Option<String>,
    #[serde(default)]
    thumbnail: Option<RawThumbnail>,
}

impl RawPage {
    /// Prefer the display `normalizedtitle` (spaces) over the DB-key `title`
    /// (underscores): article opens cache under the space form, so using it
    /// here lets a trending-prefetched body actually be a cache *hit* when the
    /// reader later opens it, and dedups against link prefetch of the same
    /// article. `fetch_article_html` re-normalizes either to the URL form.
    fn best_title(self) -> Option<String> {
        self.normalizedtitle
            .filter(|t| !t.is_empty())
            .or(self.title)
            .filter(|t| !t.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct RawArticle {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    normalizedtitle: Option<String>,
    #[serde(default)]
    views: u64,
}

impl RawArticle {
    fn best_title(self) -> Option<String> {
        self.normalizedtitle
            .filter(|t| !t.is_empty())
            .or(self.title)
            .filter(|t| !t.is_empty())
    }
}

/// One on-this-day/news linked-pages entry. Wikifeeds represents both news
/// items and onthisday entries as `{text/story, pages/links: [...]}`; this
/// shape covers the onthisday side (`RawNewsItem` covers news' own field
/// names below).
#[derive(Debug, Deserialize)]
struct RawOnThisDayItem {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    year: Option<i64>,
    #[serde(default)]
    pages: Vec<RawPage>,
}

impl RawOnThisDayItem {
    fn into_entry(self) -> OtdEntry {
        let text = crate::sanitize::sanitize_and_cap_single_line(
            self.text.as_deref().unwrap_or(""),
            FEED_TEXT_CAP,
        );
        let page_title = self.pages.into_iter().find_map(RawPage::best_title);
        OtdEntry {
            year: self.year.map(|y| y as i32),
            text,
            page_title,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawNewsItem {
    #[serde(default)]
    story: Option<String>,
    #[serde(default)]
    links: Vec<RawPage>,
}

impl RawNewsItem {
    /// `None` for a story with no usable text at all (a blank entry is
    /// dropped rather than shown as an empty row).
    fn into_item(self) -> Option<NewsItem> {
        let story = self.story?;
        let headline =
            crate::sanitize::sanitize_and_cap_single_line(&strip_tags(&story), FEED_TEXT_CAP);
        if headline.is_empty() {
            return None;
        }
        let page_title = self.links.into_iter().find_map(RawPage::best_title);
        Some(NewsItem {
            headline,
            page_title,
        })
    }
}

/// Wikifeeds' "in the news" `story` field carries simple inline HTML (mostly
/// `<a>` links to the mentioned articles); the start page shows plain text,
/// so this drops tags outright rather than pulling in a full HTML parser for
/// what is, at most, a short news blurb. Never panics on unclosed/malformed
/// tags (mirrors `ui::parse_searchmatch`'s tolerance): an unterminated `<`
/// just swallows the remainder as "in a tag", which degrades to a shorter
/// headline rather than corrupting the rest of the line.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Parses one Wikifeeds `feed/onthisday/{type}/{m}/{d}` response (FR-DL-2):
/// the body is `{"<type>": [ {text, year, pages} … ]}`, a single top-level
/// key matching the requested type. Unlike `feed/featured` (one call bundles
/// everything into the daily prefetch), each on-this-day type is its own
/// call, made on demand when `:today` opens — see
/// [`crate::api::WikiClient::fetch_onthisday_feed`]'s doc comment for why
/// that is a foreground fetch rather than a `netqueue` job.
pub fn parse_onthisday(body: &[u8], event_type: &str) -> Result<Vec<OtdEntry>, serde_json::Error> {
    let raw: serde_json::Value = serde_json::from_slice(body)?;
    let items: Vec<RawOnThisDayItem> = match raw.get(event_type) {
        Some(v) => serde_json::from_value(v.clone())?,
        None => Vec::new(),
    };
    Ok(items
        .into_iter()
        .map(RawOnThisDayItem::into_entry)
        .collect())
}

/// FR-PF-2 "once per day": the parsed feed cached under its `yyyy-mm-dd`
/// bucket. A second `should_fetch` for the same day is `false`, so the
/// executor skips the network — the once-per-day guarantee, testable with an
/// injected date string (no wall clock in the logic).
#[derive(Debug, Default)]
pub struct FeedCache {
    date: Option<String>,
    feed: Option<FeaturedFeed>,
}

impl FeedCache {
    pub fn should_fetch(&self, date: &str) -> bool {
        self.date.as_deref() != Some(date)
    }

    /// The `yyyy-mm-dd` bucket the cached feed (if any) was fetched for —
    /// the single source of truth `startpage::pick_til` keys its per-day
    /// rotation on, so the TIL widget never needs its own, separately-drifting
    /// wall-clock read (PRD's "thread the date, don't read the clock in
    /// logic" convention).
    pub fn date(&self) -> Option<&str> {
        self.date.as_deref()
    }

    pub fn store(&mut self, date: String, feed: FeaturedFeed) {
        self.date = Some(date);
        self.feed = Some(feed);
    }

    /// The parsed feed for the cached day, for the FR-DL-1 start page and
    /// FR-DL-7 TIL widget to consume without a second network call —
    /// `App::start_page_model` locks the shared `Arc<Mutex<FeedCache>>` and
    /// clones out of this on every draw while the start page is showing.
    pub fn get(&self) -> Option<&FeaturedFeed> {
        self.feed.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(title: &str, pos: usize) -> LinkCandidate {
        LinkCandidate {
            title: title.to_string(),
            lead_position: pos,
            is_cursor: false,
        }
    }

    /// The FR-PF-3 w3 affinity map, empty — the common case (no interest data,
    /// or interest off/incognito), where ranking is lead+pageviews only.
    fn no_affinity() -> HashMap<String, f64> {
        HashMap::new()
    }

    #[test]
    fn lead_position_breaks_ties_when_views_are_equal() {
        let cands = vec![cand("Late", 10), cand("Early", 0), cand("Mid", 3)];
        let views: HashMap<String, u64> = ["Late", "Early", "Mid"]
            .iter()
            .map(|t| (t.to_string(), 100))
            .collect();
        let ranked = rank_links(
            "Src",
            &cands,
            &views,
            &no_affinity(),
            RankWeights::default(),
            5,
        );
        let order: Vec<&str> = ranked.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(
            order,
            vec!["Early", "Mid", "Late"],
            "earlier position ranks higher"
        );
    }

    #[test]
    fn pageviews_dominate_when_position_is_equal() {
        let cands = vec![cand("Obscure", 0), cand("Popular", 0), cand("Medium", 0)];
        let mut views = HashMap::new();
        views.insert("Obscure".to_string(), 5);
        views.insert("Popular".to_string(), 50_000);
        views.insert("Medium".to_string(), 500);
        let ranked = rank_links(
            "Src",
            &cands,
            &views,
            &no_affinity(),
            RankWeights::default(),
            5,
        );
        let order: Vec<&str> = ranked.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(order, vec!["Popular", "Medium", "Obscure"]);
    }

    #[test]
    fn top_n_cuts_but_cursor_link_is_always_included() {
        let mut cands = vec![
            cand("A", 0),
            cand("B", 1),
            cand("C", 2),
            cand("D", 3),
            cand("Cursored", 20),
        ];
        cands[4].is_cursor = true;
        let views: HashMap<String, u64> = cands.iter().map(|c| (c.title.clone(), 100)).collect();
        let ranked = rank_links(
            "Src",
            &cands,
            &views,
            &no_affinity(),
            RankWeights::default(),
            3,
        );
        let titles: Vec<&str> = ranked.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles.len(), 4, "top-3 plus the cursor link");
        assert!(
            titles.contains(&"Cursored"),
            "cursor link forced in despite low rank"
        );
        assert!(titles.contains(&"A") && titles.contains(&"B") && titles.contains(&"C"));
    }

    #[test]
    fn cursor_link_not_duplicated_when_it_ranks_in_top_n() {
        let mut cands = vec![cand("A", 0), cand("B", 1)];
        cands[0].is_cursor = true;
        let views: HashMap<String, u64> = cands.iter().map(|c| (c.title.clone(), 100)).collect();
        let ranked = rank_links(
            "Src",
            &cands,
            &views,
            &no_affinity(),
            RankWeights::default(),
            5,
        );
        let count = ranked.iter().filter(|r| r.title == "A").count();
        assert_eq!(count, 1, "cursor link that made the cut is not added twice");
    }

    #[test]
    fn reason_strings_describe_lead_cursor_and_views() {
        let mut c = cand("Enigma machine", 0);
        assert_eq!(
            link_reason("Alan Turing", &c, 12_345),
            "linked from Alan Turing (lead, 12k views/day)"
        );
        c.is_cursor = true;
        assert_eq!(
            link_reason("Alan Turing", &c, 0),
            "linked from Alan Turing (under cursor, lead)"
        );
        let deep = cand("Footnote", 42);
        assert_eq!(
            link_reason("Alan Turing", &deep, 800),
            "linked from Alan Turing (800 views/day)"
        );
    }

    #[test]
    fn trending_reason_covers_tfa_and_ranked_mostread() {
        assert_eq!(trending_reason(None, 0), "today's featured article");
        assert_eq!(
            trending_reason(Some(3), 98_000),
            "trending #3 (98k views/day)"
        );
        assert_eq!(trending_reason(Some(7), 0), "trending #7");
    }

    #[test]
    fn morelike_reason_matches_the_prd_phrasing() {
        assert_eq!(
            morelike_reason("Cryptography", 0.82),
            "morelike your Cryptography reading, affinity 0.82"
        );
        assert_eq!(
            morelike_reason("Computer science", 4.0),
            "morelike your Computer science reading, affinity 4.00"
        );
    }

    /// FR-PF-3 w3 wiring: with equal lead position and equal pageviews, the
    /// link whose target carries higher topic affinity ranks above the one
    /// that carries none — the documented "affinity of a previously-read
    /// target" resolution (see `interest`).
    #[test]
    fn a_high_affinity_target_outranks_a_low_one_at_equal_lead_and_views() {
        let cands = vec![cand("LowAffinity", 0), cand("HighAffinity", 0)];
        let views: HashMap<String, u64> = cands.iter().map(|c| (c.title.clone(), 100)).collect();
        let mut affinity = HashMap::new();
        affinity.insert("HighAffinity".to_string(), 5.0);
        // LowAffinity intentionally absent from the map → 0.
        let weights = RankWeights {
            lead: 1.0,
            pageviews: 1.0,
            affinity: 1.0,
        };
        let ranked = rank_links("Src", &cands, &views, &affinity, weights, 5);
        assert_eq!(
            ranked[0].title, "HighAffinity",
            "the demonstrably-preferred topic's link is prefetched first"
        );
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn a_zero_affinity_weight_ignores_the_map_entirely() {
        // Even with affinity data present, weight 0 (interest off) means the
        // w3 term contributes nothing — ranking is lead+pageviews only.
        let cands = vec![cand("A", 0), cand("B", 0)];
        let views: HashMap<String, u64> = [("A", 10u64), ("B", 5000)]
            .iter()
            .map(|(t, v)| (t.to_string(), *v))
            .collect();
        let mut affinity = HashMap::new();
        affinity.insert("A".to_string(), 1000.0); // would dominate if weighted
        let weights = RankWeights {
            lead: 1.0,
            pageviews: 1.0,
            affinity: 0.0,
        };
        let ranked = rank_links("Src", &cands, &views, &affinity, weights, 5);
        assert_eq!(
            ranked[0].title, "B",
            "pageviews win; affinity is unweighted"
        );
    }

    #[test]
    fn format_views_is_compact() {
        assert_eq!(format_views(42), "42");
        assert_eq!(format_views(12_345), "12k");
        assert_eq!(format_views(1_234_567), "1.2M");
    }

    #[test]
    fn featured_feed_parses_tfa_mostread_potd_and_onthisday() {
        let body = br#"{
            "tfa": {"title": "Alan_Turing", "normalizedtitle": "Alan Turing"},
            "mostread": {"articles": [
                {"title": "Enigma_machine", "normalizedtitle": "Enigma machine", "views": 50000, "rank": 1},
                {"title": "Computer_science", "normalizedtitle": "Computer science", "views": 20000, "rank": 2}
            ]},
            "image": {"title": "File:Example.jpg"},
            "onthisday": [{"text": "a"}, {"text": "b"}]
        }"#;
        let feed = FeaturedFeed::parse(body).unwrap();
        assert_eq!(
            feed.tfa.as_deref(),
            Some("Alan Turing"),
            "prefers the space-form title so it dedups with article opens"
        );
        assert_eq!(feed.mostread.len(), 2);
        assert_eq!(feed.mostread[0].title, "Enigma machine");
        assert_eq!(feed.mostread[0].views, 50_000);
        assert_eq!(feed.potd.as_deref(), Some("File:Example.jpg"));
        assert_eq!(feed.onthisday_events, 2);
    }

    #[test]
    fn featured_feed_tolerates_missing_sections() {
        let feed = FeaturedFeed::parse(br#"{}"#).unwrap();
        assert_eq!(feed.tfa, None);
        assert!(feed.mostread.is_empty());
        assert_eq!(feed.onthisday_events, 0);
        assert_eq!(feed.extract, None);
        assert!(feed.news.is_empty());
        assert!(feed.onthisday.is_empty());
        assert_eq!(feed.potd_thumb_url, None);
    }

    /// FR-DL-1: the start page needs the TFA extract, "in the news"
    /// headlines (each with an Enter target), the onthisday entries with
    /// their linked pages, and the potd thumbnail URL for the image
    /// pipeline — all bundled into the one daily `feed/featured` call.
    #[test]
    fn featured_feed_parses_extract_news_and_potd_thumbnail() {
        let body = br#"{
            "tfa": {"title": "Alan_Turing", "normalizedtitle": "Alan Turing",
                    "extract": "Alan Turing was a mathematician.\nHe broke Enigma."},
            "image": {"title": "File:Example.jpg",
                      "thumbnail": {"source": "https://example.com/thumb.jpg"}},
            "news": [
                {"story": "<a href=\"./Enigma_machine\">Enigma</a> anniversary marked.",
                 "links": [{"title": "Enigma_machine", "normalizedtitle": "Enigma machine"}]},
                {"story": "A story with no links."}
            ],
            "onthisday": [
                {"text": "Turing born.", "year": 1912,
                 "pages": [{"title": "Alan_Turing", "normalizedtitle": "Alan Turing"}]}
            ]
        }"#;
        let feed = FeaturedFeed::parse(body).unwrap();
        assert_eq!(
            feed.extract.as_deref(),
            Some("Alan Turing was a mathematician.\nHe broke Enigma.")
        );
        assert_eq!(
            feed.potd_thumb_url.as_deref(),
            Some("https://example.com/thumb.jpg")
        );
        assert_eq!(feed.news.len(), 2);
        assert_eq!(feed.news[0].headline, "Enigma anniversary marked.");
        assert_eq!(feed.news[0].page_title.as_deref(), Some("Enigma machine"));
        assert_eq!(feed.news[1].headline, "A story with no links.");
        assert_eq!(feed.news[1].page_title, None);
        assert_eq!(feed.onthisday.len(), 1);
        assert_eq!(feed.onthisday[0].year, Some(1912));
        assert_eq!(feed.onthisday[0].text, "Turing born.");
        assert_eq!(feed.onthisday[0].page_title.as_deref(), Some("Alan Turing"));
        assert_eq!(feed.onthisday_events, 1);
    }

    #[test]
    fn news_item_with_blank_story_is_dropped_not_shown_empty() {
        let body = br#"{"news": [{"story": "<a href=\"x\"></a>"}]}"#;
        let feed = FeaturedFeed::parse(body).unwrap();
        assert!(
            feed.news.is_empty(),
            "an empty-after-stripping headline is dropped, not shown blank"
        );
    }

    /// FR-DL-2: `:today`'s per-type panel parses the dedicated
    /// `feed/onthisday/{type}` shape — a single top-level key named after
    /// the requested type — distinct from the bundled `feed/featured` call.
    #[test]
    fn parse_onthisday_extracts_the_requested_type_key() {
        let body = br#"{"births": [
            {"text": "A scientist was born.", "year": 1901,
             "pages": [{"title": "Computer_science", "normalizedtitle": "Computer science"}]}
        ]}"#;
        let entries = parse_onthisday(body, "births").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].year, Some(1901));
        assert_eq!(entries[0].text, "A scientist was born.");
        assert_eq!(entries[0].page_title.as_deref(), Some("Computer science"));
    }

    #[test]
    fn parse_onthisday_tolerates_a_missing_or_mismatched_type_key() {
        let body = br#"{"deaths": [{"text": "irrelevant"}]}"#;
        // Asked for "births" but the body only has "deaths" — a shape
        // mismatch (wrong type requested, or a wiki without this type)
        // degrades to an empty list, never an error.
        let entries = parse_onthisday(body, "births").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn feed_cache_fetches_once_per_day() {
        let mut cache = FeedCache::default();
        assert!(cache.should_fetch("2026-07-14"));
        cache.store("2026-07-14".to_string(), FeaturedFeed::default());
        assert!(!cache.should_fetch("2026-07-14"), "same day is a cache hit");
        assert!(cache.should_fetch("2026-07-15"), "a new day refetches");
    }
}
