//! PRD §6.8 / §11 / G1: the **cache-hit-rate KPI**, measured locally. §6.8
//! sets "steady-state cache-hit rate > 60% of article opens" as the number
//! that justifies the page cache (§5.7) and prefetch (§5.8); §11 promises a
//! "local-only stats screen [that] shows the user their own cache-hit rate …
//! (users can verify our headline claim themselves)". Reading history alone
//! can't answer it — a visit row records *what* was read, never *where the
//! bytes came from* — so this module keeps the one extra fact the KPI needs:
//! a per-open source tag.
//!
//! ## What an "article open" is
//!
//! **An article open is a document the reader asked for being installed in a
//! tab for reading, counted once, the first time that installed document is
//! laid out on screen.** Concretely, it is armed by the two install funnels
//! every navigation already goes through — `App::set_document` (fresh open,
//! link follow, back/forward, `gb`, bookmark/read-later/history/trail/saved/
//! offline-search/ZIM reopen, `:noredirect`, `:bilingual`'s second pane) and
//! `main::apply_tab_load_outcome` (a background tab via `Ctrl-Enter`/`F`, a
//! session-restore tab) — and consumed by the first layout of that tab
//! (`App::ensure_layout`, or `App::layout_for_tab` for a split pane).
//!
//! *Why "first laid out" rather than "installed":* the L1 tier (below) is
//! only knowable at layout time, and a background tab that is never looked
//! at was never read — counting it when it lands would let a 10-tab session
//! restore score ten free disk hits on every launch, flattering the KPI with
//! no reading behind it. Counting at first view is the same "exactly once,
//! the first time it is on screen" rule the disambiguation chooser already
//! follows (`Tab::disambig_pending`).
//!
//! **Not opens** (each would double count, or count something no reader
//! asked for): switching tabs or split-pane focus, `u` close-undo, the
//! `:vsplit` duplicate pane, a low-memory tab rehydrating on focus (all
//! return to an already-counted reading context); the SWR `r` reload (the
//! same open, now at the revalidated revision — it was counted once, as the
//! stale L2 serve it began as); a foreground 429 retry that replaces a stale
//! copy already shown for the same request (the same open, upgraded — but a
//! retry that lands where *nothing* had been served yet *is* the open, and
//! counts as network); prefetch and background revalidation (they write L2
//! and install nothing); link-preview/summary/langlinks fetches (never a
//! document); and `--dump` (a one-shot plain-text export, not a reading
//! session — it records no history either).
//!
//! ## What a "hit" is
//!
//! **A hit is an open served without the reader waiting on the network for
//! its content**: [`OpenSource::L1`] (the layout was reused from the
//! in-memory `layout::LayoutCache`), [`OpenSource::Disk`] (the L2 page
//! cache, `cache::PageCache` — including a stale-while-revalidate serve,
//! whose revalidation happens in the background after the reader already
//! has the page), and [`OpenSource::SavedOffline`] (the pinned saved-pages
//! store, a ZIM archive, or the stale-cache fallback when the network
//! failed). [`OpenSource::Network`] — a live fetch the open waited on — is
//! the only miss. Precedence when more than one applies: network always
//! wins (a live fetch that happens to reproduce an L1-cached layout was
//! still waited on), then L1, then the tier the content was read from.
//!
//! The offline fallback is counted as a hit, as the KPI's "saved/offline"
//! bucket, even though a failed network attempt preceded it: local storage
//! is what put the article on screen. It is broken out separately so the
//! reader can see how much of their rate it accounts for.
//!
//! ## Steady state
//!
//! Two figures are kept: an **all-time** count, and a **rolling window of the
//! last [`STEADY_STATE_WINDOW`] opens** — the "steady-state" figure §6.8
//! means. A count window (rather than "the last 30 days") because it reads
//! the same for a reader who opens five articles a month and one who opens
//! a hundred a day, needs no clock or timestamps at all (nothing here records
//! *when* anything was read), and ages the cold-cache first opens of a fresh
//! install out after a fixed amount of reading. 500 because at the 60%
//! threshold the binomial standard error is √(0.6·0.4/500) ≈ 2.2 points —
//! tight enough to tell 55% from 65% — while still being a few weeks of
//! ordinary reading, recent enough to reflect the cache as it is now.
//!
//! ## Privacy
//!
//! Local-only (FR-PR-1/FR-PR-2): the log is `cache_hits.json` in the state
//! directory, human-readable, and records **no titles and no timestamps** —
//! only four counters and one letter per recent open. Incognito records
//! nothing (FR-PR-3: `privacy::Write::Stats` is passive and denied, checked
//! both when an open is armed and when it is counted), and `wikitui
//! clear-data --stats` deletes it (FR-PR-4).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::PageSource;

