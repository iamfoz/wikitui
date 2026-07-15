//! Persistent reading history (PRD FR-HS-1/2/4, §6.4): a small SQLite
//! database at `$XDG_STATE_HOME/wikitui/history.sqlite` — this module is the
//! first thing in the codebase to use `rusqlite` (§6.1's stack decision),
//! chosen over another line-oriented JSONL store (`bookmarks.rs`,
//! `research.rs`) because the picker's two access patterns — "most recent
//! visit per article" and "fuzzy-ranked over however many thousand visits a
//! long-lived install accumulates" — are exactly what an index earns its
//! keep on, where a JSONL file would mean a full linear rescan per keystroke
//! of the picker's `/` filter.
//!
//! ## Schema
//!
//! One table, `visits`, one row per page load:
//!
//! ```text
//! visits(id, lang, title, opened_at, dwell_secs, referrer_lang, referrer_title)
//! ```
//!
//! `referrer_*` is the article the reader navigated *from* within the same
//! tab, `NULL` for the first page a tab ever shows. Migrated via the
//! `user_version` pragma (`migrate`) rather than a sentinel row, so a schema
//! change never has to reserve a row of its own or guess from `PRAGMA
//! table_info`.
//!
//! ## The visited cache
//!
//! `ui.rs` needs to know, for every visible link on every draw, whether its
//! target has ever been visited (PRD FR-HS-2's persistent visited-link
//! styling) — one `SELECT` per link per frame would turn scrolling a
//! link-heavy article into a disk-bound operation. Instead `History` keeps
//! the full `(lang, title)` visited set in memory (`visited`), loaded once
//! at open time and kept current by `record_visit`/`clear`, so
//! `is_visited`/`visited_titles_for_lang` are plain hash lookups.
//!
//! ## Failure posture
//!
//! History is explicitly non-critical (PRD FR-HS-4 is P0 but nothing in
//! §11's success metrics depends on it): every write here swallows its own
//! error to a `log_write_failure` stderr line rather than surfacing a
//! user-facing error or panicking. Losing one row (a crowded disk, a
//! corrupted file) is preferable to interrupting reading over it. The one
//! exception is `clear` (a deliberate, user-invoked `:history clear`/`d`
//! action), which returns a `Result` so the caller can tell the reader their
//! delete didn't stick.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

/// Schema version this build understands (the `PRAGMA user_version`
/// counterpart of `config::CONFIG_VERSION`). Bump alongside a new branch in
/// `migrate` when the shape of `visits` changes.
const SCHEMA_VERSION: i64 = 2;

/// One logged page view (PRD FR-HS-1's "title, wiki, timestamp, dwell time,
/// referrer article").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub id: i64,
    pub lang: String,
    pub title: String,
    /// Unix seconds — this module's own clock access point (`now_unix`) is
    /// the only place that reads the wall clock, so comparisons here are
    /// never against a second, independently-read "now".
    pub opened_at: i64,
    pub dwell_secs: i64,
    pub referrer_lang: Option<String>,
    pub referrer_title: Option<String>,
}

/// A saved reading position (PRD FR-NV-8): where the reader last left an
/// article, keyed to the revision it was saved against. `revid == 0` means the
/// revision was unknown at save time (degraded mode), in which case the exact
/// restore is skipped in favor of the anchor fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPosition {
    pub revid: u64,
    pub scroll: u16,
    /// The folded-heading-block set, sorted (PRD FR-NV-3 fold state).
    pub folds: Vec<usize>,
    /// The section heading the saved scroll sat in — the anchor for the
    /// revid-mismatch fallback. `None` when the scroll was above every heading.
    pub anchor: Option<String>,
}

/// What `History::clear` (PRD FR-HS-4) removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearRange {
    /// Empties the whole history.
    All,
    /// Every visit opened strictly before `cutoff` (unix seconds) — the
    /// retention-window prune's shape (`retention_prune`).
    Before(i64),
    /// Every visit opened at or after `cutoff` — `:history clear today`'s
    /// shape (cutoff = local midnight, `today_start_unix`).
    Since(i64),
    /// Every visit to one article — the picker's `d`.
    Article { lang: String, title: String },
}

pub struct History {
    conn: Connection,
    /// The full `(lang -> {title})` visited set, kept current by every
    /// mutating method — see the module doc comment's "the visited cache".
    visited: HashMap<String, HashSet<String>>,
}

