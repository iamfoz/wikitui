//! Local full-text search over saved + cached pages (PRD FR-SR-7, §6.4's
//! "SQLite for indexes/history/FTS5"): a SQLite FTS5 virtual table indexing
//! plain text extracted from every pinned saved page (`saved.rs`) and every
//! page the L2 cache currently holds (`cache.rs`), so the search box can
//! answer a query with no network at all.
//!
//! ## FTS5 availability
//!
//! `rusqlite`'s `bundled` feature (already used by `history.rs`) vendors its
//! own SQLite amalgamation, but `bundled` alone does **not** compile FTS5 in
//! — that needs `rusqlite`'s own `fts5` cargo feature (which sets
//! `SQLITE_ENABLE_FTS5` on the bundled build), added alongside `bundled` in
//! `Cargo.toml` for this module. `fts5_is_compiled_into_the_bundled_sqlite`
//! below proves it by actually creating an FTS5 virtual table and querying
//! it — the ground truth this whole module depends on, not a version guess.
//! Had that come back negative, the documented fallback would be a plain
//! table plus a `LIKE '%term%'` scan (no ranking, no snippet highlighting) —
//! it did not come back negative, so no such fallback exists in this build.
//!
//! ## Location: derived, rebuildable, lives in the cache dir
//!
//! `$XDG_CACHE_HOME/wikitui/search-index.sqlite` (PRD §6.4: the cache dir is
//! "never synced; safe to delete") — a sibling of `cache.rs`'s own `pages/`
//! tree, not `history.sqlite`'s state dir or the saved-pages store's data
//! dir. Unlike history (irreplaceable reading history) or saved pages
//! (pinned, integrity-checked user content), every row here is *derived*
//! from content that already lives durably elsewhere (saved HTML, or a
//! cache blob) — deleting the index file loses nothing but search, until
//! `reindex` rebuilds it from whatever saved/cached content still exists.
//! Living in the cache dir means an install that has never opened the
//! search box, or one whose index got corrupted, is never worse off than a
//! cold cache: read failures throughout this module degrade to "no results"
//! or an empty rebuild, never a crash or a blocked write elsewhere.
//!
//! ## Schema & scoping
//!
//! One FTS5 table, `docs(wiki, lang, title, kind, body)`. `wiki`/`lang`/
//! `kind` are `UNINDEXED` — exact metadata, not tokenized text — so a query
//! only ever matches `title`/`body`, while `wiki`/`lang` still filter
//! results via an ordinary `WHERE` equality (PRD FR-ML-4: offline search is
//! scoped to the active wiki, matching the online `search/page` endpoint's
//! own per-wiki scope — see `main::run_offline_search`'s doc comment). The
//! rowid is a deterministic hash of `wiki\0lang\0title` (`doc_rowid`,
//! reusing `cache::fnv1a` — the same "stable across runs, dependency-free"
//! hash `cache.rs`/`saved.rs` already share), so `index`/`remove` address a
//! row directly without a lookup query first; a hash collision between two
//! distinct `(wiki, lang, title)` triples is astronomically unlikely at any
//! realistic install size and, like `cache::safe_name`'s own long-title hash
//! fallback, is an accepted, documented risk rather than something this
//! module defends against.
//!
//! ## Populate / remove
//!
//! - `index` upserts (delete-then-insert — the portable way to replace a
//!   row in an FTS5 table by rowid) plain text under `(wiki, lang, title)`,
//!   called from `main.rs` right after a page is saved
//!   (`saved::SavedPages::save`) or freshly fetched into L2
//!   (`cache::PageCache::put`, at the interactive-open, revalidation, and
//!   background-prefetch call sites).
//! - `remove` deletes a row outright — wired to an explicit un-save (`d` in
//!   the saved-pages browser).
//! - Cache **eviction** does *not* call `remove`: `cache.rs`'s LRU/SLRU sweep
//!   has no reference to this module (keeping the storage modules mutually
//!   ignorant of each other — the existing convention; `cache.rs` doesn't
//!   know about `saved.rs` either), so an evicted article's index row goes
//!   stale rather than disappearing the instant its blob does. This is a
//!   deliberate "tolerate stale entries + verify-on-hit" choice:
//!   `main::open_offline_result` re-checks the saved store then the cache
//!   before ever rendering a hit, and lazily prunes the row (`remove`) the
//!   moment a hit's content turns out to be gone — so a stale row costs one
//!   wasted result at worst, never a wrong or broken open.
//!
//! ## Ranking & snippets
//!
//! `search` ranks by FTS5's own `bm25()` (ascending — smaller is a better
//! match) and extracts a snippet via `snippet(docs, -1, ...)` (column `-1`:
//! "best matching column," so a title-only match still gets a sensible
//! snippet instead of an unhighlighted chunk of body text). The snippet's
//! match markers are the exact `<span class="searchmatch">...</span>`
//! markup the online `search/page` endpoint's excerpts already use — see
//! `main::run_offline_search`'s doc comment for why that isn't a
//! coincidence: it lets an offline result ride `ui::draw_results`'s existing
//! `parse_searchmatch` highlighting unchanged, with zero rendering-side
//! special-casing for "this result came from offline search."
//!
//! ## A known limitation: saved pages aren't wiki-scoped yet
//!
//! `saved::SavedPages` itself still keys purely on `(lang, title)` — the
//! wiki-scoping pass (`api::wiki_scope`, PRD FR-ML-4) reached `cache.rs` and
//! session state but not the saved-pages store. This module still tags a
//! saved page's index row with the wiki it was saved *on* (known at the
//! call site in `main.rs`), so search scoping is correct for the common
//! case; but if the same title is ever saved from two different wikis, the
//! underlying store itself only keeps the most recent save (a pre-existing
//! gap, not introduced here), so the *other* wiki's index row can end up
//! pointing at content that isn't what it says. `main::open_offline_result`'s
//! saved-store lookup still can't disambiguate that case, same as `saved.rs`
//! itself can't today.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

