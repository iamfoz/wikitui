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
//! visits(id, wiki, lang, title, opened_at, dwell_secs, referrer_wiki, referrer_lang, referrer_title)
//! ```
//!
//! `referrer_*` is the article the reader navigated *from* within the same
//! tab, `NULL` for the first page a tab ever shows. Migrated via the
//! `user_version` pragma (`migrate`) rather than a sentinel row, so a schema
//! change never has to reserve a row of its own or guess from `PRAGMA
//! table_info`.
//!
//! `wiki`/`referrer_wiki` (schema v4, PRD FR-ML-4/FR-HS-3) are the same
//! `api::wiki_scope` string every other wiki-scoped store keys on (`""` =
//! default Wikipedia) — added after `positions` already gained the same
//! dimension in v3, so a visit recorded before this migration backfills as
//! the default wiki, never a lost/guessed scope. The trail view (`trail.rs`)
//! is what actually needed this: a wander graph's nodes must carry `wiki` to
//! keep a same-titled article on two wikis as two distinct nodes, and its
//! edges are exactly this table's `referrer_*` columns.
//!
//! ## The visited cache
//!
//! `ui.rs` needs to know, for every visible link on every draw, whether its
//! target has ever been visited (PRD FR-HS-2's persistent visited-link
//! styling) — one `SELECT` per link per frame would turn scrolling a
//! link-heavy article into a disk-bound operation. Instead `History` keeps
//! the full `(wiki, lang, title)` visited set in memory (`visited`), loaded
//! once at open time and kept current by `record_visit`/`clear`, so
//! `is_visited`/`visited_titles_for_lang` are plain hash lookups. Keyed on
//! `(wiki, lang)` rather than `lang` alone (PRD FR-ML-4): a same-titled
//! article read on one wiki must not paint as visited on a different wiki
//! that happens to share the title and language, the same cross-wiki
//! isolation `recent`'s dedup and `positions` already give the rest of
//! history.
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
const SCHEMA_VERSION: i64 = 4;

/// One logged page view (PRD FR-HS-1's "title, wiki, timestamp, dwell time,
/// referrer article").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub id: i64,
    /// PRD FR-ML-4/FR-HS-3: the `api::wiki_scope` this visit was read on
    /// (`""` = default Wikipedia), same convention as `tab::Tab::wiki` — a
    /// visit recorded before schema v4 backfills as `""`.
    pub wiki: String,
    pub lang: String,
    pub title: String,
    /// Unix seconds — this module's own clock access point (`now_unix`) is
    /// the only place that reads the wall clock, so comparisons here are
    /// never against a second, independently-read "now".
    pub opened_at: i64,
    pub dwell_secs: i64,
    /// The referrer's wiki scope, `None` alongside `referrer_lang`/
    /// `referrer_title` for a tab's first page. A visit recorded before
    /// schema v4 has a referrer with no recorded wiki (`referrer_lang`/
    /// `referrer_title` are `Some`, `referrer_wiki` is `None`) — treated as
    /// the default wiki by every reader of this field (`trail.rs`'s
    /// `unwrap_or_default`), rather than a distinguishable "unknown wiki".
    pub referrer_wiki: Option<String>,
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
    /// Every visit to one article — the picker's `d`. Scoped by `wiki`
    /// (PRD FR-ML-4, `api::wiki_scope`; `""` = default Wikipedia) as well as
    /// `lang`/`title`, so deleting one wiki's row for a title never also
    /// deletes a same-titled article read on a different wiki.
    Article {
        wiki: String,
        lang: String,
        title: String,
    },
}

pub struct History {
    conn: Connection,
    /// The full `(wiki -> lang -> {title})` visited set, kept current by
    /// every mutating method — see the module doc comment's "the visited
    /// cache". Nested (rather than a `(wiki, lang)` tuple key) so the
    /// per-draw `visited_titles_for_lang` lookup stays two plain `&str`
    /// hash lookups with no allocation, the same cost `lang`-only lookups
    /// had before wiki scoping.
    visited: HashMap<String, HashMap<String, HashSet<String>>>,
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