impl History {
    /// Opens (creating if needed) the real on-disk history at the platform
    /// state directory (PRD §6.4: `$XDG_STATE_HOME/wikitui/history.sqlite`).
    /// Falls back to an in-memory store — history simply doesn't persist
    /// this run — when no platform directory can be determined at all;
    /// callers never see an error from this, matching `BookmarkStore::load`
    /// and friends' "a missing/unavailable store is not a reason to refuse
    /// to start the reader" convention.
    pub fn open() -> Self {
        match history_path() {
            Some(path) => Self::open_at(&path),
            None => Self::in_memory(),
        }
    }

    /// Opens the database at an explicit path, creating parent directories
    /// as needed. Tests use this against a tempdir file — NEVER the real
    /// state dir — so a schema migration or a crash-recovery scenario can
    /// be exercised against a throwaway file that survives a process
    /// restart (unlike `in_memory`).
    pub fn open_at(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Connection::open(path) {
            Ok(conn) => Self::from_connection(conn),
            Err(e) => {
                log_write_failure("open", &e);
                Self::in_memory()
            }
        }
    }

    /// A history with no on-disk persistence at all: the default for
    /// `App::new` (see its doc comment) and every test in this module. Also
    /// the natural home for a future incognito mode (PRD FR-PR-3) — though
    /// this chunk implements incognito as a gate on *recording*
    /// (`App.incognito`), not by swapping the whole store, since dwell/
    /// visited-cache bookkeeping still needs somewhere to live for the
    /// session even when nothing is meant to persist.
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory()
            .expect("an in-memory sqlite connection cannot fail to open");
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Self {
        if let Err(e) = migrate(&conn) {
            log_write_failure("migrate", &e);
        }
        let visited = load_visited(&conn);
        Self { conn, visited }
    }

    /// Records a visit to `(lang, title)`, returning the new row's id (for
    /// later `update_dwell`) or `None` if the write failed — a failure here
    /// is swallowed to a debug-log line (see the module doc comment), never
    /// a crash or a user-facing error, since history is non-critical.
    /// `referrer` is `(lang, title)` of the article the reader navigated
    /// from within the same tab, or `None` for a tab's first page.
    pub fn record_visit(
        &mut self,
        lang: &str,
        title: &str,
        referrer: Option<(&str, &str)>,
    ) -> Option<i64> {
        let now = now_unix();
        let (referrer_lang, referrer_title) = match referrer {
            Some((l, t)) => (Some(l), Some(t)),
            None => (None, None),
        };
        let result = self.conn.execute(
            "INSERT INTO visits (lang, title, opened_at, dwell_secs, referrer_lang, referrer_title)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![lang, title, now, referrer_lang, referrer_title],
        );
        match result {
            Ok(_) => {
                let id = self.conn.last_insert_rowid();
                self.visited
                    .entry(lang.to_string())
                    .or_default()
                    .insert(title.to_string());
                Some(id)
            }
            Err(e) => {
                log_write_failure("record_visit", &e);
                None
            }
        }
    }

    /// Adds `additional_secs` to a visit's accumulated dwell time (PRD
    /// FR-HS-1's "dwell time"). Best-effort, like every write here — a
    /// failure never reaches the reader.
    pub fn update_dwell(&mut self, id: i64, additional_secs: u64) {
        if additional_secs == 0 {
            return; // nothing to add — also avoids a write on every tab switch
        }
        if let Err(e) = self.conn.execute(
            "UPDATE visits SET dwell_secs = dwell_secs + ?1 WHERE id = ?2",
            params![additional_secs as i64, id],
        ) {
            log_write_failure("update_dwell", &e);
        }
    }

    /// PRD FR-NV-8: save (replacing any prior) the reading position for
    /// `(lang, title)`. Best-effort like the other passive writes here — a
    /// failure is swallowed to a log line, never surfaced. `folds` is stored as
    /// a sorted comma-separated block-index list.
    pub fn save_position(
        &mut self,
        lang: &str,
        title: &str,
        revid: u64,
        scroll: u16,
        folds: &[usize],
        anchor: Option<&str>,
    ) {
        let folds_csv = folds
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let now = now_unix();
        if let Err(e) = self.conn.execute(
            "INSERT INTO positions (lang, title, revid, scroll, folds, anchor, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(lang, title) DO UPDATE SET
                revid = ?3, scroll = ?4, folds = ?5, anchor = ?6, updated_at = ?7",
            params![
                lang,
                title,
                revid as i64,
                scroll as i64,
                folds_csv,
                anchor,
                now
            ],
        ) {
            log_write_failure("save_position", &e);
        }
    }

