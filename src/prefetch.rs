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
/// `interest` (affinity) is 0 until the FR-PF-3 interest model lands (v1.x);
/// the term and its weight are kept so turning it on is a one-line change.
///
/// Returns up to `top_n` links, highest score first, **plus** the cursor link
/// if the reader has one focused and it didn't already make the cut — "the
/// link under the cursor is always a candidate" (FR-PF-1).
pub fn rank_links(
    article_title: &str,
    candidates: &[LinkCandidate],
    pageviews: &HashMap<String, u64>,
    weights: RankWeights,
    top_n: usize,
) -> Vec<RankedLink> {
    let mut scored: Vec<(f64, &LinkCandidate, u64)> = candidates
        .iter()
        .map(|c| {
            let views = pageviews.get(&c.title).copied().unwrap_or(0);
            let lead_term = weights.lead * lead_score(c.lead_position);
            let views_term = weights.pageviews * ((views as f64) + 1.0).ln();
            // Affinity term is the FR-PF-3 seam (0 for now).
            let affinity_term = weights.affinity * 0.0;
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

// -- Wikifeeds featured-content parsing (FR-PF-2 / FR-DL-1 seam) -----------

/// The parsed slice of the daily featured feed prefetch needs, plus the fields
/// the start page (FR-DL-1 / B9) will consume — exposed so B9 can call
/// `FeedCache::get` instead of making a second feed request (that is the "one
/// daily Wikifeeds call" the PRD insists on).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeaturedFeed {
    /// Today's featured article title (fetchable form).
    pub tfa: Option<String>,
    /// Most-read articles with their view counts, in rank order.
    pub mostread: Vec<MostRead>,
    /// Picture-of-the-day title, for the B9 start page (not prefetched here —
    /// prefetch fills L2 article bodies only, no images).
    pub potd: Option<String>,
    /// Count of on-this-day events, for the B9 "on this day" strip.
    pub onthisday_events: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MostRead {
    pub title: String,
    pub views: u64,
}

impl FeaturedFeed {
    /// Parse a Wikifeeds `feed/featured/{y}/{m}/{d}` response body.
    pub fn parse(body: &[u8]) -> Result<Self, serde_json::Error> {
        let raw: RawFeed = serde_json::from_slice(body)?;
        let tfa = raw.tfa.and_then(|p| p.best_title());
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
        let potd = raw.image.and_then(|p| p.best_title());
        Ok(Self {
            tfa,
            mostread,
            potd,
            onthisday_events: raw.onthisday.len(),
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
    onthisday: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawMostRead {
    #[serde(default)]
    articles: Vec<RawArticle>,
}

#[derive(Debug, Deserialize)]
struct RawPage {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    normalizedtitle: Option<String>,
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

    pub fn store(&mut self, date: String, feed: FeaturedFeed) {
        self.date = Some(date);
        self.feed = Some(feed);
    }

    /// The parsed feed for the cached day, for the FR-DL-1 start page (B9) to
    /// consume without a second network call. B9 is a later chunk — this is its
    /// documented seam, wired in tests but not yet called from the bin.
    #[allow(dead_code)]
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

    #[test]
    fn lead_position_breaks_ties_when_views_are_equal() {
        let cands = vec![cand("Late", 10), cand("Early", 0), cand("Mid", 3)];
        let views: HashMap<String, u64> = ["Late", "Early", "Mid"]
            .iter()
            .map(|t| (t.to_string(), 100))
            .collect();
        let ranked = rank_links("Src", &cands, &views, RankWeights::default(), 5);
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
        let ranked = rank_links("Src", &cands, &views, RankWeights::default(), 5);
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
        let ranked = rank_links("Src", &cands, &views, RankWeights::default(), 3);
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
        let ranked = rank_links("Src", &cands, &views, RankWeights::default(), 5);
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