/// How many recent opens the steady-state figure covers — see the module doc
/// comment's "Steady state" for why a count and why 500.
pub const STEADY_STATE_WINDOW: usize = 500;

/// Where one article open was served from (see the module doc comment for the
/// precedence rules and what counts as a hit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenSource {
    /// The laid-out render was reused from the in-memory L1 layout cache
    /// (`layout::LayoutCache`, PRD FR-OFF-1) — no relayout.
    L1,
    /// Read from the L2 disk page cache (`cache::PageCache`), including a
    /// stale-while-revalidate serve.
    Disk,
    /// The pinned saved-pages store, a ZIM archive, or the stale-cache
    /// fallback when the network attempt failed (PRD FR-OFF-6's ○ state).
    SavedOffline,
    /// A live network fetch the open waited on — the one miss.
    Network,
}

impl OpenSource {
    /// The fetch tier a tab's [`PageSource`] implies, before the L1 check —
    /// `None` for [`PageSource::None`] (nothing was fetched: an unarmed
    /// install, e.g. a test fixture or a split duplicate). `Offline`,
    /// `Saved` and `Zim` all fold into [`Self::SavedOffline`].
    pub fn from_page_source(source: PageSource) -> Option<Self> {
        match source {
            PageSource::None => None,
            PageSource::Live => Some(Self::Network),
            PageSource::Cached { .. } => Some(Self::Disk),
            PageSource::Offline { .. } | PageSource::Saved { .. } | PageSource::Zim => {
                Some(Self::SavedOffline)
            }
        }
    }

    /// Final classification once the first layout has happened: network
    /// always stands (the reader waited on it); anything else is upgraded to
    /// L1 when that layout was an L1 cache hit.
    pub fn resolve(tier: Self, l1_hit: bool) -> Self {
        if l1_hit && tier != Self::Network {
            Self::L1
        } else {
            tier
        }
    }

    /// One-letter code for the on-disk recent-opens string (see
    /// [`RECENT_LEGEND`]).
    fn code(self) -> char {
        match self {
            Self::L1 => 'M',
            Self::Disk => 'D',
            Self::SavedOffline => 'S',
            Self::Network => 'N',
        }
    }

    fn from_code(c: char) -> Option<Self> {
        match c {
            'M' => Some(Self::L1),
            'D' => Some(Self::Disk),
            'S' => Some(Self::SavedOffline),
            'N' => Some(Self::Network),
            _ => None,
        }
    }
}

/// Written into the file beside the recent-opens string so the state file
/// explains itself to a reader inspecting it by hand (FR-PR-2's
/// "human-readable state files"). Ignored on load.
const RECENT_LEGEND: &str = "oldest to newest; M = L1 memory layout cache, D = disk page cache, S = saved/offline, N = network";

/// Per-source open counts — the all-time totals, or the steady-state window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    #[serde(default)]
    pub l1: u64,
    #[serde(default)]
    pub disk: u64,
    #[serde(default)]
    pub saved_offline: u64,
    #[serde(default)]
    pub network: u64,
}

impl Counts {
    fn add(&mut self, source: OpenSource) {
        match source {
            OpenSource::L1 => self.l1 += 1,
            OpenSource::Disk => self.disk += 1,
            OpenSource::SavedOffline => self.saved_offline += 1,
            OpenSource::Network => self.network += 1,
        }
    }

    /// Every counted open.
    pub fn total(&self) -> u64 {
        self.l1 + self.disk + self.saved_offline + self.network
    }

    /// Opens served without waiting on the network.
    pub fn hits(&self) -> u64 {
        self.l1 + self.disk + self.saved_offline
    }

    /// The hit rate as a whole percentage, **rounded down** — so a displayed
    /// "60%" never stands in for 59.6% against a "> 60%" target — or `None`
    /// when there is nothing to divide (no opens yet: say so, never "0%").
    pub fn hit_percent(&self) -> Option<u64> {
        let total = self.total();
        (total > 0).then(|| self.hits() * 100 / total)
    }
}

/// The on-disk shape (`cache_hits.json`). `recent` is one [`OpenSource`]
/// letter per open, oldest first, capped at [`STEADY_STATE_WINDOW`].
#[derive(Debug, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    all_time: Counts,
    #[serde(default)]
    recent: String,
    #[serde(default)]
    recent_legend: String,
}

const ON_DISK_VERSION: u32 = 1;

/// The persisted open log: all-time counts plus the steady-state window.
/// `App::new` starts with an empty, never-persisted one (tests never touch
/// the real state dir); `main::run` loads the real file and sets
/// `App::open_log_path`, mirroring the interest model's split.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenLog {
    all_time: Counts,
    recent: VecDeque<OpenSource>,
}