    /// Records a visit to `(wiki, lang, title)`, returning the new row's id
    /// (for later `update_dwell`) or `None` if the write failed — a failure
    /// here is swallowed to a debug-log line (see the module doc comment),
    /// never a crash or a user-facing error, since history is non-critical.
    /// `referrer` is `(wiki, lang, title)` of the article the reader
    /// navigated from within the same tab, or `None` for a tab's first page.
    pub fn record_visit(
        &mut self,
        wiki: &str,
        lang: &str,
        title: &str,
        referrer: Option<(&str, &str, &str)>,
    ) -> Option<i64> {
        let now = now_unix();
        let (referrer_wiki, referrer_lang, referrer_title) = match referrer {
            Some((w, l, t)) => (Some(w), Some(l), Some(t)),
            None => (None, None, None),
        };
        let result = self.conn.execute(
            "INSERT INTO visits (wiki, lang, title, opened_at, dwell_secs, referrer_wiki, referrer_lang, referrer_title)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
            params![wiki, lang, title, now, referrer_wiki, referrer_lang, referrer_title],
        );
        match result {
            Ok(_) => {
                let id = self.conn.last_insert_rowid();
                self.visited
                    .entry(wiki.to_string())
                    .or_default()
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
    /// `(wiki, lang, title)`. Best-effort like the other passive writes here —
    /// a failure is swallowed to a log line, never surfaced. `folds` is stored
    /// as a sorted comma-separated block-index list.
    // The wiki scope (PRD FR-ML-4) puts this one dimension past clippy's arg
    // ceiling; the fields are a flat position record, not a struct worth
    // naming.
    #[allow(clippy::too_many_arguments)]
    pub fn save_position(
        &mut self,
        wiki: &str,
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
            "INSERT INTO positions (wiki, lang, title, revid, scroll, folds, anchor, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(wiki, lang, title) DO UPDATE SET
                revid = ?4, scroll = ?5, folds = ?6, anchor = ?7, updated_at = ?8",
            params![
                wiki,
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

    /// PRD FR-NV-8: the saved reading position for `(wiki, lang, title)`, or
    /// `None` if none was ever stored (or the read failed). The wiki scope
    /// (PRD FR-ML-4) keeps a resume from ever restoring another wiki's
    /// position onto a same-titled article.
    pub fn position(&self, wiki: &str, lang: &str, title: &str) -> Option<SavedPosition> {
        let row = self.conn.query_row(
            "SELECT revid, scroll, folds, anchor FROM positions \
             WHERE wiki = ?1 AND lang = ?2 AND title = ?3",
            params![wiki, lang, title],
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

    /// The most recent visit to each distinct `(wiki, lang, title)`,
    /// most-recent first (PRD FR-HS-1's picker default, and `search`'s
    /// candidate pool with an empty query). Dedup picks the highest `id` per
    /// article (i.e. the latest insert) rather than `MAX(opened_at)`, so two
    /// visits landing in the same wall-clock second never produce a spurious
    /// second row. Grouping includes `wiki` (PRD FR-ML-4, schema v4) so a
    /// same-titled article read on two different wikis stays two rows, not
    /// one collapsing the other's dwell/referrer away.
    pub fn recent(&self, limit: usize) -> Vec<Visit> {
        let limit = limit as i64;
        let query = self.conn.prepare(
            "SELECT v.id, v.wiki, v.lang, v.title, v.opened_at, v.dwell_secs, v.referrer_wiki, v.referrer_lang, v.referrer_title
             FROM visits v
             WHERE v.id IN (SELECT MAX(id) FROM visits GROUP BY wiki, lang, title)
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
            "SELECT id, wiki, lang, title, opened_at, dwell_secs, referrer_wiki, referrer_lang, referrer_title
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

    /// Whether `(wiki, lang, title)` has ever been visited (PRD FR-HS-2) — an
    /// in-memory hash lookup, not a query (see the module doc comment's
    /// "the visited cache"). `ui.rs`'s per-draw check goes through
    /// `visited_titles_for_lang` instead (one batch `extend` beats one
    /// lookup per link), so this single-title primitive isn't wired into
    /// any call site yet outside this module's own tests — kept as the
    /// documented alternative the brief asked for, the same "not consumed
    /// by anything in this phase" posture `cache.rs`'s `etag` field takes.
    #[allow(dead_code)]
    pub fn is_visited(&self, wiki: &str, lang: &str, title: &str) -> bool {
        self.visited
            .get(wiki)
            .and_then(|by_lang| by_lang.get(lang))
            .is_some_and(|set| set.contains(title))
    }

    /// Every visited title in `(wiki, lang)`, for `ui.rs` to fold into its
    /// per-render visited set in one `extend` call instead of one lookup
    /// per link. `None` when nothing has ever been visited in that wiki's
    /// edition of that language (equivalent to an empty set) — PRD FR-ML-4:
    /// a title visited on a *different* wiki must not surface here, or a
    /// same-titled article there would incorrectly paint as visited too.
    pub fn visited_titles_for_lang(&self, wiki: &str, lang: &str) -> Option<&HashSet<String>> {
        self.visited.get(wiki).and_then(|by_lang| by_lang.get(lang))
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
            ClearRange::Article { wiki, lang, title } => self.conn.execute(
                "DELETE FROM visits WHERE wiki = ?1 AND lang = ?2 AND title = ?3",
                params![wiki, lang, title],
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
        // CORR-L5: `max_age_days` is user config (`config::resolve`'s
        // validation only rejects negative TOML values, not absurdly large
        // ones), so an unclamped `as i64` cast or `* 86_400` multiply can
        // wrap negative or overflow-panic in a debug build. A negative
        // wrapped value here would turn into a far-future cutoff that
        // deletes *all* history instead of nothing. `try_from` + saturating
        // arithmetic means the worst an absurd value can do is saturate the
        // cutoff to `i64::MIN` — i.e. prune nothing, the same as too-small a
        // retention window pruning everything is never silently inverted.
        let days = i64::try_from(max_age_days).unwrap_or(i64::MAX);
        let age_secs = days.saturating_mul(86_400);
        let cutoff = now_unix().saturating_sub(age_secs);
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
        wiki: row.get(1)?,
        lang: row.get(2)?,
        title: row.get(3)?,
        opened_at: row.get(4)?,
        dwell_secs: row.get(5)?,
        referrer_wiki: row.get(6)?,
        referrer_lang: row.get(7)?,
        referrer_title: row.get(8)?,
    })
}

fn load_visited(conn: &Connection) -> HashMap<String, HashMap<String, HashSet<String>>> {
    let mut map: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
    let result = (|| -> rusqlite::Result<()> {
        let mut stmt = conn.prepare("SELECT DISTINCT wiki, lang, title FROM visits")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows.flatten() {
            let (wiki, lang, title) = row;
            map.entry(wiki)
                .or_default()
                .entry(lang)
                .or_default()
                .insert(title);
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
    // Each step runs its DDL *and* its `user_version` bump inside ONE
    // transaction (CORR-M8). SQLite's DDL and the `user_version` header write
    // are both transactional, so a crash mid-migration rolls the step back
    // wholesale — the version never advances past a step that didn't fully
    // apply, and the step simply re-runs from a clean state next launch. This
    // closes the old failure mode where each `execute_batch` auto-committed
    // per statement with the version bump last: a crash after the DDL but
    // before the bump left the schema half-changed *and* eligible to re-run
    // the DDL, wedging on e.g. a duplicate-column error forever. The `IF NOT
    // EXISTS` guards below (and the explicit `column_exists` check in v4, which
    // `ADD COLUMN` has no `IF NOT EXISTS` form for) additionally make each step
    // idempotent, so even a database left half-migrated by the *old* code
    // recovers rather than wedges.
    if version < 1 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
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
        tx.commit()?;
    }
    if version < 2 {
        // PRD FR-NV-8 reading-position memory: one row per article, replaced on
        // each save. `revid` decides exact-vs-anchor restore; `folds` is the
        // sorted folded-heading-block set as a comma-separated list; `anchor`
        // is the section heading the saved scroll sat in (the revid-mismatch
        // fallback target). Sharing history.sqlite means `clear-data --history`
        // (a whole-file delete) already covers positions.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
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
        tx.commit()?;
    }
    if version < 3 {
        // PRD FR-ML-4: reading-position memory gains a wiki dimension so a
        // resume never crosses wikis (a same-titled article on another wiki
        // is a different page). SQLite can't add a column to a composite
        // PRIMARY KEY in place, so the table is rebuilt: every existing row
        // is the default Wikipedia scope (the only wiki before this), copied
        // over with `wiki = ''` — no position is lost. `IF NOT EXISTS`/the
        // idempotent copy make a fresh database (which just created the v2
        // shape above) migrate forward cleanly too, and the transaction makes
        // the rebuild all-or-nothing so a crash never strands `positions_v3`.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS positions_v3 (
                wiki TEXT NOT NULL DEFAULT '',
                lang TEXT NOT NULL,
                title TEXT NOT NULL,
                revid INTEGER NOT NULL DEFAULT 0,
                scroll INTEGER NOT NULL DEFAULT 0,
                folds TEXT NOT NULL DEFAULT '',
                anchor TEXT,
                updated_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (wiki, lang, title)
            );
            INSERT INTO positions_v3 (wiki, lang, title, revid, scroll, folds, anchor, updated_at)
                SELECT '', lang, title, revid, scroll, folds, anchor, updated_at FROM positions;
            DROP TABLE positions;
            ALTER TABLE positions_v3 RENAME TO positions;
            PRAGMA user_version = 3;",
        )?;
        tx.commit()?;
    }
    if version < 4 {
        // PRD FR-ML-4/FR-HS-3: `visits` gains the same wiki dimension
        // `positions` got in v3 — plain `ADD COLUMN`s suffice here (unlike
        // v3's rebuild) since `visits`' primary key is just `id`, no
        // composite key to widen. Every existing row backfills as the
        // default wiki (`''`) for `wiki` and `NULL` (no recorded wiki) for
        // `referrer_wiki` — a pre-v4 visit's referrer is still known by
        // lang/title, just not by wiki, so it isn't lost, only less precise
        // (`trail.rs` treats a `None` referrer_wiki as the default wiki).
        // `ADD COLUMN` has no `IF NOT EXISTS`, so each is guarded by an
        // explicit column check: a database whose columns were added by an
        // interrupted pre-transaction migration (but whose version never
        // reached 4) re-runs cleanly instead of erroring on a duplicate column.
        let tx = conn.unchecked_transaction()?;
        if !column_exists(&tx, "visits", "wiki")? {
            tx.execute_batch("ALTER TABLE visits ADD COLUMN wiki TEXT NOT NULL DEFAULT '';")?;
        }
        if !column_exists(&tx, "visits", "referrer_wiki")? {
            tx.execute_batch("ALTER TABLE visits ADD COLUMN referrer_wiki TEXT;")?;
        }
        tx.execute_batch("PRAGMA user_version = 4;")?;
        tx.commit()?;
    }
    Ok(())
}

/// Whether `table` already has a column named `column` — the idempotency guard
/// for `ADD COLUMN` migration steps, which SQLite offers no `IF NOT EXISTS`
/// form for. `table` is always a compile-time-constant identifier here (never
/// reader input), so interpolating it into the `PRAGMA` is safe. A missing
/// table simply yields no rows (hence `false`), never an error.
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The real on-disk location (PRD §6.4): `$XDG_STATE_HOME/wikitui/
/// history.sqlite` — a harmless subdirectory rather than mixing
/// history.sqlite in with bookmarks.jsonl et al. See `paths::
/// wikitui_state_dir` for the platform resolution itself.
pub(crate) fn history_path() -> Option<PathBuf> {
    Some(crate::paths::wikitui_state_dir()?.join("history.sqlite"))
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
        assert!(history.position("", "en", "Alan Turing").is_none());
        history.save_position("", "en", "Alan Turing", 42, 17, &[3, 8], Some("Legacy"));
        let pos = history.position("", "en", "Alan Turing").expect("saved");
        assert_eq!(pos.revid, 42);
        assert_eq!(pos.scroll, 17);
        assert_eq!(pos.folds, vec![3, 8]);
        assert_eq!(pos.anchor.as_deref(), Some("Legacy"));
    }

    #[test]
    fn saving_a_position_replaces_the_prior_one_for_that_article() {
        let mut history = History::in_memory();
        history.save_position("", "en", "Alan Turing", 1, 5, &[], None);
        history.save_position("", "en", "Alan Turing", 2, 30, &[], Some("History"));
        let pos = history.position("", "en", "Alan Turing").unwrap();
        assert_eq!(pos.revid, 2);
        assert_eq!(pos.scroll, 30);
        assert_eq!(pos.anchor.as_deref(), Some("History"));
    }

    #[test]
    fn a_saved_position_survives_a_reopen_of_the_same_file() {
        let path = temp_path();
        {
            let mut history = History::open_at(&path);
            history.save_position("", "en", "Alan Turing", 7, 12, &[1], Some("Career"));
        }
        let reopened = History::open_at(&path);
        let pos = reopened
            .position("", "en", "Alan Turing")
            .expect("persisted");
        assert_eq!(pos.scroll, 12);
        assert_eq!(pos.folds, vec![1]);
        let _ = std::fs::remove_file(&path);
    }

    /// PRD FR-ML-4: a saved position is scoped to its wiki, so the *same*
    /// `(lang, title)` on two different wikis keeps two independent positions
    /// — a resume never restores one wiki's scroll/fold onto another wiki's
    /// same-titled article.
    #[test]
    fn positions_are_isolated_per_wiki() {
        let mut history = History::in_memory();
        history.save_position("", "en", "Mercury", 1, 10, &[], Some("Planet"));
        history.save_position(
            "wiktionary",
            "en",
            "Mercury",
            2,
            90,
            &[3],
            Some("Etymology"),
        );

        let default = history.position("", "en", "Mercury").expect("default wiki");
        assert_eq!(default.scroll, 10);
        assert_eq!(default.anchor.as_deref(), Some("Planet"));

        let sister = history
            .position("wiktionary", "en", "Mercury")
            .expect("sister wiki");
        assert_eq!(sister.scroll, 90);
        assert_eq!(sister.folds, vec![3]);

        assert!(
            history.position("wikivoyage", "en", "Mercury").is_none(),
            "a third wiki has its own (empty) position, never another wiki's"
        );
    }

    /// Migration proof (PRD FR-ML-4): a v2 database (positions with no wiki
    /// column) migrates to v3 with every existing row preserved under the
    /// empty/default-Wikipedia scope — no position is lost when wiki scoping
    /// is introduced.
    #[test]
    fn a_v2_position_migrates_forward_as_the_default_wiki() {
        let path = temp_path();
        {
            // Hand-build the exact v2 schema and seed one row, bypassing the
            // v3-aware `save_position`.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE positions (
                    lang TEXT NOT NULL,
                    title TEXT NOT NULL,
                    revid INTEGER NOT NULL DEFAULT 0,
                    scroll INTEGER NOT NULL DEFAULT 0,
                    folds TEXT NOT NULL DEFAULT '',
                    anchor TEXT,
                    updated_at INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (lang, title)
                );
                INSERT INTO positions (lang, title, revid, scroll, folds, anchor, updated_at)
                    VALUES ('en', 'Alan Turing', 7, 33, '1,4', 'Career', 100);
                PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        // Opening runs the v2 -> v3 migration.
        let history = History::open_at(&path);
        let pos = history
            .position("", "en", "Alan Turing")
            .expect("the pre-wiki row must survive as the default wiki");
        assert_eq!(pos.scroll, 33);
        assert_eq!(pos.folds, vec![1, 4]);
        assert_eq!(pos.anchor.as_deref(), Some("Career"));
        let _ = std::fs::remove_file(&path);
    }

    /// Migration proof (PRD FR-ML-4/FR-HS-3): a v3 database (`visits` with no
    /// wiki columns at all) migrates to v4 with every existing row preserved
    /// under the default wiki — no visit is lost when the trail view's wiki
    /// dimension is introduced, and a referrer recorded before v4 comes back
    /// with a wiki-less (`None`) referrer rather than a guessed one.
    #[test]
    fn a_v3_visit_migrates_forward_as_the_default_wiki() {
        let path = temp_path();
        {
            // Hand-build the exact pre-v4 `visits` shape and seed two rows
            // (one with a referrer), bypassing the v4-aware `record_visit`.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE visits (
                    id INTEGER PRIMARY KEY,
                    lang TEXT NOT NULL,
                    title TEXT NOT NULL,
                    opened_at INTEGER NOT NULL,
                    dwell_secs INTEGER NOT NULL DEFAULT 0,
                    referrer_lang TEXT,
                    referrer_title TEXT
                );
                INSERT INTO visits (lang, title, opened_at, dwell_secs, referrer_lang, referrer_title)
                    VALUES ('en', 'Alan Turing', 100, 30, NULL, NULL);
                INSERT INTO visits (lang, title, opened_at, dwell_secs, referrer_lang, referrer_title)
                    VALUES ('en', 'Enigma machine', 200, 15, 'en', 'Alan Turing');
                PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        // Opening runs the v3 -> v4 migration.
        let history = History::open_at(&path);
        let visits = history.all_visits();
        assert_eq!(visits.len(), 2, "no pre-v4 visit is lost");
        let turing = visits.iter().find(|v| v.title == "Alan Turing").unwrap();
        assert_eq!(turing.wiki, "", "backfills as the default wiki");
        let enigma = visits.iter().find(|v| v.title == "Enigma machine").unwrap();
        assert_eq!(enigma.wiki, "");
        assert_eq!(enigma.referrer_title.as_deref(), Some("Alan Turing"));
        assert_eq!(
            enigma.referrer_wiki, None,
            "a pre-v4 referrer has no recorded wiki — not lost, just less precise"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// CORR-M8: a v4 migration interrupted after its `ADD COLUMN`s ran but
    /// before the `user_version` bump committed (the half-state the old
    /// per-statement-autocommit code could leave on a crash) must re-run
    /// cleanly. Simulate it by hand-adding the v4 columns while leaving the
    /// version at 3; opening must finish the bump to 4 rather than wedge
    /// forever on a "duplicate column name" error.
    #[test]
    fn an_interrupted_v4_migration_re_runs_idempotently_without_wedging() {
        let path = temp_path();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE visits (
                    id INTEGER PRIMARY KEY,
                    lang TEXT NOT NULL,
                    title TEXT NOT NULL,
                    opened_at INTEGER NOT NULL,
                    dwell_secs INTEGER NOT NULL DEFAULT 0,
                    referrer_lang TEXT,
                    referrer_title TEXT
                );
                CREATE TABLE positions (
                    wiki TEXT NOT NULL DEFAULT '',
                    lang TEXT NOT NULL,
                    title TEXT NOT NULL,
                    revid INTEGER NOT NULL DEFAULT 0,
                    scroll INTEGER NOT NULL DEFAULT 0,
                    folds TEXT NOT NULL DEFAULT '',
                    anchor TEXT,
                    updated_at INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (wiki, lang, title)
                );
                INSERT INTO visits (lang, title, opened_at) VALUES ('en', 'Alan Turing', 100);
                -- The v4 DDL ran, but the version bump did not (the crash window).
                ALTER TABLE visits ADD COLUMN wiki TEXT NOT NULL DEFAULT '';
                ALTER TABLE visits ADD COLUMN referrer_wiki TEXT;
                PRAGMA user_version = 3;",
            )
            .unwrap();
        }

        // Re-running migrate must finish the bump to 4, not error on the
        // already-present columns.
        let history = History::open_at(&path);
        let version: i64 = history
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, SCHEMA_VERSION,
            "the interrupted migration must complete rather than wedge at v3"
        );

        // The pre-existing visit survives, readable via the v4 wiki column.
        let visits = history.all_visits();
        assert_eq!(visits.len(), 1);
        assert_eq!(
            visits[0].wiki, "",
            "the backfilled visit reads as the default wiki"
        );

        // A second open stays a clean no-op.
        let reopened = History::open_at(&path);
        let again: i64 = reopened
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(again, SCHEMA_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opening_the_same_file_twice_is_idempotent() {
        let path = temp_path();
        {
            let mut history = History::open_at(&path);
            history.record_visit("", "en", "Alan Turing", None);
        }
        // Re-opening must not fail, wipe, or duplicate the schema — the
        // migration's `IF NOT EXISTS`/version-gate must make this a no-op.
        let mut reopened = History::open_at(&path);
        assert_eq!(reopened.recent(10).len(), 1);
        reopened.record_visit("", "en", "Enigma machine", None);
        assert_eq!(History::open_at(&path).recent(10).len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn in_memory_history_starts_empty_and_never_touches_disk() {
        let history = History::in_memory();
        assert!(history.recent(10).is_empty());
        assert!(!history.is_visited("", "en", "Alan Turing"));
    }

    // ---- record + recent (dedup) -------------------------------------------

    #[test]
    fn record_visit_returns_an_id_and_appears_in_recent() {
        let mut history = History::in_memory();
        let id = history
            .record_visit("", "en", "Alan Turing", None)
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
        history.record_visit("", "en", "Alan Turing", None);
        history.record_visit("", "en", "Enigma machine", Some(("", "en", "Alan Turing")));
        let recent = history.recent(10);
        let enigma = recent.iter().find(|v| v.title == "Enigma machine").unwrap();
        assert_eq!(enigma.referrer_wiki.as_deref(), Some(""));
        assert_eq!(enigma.referrer_lang.as_deref(), Some("en"));
        assert_eq!(enigma.referrer_title.as_deref(), Some("Alan Turing"));
    }

    /// PRD FR-ML-4/FR-HS-3: a visit's own wiki and its referrer's wiki both
    /// round-trip through `record_visit` -> `all_visits`/`recent` — the
    /// trail view's node/edge identity depends on both surviving intact.
    #[test]
    fn record_visit_stores_a_non_default_wiki_and_its_referrers_wiki() {
        let mut history = History::in_memory();
        history.record_visit("wiktionary", "en", "Mercury", None);
        history.record_visit(
            "wiktionary",
            "en",
            "Quicksilver",
            Some(("wiktionary", "en", "Mercury")),
        );
        let all = history.all_visits();
        let quicksilver = all.iter().find(|v| v.title == "Quicksilver").unwrap();
        assert_eq!(quicksilver.wiki, "wiktionary");
        assert_eq!(quicksilver.referrer_wiki.as_deref(), Some("wiktionary"));
        assert_eq!(quicksilver.referrer_title.as_deref(), Some("Mercury"));
    }

    /// PRD FR-ML-4: `recent`'s per-article dedup groups on `(wiki, lang,
    /// title)`, not just `(lang, title)` — a same-titled article read on two
    /// different wikis must stay two distinct rows, never one collapsing
    /// the other's dwell/referrer away.
    #[test]
    fn recent_keeps_a_same_titled_article_on_two_wikis_as_two_rows() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Mercury", None);
        history.record_visit("wiktionary", "en", "Mercury", None);
        let recent = history.recent(10);
        assert_eq!(
            recent.len(),
            2,
            "two wikis' Mercury are two distinct history rows"
        );
        assert!(recent.iter().any(|v| v.wiki.is_empty()));
        assert!(recent.iter().any(|v| v.wiki == "wiktionary"));
    }

    #[test]
    fn recent_dedups_to_the_most_recent_visit_per_article() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Alan Turing", None);
        history.record_visit("", "en", "Enigma machine", None);
        history.record_visit("", "en", "Alan Turing", None); // revisit

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
            history.record_visit("", "en", title, None);
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
        let id = history.record_visit("", "en", "Alan Turing", None).unwrap();
        history.update_dwell(id, 30);
        history.update_dwell(id, 15);
        let visit = history.recent(10).into_iter().find(|v| v.id == id).unwrap();
        assert_eq!(visit.dwell_secs, 45);
    }

    #[test]
    fn update_dwell_of_zero_is_a_harmless_no_op() {
        let mut history = History::in_memory();
        let id = history.record_visit("", "en", "Alan Turing", None).unwrap();
        history.update_dwell(id, 0);
        let visit = history.recent(10).into_iter().next().unwrap();
        assert_eq!(visit.dwell_secs, 0);
    }

    // ---- fuzzy search ranking -------------------------------------------------

    #[test]
    fn empty_query_search_matches_recent() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Alan Turing", None);
        history.record_visit("", "en", "Enigma machine", None);
        assert_eq!(history.search("", 10), history.recent(10));
        assert_eq!(history.search("   ", 10), history.recent(10));
    }

    #[test]
    fn search_excludes_non_matching_titles() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Alan Turing", None);
        history.record_visit("", "en", "Enigma machine", None);
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
        let old_id = history.record_visit("", "en", "abc project", None).unwrap();
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
        history.record_visit("", "en", &weak_title, None);

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
        let old_id = history
            .record_visit("", "en", "Turing Award", None)
            .unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 100 * 86_400, old_id],
            )
            .unwrap();
        history.record_visit("", "en", "Turing Machine", None); // just now, same quality

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
        assert!(!history.is_visited("", "en", "Alan Turing"));
        history.record_visit("", "en", "Alan Turing", None);
        assert!(history.is_visited("", "en", "Alan Turing"));
        assert!(
            !history.is_visited("", "de", "Alan Turing"),
            "scoped per language"
        );
    }

    /// PRD FR-ML-4/FR-HS-2: a title visited on one wiki must not paint a
    /// same-titled article on a *different* wiki as visited too — the
    /// visited cache is scoped by wiki, not just language, mirroring the
    /// isolation `recent`'s dedup and `positions` already give the rest of
    /// this module (`recent_keeps_a_same_titled_article_on_two_wikis_as_two_rows`,
    /// `positions_are_isolated_per_wiki`).
    #[test]
    fn visited_is_isolated_per_wiki() {
        let mut history = History::in_memory();
        history.record_visit("wiktionary", "en", "Mercury", None);

        assert!(
            history.is_visited("wiktionary", "en", "Mercury"),
            "visited on the wiki it was actually read on"
        );
        assert!(
            !history.is_visited("", "en", "Mercury"),
            "a visit on wiktionary must not mark the default wiki's Mercury visited"
        );
        assert!(
            !history.is_visited("wikivoyage", "en", "Mercury"),
            "nor any other third wiki"
        );

        assert!(
            history
                .visited_titles_for_lang("wiktionary", "en")
                .is_some_and(|set| set.contains("Mercury"))
        );
        assert!(
            history
                .visited_titles_for_lang("", "en")
                .is_none_or(|set| !set.contains("Mercury")),
            "the default wiki's visited set must not include wiktionary's Mercury"
        );
    }

    #[test]
    fn visited_set_survives_reopening_the_same_file() {
        let path = temp_path();
        {
            let mut history = History::open_at(&path);
            history.record_visit("", "en", "Alan Turing", None);
        }
        let reopened = History::open_at(&path);
        assert!(reopened.is_visited("", "en", "Alan Turing"));
        assert!(
            reopened
                .visited_titles_for_lang("", "en")
                .is_some_and(|set| set.contains("Alan Turing"))
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---- clear ----------------------------------------------------------------

    #[test]
    fn clear_all_empties_history_and_the_visited_cache() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Alan Turing", None);
        history.record_visit("", "en", "Enigma machine", None);
        let removed = history.clear(ClearRange::All).unwrap();
        assert_eq!(removed, 2);
        assert!(history.recent(10).is_empty());
        assert!(!history.is_visited("", "en", "Alan Turing"));
    }

    #[test]
    fn clear_before_a_cutoff_keeps_newer_rows() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("", "en", "Old Article", None).unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 10 * 86_400, old_id],
            )
            .unwrap();
        history.record_visit("", "en", "New Article", None);

        let cutoff = now_unix() - 5 * 86_400;
        let removed = history.clear(ClearRange::Before(cutoff)).unwrap();
        assert_eq!(removed, 1);
        let recent = history.recent(10);
        let remaining: Vec<&str> = recent.iter().map(|v| v.title.as_str()).collect();
        assert_eq!(remaining, vec!["New Article"]);
        assert!(!history.is_visited("", "en", "Old Article"));
        assert!(history.is_visited("", "en", "New Article"));
    }

    #[test]
    fn clear_one_article_leaves_the_rest() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Alan Turing", None);
        history.record_visit("", "en", "Enigma machine", None);
        let removed = history
            .clear(ClearRange::Article {
                wiki: String::new(),
                lang: "en".to_string(),
                title: "Alan Turing".to_string(),
            })
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!history.is_visited("", "en", "Alan Turing"));
        assert!(history.is_visited("", "en", "Enigma machine"));
    }

    /// CORR-M5: `ClearRange::Article` is scoped by wiki, so the picker's `d`
    /// on one wiki's row for a title never also deletes a same-titled article
    /// read on a different wiki. Before wiki-scoping the DELETE, clearing the
    /// Wiktionary "Mercury" row would also drop Wikipedia's, since the
    /// predicate matched only `lang`/`title`.
    #[test]
    fn clear_one_article_on_one_wiki_leaves_the_same_title_on_another_wiki() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Mercury", None);
        history.record_visit("wiktionary", "en", "Mercury", None);

        let removed = history
            .clear(ClearRange::Article {
                wiki: "wiktionary".to_string(),
                lang: "en".to_string(),
                title: "Mercury".to_string(),
            })
            .unwrap();
        assert_eq!(removed, 1, "only the Wiktionary row is deleted");
        assert!(
            !history.is_visited("wiktionary", "en", "Mercury"),
            "the cleared wiki's row is gone"
        );
        assert!(
            history.is_visited("", "en", "Mercury"),
            "the other wiki's same-titled article survives"
        );
    }

