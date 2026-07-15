//! PRD FR-PF-3's interest-learning prefetch model — "local, private,
//! inspectable ... No neural nets; the whole model is KBs of human-readable
//! state." This module is the whole of that promise: a topic-affinity vector
//! over article categories, exponentially time-decayed, driven by reading
//! signals, persisted as pretty-printed JSON in `$XDG_STATE`, and turned into
//! `morelike:` prefetch candidates. It is pure data + arithmetic — no network,
//! no wall clock read in the logic (every mutation takes an injected `now`,
//! the codebase's "thread a timestamp, never read the clock ad hoc"
//! convention, cf. `netqueue::Clock` / `cache`'s age math) — so all of it is
//! unit-testable without a running app.
//!
//! ## What the model *is*
//!
//! One number per Wikipedia category: an **affinity score**. Reading signals
//! add to the affinity of the categories the read article belongs to; time
//! decays every score toward zero. That is the entire model — a
//! `category -> f64` map plus the timestamp the scores were last decayed to.
//! `interest.json` is exactly that map, human-readable (`:interests` shows it
//! live; `cat`ting the file shows the same thing).
//!
//! ## Signals (FR-PF-3 exact values)
//!
//! | signal | amount | when |
//! |---|---|---|
//! | open | +1.0 | the article was opened ([`SIGNAL_OPEN`]) |
//! | normalized dwell | ≤ +2.0 | time spent, normalized ([`normalized_dwell`]) |
//! | scroll ≥ 70% | +1.0 | scrolled past 70% ([`SIGNAL_SCROLL`]) |
//! | bookmark | +3.0 | `m` ([`SIGNAL_BOOKMARK`]) |
//! | save | +5.0 | `S` ([`SIGNAL_SAVE`]) |
//! | "not interested" | -10.0 | `:not-interested` ([`SIGNAL_NOT_INTERESTED`]) |
//!
//! Each signal is added to *every* (non-maintenance) category the article
//! carries. Dwell is normalized so a fully-read article contributes the whole
//! +2.0 and a glance contributes proportionally less — see [`normalized_dwell`].
//!
//! ## Time decay (half-life 30 d, config `interest_half_life_days`)
//!
//! Before any new signal is added, every existing score is multiplied by
//! `0.5 ^ (elapsed_days / half_life_days)` — an exponential decay whose
//! half-life is the config value (default 30 days). So a score of 4.0
//! untouched for exactly one half-life is 2.0 by the time the next signal
//! lands; **decay happens first, then the new signal is added** (the ordering
//! [`decay`]/[`apply_signal_for_title`] lock in tests). Decay is applied lazily
//! on mutation rather than on a timer, keyed off [`InterestModel::last_update`]:
//! the model is identical either way and lazy decay needs no background task.
//!
//! ## Maintenance-category filtering
//!
//! Wikipedia pages carry two kinds of category: real topics ("Cryptography")
//! and maintenance/tracking bookkeeping ("Articles with dead external links",
//! "CS1 maint: multiple names", "Webarchive template wayback links",
//! "Use dmy dates from ...", hidden categories). Only topics are signal about
//! what the reader *likes*; the bookkeeping ones are noise present on nearly
//! every article. We drop them two ways: the batched fetch asks the API for
//! `clshow=!hidden` (dropping API-flagged hidden categories server-side), and
//! [`is_maintenance_category`] drops the large class of maintenance categories
//! that are *not* flagged hidden by name pattern. The name filter is a
//! documented heuristic (see its own comment) — deliberately conservative so a
//! real topic is never mistaken for maintenance.
//!
//! ## The w3 affinity term and the "target categories are unknown" problem
//!
//! FR-PF-1 ranks a page's outgoing links by
//! `w1·lead + w2·log(views) + w3·affinity`, where `affinity` is "the link
//! *target's* topic affinity". But we don't know a target's categories until
//! we fetch it — and the whole point of ranking is to decide *whether* to
//! fetch it. This module resolves that chicken-and-egg honestly: affinity is
//! known only for a target we have **already read this session** (its
//! categories are in [`InterestModel::article_categories`], populated as you
//! read). [`InterestModel::affinity_of_title`] returns that summed affinity
//! for a seen target and `0.0` for an unseen one. So w3 only ever *raises* a
//! link to a topic the reader has demonstrably engaged with (a revisit, or a
//! link shared between two articles in the same session) — it never fabricates
//! affinity for a target it has no evidence about. `article_categories` is a
//! session-only cache (`#[serde(skip)]`) so `interest.json` stays the tiny,
//! pure category vector the PRD promises.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// FR-PF-3's default half-life for the exponential decay (`interest_half_life_
/// days` config default). Thirty days: a topic you stop reading fades to half
/// its weight in a month, to a quarter in two, so the model tracks a moving
/// window of interest rather than an all-time tally.
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 30.0;