impl OpenLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one open: bumps the all-time total and pushes onto the window,
    /// dropping the oldest entry once the window is full.
    pub fn record(&mut self, source: OpenSource) {
        self.all_time.add(source);
        self.recent.push_back(source);
        while self.recent.len() > STEADY_STATE_WINDOW {
            self.recent.pop_front();
        }
    }

    pub fn all_time(&self) -> Counts {
        self.all_time
    }

    /// Counts over the steady-state window (the last [`STEADY_STATE_WINDOW`]
    /// opens, or every open when there have been fewer).
    pub fn window(&self) -> Counts {
        let mut c = Counts::default();
        for s in &self.recent {
            c.add(*s);
        }
        c
    }

    /// Load from `path`; a missing, unreadable or corrupt file is an empty
    /// log (the KPI is non-critical — same posture as `interest.json`).
    /// Unknown letters in `recent` are skipped rather than failing the load.
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let Ok(disk) = serde_json::from_slice::<OnDisk>(&bytes) else {
            return Self::default();
        };
        let mut recent: VecDeque<OpenSource> = disk
            .recent
            .chars()
            .filter_map(OpenSource::from_code)
            .collect();
        while recent.len() > STEADY_STATE_WINDOW {
            recent.pop_front();
        }
        Self {
            all_time: disk.all_time,
            recent,
        }
    }

    /// Atomic pretty-printed JSON write (`atomicio::write_atomic`), so a crash
    /// mid-write never leaves a truncated file behind.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let disk = OnDisk {
            version: ON_DISK_VERSION,
            all_time: self.all_time,
            recent: self.recent.iter().map(|s| s.code()).collect(),
            recent_legend: RECENT_LEGEND.to_string(),
        };
        let json = serde_json::to_string_pretty(&disk)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::atomicio::write_atomic(path, json.as_bytes())
    }
}

/// `$XDG_STATE_HOME/wikitui/cache_hits.json`, beside `interest.json` and
/// `history.sqlite`. `None` when no platform state directory resolves.
pub fn open_log_path() -> Option<PathBuf> {
    Some(crate::paths::wikitui_state_dir()?.join("cache_hits.json"))
}

/// The per-source breakdown, e.g. "L1 80 · disk 56 · saved/offline 4 ·
/// network 72" — shared by the CLI and the `:stats` panel.
pub fn breakdown(c: &Counts) -> String {
    format!(
        "L1 {} · disk {} · saved/offline {} · network {}",
        c.l1, c.disk, c.saved_offline, c.network
    )
}

/// The steady-state headline, e.g. "64% of the last 212 article opens", or
/// the explicit no-data wording (never "0%" for "nothing measured").
pub fn describe_window(c: &Counts) -> String {
    match c.hit_percent() {
        Some(p) => format!(
            "{p}% of the last {} article open{}",
            c.total(),
            if c.total() == 1 { "" } else { "s" }
        ),
        None => "no article opens recorded yet".to_string(),
    }
}