/// Schema version this build understands (mirrors `history.rs`'s
/// `SCHEMA_VERSION` / `config::CONFIG_VERSION` migration shape).
const SCHEMA_VERSION: i64 = 1;

/// Where one indexed document's plain text came from — surfaced to the
/// reader (`main::run_offline_search`'s "(offline · saved)"/"(offline ·
/// cached)" label) and used by `main::open_offline_result` to decide which
/// store to try first when a hit is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Saved,
    Cached,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Saved => "saved",
            Kind::Cached => "cached",
        }
    }

    /// A row's `kind` column is only ever written by [`OfflineIndex::index`]
    /// as one of the two labels above, so an unrecognized value (there
    /// shouldn't be one) degrades to `Cached` rather than failing the whole
    /// query — the same "corrupt metadata degrades, never panics" posture
    /// every other store in this codebase takes for its own on-disk enums.
    fn parse(s: &str) -> Kind {
        match s {
            "saved" => Kind::Saved,
            _ => Kind::Cached,
        }
    }
}

/// One offline search hit (PRD FR-SR-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineHit {
    pub wiki: String,
    pub lang: String,
    pub title: String,
    pub kind: Kind,
    /// HTML snippet with `<span class="searchmatch">` markup around matched
    /// terms — see the module doc's "Ranking & snippets".
    pub snippet: String,
}

/// What one [`OfflineIndex::reindex`] rebuild did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReindexReport {
    pub indexed: usize,
}

/// The offline search index. Holds a persistent connection behind
/// `Arc<Mutex<_>>` rather than reopening per call (unlike `cache.rs`/
/// `saved.rs`'s bare-`PathBuf` stores): FTS5's `:memory:` mode needs one
/// live connection to survive at all (a fresh in-memory database per call
/// would lose every row immediately, breaking `in_memory()` for tests), and
/// a shared, lockable connection is also exactly what lets this same handle
/// be cloned into `main.rs`'s spawned background tasks (prefetch,
/// revalidation) the same way `cache::PageCache` already is, instead of
/// needing its own separate threading story.
#[derive(Clone)]
pub struct OfflineIndex {
    conn: Arc<Mutex<Connection>>,
}