/// FR-PF-3 signal weights, the exact values the PRD lists.
pub const SIGNAL_OPEN: f64 = 1.0;
pub const SIGNAL_SCROLL: f64 = 1.0;
pub const SIGNAL_BOOKMARK: f64 = 3.0;
pub const SIGNAL_SAVE: f64 = 5.0;
/// "explicit 'not interested' strongly negative" — a single press outweighs
/// several positive signals so the reader can decisively veto a topic.
pub const SIGNAL_NOT_INTERESTED: f64 = -10.0;
/// The ceiling for the normalized dwell signal (FR-PF-3's "normalized dwell
/// ≤ +2.0").
pub const DWELL_SIGNAL_MAX: f64 = 2.0;

/// Below this magnitude a decayed score is dropped from the map entirely — it
/// keeps `interest.json` from accumulating a long tail of effectively-zero
/// categories the reader glanced at once a year ago, so the file stays the
/// "KBs of human-readable state" the PRD promises.
const PRUNE_EPSILON: f64 = 0.01;

/// FR-PF-3's dwell normalization (documented in the module comment's signal
/// table): `min(dwell / expected_read_time, 1.0) * 2.0`. A reader who spent at
/// least the estimated reading time contributes the full +2.0; a shorter dwell
/// contributes proportionally less; `expected_read_secs <= 0` (an empty or
/// unmeasured article) contributes nothing rather than dividing by zero.
pub fn normalized_dwell(dwell_secs: i64, expected_read_secs: f64) -> f64 {
    if dwell_secs <= 0 || expected_read_secs <= 0.0 {
        return 0.0;
    }
    ((dwell_secs as f64 / expected_read_secs).min(1.0)) * DWELL_SIGNAL_MAX
}

/// The multiplicative decay applied to every score for `elapsed_secs` of
/// elapsed time at the given half-life. `0.5 ^ (elapsed_days / half_life_days)`
/// — exactly `0.5` at one half-life, so a score halves per `half_life_days`.
/// Non-positive elapsed time or a non-positive half-life is a no-op (factor
/// `1.0`), never a panic or a divide-by-zero.
pub fn decay_factor(elapsed_secs: i64, half_life_days: f64) -> f64 {
    if elapsed_secs <= 0 || half_life_days <= 0.0 {
        return 1.0;
    }
    let elapsed_days = elapsed_secs as f64 / 86_400.0;
    0.5_f64.powf(elapsed_days / half_life_days)
}

/// One seed for interest-driven `morelike:` candidate generation (FR-PF-3's
/// "candidates via `morelike:` on top-affinity reads"): a recently-read
/// article, the dominant topic that made it high-affinity, and that affinity
/// value — the three pieces the FR-PF-4 reason string
/// ("morelike your Cryptography reading, affinity 0.82") needs.
#[derive(Debug, Clone, PartialEq)]
pub struct MorelikeSeed {
    pub title: String,
    pub category: String,
    pub affinity: f64,
}