    /// PRD FR-NV-8: the saved reading position for `(lang, title)`, or `None`
    /// if none was ever stored (or the read failed).
    pub fn position(&self, lang: &str, title: &str) -> Option<SavedPosition> {
        let row = self.conn.query_row(
            "SELECT revid, scroll, folds, anchor FROM positions WHERE lang = ?1 AND title = ?2",
            params![lang, title],
            |row| {
                let revid: i64 = row.get(0)?;
                let scroll: i64 = row.get(1)?;
                let folds: String = row.get(2)?;
                let anchor: Option<String> = row.get(3)?;
                Ok((revid, scroll, folds, anchor))
            },
        );
        match row {
            Ok((revid, scroll, folds_csv, anchor)) => Some(SavedPosition {
                revid: revid.max(0) as u64,
                scroll: scroll.clamp(0, u16::MAX as i64) as u16,
                folds: folds_csv
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .filter_map(|s| s.parse().ok())
                    .collect(),
                anchor: anchor.filter(|s| !s.is_empty()),
            }),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                log_write_failure("position", &e);
                None
            }
        }
    }

    /// The most recent visit to each distinct `(lang, title)`, most-recent
    /// first (PRD FR-HS-1's picker default, and `search`'s candidate pool
    /// with an empty query). Dedup picks the highest `id` per article
    /// (i.e. the latest insert) rather than `MAX(opened_at)`, so two visits
    /// landing in the same wall-clock second never produce a spurious
    /// second row.
    pub fn recent(&self, limit: usize) -> Vec<Visit> {
        let limit = limit as i64;
        let query = self.conn.prepare(
            "SELECT v.id, v.lang, v.title, v.opened_at, v.dwell_secs, v.referrer_lang, v.referrer_title
             FROM visits v
             WHERE v.id IN (SELECT MAX(id) FROM visits GROUP BY lang, title)
             ORDER BY v.opened_at DESC, v.id DESC
             LIMIT ?1",
        );
        let mut stmt = match query {
            Ok(stmt) => stmt,
            Err(e) => {
                log_write_failure("recent", &e);
                return Vec::new();
            }
        };
        let rows = stmt.query_map(params![limit], row_to_visit);
        match rows {
            Ok(iter) => iter.filter_map(Result::ok).collect(),
            Err(e) => {
                log_write_failure("recent", &e);
                Vec::new()
            }
        }
    }

    /// Every visit row, oldest first — the raw, *non-deduplicated* log
    /// (PRD FR-PC-3 reading stats need total time and per-day streaks, which
    /// `recent`'s one-row-per-article dedup would collapse away). Best-effort
    /// like the other reads here: a query failure yields an empty list, never
    /// an error, so `wikitui stats` degrades to "nothing recorded" rather than
    /// refusing to run.
    pub fn all_visits(&self) -> Vec<Visit> {
        let query = self.conn.prepare(
            "SELECT id, lang, title, opened_at, dwell_secs, referrer_lang, referrer_title
             FROM visits ORDER BY opened_at ASC, id ASC",
        );
        let mut stmt = match query {
            Ok(stmt) => stmt,
            Err(e) => {
                log_write_failure("all_visits", &e);
                return Vec::new();
            }
        };
        let rows = stmt.query_map([], row_to_visit);
        match rows {
            Ok(iter) => iter.filter_map(Result::ok).collect(),
            Err(e) => {
                log_write_failure("all_visits", &e);
                Vec::new()
            }
        }
    }

    /// Fuzzy-ranked search over article titles (PRD FR-HS-1's Ctrl-h
    /// picker), recency-weighted. An empty (or whitespace-only) query is
    /// the same list `recent` returns — plain recency, nothing to rank.
    ///
    /// **Ranking.** Each candidate (the most-recent visit per article, same
    /// pool as `recent`) gets `score = fuzzy_score(title, query) * 10.0 +
    /// recency`, where `recency = 1.0 / (1.0 + age_days)` is in `(0, 1]`.
    /// Match quality is scaled by 10 — its own range is also `(0, 1]` — so a
    /// meaningfully better subsequence match always outranks a meaningfully
    /// worse one regardless of how old either visit is; recency only
    /// breaks ties (or near-ties) among comparably good matches. This is
    /// deliberate: the picker's job is "find the article whose title
    /// matches what I typed," not "show me what I read most recently that
    /// vaguely matches" — a strong old match beats a weak recent one (see
    /// this module's tests for the concrete case).
    pub fn search(&self, query: &str, limit: usize) -> Vec<Visit> {
        if query.trim().is_empty() {
            return self.recent(limit);
        }
        let now = now_unix();
        let mut scored: Vec<(f64, Visit)> = self
            .recent(SEARCH_CANDIDATE_POOL)
            .into_iter()
            .filter_map(|visit| {
                let quality = crate::fuzzy::fuzzy_score(&visit.title, query)?;
                let age_days = ((now - visit.opened_at).max(0) as f64) / 86_400.0;
                let recency = 1.0 / (1.0 + age_days);
                let score = quality * 10.0 + recency;
                Some((score, visit))
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(limit).map(|(_, v)| v).collect()
    }

    /// Whether `(lang, title)` has ever been visited (PRD FR-HS-2) — an
    /// in-memory hash lookup, not a query (see the module doc comment's
    /// "the visited cache"). `ui.rs`'s per-draw check goes through
    /// `visited_titles_for_lang` instead (one batch `extend` beats one
    /// lookup per link), so this single-title primitive isn't wired into
    /// any call site yet outside this module's own tests — kept as the
    /// documented alternative the brief asked for, the same "not consumed
    /// by anything in this phase" posture `cache.rs`'s `etag` field takes.
    #[allow(dead_code)]
    pub fn is_visited(&self, lang: &str, title: &str) -> bool {
        self.visited
            .get(lang)
            .is_some_and(|set| set.contains(title))
    }

    /// Every visited title in `lang`, for `ui.rs` to fold into its
    /// per-render visited set in one `extend` call instead of one lookup
    /// per link. `None` when nothing has ever been visited in that
    /// language (equivalent to an empty set).
    pub fn visited_titles_for_lang(&self, lang: &str) -> Option<&HashSet<String>> {
        self.visited.get(lang)
    }

    /// PRD FR-HS-4's `:history clear` / picker `d`: deletes the requested
    /// range and returns how many rows were removed, or the underlying
    /// error — unlike every other write in this module, a clear is a
    /// deliberate user action and its failure is worth surfacing (the
    /// caller reports it in the status/notice line).
    pub fn clear(&mut self, range: ClearRange) -> rusqlite::Result<usize> {
        let affected = match &range {
            ClearRange::All => self.conn.execute("DELETE FROM visits", [])?,
            ClearRange::Before(cutoff) => self
                .conn
                .execute("DELETE FROM visits WHERE opened_at < ?1", params![cutoff])?,
            ClearRange::Since(cutoff) => self
                .conn
                .execute("DELETE FROM visits WHERE opened_at >= ?1", params![cutoff])?,
            ClearRange::Article { lang, title } => self.conn.execute(
                "DELETE FROM visits WHERE lang = ?1 AND title = ?2",
                params![lang, title],
            )?,
        };
        self.visited = load_visited(&self.conn);
        Ok(affected)
    }

    /// Run once at startup (PRD FR-HS-4's retention window): drops visits
    /// older than `max_age_days`. `0` means "keep forever" — the documented
    /// default (`config::resolve`'s `[history] retention_days`) — and is a
    /// no-op here rather than "prune everything older than today," which a
    /// naive `cutoff = now - 0` would otherwise do. Best-effort like every
    /// non-user-initiated write in this module: a failure is swallowed
    /// (via `clear`'s own error, discarded here) rather than blocking
    /// startup.
    pub fn retention_prune(&mut self, max_age_days: u64) {
        if max_age_days == 0 {
            return;
        }
        let cutoff = now_unix() - (max_age_days as i64) * 86_400;
        if let Err(e) = self.clear(ClearRange::Before(cutoff)) {
            log_write_failure("retention_prune", &e);
        }
    }
}

/// The candidate pool `search` ranks over: large enough that, in any
/// realistic install, it's effectively "every article ever visited," while
/// still being a bounded query rather than an unbounded `SELECT *`.
const SEARCH_CANDIDATE_POOL: usize = 10_000;

fn row_to_visit(row: &rusqlite::Row) -> rusqlite::Result<Visit> {
    Ok(Visit {
        id: row.get(0)?,
        lang: row.get(1)?,
        title: row.get(2)?,
        opened_at: row.get(3)?,
        dwell_secs: row.get(4)?,
        referrer_lang: row.get(5)?,
        referrer_title: row.get(6)?,
    })
}

fn load_visited(conn: &Connection) -> HashMap<String, HashSet<String>> {
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    let result = (|| -> rusqlite::Result<()> {
        let mut stmt = conn.prepare("SELECT DISTINCT lang, title FROM visits")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows.flatten() {
            let (lang, title) = row;
            map.entry(lang).or_default().insert(title);
        }
        Ok(())
    })();
    if let Err(e) = result {
        log_write_failure("load_visited", &e);
    }
    map
}

/// Creates `visits` and its indexes if this is a fresh (or pre-history)
/// database, via the `user_version` pragma — mirroring `config::
/// CONFIG_VERSION`'s migration shape. A no-op once a database is already at
/// `SCHEMA_VERSION`, so opening the same file repeatedly (every launch) does
/// no redundant DDL.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(()); // already current — no redundant DDL on every launch
    }
    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS visits (
                id INTEGER PRIMARY KEY,
                lang TEXT NOT NULL,
                title TEXT NOT NULL,
                opened_at INTEGER NOT NULL,
                dwell_secs INTEGER NOT NULL DEFAULT 0,
                referrer_lang TEXT,
                referrer_title TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_visits_lang_title ON visits(lang, title);
            CREATE INDEX IF NOT EXISTS idx_visits_opened_at ON visits(opened_at);
            PRAGMA user_version = 1;",
        )?;
    }
    if version < 2 {
        // PRD FR-NV-8 reading-position memory: one row per article, replaced on
        // each save. `revid` decides exact-vs-anchor restore; `folds` is the
        // sorted folded-heading-block set as a comma-separated list; `anchor`
        // is the section heading the saved scroll sat in (the revid-mismatch
        // fallback target). Sharing history.sqlite means `clear-data --history`
        // (a whole-file delete) already covers positions.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS positions (
                lang TEXT NOT NULL,
                title TEXT NOT NULL,
                revid INTEGER NOT NULL DEFAULT 0,
                scroll INTEGER NOT NULL DEFAULT 0,
                folds TEXT NOT NULL DEFAULT '',
                anchor TEXT,
                updated_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (lang, title)
            );
            PRAGMA user_version = 2;",
        )?;
    }
    Ok(())
}