    // ---- retention prune --------------------------------------------------------

    #[test]
    fn retention_prune_of_zero_days_keeps_forever() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("", "en", "Ancient", None).unwrap();
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

    /// CORR-L5: before this fix, an absurd `retention_days` (bigger than
    /// `i64::MAX`) wrapped negative through the bare `as i64` cast, and the
    /// subsequent `* 86_400` could overflow-panic in a debug build — or, if
    /// it didn't panic, the wrapped-negative value produced a far-future
    /// cutoff that deleted *all* history instead of the intended "keep
    /// (almost) forever". Saturating arithmetic must instead prune nothing.
    #[test]
    fn retention_prune_with_an_absurd_retention_days_does_not_delete_everything() {
        let mut history = History::in_memory();
        history.record_visit("", "en", "Just Visited", None);
        assert_eq!(history.recent(10).len(), 1);

        history.retention_prune(u64::MAX);

        assert_eq!(
            history.recent(10).len(),
            1,
            "an absurd retention window must not wipe out history that's seconds old"
        );
    }

    #[test]
    fn retention_prune_drops_old_rows_and_keeps_new_ones() {
        let mut history = History::in_memory();
        let old_id = history.record_visit("", "en", "Old Article", None).unwrap();
        history
            .conn
            .execute(
                "UPDATE visits SET opened_at = ?1 WHERE id = ?2",
                params![now_unix() - 90 * 86_400, old_id],
            )
            .unwrap();
        history.record_visit("", "en", "New Article", None);

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