/// The interest-affinity model (FR-PF-3). Serialized to `interest.json` as
/// exactly its category vector, half-life, and last-decay timestamp — the
/// `article_categories` session cache is `#[serde(skip)]` (see the module doc
/// comment's w3 section) so the on-disk file stays small and readable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterestModel {
    /// The model proper: category (display form, no `Category:` prefix) ->
    /// affinity score. A `BTreeMap` so the JSON is deterministically ordered
    /// and diff-friendly (a reader `cat`ting the file, or syncing it via git,
    /// sees stable output), not hash-random.
    #[serde(default)]
    categories: BTreeMap<String, f64>,
    /// The decay half-life in days (`interest_half_life_days`). Persisted for
    /// inspection; `load` overwrites it from config so config stays
    /// authoritative.
    #[serde(default = "default_half_life")]
    half_life_days: f64,
    /// Unix seconds the scores were last decayed to — the single reference
    /// point every decay computes elapsed time from, so decay is never applied
    /// twice or against a second, independently-read "now".
    #[serde(default)]
    last_update: i64,
    /// Session-only `(wiki, title) -> cleaned categories` cache (see the
    /// module doc comment): powers `affinity_of_title` (the w3 term) and
    /// `morelike_seeds`. Keyed by wiki scope too (PRD FR-ML-4) so a
    /// same-titled article on a different wiki never lends its categories to
    /// this one's affinity. Never serialized — `interest.json` is the
    /// category vector alone.
    #[serde(skip)]
    article_categories: HashMap<(String, String), Vec<String>>,
}

fn default_half_life() -> f64 {
    DEFAULT_HALF_LIFE_DAYS
}

impl Default for InterestModel {
    fn default() -> Self {
        Self {
            categories: BTreeMap::new(),
            half_life_days: DEFAULT_HALF_LIFE_DAYS,
            last_update: 0,
            article_categories: HashMap::new(),
        }
    }
}

impl InterestModel {
    /// A fresh, empty model with the given half-life.
    pub fn new(half_life_days: f64) -> Self {
        Self {
            half_life_days: if half_life_days > 0.0 {
                half_life_days
            } else {
                DEFAULT_HALF_LIFE_DAYS
            },
            ..Self::default()
        }
    }

    /// The configured half-life (days) — read by `:interests` for its
    /// transparency line.
    pub fn half_life_days(&self) -> f64 {
        self.half_life_days
    }

    /// How many categories the model currently tracks (the `:interests`
    /// header count).
    pub fn len(&self) -> usize {
        self.categories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
    }

    /// Decay every score to `now` and drop any that fell below [`PRUNE_EPSILON`]
    /// (magnitude), advancing [`last_update`](Self::last_update). Public so the
    /// `:interests` view and tests can bring the model current without adding a
    /// signal; every mutating path calls it first, so a signal is always added
    /// *after* decay (the documented ordering).
    pub fn decay(&mut self, now: i64) {
        // Only elapsed time decays: `now <= last_update` is a no-op (a signal
        // at or before the reference point, or the very first signal on a
        // fresh model where `last_update` is still 0 — decaying the empty
        // score set would be harmless but pointless). A fresh model's first
        // real-timestamp signal decays nothing (no scores yet) and simply
        // advances `last_update`.
        if now > self.last_update {
            let factor = decay_factor(now - self.last_update, self.half_life_days);
            if factor < 1.0 {
                for v in self.categories.values_mut() {
                    *v *= factor;
                }
                self.categories.retain(|_, v| v.abs() >= PRUNE_EPSILON);
            }
            self.last_update = now;
        }
    }

    /// Record that `title` was opened with these raw (API-form, possibly
    /// `Category:`-prefixed, possibly maintenance) categories, and apply the
    /// FR-PF-3 **open** signal (+1.0) to its topic categories. Stores the
    /// cleaned category list in the session cache (for `affinity_of_title` /
    /// `morelike_seeds`). This is the one place a signal and a categories-cache
    /// update happen together, because "we now know this article's categories"
    /// and "the article was opened" are the same event.
    pub fn note_open(&mut self, wiki: &str, title: &str, raw_categories: &[String], now: i64) {
        let clean = clean_categories(raw_categories);
        self.decay(now);
        for cat in &clean {
            *self.categories.entry(cat.clone()).or_insert(0.0) += SIGNAL_OPEN;
        }
        self.remember_article(wiki, title, clean);
    }