/// The real on-disk location (PRD §6.4): `$XDG_STATE_HOME/wikitui/
/// history.sqlite`. `ProjectDirs::state_dir()` is `Some` on Linux/BSD
/// (honoring `$XDG_STATE_HOME`) but `None` on macOS/Windows, where the
/// `directories` crate has no state-dir concept distinct from the data
/// dir — falls back to `<data_dir>/state` there, a documented, harmless
/// subdirectory rather than mixing history.sqlite in with bookmarks.jsonl
/// et al.
pub(crate) fn history_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("history.sqlite"))
}

/// A history write's own failure is never a crash or a user-facing error
/// (see the module doc comment) — this is the one place that fact is
/// visible at all, a debug-log line to stderr for anyone with a terminal
/// attached, mirroring how `main.rs` reports non-fatal config issues.
fn log_write_failure(op: &str, err: &rusqlite::Error) {
    eprintln!("wikitui: history: {op} failed: {err}");
}

/// The single clock access point for this module (mirrors `research::today`
/// and `bookmarks::now_ts`): every timestamp `history` writes or compares
/// against goes through this, so nothing here ever reads `SystemTime`/
/// `chrono::Utc` ad hoc. `pub(crate)` so `ui.rs`'s picker — which needs "now"
/// to turn a `Visit`'s `opened_at` into a relative-time label — reuses this
/// instead of a second, independent clock read.
pub(crate) fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Local midnight (start of today), as a unix timestamp — `:history clear
/// today`'s cutoff (PRD FR-HS-4). Local, not UTC, for the same reason
/// `research::today` uses the researcher's own calendar day: "today" means
/// the reader's morning-to-now, not whatever day it happens to be in
/// Greenwich. Falls back to the current instant on the (very rare) DST
/// transition where local midnight is ambiguous or doesn't exist, rather
/// than failing — "clear today" still clears *something* sensible.
pub fn today_start_unix() -> i64 {
    let now = chrono::Local::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.and_local_timezone(chrono::Local).single())
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| now.timestamp())
}