/// The all-time headline, e.g. "61% of 1024 article opens".
pub fn describe_all_time(c: &Counts) -> String {
    match c.hit_percent() {
        Some(p) => format!(
            "{p}% of {} article open{}",
            c.total(),
            if c.total() == 1 { "" } else { "s" }
        ),
        None => "no article opens recorded yet".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-hitrate-test-{tag}-{}-{n}.json",
            std::process::id()
        ))
    }

    #[test]
    fn page_source_maps_to_the_fetch_tier() {
        assert_eq!(OpenSource::from_page_source(PageSource::None), None);
        assert_eq!(
            OpenSource::from_page_source(PageSource::Live),
            Some(OpenSource::Network)
        );
        assert_eq!(
            OpenSource::from_page_source(PageSource::Cached { age_secs: 5 }),
            Some(OpenSource::Disk)
        );
        for s in [
            PageSource::Offline { age_secs: 5 },
            PageSource::Saved { age_secs: 5 },
            PageSource::Zim,
        ] {
            assert_eq!(
                OpenSource::from_page_source(s),
                Some(OpenSource::SavedOffline),
                "{s:?}"
            );
        }
    }

    #[test]
    fn resolve_upgrades_local_tiers_to_l1_but_never_network() {
        assert_eq!(OpenSource::resolve(OpenSource::Disk, true), OpenSource::L1);
        assert_eq!(
            OpenSource::resolve(OpenSource::SavedOffline, true),
            OpenSource::L1
        );
        assert_eq!(
            OpenSource::resolve(OpenSource::Network, true),
            OpenSource::Network,
            "a live fetch was waited on even if its layout was already in L1"
        );
        assert_eq!(
            OpenSource::resolve(OpenSource::Disk, false),
            OpenSource::Disk
        );
    }

    #[test]
    fn only_network_is_a_miss() {
        let mut log = OpenLog::new();
        log.record(OpenSource::L1);
        log.record(OpenSource::Disk);
        log.record(OpenSource::SavedOffline);
        assert_eq!(log.window().hit_percent(), Some(100));
        log.record(OpenSource::Network);
        assert_eq!(log.window().hits(), 3);
        assert_eq!(log.window().hit_percent(), Some(75));
    }

    #[test]
    fn empty_log_reports_no_data_rather_than_zero_percent() {
        let log = OpenLog::new();
        assert_eq!(log.window().hit_percent(), None);
        assert_eq!(
            describe_window(&log.window()),
            "no article opens recorded yet"
        );
        assert_eq!(
            describe_all_time(&log.all_time()),
            "no article opens recorded yet"
        );
    }

    #[test]
    fn window_description_and_breakdown_match_the_documented_shape() {
        let mut log = OpenLog::new();
        for _ in 0..80 {
            log.record(OpenSource::L1);
        }
        for _ in 0..56 {
            log.record(OpenSource::Disk);
        }
        for _ in 0..4 {
            log.record(OpenSource::SavedOffline);
        }
        for _ in 0..72 {
            log.record(OpenSource::Network);
        }
        // 140 / 212 = 66.04% → 66 (rounded down).
        assert_eq!(
            describe_window(&log.window()),
            "66% of the last 212 article opens"
        );
        assert_eq!(
            breakdown(&log.window()),
            "L1 80 · disk 56 · saved/offline 4 · network 72"
        );
    }

    #[test]
    fn hit_percent_rounds_down_so_it_never_overstates() {
        // 299 hits / 500 = 59.8% — must read 59, not 60.
        let c = Counts {
            l1: 0,
            disk: 299,
            saved_offline: 0,
            network: 201,
        };
        assert_eq!(c.hit_percent(), Some(59));
        let one = Counts {
            network: 1,
            ..Counts::default()
        };
        assert_eq!(one.hit_percent(), Some(0));
        assert_eq!(describe_window(&one), "0% of the last 1 article open");
    }

    #[test]
    fn the_window_keeps_only_the_last_500_opens_while_all_time_keeps_counting() {
        let mut log = OpenLog::new();
        // A cold start: 500 network misses...
        for _ in 0..STEADY_STATE_WINDOW {
            log.record(OpenSource::Network);
        }
        assert_eq!(log.window().hit_percent(), Some(0));
        // ...then the cache warms up: 400 disk hits push 400 misses out.
        for _ in 0..400 {
            log.record(OpenSource::Disk);
        }
        let w = log.window();
        assert_eq!(w.total(), STEADY_STATE_WINDOW as u64);
        assert_eq!(w.disk, 400);
        assert_eq!(w.network, 100);
        assert_eq!(w.hit_percent(), Some(80), "steady state reflects now");
        let all = log.all_time();
        assert_eq!(all.total(), 900);
        assert_eq!(all.network, 500);
        assert_eq!(all.hit_percent(), Some(44), "400/900, rounded down");
        assert_eq!(describe_all_time(&all), "44% of 900 article opens");
    }

    #[test]
    fn save_then_load_round_trips_and_the_file_is_human_readable() {
        let path = temp_path("roundtrip");
        let mut log = OpenLog::new();
        log.record(OpenSource::Network);
        log.record(OpenSource::Disk);
        log.record(OpenSource::L1);
        log.record(OpenSource::SavedOffline);
        log.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"recent\": \"NDMS\""), "{text}");
        assert!(text.contains("recent_legend"));
        // FR-PR-2 / privacy: counts and letters only — nothing that says
        // *what* was read or *when*.
        assert!(!text.contains("title"));
        assert!(!text.contains("opened_at"));

        let loaded = OpenLog::load(&path);
        assert_eq!(loaded, log);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_an_empty_log() {
        let missing = temp_path("missing");
        assert_eq!(OpenLog::load(&missing), OpenLog::new());
        let corrupt = temp_path("corrupt");
        std::fs::write(&corrupt, b"{not json").unwrap();
        assert_eq!(OpenLog::load(&corrupt), OpenLog::new());
        let _ = std::fs::remove_file(&corrupt);
    }

    #[test]
    fn load_skips_unknown_letters_and_trims_an_oversized_window() {
        let path = temp_path("oversized");
        let recent: String = std::iter::repeat_n('D', STEADY_STATE_WINDOW + 10)
            .chain("x?N".chars())
            .collect();
        let json =
            format!(r#"{{"version":1,"all_time":{{"disk":510,"network":1}},"recent":"{recent}"}}"#);
        std::fs::write(&path, json).unwrap();
        let log = OpenLog::load(&path);
        let w = log.window();
        assert_eq!(w.total(), STEADY_STATE_WINDOW as u64);
        assert_eq!(w.network, 1, "the newest (last) entry survives the trim");
        assert_eq!(log.all_time().disk, 510);
        let _ = std::fs::remove_file(&path);
    }
}