    /// Apply `amount` (a signal weight) to `title`'s known categories, if we
    /// have them cached. Returns whether any category was affected — `false`
    /// when the article's categories aren't known yet (never fetched this
    /// session), which the caller surfaces rather than silently doing nothing.
    /// The bookmark/save/scroll/dwell/not-interested signals all route through
    /// here.
    pub fn apply_signal_for_title(
        &mut self,
        wiki: &str,
        title: &str,
        amount: f64,
        now: i64,
    ) -> bool {
        let Some(clean) = self
            .article_categories
            .get(&(wiki.to_string(), title.to_string()))
            .cloned()
        else {
            return false;
        };
        if clean.is_empty() {
            return false;
        }
        self.decay(now);
        for cat in &clean {
            *self.categories.entry(cat.clone()).or_insert(0.0) += amount;
        }
        true
    }

    /// FR-PF-3's explicit "not interested": tank `title`'s categories by
    /// [`SIGNAL_NOT_INTERESTED`]. Convenience wrapper over
    /// [`apply_signal_for_title`].
    pub fn not_interested(&mut self, wiki: &str, title: &str, now: i64) -> bool {
        self.apply_signal_for_title(wiki, title, SIGNAL_NOT_INTERESTED, now)
    }

    /// Store `title`'s cleaned categories in the session cache, bounding it so
    /// a long browsing session can't grow it without limit. When full, the
    /// arbitrarily-chosen oldest-by-key entry is dropped — this is a
    /// best-effort lookup cache for w3/morelike, not the model, so a cache miss
    /// only means "no affinity boost for that target," never wrong data.
    fn remember_article(&mut self, wiki: &str, title: &str, clean: Vec<String>) {
        const MAX_ARTICLE_CATEGORIES: usize = 512;
        let key = (wiki.to_string(), title.to_string());
        if !self.article_categories.contains_key(&key)
            && self.article_categories.len() >= MAX_ARTICLE_CATEGORIES
            && let Some(victim) = self.article_categories.keys().next().cloned()
        {
            self.article_categories.remove(&victim);
        }
        self.article_categories.insert(key, clean);
    }

    /// Whether this `(wiki, title)`'s categories are already known this session
    /// (in the article cache) — so the caller can skip re-fetching them and
    /// apply the open signal from the cache instead.
    pub fn knows_categories(&self, wiki: &str, title: &str) -> bool {
        self.article_categories
            .contains_key(&(wiki.to_string(), title.to_string()))
    }

    /// The summed affinity of `(wiki, title)`'s categories — the FR-PF-1 w3
    /// term for a link *to* this title. `0.0` for a title whose categories we
    /// haven't seen this session (see the module doc comment's w3 section):
    /// unknown targets get no affinity boost, they never get a fabricated one.
    pub fn affinity_of_title(&self, wiki: &str, title: &str) -> f64 {
        self.article_categories
            .get(&(wiki.to_string(), title.to_string()))
            .map(|cats| self.sum_affinity(cats))
            .unwrap_or(0.0)
    }

    fn sum_affinity(&self, clean_cats: &[String]) -> f64 {
        clean_cats
            .iter()
            .map(|c| self.categories.get(c).copied().unwrap_or(0.0))
            .sum()
    }

    /// The top `n` categories by affinity, highest first (ties broken
    /// alphabetically for a stable display). Powers `:interests`, `wikitui
    /// stats --explain`, and the reading-stats topic distribution.
    pub fn top_categories(&self, n: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self
            .categories
            .iter()
            .map(|(k, &s)| (k.clone(), s))
            .collect();
        v.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        v.truncate(n);
        v
    }