/// A human "N ago" relative-time label for the history picker (PRD
/// FR-HS-1), e.g. "2h ago". Pure function of an age in seconds, so it's
/// testable without a clock — mirrors `cache::age_human`'s bucketing
/// (seconds/minutes/hours/days) with the "ago" suffix a picker wants that a
/// cache-age status prefix doesn't.
pub fn relative_time(age_secs: i64) -> String {
    let age_secs = age_secs.max(0) as u64;
    match age_secs {
        0..=9 => "just now".to_string(),
        10..=59 => format!("{age_secs}s ago"),
        60..=3599 => format!("{}m ago", age_secs / 60),
        3600..=86_399 => format!("{}h ago", age_secs / 3600),
        _ => format!("{}d ago", age_secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique tempdir-file path per test (tests run in parallel) — NEVER
    /// the real state dir. Cleaned up at the end of each test that uses one.
    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-test-history-{}-{n}.sqlite",
            std::process::id()
        ))
    }

    // ---- Schema / migration ------------------------------------------------

    #[test]
    fn a_fresh_database_migrates_to_the_current_schema_version() {
        let history = History::in_memory();
        let version: i64 = history
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    // ---- PRD FR-NV-8 reading-position memory ------------------------------

    #[test]
    fn save_and_read_a_reading_position_round_trips() {
        let mut history = History::in_memory();
        assert!(history.position("en", "Alan Turing").is_none());
        history.save_position("en", "Alan Turing", 42, 17, &[3, 8], Some("Legacy"));
        let pos = history.position("en", "Alan Turing").expect("saved");
        assert_eq!(pos.revid, 42);
        assert_eq!(pos.scroll, 17);
        assert_eq!(pos.folds, vec![3, 8]);
        assert_eq!(pos.anchor.as_deref(), Some("Legacy"));
    }

    #[test]
    fn saving_a_position_replaces_the_prior_one_for_that_article() {
        let mut history = History::in_memory();
        history.save_position("en", "Alan Turing", 1, 5, &[], None);
        history.save_position("en", "Alan Turing", 2, 30, &[], Some("History"));
        let pos = history.position("en", "Alan Turing").unwrap();
        assert_eq!(pos.revid, 2);
        assert_eq!(pos.scroll, 30);
        assert_eq!(pos.anchor.as_deref(), Some("History"));
    }

    #[test]
    fn a_saved_position_survives_a_reopen_of_the_same_file() {
        let path = temp_path();
        {
            let mut history = History::open_at(&path);
            history.save_position("en", "Alan Turing", 7, 12, &[1], Some("Career"));
        }
        let reopened = History::open_at(&path);
        let pos = reopened.position("en", "Alan Turing").expect("persisted");
        assert_eq!(pos.scroll, 12);
        assert_eq!(pos.folds, vec![1]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opening_the_same_file_twice_is_idempotent() {
        let path = temp_path();
        {
            let mut history = History::open_at(&path);
            history.record_visit("en", "Alan Turing", None);
        }
        // Re-opening must not fail, wipe, or duplicate the schema — the
        // migration's `IF NOT EXISTS`/version-gate must make this a no-op.
        let mut reopened = History::open_at(&path);
        assert_eq!(reopened.recent(10).len(), 1);
        reopened.record_visit("en", "Enigma machine", None);
        assert_eq!(History::open_at(&path).recent(10).len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn in_memory_history_starts_empty_and_never_touches_disk() {
        let history = History::in_memory();
        assert!(history.recent(10).is_empty());
        assert!(!history.is_visited("en", "Alan Turing"));
    }

    // ---- record + recent (dedup) -------------------------------------------

    #[test]
    fn record_visit_returns_an_id_and_appears_in_recent() {
        let mut history = History::in_memory();
        let id = history
            .record_visit("en", "Alan Turing", None)
            .expect("write must succeed against an in-memory db");
        assert!(id > 0);
        let recent = history.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].lang, "en");
        assert_eq!(recent[0].title, "Alan Turing");
        assert_eq!(recent[0].referrer_title, None);
    }

    #[test]
    fn record_visit_stores_the_referrer() {
        let mut history = History::in_memory();
        history.record_visit("en", "Alan Turing", None);
        history.record_visit("en", "Enigma machine", Some(("en", "Alan Turing")));
        let recent = history.recent(10);
        let enigma = recent.iter().find(|v| v.title == "Enigma machine").unwrap();
        assert_eq!(enigma.referrer_lang.as_deref(), Some("en"));
        assert_eq!(enigma.referrer_title.as_deref(), Some("Alan Turing"));
    }

    #[test]
    fn recent_dedups_to_the_most_recent_visit_per_article() {
        let mut history = History::in_memory();
        history.record_visit("en", "Alan Turing", None);
        history.record_visit("en", "Enigma machine", None);
        history.record_visit("en", "Alan Turing", None); // revisit

        let recent = history.recent(10);
        let titles: Vec<&str> = recent.iter().map(|v| v.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Alan Turing", "Enigma machine"],
            "one row per article, most-recently-visited article first"
        );
    }

    #[test]
    fn recent_orders_most_recent_first_and_respects_the_limit() {
        let mut history = History::in_memory();
        for title in ["A", "B", "C"] {
            history.record_visit("en", title, None);
        }
        let all = history.recent(10);
        let titles: Vec<&str> = all.iter().map(|v| v.title.as_str()).collect();
        assert_eq!(titles, vec!["C", "B", "A"]);

        let limited = history.recent(2);
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].title, "C");
        assert_eq!(limited[1].title, "B");
    }

    // ---- dwell --------------------------------------------------------------

    #[test]
    fn update_dwell_accumulates_across_multiple_calls() {
        let mut history = History::in_memory();
        let id = history.record_visit("en", "Alan Turing", None).unwrap();
        history.update_dwell(id, 30);
        history.update_dwell(id, 15);
        let visit = history.recent(10).into_iter().find(|v| v.id == id).unwrap();
        assert_eq!(visit.dwell_secs, 45);
    }

    #[test]
    fn update_dwell_of_zero_is_a_harmless_no_op() {
        let mut history = History::in_memory();
        let id = history.record_visit("en", "Alan Turing", None).unwrap();
        history.update_dwell(id, 0);
        let visit = history.recent(10).into_iter().next().unwrap();
        assert_eq!(visit.dwell_secs, 0);
    }

    // ---- fuzzy search ranking -------------------------------------------------

    #[test]
    fn empty_query_search_matches_recent() {
        let mut history = History::in_memory();
        history.record_visit("en", "Alan Turing", None);
        history.record_visit("en", "Enigma machine", None);
        assert_eq!(history.search("", 10), history.recent(10));
        assert_eq!(history.search("   ", 10), history.recent(10));
    }

    #[test]
    fn search_excludes_non_matching_titles() {
        let mut history = History::in_memory();
        history.record_visit("en", "Alan Turing", None);
        history.record_visit("en", "Enigma machine", None);
        let hits = history.search("turing", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Alan Turing");
    }

    /// The documented tie-break (see `History::search`'s doc comment): a
    /// strong match on an old visit outranks a weak (heavily scattered)
    /// match on a much more recent one. Query "abc" — a contiguous
    /// substring of "abc project" (quality 1.0) vs. a scattered
    /// subsequence of a title with the same three letters spread far apart
    /// (quality close to 0) opened moments ago.
    #[test]
    fn search_ranks_a_strong_old_match_above_a_weak_recent_one() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("en", "abc project", None).unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 400 * 86_400, old_id],
            )
            .unwrap();
        // "a" then 30 filler chars then "b" then 30 filler chars then "c":
        // a very weak, heavily-scattered match on "abc", visited just now.
        let weak_title = format!("a{}b{}c", "x".repeat(30), "x".repeat(30));
        history.record_visit("en", &weak_title, None);

        let ranked = history.search("abc", 10);
        assert_eq!(
            ranked[0].title, "abc project",
            "a strong match 400 days old must still outrank a weak match from just now"
        );
        assert_eq!(ranked[1].title, weak_title);
    }

    #[test]
    fn search_prefers_more_recent_among_equal_quality_matches() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("en", "Turing Award", None).unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 100 * 86_400, old_id],
            )
            .unwrap();
        history.record_visit("en", "Turing Machine", None); // just now, same quality

        let ranked = history.search("turing", 10);
        assert_eq!(
            ranked[0].title, "Turing Machine",
            "among comparably strong matches, the more recent one wins"
        );
    }

    // ---- visited cache --------------------------------------------------------

    #[test]
    fn record_visit_makes_is_visited_true_immediately() {
        let mut history = History::in_memory();
        assert!(!history.is_visited("en", "Alan Turing"));
        history.record_visit("en", "Alan Turing", None);
        assert!(history.is_visited("en", "Alan Turing"));
        assert!(
            !history.is_visited("de", "Alan Turing"),
            "scoped per language"
        );
    }

    #[test]
    fn visited_set_survives_reopening_the_same_file() {
        let path = temp_path();
        {
            let mut history = History::open_at(&path);
            history.record_visit("en", "Alan Turing", None);
        }
        let reopened = History::open_at(&path);
        assert!(reopened.is_visited("en", "Alan Turing"));
        assert!(
            reopened
                .visited_titles_for_lang("en")
                .is_some_and(|set| set.contains("Alan Turing"))
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---- clear ----------------------------------------------------------------

    #[test]
    fn clear_all_empties_history_and_the_visited_cache() {
        let mut history = History::in_memory();
        history.record_visit("en", "Alan Turing", None);
        history.record_visit("en", "Enigma machine", None);
        let removed = history.clear(ClearRange::All).unwrap();
        assert_eq!(removed, 2);
        assert!(history.recent(10).is_empty());
        assert!(!history.is_visited("en", "Alan Turing"));
    }

    #[test]
    fn clear_before_a_cutoff_keeps_newer_rows() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("en", "Old Article", None).unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 10 * 86_400, old_id],
            )
            .unwrap();
        history.record_visit("en", "New Article", None);

        let cutoff = now_unix() - 5 * 86_400;
        let removed = history.clear(ClearRange::Before(cutoff)).unwrap();
        assert_eq!(removed, 1);
        let recent = history.recent(10);
        let remaining: Vec<&str> = recent.iter().map(|v| v.title.as_str()).collect();
        assert_eq!(remaining, vec!["New Article"]);
        assert!(!history.is_visited("en", "Old Article"));
        assert!(history.is_visited("en", "New Article"));
    }

    #[test]
    fn clear_one_article_leaves_the_rest() {
        let mut history = History::in_memory();
        history.record_visit("en", "Alan Turing", None);
        history.record_visit("en", "Enigma machine", None);
        let removed = history
            .clear(ClearRange::Article {
                lang: "en".to_string(),
                title: "Alan Turing".to_string(),
            })
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!history.is_visited("en", "Alan Turing"));
        assert!(history.is_visited("en", "Enigma machine"));
    }

    // ---- retention prune --------------------------------------------------------

    #[test]
    fn retention_prune_of_zero_days_keeps_forever() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("en", "Ancient", None).unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 10_000 * 86_400, old_id],
            )
            .unwrap();
        history.retention_prune(0);
        assert_eq!(history.recent(10).len(), 1, "0 days means keep forever");
    }

    #[test]
    fn retention_prune_drops_old_rows_and_keeps_new_ones() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("en", "Old Article", None).unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 90 * 86_400, old_id],
            )
            .unwrap();
        history.record_visit("en", "New Article", None);

        history.retention_prune(30);
        let recent = history.recent(10);
        let remaining: Vec<&str> = recent.iter().map(|v| v.title.as_str()).collect();
        assert_eq!(remaining, vec!["New Article"]);
    }

    // ---- relative_time (pure fn) ------------------------------------------------

    #[test]
    fn relative_time_buckets_sensibly() {
        assert_eq!(relative_time(0), "just now");
        assert_eq!(relative_time(9), "just now");
        assert_eq!(relative_time(10), "10s ago");
        assert_eq!(relative_time(59), "59s ago");
        assert_eq!(relative_time(60), "1m ago");
        assert_eq!(relative_time(3 * 3600 + 100), "3h ago");
        assert_eq!(relative_time(2 * 86_400 + 5), "2d ago");
    }

    #[test]
    fn relative_time_never_panics_on_a_negative_age() {
        // A defensive clamp: a clock adjustment or test fixture could hand
        // this a negative delta; it must degrade to "just now", not panic.
        assert_eq!(relative_time(-5), "just now");
    }

    // ---- today_start_unix -------------------------------------------------------

    #[test]
    fn today_start_is_at_or_before_now_and_within_one_day() {
        let start = today_start_unix();
        let now = now_unix();
        assert!(start <= now, "midnight must not be in the future");
        assert!(now - start < 86_400, "must be today's midnight, not older");
    }
}