impl OfflineIndex {
    /// Opens (creating if needed) the real on-disk index at the platform
    /// cache directory (see the module doc's "Location"). Falls back to an
    /// in-memory store — search simply isn't durable this run — when no
    /// platform directory can be determined, mirroring `History::open`'s
    /// documented convention.
    pub fn open() -> Self {
        match index_path() {
            Some(path) => Self::open_at(&path),
            None => Self::in_memory(),
        }
    }

    /// Opens the database at an explicit path, creating parent directories
    /// as needed. Tests use this against a tempdir file — never the real
    /// cache dir.
    pub fn open_at(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Connection::open(path) {
            Ok(conn) => Self::from_connection(conn),
            Err(e) => {
                log_failure("open", &e);
                Self::in_memory()
            }
        }
    }

    /// An index with no on-disk persistence at all — `App::new`'s default
    /// (see `App.search_index`'s doc comment for why, mirroring
    /// `History::in_memory`'s own rationale: several `main.rs` functions
    /// index as a side effect of ordinary navigation, so defaulting to a
    /// real on-disk store here would make `cargo test` write into the
    /// developer's actual cache directory on every run that opens a
    /// document through those paths).
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory()
            .expect("an in-memory sqlite connection cannot fail to open");
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Self {
        if let Err(e) = migrate(&conn) {
            log_failure("migrate", &e);
        }
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Upserts `(wiki, lang, title)`'s plain text under `kind` (see the
    /// module doc's "Populate / remove"). Best-effort: a write failure (a
    /// full disk, a corrupted index file, a poisoned lock) is logged and
    /// swallowed, mirroring every other non-critical store write in this
    /// codebase — losing one search row is never worth interrupting the
    /// save/cache write that triggered it.
    pub fn index(&self, wiki: &str, lang: &str, title: &str, kind: Kind, body: &str) {
        let rowid = doc_rowid(wiki, lang, title);
        let Ok(conn) = self.conn.lock() else {
            return;
        };
        let result = (|| -> rusqlite::Result<()> {
            conn.execute("DELETE FROM docs WHERE rowid = ?1", params![rowid])?;
            conn.execute(
                "INSERT INTO docs(rowid, wiki, lang, title, kind, body) VALUES (?1,?2,?3,?4,?5,?6)",
                params![rowid, wiki, lang, title, kind.label(), body],
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            log_failure("index", &e);
        }
    }

    /// Removes `(wiki, lang, title)`'s row outright — an explicit un-save,
    /// or `main::open_offline_result`'s lazy stale-row prune (see the module
    /// doc's "Populate / remove"). A miss is a silent no-op, same as
    /// removing something already absent from any other store here.
    pub fn remove(&self, wiki: &str, lang: &str, title: &str) {
        let rowid = doc_rowid(wiki, lang, title);
        let Ok(conn) = self.conn.lock() else {
            return;
        };
        if let Err(e) = conn.execute("DELETE FROM docs WHERE rowid = ?1", params![rowid]) {
            log_failure("remove", &e);
        }
    }

    /// FR-SR-7's offline query: `query` against `docs`, scoped to `wiki` +
    /// `lang` (see the module doc's "Schema & scoping"), ranked by `bm25()`
    /// ascending (smaller = better match — FTS5's own convention). An
    /// empty/whitespace query returns no hits; `query` is split into
    /// individually-quoted phrase terms (`fts5_query_escape`) before it ever
    /// reaches FTS5's MATCH parser, so stray syntax characters a reader
    /// types (`"`, `:`, `*`, `-`) are treated as plain words instead of
    /// FTS5 operators or a MATCH syntax error — a search box has no good way
    /// to surface "your query has a syntax error" to someone typing plain
    /// words. A query that still somehow fails is the same "log and return
    /// no hits" degradation as every other read here.
    pub fn search(&self, wiki: &str, lang: &str, query: &str, limit: usize) -> Vec<OfflineHit> {
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let fts_query = fts5_query_escape(query);
        let result = (|| -> rusqlite::Result<Vec<OfflineHit>> {
            let mut stmt = conn.prepare(
                "SELECT wiki, lang, title, kind, \
                        snippet(docs, -1, '<span class=\"searchmatch\">', '</span>', '…', 12) \
                 FROM docs \
                 WHERE docs MATCH ?1 AND wiki = ?2 AND lang = ?3 \
                 ORDER BY bm25(docs) \
                 LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![fts_query, wiki, lang, limit as i64], |row| {
                Ok(OfflineHit {
                    wiki: row.get(0)?,
                    lang: row.get(1)?,
                    title: row.get(2)?,
                    kind: Kind::parse(&row.get::<_, String>(3)?),
                    snippet: row.get(4)?,
                })
            })?;
            Ok(rows.filter_map(Result::ok).collect())
        })();
        match result {
            Ok(hits) => hits,
            Err(e) => {
                log_failure("search", &e);
                Vec::new()
            }
        }
    }

    /// Empties every row — `reindex`'s first step, and `wikitui reindex`'s
    /// "start from scratch" contract.
    pub fn clear(&self) {
        let Ok(conn) = self.conn.lock() else {
            return;
        };
        if let Err(e) = conn.execute("DELETE FROM docs", []) {
            log_failure("clear", &e);
        }
    }

    /// Rebuilds the index from scratch over `docs` — `(wiki, lang, title,
    /// kind, body)` tuples the caller (`main.rs`'s `wikitui reindex` and
    /// first-run/corruption recovery) has already gathered from
    /// `saved::SavedPages` and `cache::PageCache` and reduced to plain text
    /// (`doc::parse_article_html` + `doc::render_plain`) — this module never
    /// depends on the document model or either store directly, the same
    /// "storage modules stay mutually ignorant" convention `cache.rs`/
    /// `saved.rs` already follow. Clears every existing row first, so a
    /// stale row from a page removed since the last successful populate
    /// never survives a reindex.
    pub fn reindex(
        &self,
        docs: impl IntoIterator<Item = (String, String, String, Kind, String)>,
    ) -> ReindexReport {
        self.clear();
        let mut indexed = 0usize;
        for (wiki, lang, title, kind, body) in docs {
            self.index(&wiki, &lang, &title, kind, &body);
            indexed += 1;
        }
        ReindexReport { indexed }
    }

    /// Row count — `wikitui reindex`'s summary line and this module's tests.
    pub fn len(&self) -> usize {
        let Ok(conn) = self.conn.lock() else {
            return 0;
        };
        conn.query_row("SELECT COUNT(*) FROM docs", [], |r| r.get::<_, i64>(0))
            .map(|n| n.max(0) as usize)
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Creates the `docs` FTS5 table if this is a fresh (or pre-search-index)
/// database, via the `user_version` pragma — mirroring `history.rs`'s
/// `migrate`. A no-op once a database is already at `SCHEMA_VERSION`.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(()); // already current — no redundant DDL on every launch
    }
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(
            wiki UNINDEXED,
            lang UNINDEXED,
            title,
            kind UNINDEXED,
            body,
            tokenize = 'unicode61 remove_diacritics 2'
        );
        PRAGMA user_version = 1;",
    )
}

/// The real on-disk location (PRD §6.4): `$XDG_CACHE_HOME/wikitui/
/// search-index.sqlite` — see the module doc's "Location".
fn index_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "wikitui")
        .map(|dirs| dirs.cache_dir().join("search-index.sqlite"))
}