    /// FR-PF-3 candidate generation: seeds for `morelike:` prefetch, drawn from
    /// `recent_titles` (history order, most-recent first) and ranked by each
    /// article's summed affinity. Only articles with known categories and a
    /// *positive* summed affinity seed (a topic the reader engaged with, not
    /// one they vetoed), and each seed reports its single highest-scoring
    /// category as the "reading" the reason string names.
    pub fn morelike_seeds(&self, recent_titles: &[String], max_seeds: usize) -> Vec<MorelikeSeed> {
        let mut seeds: Vec<MorelikeSeed> = recent_titles
            .iter()
            .filter_map(|title| {
                // Recent-reads arrive as bare titles, so a seed matches this
                // title in *any* wiki scope (PRD FR-ML-4): the category cache
                // is `(wiki, title)`-keyed, but seed selection is a heuristic
                // over "articles you engaged with", not a wiki-exact lookup.
                let cats = self
                    .article_categories
                    .iter()
                    .find(|((_, t), _)| t == title)
                    .map(|(_, cats)| cats)?;
                let affinity = self.sum_affinity(cats);
                if affinity <= 0.0 {
                    return None;
                }
                let category = self.dominant_category(cats)?;
                Some(MorelikeSeed {
                    title: title.clone(),
                    category,
                    affinity,
                })
            })
            .collect();
        seeds.sort_by(|a, b| {
            b.affinity
                .partial_cmp(&a.affinity)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.title.cmp(&b.title))
        });
        seeds.truncate(max_seeds);
        seeds
    }

    /// The highest-scoring category among `clean_cats` (the topic driving a
    /// morelike seed's reason string). Ties break alphabetically for a stable
    /// choice.
    fn dominant_category(&self, clean_cats: &[String]) -> Option<String> {
        clean_cats
            .iter()
            .max_by(|a, b| {
                let sa = self.categories.get(*a).copied().unwrap_or(0.0);
                let sb = self.categories.get(*b).copied().unwrap_or(0.0);
                sa.partial_cmp(&sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.cmp(a))
            })
            .cloned()
    }

    // ---- Persistence (FR-PR-2 "human-readable state files") ---------------

    /// Load the model from `path`, overriding its half-life with `half_life_days`
    /// (config stays authoritative — the persisted half-life is informational).
    /// Any read/parse failure yields a fresh empty model rather than an error:
    /// the interest model is non-critical, exactly like `history::History`'s
    /// "a missing/corrupt store is not a reason to refuse to start" posture.
    pub fn load(path: &Path, half_life_days: f64) -> Self {
        let mut model = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<InterestModel>(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        if half_life_days > 0.0 {
            model.half_life_days = half_life_days;
        }
        model
    }

    /// Serialize to pretty-printed JSON at `path`, creating parent directories.
    /// Best-effort: a write failure returns `Err` for the caller to log, never
    /// panics (mirrors `history`'s swallow-to-a-log-line posture).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }
}

/// The real on-disk location (PRD §6.4: `$XDG_STATE_HOME/wikitui/interest.json`,
/// alongside `history.sqlite`). `None` when no platform state directory can be
/// determined — mirrors `history::history_path` exactly (state dir on
/// Linux/BSD, `<data_dir>/state` on macOS/Windows where `directories` has no
/// state-dir concept).
pub fn interest_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("interest.json"))
}

/// Strip a leading `Category:` (any wiki's canonical namespace form arrives
/// as `Category:Foo`; the mock and Action API alike) and trim, giving the
/// display/storage form used as the map key.
fn normalize_category(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_ns = trimmed
        .strip_prefix("Category:")
        .or_else(|| trimmed.strip_prefix("category:"))
        .unwrap_or(trimmed);
    without_ns.trim().to_string()
}

/// Clean a raw category list: normalize each, drop maintenance/tracking
/// categories ([`is_maintenance_category`]), drop empties, and dedup while
/// preserving order. The single funnel every category goes through before it
/// can touch a score or the article cache.
fn clean_categories(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        if is_maintenance_category(r) {
            continue;
        }
        let name = normalize_category(r);
        if name.is_empty() || out.contains(&name) {
            continue;
        }
        out.push(name);
    }
    out
}

/// Whether a category is Wikipedia maintenance/tracking bookkeeping rather
/// than a real topic (see the module doc comment's filtering section). A
/// **documented heuristic**, deliberately conservative so a genuine topic is
/// never dropped: a category matches if, after stripping `Category:`, it
/// *starts with* one of the known maintenance phrase prefixes, or *contains*
/// one of a few unambiguous maintenance tokens. These patterns cover the
/// overwhelmingly common tracking categories (CS1 citation maintenance, dead-
/// link/webarchive tracking, date/description-format templates, "Articles
/// with/containing/needing ..." families, Wikidata-linked bookkeeping) that
/// are present on nearly every article yet say nothing about reader interest —
/// and that the API's `clshow=!hidden` filter misses because most of them are
/// not actually flagged `hidden`.
pub fn is_maintenance_category(raw: &str) -> bool {
    let name = normalize_category(raw);
    /// Case-sensitively matched maintenance phrase *prefixes* (real Wikipedia
    /// category names are consistently Title Case; matching the exact prefix
    /// avoids clobbering a topic that merely shares a word).
    const MAINT_PREFIXES: &[&str] = &[
        "Articles with",
        "Articles containing",
        "Articles needing",
        "Articles lacking",
        "Articles to be",
        "Articles using",
        "Articles that",
        "Articles where",
        "All articles",
        "All pages",
        "All Wikipedia",
        "All stub articles",
        "Wikipedia articles",
        "Wikipedia indefinitely",
        "Wikipedia pages",
        "Wikipedia references",
        "Wikipedia introduction",
        "Use dmy dates",
        "Use mdy dates",
        "Use British English",
        "Use American English",
        "Short description",
        "Pages using",
        "Pages with",
        "Commons category",
        "Coordinates on",
        "Webarchive",
        "CS1",
        "Redirects",
        "Disambiguation pages",
        "Interlanguage link",
        "EngvarB",
        "Good articles",
        "Featured articles",
    ];
    /// Unambiguous maintenance *substrings* (a token that only ever appears in
    /// bookkeeping categories, so a substring test is safe).
    const MAINT_SUBSTRINGS: &[&str] = &["Wikidata", "maint:", "template wayback", "stub"];
    MAINT_PREFIXES.iter().any(|p| name.starts_with(p))
        || MAINT_SUBSTRINGS.iter().any(|s| name.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn cats(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn score(model: &InterestModel, cat: &str) -> f64 {
        model.categories.get(cat).copied().unwrap_or(0.0)
    }

    // ---- maintenance filtering ------------------------------------------

    #[test]
    fn maintenance_categories_are_filtered_topics_are_kept() {
        assert!(is_maintenance_category(
            "Category:Articles with dead external links"
        ));
        assert!(is_maintenance_category(
            "CS1 maint: multiple names: authors list"
        ));
        assert!(is_maintenance_category(
            "Category:Webarchive template wayback links"
        ));
        assert!(is_maintenance_category("Use dmy dates from January 2020"));
        assert!(is_maintenance_category("Coordinates on Wikidata"));
        assert!(is_maintenance_category("All stub articles"));
        // Real topics must survive.
        assert!(!is_maintenance_category("Category:Cryptography"));
        assert!(!is_maintenance_category(
            "Category:British computer scientists"
        ));
        assert!(!is_maintenance_category("Encryption devices"));
    }

    #[test]
    fn clean_categories_strips_prefix_drops_maintenance_and_dedups() {
        let cleaned = clean_categories(&cats(&[
            "Category:Cryptography",
            "Category:Articles with dead external links",
            "Category:Cryptography", // duplicate
            "Encryption devices",
        ]));
        assert_eq!(cleaned, vec!["Cryptography", "Encryption devices"]);
    }

    // ---- signal application ---------------------------------------------

    #[test]
    fn open_signal_adds_one_to_each_topic_category() {
        let mut m = InterestModel::new(30.0);
        m.note_open(
            "",
            "Enigma machine",
            &cats(&["Category:Cryptography", "Category:Encryption devices"]),
            1_000,
        );
        assert_eq!(score(&m, "Cryptography"), SIGNAL_OPEN);
        assert_eq!(score(&m, "Encryption devices"), SIGNAL_OPEN);
    }

    #[test]
    fn each_signal_type_adds_its_documented_amount() {
        let mut m = InterestModel::new(30.0);
        // Seed the article's categories (an open) so title-keyed signals apply.
        m.note_open("", "Enigma machine", &cats(&["Category:Cryptography"]), 0);
        assert_eq!(score(&m, "Cryptography"), 1.0, "open = +1.0");

        assert!(m.apply_signal_for_title("", "Enigma machine", SIGNAL_BOOKMARK, 0));
        assert_eq!(score(&m, "Cryptography"), 1.0 + 3.0, "bookmark = +3.0");

        assert!(m.apply_signal_for_title("", "Enigma machine", SIGNAL_SAVE, 0));
        assert_eq!(score(&m, "Cryptography"), 4.0 + 5.0, "save = +5.0");

        assert!(m.apply_signal_for_title("", "Enigma machine", SIGNAL_SCROLL, 0));
        assert_eq!(score(&m, "Cryptography"), 9.0 + 1.0, "scroll = +1.0");
    }

    #[test]
    fn a_signal_for_an_unknown_article_is_a_no_op_and_reports_false() {
        let mut m = InterestModel::new(30.0);
        assert!(
            !m.apply_signal_for_title("", "Never Opened", SIGNAL_BOOKMARK, 0),
            "no categories known → no-op, reported to the caller"
        );
        assert!(m.is_empty());
    }

    #[test]
    fn dwell_normalization_caps_at_two_and_scales_below() {
        // A full read (dwell >= expected) contributes the whole +2.0.
        assert_eq!(normalized_dwell(300, 300.0), 2.0);
        assert_eq!(normalized_dwell(600, 300.0), 2.0, "over-read still caps");
        // Half the expected time contributes half of +2.0.
        assert_eq!(normalized_dwell(150, 300.0), 1.0);
        // Degenerate inputs contribute nothing rather than dividing by zero.
        assert_eq!(normalized_dwell(0, 300.0), 0.0);
        assert_eq!(normalized_dwell(100, 0.0), 0.0);
    }

    #[test]
    fn not_interested_tanks_the_articles_categories() {
        let mut m = InterestModel::new(30.0);
        m.note_open("", "Enigma machine", &cats(&["Category:Cryptography"]), 0);
        // A few positive signals build it up...
        m.apply_signal_for_title("", "Enigma machine", SIGNAL_SAVE, 0);
        assert!(score(&m, "Cryptography") > 0.0);
        // ...then "not interested" drives it strongly negative.
        assert!(m.not_interested("", "Enigma machine", 0));
        assert_eq!(score(&m, "Cryptography"), 1.0 + 5.0 - 10.0);
        assert!(
            score(&m, "Cryptography") < 0.0,
            "a veto outweighs the reads"
        );
    }

    // ---- time decay ------------------------------------------------------

    #[test]
    fn a_score_halves_after_one_half_life() {
        let mut m = InterestModel::new(30.0);
        // +4.0 to Cryptography at t0 (four opens, say).
        for _ in 0..4 {
            m.note_open("", "X", &cats(&["Category:Cryptography"]), 0);
        }
        assert_eq!(score(&m, "Cryptography"), 4.0);
        // Decay to exactly one half-life later: 4.0 -> 2.0.
        m.decay(30 * DAY);
        assert!(
            (score(&m, "Cryptography") - 2.0).abs() < 1e-9,
            "halved at 30d"
        );
    }

    #[test]
    fn decay_happens_before_the_new_signal_is_added() {
        let mut m = InterestModel::new(30.0);
        // Build Cryptography to 4.0 at t0.
        for _ in 0..4 {
            m.note_open("", "X", &cats(&["Category:Cryptography"]), 0);
        }
        // One half-life later, a fresh open (+1.0). Decay-then-add: 4*0.5 + 1.
        m.note_open("", "Y", &cats(&["Category:Cryptography"]), 30 * DAY);
        assert!(
            (score(&m, "Cryptography") - 3.0).abs() < 1e-9,
            "decay (4->2) then add (+1) = 3.0, not (4+1) then decay"
        );
    }

    #[test]
    fn decayed_near_zero_scores_are_pruned_to_keep_the_file_small() {
        let mut m = InterestModel::new(30.0);
        m.note_open("", "X", &cats(&["Category:Cryptography"]), 0);
        // Many half-lives later the score is negligible and dropped entirely.
        m.decay(3000 * DAY);
        assert!(
            m.is_empty(),
            "a long-untouched score is pruned, not kept as ~0"
        );
    }

    // ---- w3 affinity of a title -----------------------------------------

    #[test]
    fn affinity_of_a_seen_title_sums_its_categories_unseen_is_zero() {
        let mut m = InterestModel::new(30.0);
        m.note_open("", "Enigma machine", &cats(&["Category:Cryptography"]), 0);
        m.apply_signal_for_title("", "Enigma machine", SIGNAL_SAVE, 0); // Crypto = 6.0
        m.note_open(
            "",
            "Computer science",
            &cats(&["Category:Computer science"]),
            0,
        ); // CS = 1.0
        assert_eq!(m.affinity_of_title("", "Enigma machine"), 6.0);
        assert_eq!(m.affinity_of_title("", "Computer science"), 1.0);
        assert_eq!(
            m.affinity_of_title("", "Some Unread Target"),
            0.0,
            "an unseen target gets no fabricated affinity (the w3 resolution)"
        );
    }

    // ---- candidate generation -------------------------------------------

    #[test]
    fn morelike_seeds_rank_recent_reads_by_affinity_and_name_the_topic() {
        let mut m = InterestModel::new(30.0);
        // Enigma carries two crypto-ish topics and gets saved → highest
        // article affinity; Alan Turing shares only one; CS is unrelated.
        m.note_open(
            "",
            "Enigma machine",
            &cats(&["Category:Cryptography", "Category:Encryption devices"]),
            0,
        );
        m.note_open("", "Alan Turing", &cats(&["Category:Cryptography"]), 0);
        m.apply_signal_for_title("", "Enigma machine", SIGNAL_SAVE, 0); // Crypto & EncDev boosted
        m.note_open(
            "",
            "Computer science",
            &cats(&["Category:Computer science"]),
            0,
        );

        let recent = cats(&["Computer science", "Alan Turing", "Enigma machine"]);
        let seeds = m.morelike_seeds(&recent, 2);
        assert_eq!(seeds.len(), 2, "capped at max_seeds");
        assert_eq!(seeds[0].title, "Enigma machine", "highest affinity first");
        assert_eq!(
            seeds[0].category, "Cryptography",
            "the dominant topic names the seed"
        );
        assert!(seeds[0].affinity > seeds[1].affinity);
    }

    #[test]
    fn morelike_seeds_skip_vetoed_and_unknown_articles() {
        let mut m = InterestModel::new(30.0);
        m.note_open("", "Enigma machine", &cats(&["Category:Cryptography"]), 0);
        m.not_interested("", "Enigma machine", 0); // now negative affinity
        let recent = cats(&["Enigma machine", "Never Opened"]);
        assert!(
            m.morelike_seeds(&recent, 5).is_empty(),
            "a vetoed read and an unknown title both fail to seed"
        );
    }

    // ---- persistence round-trip -----------------------------------------

    #[test]
    fn persist_and_reload_round_trips_as_human_readable_json() {
        let dir = std::env::temp_dir().join(format!(
            "wikitui-interest-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("interest.json");
        let mut m = InterestModel::new(30.0);
        m.note_open("", "Enigma machine", &cats(&["Category:Cryptography"]), 42);
        m.apply_signal_for_title("", "Enigma machine", SIGNAL_BOOKMARK, 42);
        m.save(&path).expect("save");

        // The file is human-readable JSON naming the category and its score.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Cryptography"), "category name is plain text");
        assert!(text.contains("categories"), "pretty-printed top-level keys");

        let reloaded = InterestModel::load(&path, 30.0);
        assert_eq!(
            reloaded.top_categories(1),
            vec![("Cryptography".to_string(), 4.0)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_of_a_missing_file_is_an_empty_model_not_an_error() {
        let m = InterestModel::load(Path::new("/nonexistent/interest.json"), 45.0);
        assert!(m.is_empty());
        assert_eq!(
            m.half_life_days(),
            45.0,
            "config half-life wins over the default"
        );
    }

    #[test]
    fn top_categories_orders_by_score_then_name() {
        let mut m = InterestModel::new(30.0);
        m.note_open("", "A", &cats(&["Category:Zebra topic"]), 0);
        m.note_open("", "B", &cats(&["Category:Cryptography"]), 0);
        m.apply_signal_for_title("", "B", SIGNAL_SAVE, 0); // Crypto highest
        let top = m.top_categories(10);
        assert_eq!(top[0].0, "Cryptography");
        assert_eq!(top[1].0, "Zebra topic");
    }
}