/// A deterministic rowid for `(wiki, lang, title)`, letting `index`/`remove`
/// address a row directly (see the module doc's "Schema & scoping"). The
/// NUL separator makes `("a", "b|c", ...)` and `("a|b", "c", ...)` hash
/// differently even though a naive string-concat of the three fields alone
/// would collide; NUL cannot appear in a wiki name, language code, or a
/// sanitized article title in practice, so this is not itself a source of
/// collisions the way *not* separating the fields at all would be.
fn doc_rowid(wiki: &str, lang: &str, title: &str) -> i64 {
    let key = format!("{wiki}\u{0}{lang}\u{0}{title}");
    crate::cache::fnv1a(key.as_bytes()) as i64
}

/// Turns a reader's plain-language query into an FTS5 MATCH expression that
/// treats every word as a literal phrase, ANDed together (FTS5's implicit
/// default between space-separated terms) — see `OfflineIndex::search`'s
/// doc comment for why: quoting each word neutralizes column-filter syntax
/// (`title:x`), wildcards (`x*`), the `-` exclude prefix, and unbalanced
/// quotes, none of which a search box's reader ever intends when they type
/// an ordinary word. A double quote inside a word is escaped by doubling,
/// FTS5 string-literal style.
fn fts5_query_escape(query: &str) -> String {
    query
        .split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An index write/read's own failure is never a crash or a user-facing
/// error (see the module doc comment) — this is the one place that fact is
/// visible at all, a debug-log line to stderr, mirroring
/// `history::log_write_failure`.
fn log_failure(op: &str, err: &rusqlite::Error) {
    eprintln!("wikitui: offline_search: {op} failed: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- FTS5 availability (the ground-truth this whole module depends on) --

    /// PRD §6.4 / FR-SR-7: proves the bundled SQLite (`rusqlite`'s `bundled`
    /// feature) actually has FTS5 compiled in, by creating a virtual table
    /// and running a real MATCH query against it — not just checking a
    /// version string or a cargo feature flag. See the module doc's "FTS5
    /// availability" for what the documented fallback would have been had
    /// this failed.
    #[test]
    fn fts5_is_compiled_into_the_bundled_sqlite() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE probe USING fts5(body); \
             INSERT INTO probe(body) VALUES ('hello offline world');",
        )
        .expect(
            "FTS5 must be compiled into the bundled SQLite — add rusqlite's \
             `fts5` cargo feature alongside `bundled` if this fails",
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM probe WHERE probe MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "a real FTS5 MATCH query must find the row");
    }

    // ---- schema / migration --------------------------------------------

    #[test]
    fn a_fresh_database_migrates_to_the_current_schema_version() {
        let idx = OfflineIndex::in_memory();
        let conn = idx.conn.lock().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn opening_the_same_file_twice_is_idempotent() {
        let path = temp_path();
        {
            let idx = OfflineIndex::open_at(&path);
            idx.index("", "en", "Alan Turing", Kind::Saved, "computer scientist");
        }
        let reopened = OfflineIndex::open_at(&path);
        assert_eq!(reopened.len(), 1, "the row must survive a reopen");
        reopened.index("", "en", "Enigma", Kind::Cached, "encryption machine");
        assert_eq!(OfflineIndex::open_at(&path).len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    // ---- index / remove / search round trip -----------------------------

    #[test]
    fn indexed_content_is_found_by_a_body_word() {
        let idx = OfflineIndex::in_memory();
        idx.index(
            "",
            "en",
            "Alan Turing",
            Kind::Saved,
            "Alan Turing was a British mathematician and computer scientist.",
        );
        let hits = idx.search("", "en", "mathematician", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Alan Turing");
        assert_eq!(hits[0].kind, Kind::Saved);
    }

    #[test]
    fn indexing_the_same_key_twice_replaces_rather_than_duplicates() {
        let idx = OfflineIndex::in_memory();
        idx.index(
            "",
            "en",
            "Draft",
            Kind::Cached,
            "first version alpha-marker",
        );
        idx.index(
            "",
            "en",
            "Draft",
            Kind::Cached,
            "second version beta-marker",
        );
        assert_eq!(idx.len(), 1, "same (wiki,lang,title) must not duplicate");
        assert!(
            idx.search("", "en", "alpha-marker", 10).is_empty(),
            "the old body text must be gone after re-indexing"
        );
        assert_eq!(idx.search("", "en", "beta-marker", 10).len(), 1);
    }

    #[test]
    fn remove_deletes_the_row_and_it_no_longer_matches() {
        let idx = OfflineIndex::in_memory();
        idx.index("", "en", "Alan Turing", Kind::Saved, "computer scientist");
        assert_eq!(idx.len(), 1);
        idx.remove("", "en", "Alan Turing");
        assert_eq!(idx.len(), 0);
        assert!(idx.search("", "en", "scientist", 10).is_empty());
    }

    #[test]
    fn removing_an_absent_row_is_a_harmless_no_op() {
        let idx = OfflineIndex::in_memory();
        idx.remove("", "en", "Never Indexed"); // must not panic
        assert_eq!(idx.len(), 0);
    }

    // ---- bm25 ranking ----------------------------------------------------

    #[test]
    fn search_ranks_the_stronger_match_first() {
        let idx = OfflineIndex::in_memory();
        idx.index(
            "",
            "en",
            "Alan Turing",
            Kind::Saved,
            "Turing was a mathematician. Turing pioneered computing. Turing broke Enigma.",
        );
        idx.index(
            "",
            "en",
            "Enigma machine",
            Kind::Cached,
            "The Enigma was a cipher machine; it is occasionally linked to Turing in passing.",
        );
        let hits = idx.search("", "en", "turing", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].title, "Alan Turing",
            "the article mentioning the term far more often should rank first"
        );
    }

    // ---- snippet highlighting ---------------------------------------------

    #[test]
    fn snippet_wraps_matched_terms_in_searchmatch_markup() {
        let idx = OfflineIndex::in_memory();
        idx.index(
            "",
            "en",
            "Alan Turing",
            Kind::Saved,
            "Alan Turing was a British mathematician and computer scientist.",
        );
        let hits = idx.search("", "en", "mathematician", 10);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("<span class=\"searchmatch\">"),
            "snippet must carry the same searchmatch markup ui::parse_searchmatch expects: {:?}",
            hits[0].snippet
        );
        assert!(hits[0].snippet.to_lowercase().contains("mathematician"));
    }

    // ---- wiki + lang scoping (PRD FR-ML-4) ---------------------------------

    #[test]
    fn same_title_on_two_wikis_is_indexed_and_searched_separately() {
        let idx = OfflineIndex::in_memory();
        idx.index(
            "",
            "en",
            "Mercury",
            Kind::Cached,
            "Mercury is the smallest planet in the solar system.",
        );
        idx.index(
            "wiktionary",
            "en",
            "Mercury",
            Kind::Cached,
            "Mercury: a heavy metal element, and a Roman god of commerce.",
        );

        let default_hits = idx.search("", "en", "planet", 10);
        assert_eq!(default_hits.len(), 1);
        assert_eq!(default_hits[0].wiki, "");

        assert!(
            idx.search("wiktionary", "en", "planet", 10).is_empty(),
            "the sister wiki's entry has no 'planet' — must never leak across wikis"
        );
        let sister_hits = idx.search("wiktionary", "en", "god", 10);
        assert_eq!(sister_hits.len(), 1);
        assert_eq!(sister_hits[0].wiki, "wiktionary");
        assert!(
            idx.search("", "en", "god", 10).is_empty(),
            "the default wiki's entry has no 'god' — scoping must not be reversed either"
        );
    }

    #[test]
    fn same_title_in_two_languages_is_indexed_and_searched_separately() {
        let idx = OfflineIndex::in_memory();
        idx.index(
            "",
            "en",
            "Turing",
            Kind::Cached,
            "computer scientist and codebreaker",
        );
        idx.index(
            "",
            "de",
            "Turing",
            Kind::Cached,
            "britischer Mathematiker und Informatiker",
        );
        assert_eq!(idx.search("", "en", "codebreaker", 10).len(), 1);
        assert!(idx.search("", "de", "codebreaker", 10).is_empty());
        assert_eq!(idx.search("", "de", "Mathematiker", 10).len(), 1);
    }

    // ---- reindex ------------------------------------------------------------

    #[test]
    fn reindex_clears_stale_rows_and_rebuilds_from_the_given_documents() {
        let idx = OfflineIndex::in_memory();
        idx.index(
            "",
            "en",
            "Stale",
            Kind::Cached,
            "should not survive a reindex",
        );

        let report = idx.reindex(vec![
            (
                "".to_string(),
                "en".to_string(),
                "Alan Turing".to_string(),
                Kind::Saved,
                "computer scientist".to_string(),
            ),
            (
                "".to_string(),
                "en".to_string(),
                "Enigma".to_string(),
                Kind::Cached,
                "encryption machine".to_string(),
            ),
        ]);

        assert_eq!(report.indexed, 2);
        assert_eq!(idx.len(), 2);
        assert!(
            idx.search("", "en", "should", 10).is_empty(),
            "the pre-reindex stale row must be gone"
        );
        assert_eq!(idx.search("", "en", "scientist", 10).len(), 1);
        assert_eq!(idx.search("", "en", "encryption", 10).len(), 1);
    }

    #[test]
    fn reindex_of_an_empty_document_list_leaves_an_empty_index() {
        let idx = OfflineIndex::in_memory();
        idx.index("", "en", "Stale", Kind::Cached, "leftover");
        let report = idx.reindex(std::iter::empty());
        assert_eq!(report.indexed, 0);
        assert!(idx.is_empty());
    }

    // ---- graceful empty / malformed-query handling -------------------------

    #[test]
    fn search_on_an_empty_index_returns_no_hits_not_an_error() {
        let idx = OfflineIndex::in_memory();
        assert!(idx.search("", "en", "anything", 10).is_empty());
    }

    #[test]
    fn an_empty_or_whitespace_query_returns_no_hits() {
        let idx = OfflineIndex::in_memory();
        idx.index("", "en", "Alan Turing", Kind::Saved, "computer scientist");
        assert!(idx.search("", "en", "", 10).is_empty());
        assert!(idx.search("", "en", "   ", 10).is_empty());
    }

    #[test]
    fn queries_with_raw_fts5_syntax_characters_never_panic_or_error() {
        let idx = OfflineIndex::in_memory();
        idx.index("", "en", "Test", Kind::Cached, "hello offline world");
        for q in [
            "\"unterminated",
            "col:term",
            "hello*",
            "-hello",
            "()",
            "hello \"world\"",
        ] {
            let _ = idx.search("", "en", q, 10); // must not panic
        }
        // A quoted-phrase query for two words that are both actually present
        // still finds the row — proving the escaping doesn't break ordinary
        // multi-word matching along the way.
        assert_eq!(idx.search("", "en", "hello world", 10).len(), 1);
    }

    #[test]
    fn limit_caps_the_number_of_hits_returned() {
        let idx = OfflineIndex::in_memory();
        for i in 0..5 {
            idx.index(
                "",
                "en",
                &format!("Article {i}"),
                Kind::Cached,
                "shared searchable term",
            );
        }
        assert_eq!(idx.search("", "en", "shared", 2).len(), 2);
        assert_eq!(idx.search("", "en", "shared", 100).len(), 5);
    }

    // ---- doc_rowid / fts5_query_escape (pure helpers) ----------------------

    #[test]
    fn doc_rowid_is_stable_and_distinguishes_its_three_fields() {
        let a = doc_rowid("", "en", "Mercury");
        let b = doc_rowid("", "en", "Mercury");
        assert_eq!(a, b, "must be deterministic");
        assert_ne!(a, doc_rowid("wiktionary", "en", "Mercury"));
        assert_ne!(a, doc_rowid("", "de", "Mercury"));
        assert_ne!(a, doc_rowid("", "en", "Venus"));
    }

    #[test]
    fn fts5_query_escape_quotes_every_word() {
        assert_eq!(fts5_query_escape("Alan Turing"), "\"Alan\" \"Turing\"");
        assert_eq!(fts5_query_escape("solo"), "\"solo\"");
    }

    fn temp_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-test-offline-search-{}-{n}.sqlite",
            std::process::id()
        ))
    }
}
