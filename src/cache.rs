//! The two-layer page cache (PRD FR-OFF-1..3): L2 is zstd-compressed raw
//! Parsoid HTML plus a small metadata header, on disk under the platform
//! cache directory; L1 (the laid-out render cache) lives in `App` instead
//! (`layout::LayoutCache`) since it's an in-memory-only concern of the
//! reading view, not storage.
//!
//! ## Layout
//!
//! ```text
//! <cache_dir>/pages/
//!   blob/{wiki}/{lang}/{revid}-{title_hash:016x}.zst  -- content, immutable per revid
//!   page/{wiki}/{lang}/{title_or_hash}.json           -- {wiki, revid, fetched_at, etag}
//!   {lang}/{title_or_hash}.html                       -- pre-upgrade format (see below)
//! ```
//!
//! ## Wiki scoping (PRD FR-ML-4/5)
//!
//! Every key carries a **wiki** dimension (`api::wiki_scope`) so a title
//! opened on one wiki can never be served another wiki's cached copy of the
//! same `(lang, title)` after a runtime `:wiki` switch. The primary
//! Wikipedia entry's scope is the empty string, and the `{wiki}` path
//! segment is *omitted* for it — so `blob/en/…` / `page/en/…` are exactly
//! the paths the pre-multi-wiki code wrote. Every entry that code left on
//! disk is therefore read back transparently as the default wiki's, with no
//! migration pass and no refetch (see `wiki_scope`'s own doc comment). A
//! non-default wiki gets a `{wiki}` segment (`blob/wiktionary/en/…`) — a
//! disjoint subtree, so its entries and eviction accounting never touch the
//! default wiki's.
//!
//! Content is keyed by **revid** where the server tells us one (via the
//! `ETag` on the HTML response, parsed in `api::parse_revid_from_etag`):
//! a blob at a given revid is never rewritten once it exists — FR-OFF-1's
//! "immutable, only evicted" contract — so two titles (or two fetches of
//! the same title over time) that land on the same revid share one blob.
//! The title index is the mutable half: it points at whichever revid is
//! current for that title and carries the bookkeeping (`fetched_at`,
//! `etag`) that staleness decisions need. When a server doesn't expose a
//! revid at all (older mock endpoints, some third-party wikis), content
//! degrades to `revid = 0` — still content-addressed by the *pair*
//! `(revid, title hash)`, so distinct titles never collide, just without
//! the cross-title sharing or the "never re-fetch this exact revid again"
//! win real revids give.
//!
//! ## Migration from the pre-upgrade format
//!
//! Before this module, a cache entry was one flat file per (lang, title)
//! at `<cache_dir>/pages/{lang}/{title}.html` — first line a unix
//! timestamp, rest the raw HTML (no compression, no revid). Two ways to
//! handle those leftovers on upgrade: silently ignore them (simplest, but
//! throws away whatever the reader had already warmed, forcing a refetch
//! storm on the first article reopened after upgrading) or transparently
//! migrate them. This module chooses **migrate**: `get` falls back to the
//! old path on a miss, and if a valid old-format entry is there, folds it
//! into the new layout (content at `revid = 0`, original `fetched_at`
//! preserved so its age is honest rather than reset to "just fetched") and
//! deletes the old file — so it is read exactly once in the old format,
//! then lives entirely in the new one from then on. A corrupted or
//! unparseable old-format file is treated the same as any other cache
//! miss (never a crash): the article simply refetches.
//!
//! Eviction (the LRU cap) only accounts for the new `blob/`+`page/` tree;
//! any not-yet-migrated old-format leftovers sit outside the cap until
//! they're read (and migrated) or the user clears the cache directory by
//! hand — a documented simplification, not an oversight.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// PRD FR-OFF-3: default cache cap 500 MB.
pub const DEFAULT_MAX_BYTES: u64 = 500 * 1024 * 1024;

/// PRD FR-OFF-2's "revalidate after" backstop (renamed from the MVP's
/// "refetch after" meaning now that stale-while-revalidate exists: a cached
/// copy this old is still served *instantly*, just with a background check
/// alongside it — see `swr_decision`).
pub const FRESH_TTL_SECS: u64 = 24 * 60 * 60;

/// PRD FR-OFF-2's force-refetch backstop: entries older than this are
/// treated as absent on open (network-first, same as a cold cache), rather
/// than served-then-revalidated — a ceiling on how stale "instant" is
/// allowed to look even when a background check would eventually catch up.
pub const DEFAULT_FORCE_REFETCH_SECS: u64 = 30 * 24 * 60 * 60;

/// zstd compression level for L2 blobs: favors fetch/write latency over
/// ratio (article HTML is text, which zstd already compresses well even at
/// low levels; this cache's whole point is instant reads, not archival
/// density).
const ZSTD_LEVEL: i32 = 3;

/// PRD FR-OFF-3 v1.0's SLRU protected-segment budget: the *protected*
/// segment (see [`Segment`]) may only claim this fraction of the overall
/// cap. Without a ceiling, "protected" would drift into "un-evictable" if
/// enough entries earned a second read — this keeps the probationary
/// segment guaranteed at least `1.0 - PROTECTED_SEGMENT_FRACTION` of the
/// cap's worth of room to absorb a fresh binge, and gives eviction
/// somewhere to go (the oldest protected entries beyond the budget) even
/// when literally everything has been re-read at least once. 80% is the
/// documented v1.0 default — not user-configurable yet, matching how
/// `FRESH_TTL_SECS`/`ZSTD_LEVEL` above are also fixed constants rather than
/// `[cache]` config keys.
const PROTECTED_SEGMENT_FRACTION: f64 = 0.8;

/// PRD FR-PR-5: resolves the pages-cache root directory — an explicit
/// `[cache] dir` config override, if given, else the platform cache
/// directory (`$XDG_CACHE_HOME/wikitui/pages` on Linux, honored
/// automatically by the `directories` crate since it's what `ProjectDirs`
/// reads). `None` only when no override was given *and* no platform
/// directory could be determined at all — the same silent-disable
/// `PageCache::open` already documented before this override existed.
/// Shared by `PageCache::open` and `doctor`'s report (so the doctor shows
/// exactly the directory a real run would use) and `cleardata` (so
/// `clear-data --cache` deletes exactly that directory).
pub fn resolve_pages_dir(dir_override: Option<&Path>) -> Option<PathBuf> {
    match dir_override {
        Some(dir) => Some(dir.join("pages")),
        None => directories::ProjectDirs::from("", "", "wikitui")
            .map(|dirs| dirs.cache_dir().join("pages")),
    }
}

/// How much [`PageCache::wipe_incognito_entries`] actually removed —
/// reported at session end (and at the startup crash-recovery sweep) so the
/// reader can see incognito cleanup actually happened, mirroring FR-PF-4's
/// "transparency" posture for prefetch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WipeReport {
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Clone)]
pub struct PageCache {
    /// `None` when no cache directory could be determined — every lookup
    /// misses and every store is a no-op, but reading still works.
    dir: Option<PathBuf>,
    max_bytes: u64,
    fresh_ttl_secs: u64,
    force_refetch_secs: u64,
    /// PRD FR-PR-3: this run's incognito state, as it applies to *cache*
    /// writes specifically (`privacy::Write::Cache` is never denied —
    /// see that module's doc comment — only tagged for wipe). `Arc` so every
    /// clone of this `PageCache` (the background prefetch/revalidation
    /// executor holds one — see `main::run`'s `BgExecutor`) observes the
    /// same flag: a runtime `zz` toggle must affect fetches already in
    /// flight on the substrate, not just ones started after it.
    incognito: Arc<AtomicBool>,
}

pub struct CachedPage {
    pub html: String,
    /// Seconds since this content was fetched from the network (or, after a
    /// silent SWR "touch", since the last revalidation confirmed it's still
    /// current — see `touch_fetched_at`).
    pub age_secs: u64,
    /// The MediaWiki revision id this content was fetched at, or `0` when
    /// the server never told us (degraded mode — see the module doc
    /// comment).
    pub revid: u64,
    /// Carried through from the fetch that stored this entry. Not
    /// consumed by anything in this phase — conditional requests
    /// (`If-None-Match`) are PRD SP-5's spike, explicitly future work —
    /// but round-tripping it now means the index format doesn't need a
    /// second migration once that lands.
    #[allow(dead_code)]
    pub etag: Option<String>,
}

/// PRD FR-OFF-3 v1.0's SLRU refinement over the MVP's plain-mtime LRU: two
/// segments, so a binge session's flood of never-revisited articles (all
/// `Probationary`) can't flush the handful of articles a reader actually
/// keeps coming back to (`Protected`). New entries always start
/// `Probationary`; `PageCache::get_current_format` promotes an entry the
/// *second* time it's read (not the first — a single open is
/// indistinguishable from a one-off binge read, see that function's doc
/// comment). Eviction (`evict_to_cap`) always drains every `Probationary`
/// candidate, oldest first, before touching a single `Protected` one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum Segment {
    #[default]
    Probationary,
    Protected,
}

/// The on-disk shape of a `page/{wiki}/{lang}/{title}.json` title-index entry.
#[derive(Debug, Serialize, Deserialize)]
struct IndexEntry {
    revid: u64,
    fetched_at: u64,
    etag: Option<String>,
    /// PRD FR-OFF-3 v1.0 SLRU: how many times `get` has ever read this entry
    /// (monotonic, never reset — see `get_current_format`). Only the
    /// `Probationary` → `Protected` transition at `hits == 2` consults this;
    /// further reads of an already-`Protected` entry just keep incrementing
    /// it. `#[serde(default)]` so every entry written before this field
    /// existed loads as `0` (a fresh `Probationary` entry that simply
    /// hasn't been read again yet under the new policy — never
    /// retroactively protected).
    #[serde(default)]
    hits: u32,
    /// PRD FR-OFF-3 v1.0 SLRU: this entry's segment — see [`Segment`].
    /// `#[serde(default)]` for the same backward-compatibility reason as
    /// `hits`: every pre-SLRU entry loads as `Probationary`.
    #[serde(default)]
    segment: Segment,
    /// PRD FR-PR-3's "cache entries tagged for wipe at session end": `true`
    /// exactly when this entry was written while incognito was active.
    /// `#[serde(default)]` so every entry written before this field existed
    /// loads as `false` — never retroactively wiped. Set once, at write
    /// time (`put_at`); a silent revalidation `touch` (`touch_fetched_at`)
    /// preserves whatever this already was rather than re-deriving it from
    /// *this run's* current incognito state, since a touch confirms
    /// existing content is still current, it isn't a new write.
    #[serde(default)]
    incognito: bool,
    /// `wiki`/`lang`/`title` as passed to `put`, carried in the index entry
    /// itself (not just encoded in its path) so `evict_to_cap` and
    /// `wipe_incognito_entries` can recompute the exact blob path to delete
    /// alongside a tagged index, without re-deriving a title from a
    /// percent-encoded (or, for very long titles, hashed — see `safe_name`)
    /// filename. `#[serde(default)]` (empty string) for entries written
    /// before these fields existed; an empty `wiki` is exactly the default
    /// Wikipedia scope those entries belong to (see the module doc comment),
    /// so a pre-multi-wiki index reads back at its original `blob/{lang}/…`
    /// location with no special case.
    #[serde(default)]
    wiki: String,
    #[serde(default)]
    lang: String,
    #[serde(default)]
    title: String,
}

/// PRD FR-OFF-2's on-open staleness decision, as a pure function of age
/// alone (no I/O, no network) — table-tested in isolation from the
/// background-task plumbing that acts on it (`main::fetch_page`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwrDecision {
    /// Within the TTL: serve the cached copy and don't even check.
    Fresh,
    /// Past the TTL but within the force-refetch backstop: serve the
    /// cached copy immediately, then check in the background.
    RevalidateInBackground,
    /// Past the force-refetch backstop: treat as if there were no cached
    /// copy at all (network-first; the stale copy is still available as
    /// the offline-error fallback).
    ForceRefetch,
}

/// See [`SwrDecision`]. A free function (not just a method) so it's
/// directly table-testable without constructing a `PageCache`.
pub fn swr_decision(age_secs: u64, fresh_ttl_secs: u64, force_refetch_secs: u64) -> SwrDecision {
    if age_secs >= force_refetch_secs {
        SwrDecision::ForceRefetch
    } else if age_secs < fresh_ttl_secs {
        SwrDecision::Fresh
    } else {
        SwrDecision::RevalidateInBackground
    }
}

/// What a completed background revalidation (a cheap bare-metadata call,
/// PRD Appendix A's "Page metadata / latest revid" row) should do next, as
/// a pure function of the two revids involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevalidateAction {
    /// Same revid: nothing to refetch, just record that we checked
    /// (extends the TTL window silently — no user-visible notice).
    Touch,
    /// Different revid (or the cached copy's revid was never known, `0`):
    /// fetch the new HTML into L2 and surface the "updated" notice.
    Fetch,
}

/// See [`RevalidateAction`]. `cached_revid == 0` (the degraded/unknown
/// case) always fetches — there is nothing to compare against, so the only
/// honest move is to refresh and hope this time the server gives us a real
/// revid.
pub fn revalidate_action(cached_revid: u64, latest_revid: u64) -> RevalidateAction {
    if cached_revid != 0 && cached_revid == latest_revid {
        RevalidateAction::Touch
    } else {
        RevalidateAction::Fetch
    }
}

impl PageCache {
    /// `max_bytes`/`fresh_ttl_secs`/`force_refetch_secs` come from the
    /// resolved config's `[cache]` section (§6.7); the `DEFAULT_*`/
    /// `FRESH_TTL_SECS` constants are what that resolution falls back to
    /// absent a config file. `dir_override` is PRD FR-PR-5's `[cache] dir`
    /// (`None` means "use the platform cache directory" — see
    /// [`resolve_pages_dir`]).
    pub fn open(
        dir_override: Option<PathBuf>,
        max_bytes: u64,
        fresh_ttl_secs: u64,
        force_refetch_secs: u64,
    ) -> Self {
        match resolve_pages_dir(dir_override.as_deref()) {
            Some(dir) => Self::at(dir, max_bytes, fresh_ttl_secs, force_refetch_secs),
            None => Self::disabled(),
        }
    }

    /// A cache rooted at an explicit directory — tests, and `--config`'s
    /// `[cache]` override in production.
    pub fn at(dir: PathBuf, max_bytes: u64, fresh_ttl_secs: u64, force_refetch_secs: u64) -> Self {
        Self {
            dir: Some(dir),
            max_bytes,
            fresh_ttl_secs,
            force_refetch_secs,
            incognito: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A cache that never hits and never stores.
    pub fn disabled() -> Self {
        Self {
            dir: None,
            max_bytes: 0,
            fresh_ttl_secs: FRESH_TTL_SECS,
            force_refetch_secs: DEFAULT_FORCE_REFETCH_SECS,
            incognito: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Sets this run's incognito state (PRD FR-PR-3), from `--incognito` at
    /// startup and `zz`'s runtime toggle thereafter (`main::run`) — every
    /// clone of this cache (see the struct's `incognito` field doc comment)
    /// observes the change immediately.
    pub fn set_incognito(&self, on: bool) {
        self.incognito.store(on, Ordering::Relaxed);
    }

    fn is_incognito(&self) -> bool {
        self.incognito.load(Ordering::Relaxed)
    }

    /// This cache's configured on-open staleness decision for content this
    /// old — see the free function [`swr_decision`] for the pure logic.
    pub fn swr_decision(&self, age_secs: u64) -> SwrDecision {
        swr_decision(age_secs, self.fresh_ttl_secs, self.force_refetch_secs)
    }

    /// The wiki+lang path components shared by `blob_path`/`index_path`. The
    /// empty (default Wikipedia) scope contributes *no* `{wiki}` segment, so
    /// the resulting path is byte-identical to the pre-multi-wiki layout —
    /// see the module doc comment.
    fn scope_dir(root: PathBuf, wiki: &str, lang: &str) -> PathBuf {
        let root = if wiki.is_empty() {
            root
        } else {
            root.join(safe_name(wiki))
        };
        root.join(safe_name(lang))
    }

    fn blob_path(&self, wiki: &str, lang: &str, title: &str, revid: u64) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let title_hash = fnv1a(title.as_bytes());
        Some(
            Self::scope_dir(dir.join("blob"), wiki, lang)
                .join(format!("{revid}-{title_hash:016x}.zst")),
        )
    }

    fn index_path(&self, wiki: &str, lang: &str, title: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        Some(
            Self::scope_dir(dir.join("page"), wiki, lang)
                .join(format!("{}.json", safe_name(title))),
        )
    }

    /// The pre-upgrade flat-file location — still read (once) for
    /// migration; see the module doc comment.
    fn legacy_path(&self, lang: &str, title: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        Some(
            dir.join(safe_name(lang))
                .join(format!("{}.html", safe_name(title))),
        )
    }

    /// Looks up a page in `wiki`'s scope. Tries the current two-layer format
    /// first; on a miss *for the default wiki only*, falls back to a
    /// pre-upgrade flat file and migrates it in place (see the module doc
    /// comment) rather than refusing content that's still perfectly good.
    /// Either path touches mtimes as the LRU signal that this entry is still
    /// wanted.
    pub fn get(&self, wiki: &str, lang: &str, title: &str) -> Option<CachedPage> {
        self.get_current_format(wiki, lang, title)
            .or_else(|| self.migrate_legacy_entry(wiki, lang, title))
    }

    /// The side-effect-free read shared by [`Self::get`] and [`Self::peek`]:
    /// resolves the current-format index entry, reads and decompresses its
    /// blob, and returns both (with the two on-disk paths) touching nothing.
    /// Whether the read-side LRU/SLRU touch is then applied is the caller's
    /// choice — `get` applies it, `peek` doesn't.
    fn read_current_format(
        &self,
        wiki: &str,
        lang: &str,
        title: &str,
    ) -> Option<(PathBuf, PathBuf, IndexEntry, String)> {
        let index_path = self.index_path(wiki, lang, title)?;
        let text = std::fs::read_to_string(&index_path).ok()?;
        let entry: IndexEntry = serde_json::from_str(&text).ok()?;
        let blob_path = self.blob_path(wiki, lang, title, entry.revid)?;
        let compressed = std::fs::read(&blob_path).ok()?;
        let html_bytes = zstd::stream::decode_all(compressed.as_slice()).ok()?;
        let html = String::from_utf8(html_bytes).ok()?;
        Some((index_path, blob_path, entry, html))
    }

    fn get_current_format(&self, wiki: &str, lang: &str, title: &str) -> Option<CachedPage> {
        let (index_path, blob_path, entry, html) = self.read_current_format(wiki, lang, title)?;

        // Read-side LRU/SLRU touch — the whole reason `get` differs from
        // `peek`. mtime is the recency signal; the hit bump (via `touch_hit`,
        // which re-reads fresh so a concurrent `put_at` isn't clobbered) is
        // the SLRU promotion signal. Best-effort — a failure just means this
        // read went uncounted, never a miss or a panic.
        touch_mtime(&index_path);
        touch_mtime(&blob_path);
        self.touch_hit(&index_path);

        let age_secs = now_unix().saturating_sub(entry.fetched_at);
        Some(CachedPage {
            html,
            age_secs,
            revid: entry.revid,
            etag: entry.etag,
        })
    }

    /// A side-effect-free cache read (PRD FR-SR-7's reindex): identical
    /// content to [`Self::get`] for a current-format entry, but touches
    /// nothing — no mtime, no hit count, no SLRU promotion — and never
    /// migrates a legacy flat file. `reindex` bulk-reads *every* cached entry
    /// to rebuild the offline search index; routing that through `get` would
    /// promote every one-open article to `Protected` and flatten every
    /// entry's mtime to "now", defeating exactly the SLRU/recency ordering
    /// eviction depends on (CORR-M8). An indexer sees the same bytes a reader
    /// would, without disturbing the reader's eviction state.
    pub fn peek(&self, wiki: &str, lang: &str, title: &str) -> Option<CachedPage> {
        let (_, _, entry, html) = self.read_current_format(wiki, lang, title)?;
        let age_secs = now_unix().saturating_sub(entry.fetched_at);
        Some(CachedPage {
            html,
            age_secs,
            revid: entry.revid,
            etag: entry.etag,
        })
    }

    /// Applies the read-side SLRU hit bump to the entry at `index_path`,
    /// re-reading it *fresh* immediately before the atomic write-back rather
    /// than mutating the copy `get` already read. This is the title index's
    /// concurrency contract (CORR-M8): `put_at` is the authoritative writer of
    /// the content pointer (revid/fetched_at/etag), and a background
    /// revalidation may advance it between `get`'s read and this touch;
    /// starting from the freshest on-disk entry means the hit bump layers onto
    /// whatever `put_at` most recently wrote instead of resurrecting the stale
    /// revid `get` happened to read. The write goes through the atomic helper,
    /// so a reader never observes a torn index. Best-effort — an unreadable or
    /// corrupt index just means this read went uncounted, never a panic.
    fn touch_hit(&self, index_path: &Path) {
        let Ok(text) = std::fs::read_to_string(index_path) else {
            return;
        };
        let Ok(mut entry) = serde_json::from_str::<IndexEntry>(&text) else {
            return;
        };
        // PRD FR-OFF-3 v1.0 SLRU: the *second* read (not the first, merely
        // "opened once, same as any binge read") promotes an entry out of the
        // probationary segment.
        entry.hits = entry.hits.saturating_add(1);
        if entry.segment == Segment::Probationary && entry.hits >= 2 {
            entry.segment = Segment::Protected;
        }
        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = crate::atomicio::write_atomic(index_path, json.as_bytes());
        }
    }

    fn migrate_legacy_entry(&self, wiki: &str, lang: &str, title: &str) -> Option<CachedPage> {
        // The pre-upgrade flat-file format predates multi-wiki support, so it
        // only ever existed for the default Wikipedia scope; a non-default
        // wiki has no legacy file to fold in (and must not read the default
        // wiki's).
        if !wiki.is_empty() {
            return None;
        }
        let old_path = self.legacy_path(lang, title)?;
        let raw = std::fs::read_to_string(&old_path).ok()?;
        let (first_line, html) = raw.split_once('\n')?;
        let fetched_at: u64 = first_line.trim().parse().ok()?;

        // Fold it into the new format at its original fetched_at (an
        // honest age, not a falsely-fresh "just fetched now") before ever
        // returning success, then remove the old file — the "read once,
        // then rewritten" migration this module documents. Tagged
        // *non*-incognito unconditionally (CORR-M1): a pre-upgrade flat file
        // predates incognito entirely, so folding it in during an incognito
        // session must not mark this pre-existing content for the session-end
        // wipe — that would delete a warmed cache entry the reader never chose
        // to make private, and `migrate` also deletes the legacy source, so
        // the loss would be permanent.
        self.put_tagged(wiki, lang, title, html, 0, None, fetched_at, false);
        let _ = std::fs::remove_file(&old_path);

        let age_secs = now_unix().saturating_sub(fetched_at);
        Some(CachedPage {
            html: html.to_string(),
            age_secs,
            revid: 0,
            etag: None,
        })
    }

    /// Stores a freshly fetched page at `revid`, then evicts
    /// least-recently-used entries if the cache has grown past its cap.
    /// Content already on disk at this exact revid is left untouched
    /// (FR-OFF-1's immutability contract — a revid never gets rewritten,
    /// only the title index's bookkeeping moves forward); everything else
    /// (a full miss, or a full disk) degrades to a silent no-op rather
    /// than breaking reading.
    pub fn put(
        &self,
        wiki: &str,
        lang: &str,
        title: &str,
        html: &str,
        revid: u64,
        etag: Option<&str>,
    ) {
        self.put_at(wiki, lang, title, html, revid, etag, now_unix());
    }

    // Wiki + lang + title + html + revid + etag + fetched_at: the wiki scope
    // (PRD FR-ML-4) is one dimension past clippy's arg ceiling, but bundling
    // the cache key into a struct would only move the noise, not remove it.
    #[allow(clippy::too_many_arguments)]
    fn put_at(
        &self,
        wiki: &str,
        lang: &str,
        title: &str,
        html: &str,
        revid: u64,
        etag: Option<&str>,
        fetched_at: u64,
    ) {
        // A brand-new entry written during this session takes this session's
        // incognito state; a re-put of an existing entry carries the existing
        // flag forward instead (see `put_tagged`).
        self.put_tagged(
            wiki,
            lang,
            title,
            html,
            revid,
            etag,
            fetched_at,
            self.is_incognito(),
        );
    }

    // Wiki + lang + title + html + revid + etag + fetched_at + the brand-new
    // entry's incognito tag: two dimensions past clippy's ceiling for the same
    // reason `put_at` is one past it. `new_entry_incognito` is only consulted
    // when there is no existing index entry to carry a flag forward from —
    // `migrate_legacy_entry` passes `false` so a pre-upgrade file it folds in
    // is never marked private, while the normal `put_at` passes this session's
    // state.
    #[allow(clippy::too_many_arguments)]
    fn put_tagged(
        &self,
        wiki: &str,
        lang: &str,
        title: &str,
        html: &str,
        revid: u64,
        etag: Option<&str>,
        fetched_at: u64,
        new_entry_incognito: bool,
    ) {
        // PRD FR-PR-3's single gate: every cache write consults it too, not
        // just history/prefetch — `Write::Cache` always resolves to `Allow`
        // (see `privacy`'s module doc comment for why a cache write is never
        // denied outright), so this is a documented invariant check, not a
        // second, independent decision about whether to write.
        debug_assert_eq!(
            crate::privacy::decide(self.is_incognito(), crate::privacy::Write::Cache),
            crate::privacy::Verdict::Allow,
            "a cache write must never be denied outright — see privacy's module doc comment"
        );
        let (Some(blob_path), Some(index_path)) = (
            self.blob_path(wiki, lang, title, revid),
            self.index_path(wiki, lang, title),
        ) else {
            return;
        };

        if blob_path.exists() {
            // Immutable: this exact revid's bytes are never rewritten.
            touch_mtime(&blob_path);
        } else {
            let Ok(compressed) = zstd::stream::encode_all(html.as_bytes(), ZSTD_LEVEL) else {
                return;
            };
            if let Some(parent) = blob_path.parent()
                && std::fs::create_dir_all(parent).is_err()
            {
                return;
            }
            if std::fs::write(&blob_path, &compressed).is_err() {
                return;
            }
        }

        // PRD FR-OFF-3 v1.0 SLRU + FR-PR-3 incognito: a `put` for a title
        // already in the index (a revalidation-driven refetch, a ForceRefetch
        // reopen, a legacy migration) rewrites this same JSON file — carry its
        // existing `hits`/`segment` *and* `incognito` flag forward rather than
        // re-deriving them from this session. Re-deriving `hits`/`segment`
        // would strip a favorite of its eviction protection on every refresh;
        // re-deriving `incognito` from *this* session (CORR-M1) would tag a
        // pre-existing, non-incognito entry for the session-end wipe merely
        // because it was re-put while incognito — deleting content the reader
        // never chose to make private. Only a brand-new title, with nothing on
        // disk to carry forward, takes the `Probationary`/`0` default and
        // `new_entry_incognito`.
        let (hits, segment, incognito) = match std::fs::read_to_string(&index_path)
            .ok()
            .and_then(|text| serde_json::from_str::<IndexEntry>(&text).ok())
        {
            Some(existing) => (existing.hits, existing.segment, existing.incognito),
            None => (0, Segment::default(), new_entry_incognito),
        };

        let entry = IndexEntry {
            revid,
            fetched_at,
            etag: etag.map(str::to_string),
            incognito,
            wiki: wiki.to_string(),
            lang: lang.to_string(),
            title: title.to_string(),
            hits,
            segment,
        };
        let Ok(json) = serde_json::to_string(&entry) else {
            return;
        };
        // Atomic write (CORR-M8): a torn index write could otherwise leave a
        // half-written JSON a reader parses as a miss, or clobber a
        // concurrently-written fresher entry with a truncated one.
        if crate::atomicio::write_atomic(&index_path, json.as_bytes()).is_err() {
            return;
        }
        self.evict_to_cap();
    }

    /// The silent half of PRD FR-OFF-2's stale-while-revalidate: a
    /// background check found the same revid still current, so there's
    /// nothing to refetch — just move `fetched_at` forward so the reader
    /// doesn't pay for another revalidation for another TTL window. Never
    /// shown to the user (no notice, no status line change); a missing or
    /// corrupt index entry is a silent no-op like every other cache miss.
    pub fn touch_fetched_at(&self, wiki: &str, lang: &str, title: &str) {
        let Some(index_path) = self.index_path(wiki, lang, title) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&index_path) else {
            return;
        };
        let Ok(mut entry) = serde_json::from_str::<IndexEntry>(&text) else {
            return;
        };
        entry.fetched_at = now_unix();
        if let Ok(json) = serde_json::to_string(&entry) {
            let _ = crate::atomicio::write_atomic(&index_path, json.as_bytes());
        }
        if let Some(blob_path) = self.blob_path(wiki, lang, title, entry.revid) {
            touch_mtime(&blob_path);
        }
    }

    /// Deletes entries until the `blob/`+`page/` tree fits its cap again
    /// (PRD FR-OFF-3 v1.0's SLRU refinement over the MVP's hard-cap-plus-
    /// plain-LRU). Every `Segment::Probationary` candidate is drained
    /// (oldest mtime first) before a single `Segment::Protected` one is
    /// touched — see [`Segment`]'s doc comment for the promotion rule that
    /// gets an entry into the protected segment in the first place.
    ///
    /// The protected segment additionally carries its own soft budget,
    /// [`PROTECTED_SEGMENT_FRACTION`] of the overall cap: if literally every
    /// entry got re-read at some point, "protected" would otherwise mean
    /// "un-evictable", and the cache could never shrink back under cap at
    /// all. So the *oldest* protected entries beyond that budget are
    /// spliced onto the eviction order right after the (exhausted)
    /// probationary ones — still oldest-first, never ahead of a genuinely
    /// untouched probationary entry, but no longer immune either.
    ///
    /// A "candidate" here is a *logical* entry — a `page/{lang}/{title}.json`
    /// index plus the blob it currently points at, evicted together, so
    /// SLRU segment tracking (which lives in the index JSON) governs both
    /// halves as one unit. A file this pass can't attribute to a readable
    /// index — a corrupted index, or a blob orphaned by a superseded revid
    /// (PRD FR-OFF-1: a title's blob at an old revid is never rewritten,
    /// just superseded) — is still swept, exactly as the pre-SLRU flat scan
    /// did: it becomes its own single-file candidate, always `Probationary`
    /// (the least protected treatment for something this pass knows nothing
    /// about).
    fn evict_to_cap(&self) {
        let Some(dir) = self.dir.as_ref() else {
            return;
        };
        let mut all_files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        collect_files(&dir.join("blob"), &mut all_files);
        collect_files(&dir.join("page"), &mut all_files);
        let total: u64 = all_files.iter().map(|(_, size, _)| size).sum();
        if total <= self.max_bytes {
            return;
        }
        let mut excess = total - self.max_bytes;

        struct Candidate {
            files: Vec<(PathBuf, u64)>,
            mtime: SystemTime,
            segment: Segment,
        }

        let mut index_files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        collect_files(&dir.join("page"), &mut index_files);

        let mut claimed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut candidates: Vec<Candidate> = Vec::new();

        for (index_path, index_size, index_mtime) in &index_files {
            let Ok(text) = std::fs::read_to_string(index_path) else {
                continue; // falls through to the orphan pass below.
            };
            let Ok(entry) = serde_json::from_str::<IndexEntry>(&text) else {
                continue; // corrupt index: same fallback.
            };
            let mut files = vec![(index_path.clone(), *index_size)];
            claimed.insert(index_path.clone());
            if let Some(blob_path) =
                self.blob_path(&entry.wiki, &entry.lang, &entry.title, entry.revid)
                && let Ok(meta) = std::fs::metadata(&blob_path)
            {
                files.push((blob_path.clone(), meta.len()));
                claimed.insert(blob_path);
            }
            candidates.push(Candidate {
                files,
                mtime: *index_mtime,
                segment: entry.segment,
            });
        }

        // Orphans: anything under blob/ or page/ that no readable index
        // claimed above.
        for (path, size, mtime) in &all_files {
            if !claimed.contains(path) {
                candidates.push(Candidate {
                    files: vec![(path.clone(), *size)],
                    mtime: *mtime,
                    segment: Segment::Probationary,
                });
            }
        }

        let (mut protected, mut probationary): (Vec<Candidate>, Vec<Candidate>) = candidates
            .into_iter()
            .partition(|c| c.segment == Segment::Protected);
        probationary.sort_by_key(|c| c.mtime);
        protected.sort_by_key(|c| c.mtime);

        let candidate_size = |c: &Candidate| -> u64 { c.files.iter().map(|(_, s)| s).sum() };
        let protected_total: u64 = protected.iter().map(candidate_size).sum();
        let protected_cap = (self.max_bytes as f64 * PROTECTED_SEGMENT_FRACTION) as u64;
        let mut over_budget = protected_total.saturating_sub(protected_cap);
        let mut still_protected = Vec::new();
        let mut protected_overflow = Vec::new();
        for c in protected {
            if over_budget > 0 {
                over_budget = over_budget.saturating_sub(candidate_size(&c));
                protected_overflow.push(c);
            } else {
                still_protected.push(c);
            }
        }

        let eviction_order = probationary
            .into_iter()
            .chain(protected_overflow)
            .chain(still_protected);

        'evict: for candidate in eviction_order {
            for (path, size) in &candidate.files {
                if excess == 0 {
                    break 'evict;
                }
                if std::fs::remove_file(path).is_ok() {
                    excess = excess.saturating_sub(*size);
                }
            }
        }
    }

    /// PRD FR-PR-3's "cache entries tagged for wipe at session end": deletes
    /// every title-index entry written with `incognito = true` (see
    /// `IndexEntry`'s doc comment), plus the blob it points at when one is
    /// still there. Called unconditionally — both at the end of a clean run
    /// (`main::run`, right before returning) and once at the *start* of every
    /// run (`main`, right after opening the cache), the latter being the
    /// documented mitigation for a crash mid-incognito-session: the tag
    /// itself is the durable record, so a run that never got to clean up
    /// finds its own leftovers swept the next time wikitui starts, whether or
    /// not that next run is incognito too. Idempotent (a second call over an
    /// already-clean tree finds nothing) and best-effort like every other
    /// write in this module — a delete that fails (permissions, a concurrent
    /// second instance) is simply not counted, never a crash.
    pub fn wipe_incognito_entries(&self) -> WipeReport {
        let mut report = WipeReport::default();
        let Some(dir) = self.dir.as_ref() else {
            return report;
        };
        let mut index_files = Vec::new();
        collect_files(&dir.join("page"), &mut index_files);
        for (path, size, _) in index_files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_str::<IndexEntry>(&text) else {
                continue;
            };
            if !entry.incognito {
                continue;
            }
            let mut freed = 0u64;
            if let Some(blob_path) =
                self.blob_path(&entry.wiki, &entry.lang, &entry.title, entry.revid)
                && let Ok(blob_meta) = std::fs::metadata(&blob_path)
            {
                freed += blob_meta.len();
                let _ = std::fs::remove_file(&blob_path);
            }
            if std::fs::remove_file(&path).is_ok() {
                report.entries += 1;
                report.bytes += freed + size;
            }
        }
        report
    }

    /// Every currently-cached `(wiki, lang, title)` triple, read straight
    /// from the `page/` title-index JSON files (which already carry all
    /// three — see `IndexEntry`'s doc comment) rather than a second on-disk
    /// naming scheme. `offline_search`'s `reindex` (PRD FR-SR-7) is the one
    /// caller: it walks this list and re-fetches each entry's HTML via
    /// [`Self::get`] to rebuild the search index from whatever L2 currently
    /// holds. An unreadable or corrupt index file is skipped, the same
    /// best-effort posture every other read in this module takes — a
    /// reindex over a partially corrupt cache still indexes everything it
    /// can, rather than refusing outright.
    pub fn list_entries(&self) -> Vec<(String, String, String)> {
        let Some(dir) = self.dir.as_ref() else {
            return Vec::new();
        };
        let mut index_files = Vec::new();
        collect_files(&dir.join("page"), &mut index_files);
        let mut out = Vec::new();
        for (path, _, _) in index_files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_str::<IndexEntry>(&text) else {
                continue;
            };
            out.push((entry.wiki, entry.lang, entry.title));
        }
        out
    }
}

fn touch_mtime(path: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn collect_files(dir: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if let Ok(meta) = entry.metadata() {
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            out.push((path, meta.len(), mtime));
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Percent-encoding keeps names readable and filesystem-safe (no '/', no
/// ':'); very long titles could exceed the common 255-byte filename limit
/// once encoded, so those fall back to a stable hash. `pub(crate)` so the
/// pinned saved-pages store (`saved.rs`) derives on-disk names the same way,
/// rather than a second copy of this rule.
pub(crate) fn safe_name(s: &str) -> String {
    let encoded = urlencoding::encode(s).into_owned();
    if encoded.len() > 200 {
        format!("h{:016x}", fnv1a(s.as_bytes()))
    } else {
        encoded
    }
}

/// FNV-1a: tiny, dependency-free, and stable across runs and Rust
/// versions (unlike `DefaultHasher`), which cache filenames require.
/// `pub(crate)` so `saved.rs` shares the exact hash the cache's `safe_name`
/// long-title fallback uses.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// "42s" / "5m" / "3h" / "2d" — the status bar's cache-age display.
pub fn age_human(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_cache(max_bytes: u64) -> (PageCache, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("wikitui-cache-test-{}-{n}", std::process::id()));
        (
            PageCache::at(
                dir.clone(),
                max_bytes,
                FRESH_TTL_SECS,
                DEFAULT_FORCE_REFETCH_SECS,
            ),
            dir,
        )
    }

    #[test]
    fn round_trips_a_page_and_reports_a_small_age() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put(
            "",
            "en",
            "Alan Turing",
            "<html>body</html>",
            100,
            Some("W/\"100/abc\""),
        );
        let hit = cache.get("", "en", "Alan Turing").expect("hit");
        assert_eq!(hit.html, "<html>body</html>");
        assert_eq!(hit.revid, 100);
        assert_eq!(hit.etag.as_deref(), Some("W/\"100/abc\""));
        assert!(
            hit.age_secs < 5,
            "freshly written, age was {}",
            hit.age_secs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The zstd round trip is the whole point of L2's storage format: what
    /// comes back out of `get` must be byte-identical to what went into
    /// `put`, including content that compresses unusually (empty, and
    /// non-ASCII/multi-byte text).
    #[test]
    fn html_survives_zstd_round_trip_including_empty_and_multibyte_content() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Empty", "", 1, None);
        assert_eq!(cache.get("", "en", "Empty").unwrap().html, "");

        let multibyte = "<p>チューリング — café — 🎉</p>".repeat(200);
        cache.put("", "en", "Multibyte", &multibyte, 2, None);
        assert_eq!(cache.get("", "en", "Multibyte").unwrap().html, multibyte);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The index header's fields (revid, fetched_at, etag) must round-trip
    /// exactly, including the `etag: None` case (older/degraded fetches).
    #[test]
    fn index_entry_header_round_trips_with_and_without_an_etag() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "WithEtag", "html", 42, Some("W/\"42/xyz\""));
        cache.put("", "en", "NoEtag", "html", 0, None);

        let with = cache.get("", "en", "WithEtag").unwrap();
        assert_eq!(with.revid, 42);
        assert_eq!(with.etag.as_deref(), Some("W/\"42/xyz\""));

        let without = cache.get("", "en", "NoEtag").unwrap();
        assert_eq!(without.revid, 0);
        assert_eq!(without.etag, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FR-OFF-1's immutability contract: content already stored at a given
    /// revid is never rewritten, even if `put` is called again for that
    /// same (lang, title, revid) with different bytes (a bug elsewhere, or
    /// a deliberately hostile server) — the first write wins, permanently.
    #[test]
    fn content_at_a_given_revid_is_never_rewritten() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "version one", 7, Some("W/\"7/aaa\""));
        cache.put(
            "",
            "en",
            "Turing",
            "version two — must not land",
            7,
            Some("W/\"7/bbb\""),
        );

        let hit = cache.get("", "en", "Turing").unwrap();
        assert_eq!(
            hit.html, "version one",
            "the blob at revid 7 must be untouched"
        );
        // The index's own bookkeeping (fetched_at/etag) is allowed to move
        // forward even though the content didn't — only the content is
        // immutable, not the "what do we currently believe" pointer.
        assert_eq!(hit.etag.as_deref(), Some("W/\"7/bbb\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A new revid for the same title is a genuinely new blob, not an
    /// overwrite — proving immutability is scoped to "this revid", not "this
    /// title forever".
    #[test]
    fn a_new_revid_for_the_same_title_is_a_new_blob_not_an_overwrite() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "old content", 1, None);
        cache.put("", "en", "Turing", "new content", 2, None);
        assert_eq!(cache.get("", "en", "Turing").unwrap().html, "new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_fresh_style_ttl_is_configurable_not_hardcoded() {
        let short_ttl = PageCache::at(
            std::env::temp_dir(),
            DEFAULT_MAX_BYTES,
            10,
            DEFAULT_FORCE_REFETCH_SECS,
        );
        assert_eq!(short_ttl.swr_decision(5), SwrDecision::Fresh);
        assert_eq!(
            short_ttl.swr_decision(15),
            SwrDecision::RevalidateInBackground,
            "config TTL of 10s must be honored, not the 24h default"
        );
    }

    #[test]
    fn miss_on_unknown_title_and_on_disabled_cache() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        assert!(cache.get("", "en", "Nonexistent").is_none());
        let disabled = PageCache::disabled();
        disabled.put("", "en", "X", "<html/>", 1, None); // must not panic
        assert!(disabled.get("", "en", "X").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_are_isolated_per_language_and_from_each_other() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "english", 1, None);
        cache.put("", "de", "Turing", "german", 1, None);
        cache.put("", "en", "Church", "church", 2, None);
        assert_eq!(cache.get("", "en", "Turing").unwrap().html, "english");
        assert_eq!(cache.get("", "de", "Turing").unwrap().html, "german");
        assert_eq!(cache.get("", "en", "Church").unwrap().html, "church");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PRD FR-ML-4 — the headline correctness fix. The same `(lang, title)`
    /// stored on two different wikis must never collide: a `get` on wiki B
    /// for a title only wiki A has cached is a **miss** (so the caller
    /// fetches B's real content), and wiki A's entry is left intact. Against
    /// the pre-scoping code — where the on-disk key had no wiki dimension —
    /// this `get` would have returned wiki A's content, silently serving the
    /// wrong wiki's article. It now fails that way no more.
    #[test]
    fn same_title_on_two_wikis_never_collides() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("wikipedia_x", "en", "Alan Turing", "encyclopedia", 10, None);

        // A different wiki has never cached this title — a miss, not wiki A's
        // content served under the wrong project.
        assert!(
            cache.get("wiktionary", "en", "Alan Turing").is_none(),
            "a different wiki must miss, never inherit another wiki's cached copy"
        );

        // Fetching it on wiki B stores B's own content; both wikis now serve
        // their own, and neither can see the other's.
        cache.put("wiktionary", "en", "Alan Turing", "dictionary", 20, None);
        assert_eq!(
            cache.get("wikipedia_x", "en", "Alan Turing").unwrap().html,
            "encyclopedia"
        );
        assert_eq!(
            cache.get("wiktionary", "en", "Alan Turing").unwrap().html,
            "dictionary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The default (Wikipedia) scope is the empty string, and its on-disk
    /// paths carry no `{wiki}` segment — byte-identical to what the
    /// pre-multi-wiki code wrote. This pins that: a default-scope entry lands
    /// exactly at `page/{lang}/…` and `blob/{lang}/…`, so every cache
    /// warmed before wikis could be switched is read back with no migration.
    /// A non-default wiki gets its own `{wiki}` subtree instead.
    #[test]
    fn default_wiki_uses_the_legacy_pathless_layout_and_others_get_a_subtree() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "default", 1, None);
        cache.put("wiktionary", "en", "Turing", "sister", 2, None);

        // Default scope: no wiki segment (the exact pre-scoping location).
        assert!(dir.join("page").join("en").join("Turing.json").exists());
        assert!(
            dir.join("blob")
                .join("en")
                .join(format!("1-{:016x}.zst", fnv1a(b"Turing")))
                .exists()
        );
        // Non-default scope: a disjoint subtree, so it can never overwrite or
        // be evicted alongside the default wiki's identically-named entry.
        assert!(
            dir.join("page")
                .join("wiktionary")
                .join("en")
                .join("Turing.json")
                .exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Migration proof: a new-format index written before the `wiki` field
    /// existed (so its JSON has no `wiki` key) loads via `#[serde(default)]`
    /// as the empty/default scope and is served to the default wiki — never
    /// refetched, never lost — while a non-default wiki correctly misses it.
    #[test]
    fn a_pre_wiki_index_entry_is_read_as_the_default_wiki() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        // Write the blob at the default (segment-less) location, then hand-write
        // an index JSON with no `wiki` field — exactly what the old code left.
        cache.put("", "en", "Legacy", "old content", 0, None);
        let index_path = dir.join("page").join("en").join("Legacy.json");
        std::fs::write(
            &index_path,
            r#"{"revid":0,"fetched_at":1,"etag":null,"lang":"en","title":"Legacy"}"#,
        )
        .unwrap();

        assert_eq!(
            cache.get("", "en", "Legacy").unwrap().html,
            "old content",
            "a pre-wiki index entry must read back as the default wiki"
        );
        assert!(
            cache.get("wiktionary", "en", "Legacy").is_none(),
            "a non-default wiki must not inherit the default wiki's legacy entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pre-upgrade flat `.html` file predates multi-wiki support, so it is
    /// migrated only for the default wiki; a non-default wiki must not read
    /// (or migrate) it as its own.
    #[test]
    fn a_non_default_wiki_ignores_the_legacy_flat_file() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        write_legacy_entry(&dir, "en", "Turing", now_unix(), "<html>legacy</html>");
        assert!(
            cache.get("wiktionary", "en", "Turing").is_none(),
            "the pre-upgrade flat file belongs to the default wiki only"
        );
        // The default wiki still migrates it, so nothing is lost.
        assert_eq!(
            cache.get("", "en", "Turing").unwrap().html,
            "<html>legacy</html>"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slashes_and_spaces_in_titles_are_safe() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "AC/DC", "band", 1, None);
        cache.put("", "en", "New York City", "city", 2, None);
        assert_eq!(cache.get("", "en", "AC/DC").unwrap().html, "band");
        assert_eq!(cache.get("", "en", "New York City").unwrap().html, "city");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn very_long_titles_fall_back_to_a_hash_and_still_round_trip() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        let long_a = "統合デジタル通信網".repeat(15); // percent-encodes far past 200 bytes
        let long_b = format!("{long_a}違"); // near-identical sibling must not collide
        cache.put("", "en", &long_a, "content a", 1, None);
        cache.put("", "en", &long_b, "content b", 2, None);
        assert_eq!(cache.get("", "en", &long_a).unwrap().html, "content a");
        assert_eq!(cache.get("", "en", &long_b).unwrap().html, "content b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_index_entry_is_a_miss_not_a_panic() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "good", 1, None);
        // Overwrite the index (not the blob) with garbage JSON.
        let index_path = dir.join("page").join("en").join("Turing.json");
        std::fs::write(&index_path, "not json").unwrap();
        assert!(cache.get("", "en", "Turing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_blob_is_a_miss_not_a_panic() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "good", 5, None);
        let blob_path = dir
            .join("blob")
            .join("en")
            .join(format!("5-{:016x}.zst", fnv1a(b"Turing")));
        std::fs::write(&blob_path, b"not zstd data at all").unwrap();
        assert!(cache.get("", "en", "Turing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PRD FR-OFF-2's silent-touch path: `fetched_at` moves forward without
    /// touching the stored content at all.
    #[test]
    fn touch_fetched_at_refreshes_age_without_changing_content() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "content", 3, None);
        // Force an old fetched_at directly so the "before" age is visibly
        // large, then touch and confirm it collapses back near zero.
        let index_path = dir.join("page").join("en").join("Turing.json");
        let stale = IndexEntry {
            revid: 3,
            fetched_at: now_unix() - 100_000,
            etag: None,
            incognito: false,
            wiki: String::new(),
            lang: "en".to_string(),
            title: "Turing".to_string(),
            hits: 0,
            segment: Segment::Probationary,
        };
        std::fs::write(&index_path, serde_json::to_string(&stale).unwrap()).unwrap();
        assert!(cache.get("", "en", "Turing").unwrap().age_secs >= 100_000 - 5);

        cache.touch_fetched_at("", "en", "Turing");
        let touched = cache.get("", "en", "Turing").unwrap();
        assert!(
            touched.age_secs < 5,
            "touch must reset the age, got {}",
            touched.age_secs
        );
        assert_eq!(
            touched.html, "content",
            "touch must not alter the stored content"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn touch_fetched_at_on_missing_entry_does_not_panic() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.touch_fetched_at("", "en", "Nonexistent"); // must not panic
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Old-format migration --------------------------------------------

    /// Writes a pre-upgrade flat-file cache entry directly (bypassing
    /// `put`, which only ever writes the new format) — simulates a cache
    /// directory left over from before this module existed.
    fn write_legacy_entry(dir: &Path, lang: &str, title: &str, fetched_at: u64, html: &str) {
        let path = dir.join(lang).join(format!("{title}.html"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{fetched_at}\n{html}")).unwrap();
    }

    #[test]
    fn old_format_entry_is_read_once_then_migrated_to_the_new_layout() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        let old_fetched_at = now_unix() - 3600; // 1h old, preserved through migration
        write_legacy_entry(&dir, "en", "Turing", old_fetched_at, "<html>legacy</html>");

        let hit = cache
            .get("", "en", "Turing")
            .expect("legacy entry must still be readable");
        assert_eq!(hit.html, "<html>legacy</html>");
        assert_eq!(hit.revid, 0, "revid is unknown for pre-upgrade entries");
        assert!(
            (hit.age_secs as i64 - 3600).abs() < 5,
            "the original fetched_at must be preserved, not reset to now: age {}",
            hit.age_secs
        );

        // The old file is gone, and the new-format files exist in its place.
        assert!(!dir.join("en").join("Turing.html").exists());
        assert!(dir.join("page").join("en").join("Turing.json").exists());

        // A second read no longer touches (or needs) the old path at all —
        // it's served straight from the new format now.
        let second = cache
            .get("", "en", "Turing")
            .expect("still readable after migration");
        assert_eq!(second.html, "<html>legacy</html>");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_old_format_entry_is_a_miss_not_a_panic() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        let path = dir.join("en").join("Turing.html");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not-a-timestamp\nhtml").unwrap();
        assert!(cache.get("", "en", "Turing").is_none());
        // And a file with no newline at all:
        std::fs::write(&path, "no newline here").unwrap();
        assert!(cache.get("", "en", "Turing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_format_entry_is_preferred_over_a_stale_leftover_old_format_file() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "new content", 9, None);
        // A leftover old-format file for the same title must never be
        // consulted once the new format has an entry.
        write_legacy_entry(&dir, "en", "Turing", now_unix(), "stale old content");
        assert_eq!(cache.get("", "en", "Turing").unwrap().html, "new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Eviction ----------------------------------------------------------

    /// Deterministic-length, high-entropy filler so its compressed size is
    /// governed by `len` (zstd can't find repeats in it), not by the
    /// specific seed — the eviction test below needs entries whose disk
    /// footprint it can reason about without hardcoding a zstd ratio.
    fn pseudo_random_content(seed: u64, len: usize) -> String {
        use base64::Engine as _;
        let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(len);
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }

    fn dir_size(dir: &Path) -> u64 {
        let mut entries = Vec::new();
        collect_files(&dir.join("blob"), &mut entries);
        collect_files(&dir.join("page"), &mut entries);
        entries.iter().map(|(_, size, _)| size).sum()
    }

    /// Pins a file's mtime to an exact instant rather than trusting real
    /// wall-clock separation between rapid successive writes: fs mtime
    /// resolution is coarse enough (and slow enough under full-suite
    /// parallel load to *not* help) that two writes milliseconds apart can
    /// land in the same tick, leaving `evict_to_cap`'s oldest-first sort to
    /// fall back on directory-read order — which has no relation to which
    /// entry was actually read least recently. Reuses the same
    /// `set_modified` call `touch_mtime` (the production LRU signal) makes,
    /// just with an explicit target instead of "now".
    fn stamp_mtime(path: &Path, time: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(time))
            .expect("stamping mtime for a deterministic LRU order in this test");
    }

    #[test]
    fn evicts_least_recently_read_entries_once_over_cap_counting_blob_and_index_bytes() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wikitui-cache-evict-test-{}-{n}",
            std::process::id()
        ));

        // The same bytes for all three entries (only revid/title differ):
        // real zstd ratios vary slightly by input, so different seeds here
        // used to make the number of evictions needed depend on which
        // entry happened to compress smallest rather than on the LRU logic
        // under test. Equal content means equal-sized blobs, so the tight
        // `+ 20` cap below is exact regardless of compression variance.
        let content = pseudo_random_content(1, 400);

        // Write two entries with a generous cap (no eviction pressure),
        // then measure their real combined blob+index footprint so the
        // tightened cap below is exact — not a guess about zstd's ratio.
        let roomy = PageCache::at(
            dir.clone(),
            DEFAULT_MAX_BYTES,
            FRESH_TTL_SECS,
            DEFAULT_FORCE_REFETCH_SECS,
        );
        roomy.put("", "en", "First", &content, 1, None);
        roomy.put("", "en", "Second", &content, 2, None);
        // Small safety margin: at this cap, adding a same-sized third entry
        // forces eviction of exactly one LRU victim, never two.
        let cap = dir_size(&dir) + 20;

        let cache = PageCache::at(dir.clone(), cap, FRESH_TTL_SECS, DEFAULT_FORCE_REFETCH_SECS);
        // Read First so Second becomes the least-recently-used entry —
        // proving eviction follows reads, not just insertion order.
        assert!(cache.get("", "en", "First").is_some());

        // `get` just touched First's mtime to real "now", but real
        // wall-clock mtimes aren't a reliable ordering signal on their own
        // (see `stamp_mtime`'s doc comment) — pin both entries' mtimes to
        // instants 10s apart so the LRU order the rest of this test depends
        // on is exact, not a race against clock resolution.
        let now = SystemTime::now();
        let blob_path = |revid: u64, title: &str| {
            dir.join("blob")
                .join("en")
                .join(format!("{revid}-{:016x}.zst", fnv1a(title.as_bytes())))
        };
        let index_path = |title: &str| dir.join("page").join("en").join(format!("{title}.json"));
        stamp_mtime(&blob_path(2, "Second"), now - Duration::from_secs(20));
        stamp_mtime(&index_path("Second"), now - Duration::from_secs(20));
        stamp_mtime(&blob_path(1, "First"), now - Duration::from_secs(10));
        stamp_mtime(&index_path("First"), now - Duration::from_secs(10));

        cache.put("", "en", "Third", &content, 3, None); // pushes total past cap

        assert!(
            cache.get("", "en", "First").is_some(),
            "recently read: kept"
        );
        assert!(
            cache.get("", "en", "Second").is_none(),
            "least recently used: evicted (both its blob and index)"
        );
        assert!(cache.get("", "en", "Third").is_some(), "just written: kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- SLRU segments (PRD FR-OFF-3 v1.0) ----------------------------------

    /// The core SLRU promise: an entry read *twice* (promoted to
    /// `Protected`) survives an eviction wave that takes an unread neighbor
    /// instead — even when the neighbor's raw mtime looks *more* recent, the
    /// exact scenario plain mtime-LRU (the MVP policy this refines) would
    /// get backwards. This is what "a binge session doesn't flush favorites"
    /// means in practice.
    #[test]
    fn a_twice_read_entry_survives_eviction_over_a_never_reread_neighbor_even_though_it_is_older() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wikitui-cache-slru-protect-{}-{n}",
            std::process::id()
        ));
        let content = pseudo_random_content(2, 400);

        let roomy = PageCache::at(
            dir.clone(),
            DEFAULT_MAX_BYTES,
            FRESH_TTL_SECS,
            DEFAULT_FORCE_REFETCH_SECS,
        );
        roomy.put("", "en", "Favorite", &content, 1, None);
        roomy.put("", "en", "OneOff", &content, 2, None);
        let cap = dir_size(&dir) + 20;

        let cache = PageCache::at(dir.clone(), cap, FRESH_TTL_SECS, DEFAULT_FORCE_REFETCH_SECS);
        // Read "Favorite" *twice* — the second read is what promotes it to
        // the protected segment (see `Segment`'s doc comment).
        assert!(cache.get("", "en", "Favorite").is_some());
        assert!(cache.get("", "en", "Favorite").is_some());
        // "OneOff" is read exactly once — a normal open, never a re-visit —
        // so it stays probationary.
        assert!(cache.get("", "en", "OneOff").is_some());

        // Stamp mtimes so plain LRU would evict "Favorite" (older) and keep
        // "OneOff" (newer) — SLRU must invert that outcome.
        let now = SystemTime::now();
        let blob_path = |revid: u64, title: &str| {
            dir.join("blob")
                .join("en")
                .join(format!("{revid}-{:016x}.zst", fnv1a(title.as_bytes())))
        };
        let index_path = |title: &str| dir.join("page").join("en").join(format!("{title}.json"));
        stamp_mtime(&blob_path(1, "Favorite"), now - Duration::from_secs(20));
        stamp_mtime(&index_path("Favorite"), now - Duration::from_secs(20));
        stamp_mtime(&blob_path(2, "OneOff"), now - Duration::from_secs(5));
        stamp_mtime(&index_path("OneOff"), now - Duration::from_secs(5));

        cache.put("", "en", "Third", &content, 3, None); // pushes total past cap

        assert!(
            cache.get("", "en", "Favorite").is_some(),
            "protected (twice-read) survives despite being the oldest by mtime"
        );
        assert!(
            cache.get("", "en", "OneOff").is_none(),
            "probationary (read only once) is evicted first, even though its mtime looked newer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the promotion rule: a *single* read must NOT
    /// promote — otherwise every merely-opened article would be
    /// indistinguishable from a genuine favorite, and SLRU would degenerate
    /// into "protect whatever was read at all."
    #[test]
    fn a_once_read_entry_does_not_get_protection_and_is_evicted_like_plain_lru() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wikitui-cache-slru-no-promote-{}-{n}",
            std::process::id()
        ));
        let content = pseudo_random_content(3, 400);

        let roomy = PageCache::at(
            dir.clone(),
            DEFAULT_MAX_BYTES,
            FRESH_TTL_SECS,
            DEFAULT_FORCE_REFETCH_SECS,
        );
        roomy.put("", "en", "ReadOnce", &content, 1, None);
        roomy.put("", "en", "NeverRead", &content, 2, None);
        let cap = dir_size(&dir) + 20;

        let cache = PageCache::at(dir.clone(), cap, FRESH_TTL_SECS, DEFAULT_FORCE_REFETCH_SECS);
        // Exactly one read — must stay probationary.
        assert!(cache.get("", "en", "ReadOnce").is_some());

        let now = SystemTime::now();
        let blob_path = |revid: u64, title: &str| {
            dir.join("blob")
                .join("en")
                .join(format!("{revid}-{:016x}.zst", fnv1a(title.as_bytes())))
        };
        let index_path = |title: &str| dir.join("page").join("en").join(format!("{title}.json"));
        // "NeverRead" is older by mtime than "ReadOnce" — plain LRU (and
        // SLRU, since neither is protected) must evict it first regardless
        // of "ReadOnce" having been opened at all.
        stamp_mtime(&blob_path(2, "NeverRead"), now - Duration::from_secs(20));
        stamp_mtime(&index_path("NeverRead"), now - Duration::from_secs(20));
        stamp_mtime(&blob_path(1, "ReadOnce"), now - Duration::from_secs(10));
        stamp_mtime(&index_path("ReadOnce"), now - Duration::from_secs(10));

        cache.put("", "en", "Third", &content, 3, None);

        assert!(
            cache.get("", "en", "ReadOnce").is_some(),
            "still probationary, but more recently touched than NeverRead"
        );
        assert!(
            cache.get("", "en", "NeverRead").is_none(),
            "older probationary entry: evicted, same ordering plain LRU would produce"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The protected segment's own budget (PRD FR-OFF-3 v1.0:
    /// `PROTECTED_SEGMENT_FRACTION`): protection is not unconditional. If
    /// *every* entry has been re-read (nothing left in probationary), the
    /// oldest protected entries beyond the protected segment's budget must
    /// still be evicted — otherwise a cache where everything has been
    /// opened twice could never shrink back under its cap.
    #[test]
    fn protected_segment_over_its_own_budget_still_evicts_its_oldest_members() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wikitui-cache-slru-protected-overflow-{}-{n}",
            std::process::id()
        ));
        let content = pseudo_random_content(4, 400);

        let roomy = PageCache::at(
            dir.clone(),
            DEFAULT_MAX_BYTES,
            FRESH_TTL_SECS,
            DEFAULT_FORCE_REFETCH_SECS,
        );
        roomy.put("", "en", "OldFavorite", &content, 1, None);
        // Cap sized off *one* entry (not both): tight enough that fitting
        // all three (two favorites + the fresh probationary "Third" below)
        // requires evicting into the protected segment, not just draining
        // probationary.
        let cap = dir_size(&dir) + 20;
        roomy.put("", "en", "NewFavorite", &content, 2, None);

        let cache = PageCache::at(dir.clone(), cap, FRESH_TTL_SECS, DEFAULT_FORCE_REFETCH_SECS);
        // Both promoted to protected — nothing left in probationary for
        // eviction to prefer.
        assert!(cache.get("", "en", "OldFavorite").is_some());
        assert!(cache.get("", "en", "OldFavorite").is_some());
        assert!(cache.get("", "en", "NewFavorite").is_some());
        assert!(cache.get("", "en", "NewFavorite").is_some());

        let now = SystemTime::now();
        let blob_path = |revid: u64, title: &str| {
            dir.join("blob")
                .join("en")
                .join(format!("{revid}-{:016x}.zst", fnv1a(title.as_bytes())))
        };
        let index_path = |title: &str| dir.join("page").join("en").join(format!("{title}.json"));
        stamp_mtime(&blob_path(1, "OldFavorite"), now - Duration::from_secs(20));
        stamp_mtime(&index_path("OldFavorite"), now - Duration::from_secs(20));
        stamp_mtime(&blob_path(2, "NewFavorite"), now - Duration::from_secs(5));
        stamp_mtime(&index_path("NewFavorite"), now - Duration::from_secs(5));

        cache.put("", "en", "Third", &content, 3, None); // both favorites + Third now over cap

        assert!(
            cache.get("", "en", "OldFavorite").is_none(),
            "protected segment is over its own budget with both entries in it: \
             the OLDEST protected member still gets evicted"
        );
        assert!(
            cache.get("", "en", "NewFavorite").is_some(),
            "the newer protected member is spared once the older one covers the excess"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- SWR decision function (pure, table-tested) -------------------------

    #[test]
    fn swr_decision_table() {
        let fresh_ttl = 3600;
        let force_refetch = 30 * 86_400;

        assert_eq!(
            swr_decision(0, fresh_ttl, force_refetch),
            SwrDecision::Fresh
        );
        assert_eq!(
            swr_decision(fresh_ttl - 1, fresh_ttl, force_refetch),
            SwrDecision::Fresh
        );
        assert_eq!(
            swr_decision(fresh_ttl, fresh_ttl, force_refetch),
            SwrDecision::RevalidateInBackground,
            "the boundary itself is already stale, not fresh"
        );
        assert_eq!(
            swr_decision(force_refetch - 1, fresh_ttl, force_refetch),
            SwrDecision::RevalidateInBackground
        );
        assert_eq!(
            swr_decision(force_refetch, fresh_ttl, force_refetch),
            SwrDecision::ForceRefetch,
            "the force-refetch boundary itself must already force a refetch"
        );
        assert_eq!(
            swr_decision(force_refetch + 1_000_000, fresh_ttl, force_refetch),
            SwrDecision::ForceRefetch
        );
    }

    #[test]
    fn revalidate_action_table() {
        assert_eq!(
            revalidate_action(5, 5),
            RevalidateAction::Touch,
            "same revid: touch"
        );
        assert_eq!(
            revalidate_action(5, 6),
            RevalidateAction::Fetch,
            "new revid: fetch"
        );
        assert_eq!(
            revalidate_action(0, 0),
            RevalidateAction::Fetch,
            "unknown cached revid can never be confirmed unchanged"
        );
        assert_eq!(
            revalidate_action(0, 5),
            RevalidateAction::Fetch,
            "unknown cached revid always fetches"
        );
    }

    #[test]
    fn age_humanizes_sensibly() {
        assert_eq!(age_human(0), "0s");
        assert_eq!(age_human(59), "59s");
        assert_eq!(age_human(60), "1m");
        assert_eq!(age_human(3 * 3600 + 100), "3h");
        assert_eq!(age_human(2 * 86_400 + 5), "2d");
    }

    // -- Cache-dir resolution (PRD FR-PR-5) ---------------------------------

    #[test]
    fn resolve_pages_dir_honors_an_explicit_override() {
        let dir = resolve_pages_dir(Some(Path::new("/tmp/somewhere-custom")))
            .expect("an explicit override always resolves");
        assert_eq!(dir, Path::new("/tmp/somewhere-custom/pages"));
    }

    #[test]
    fn resolve_pages_dir_falls_back_to_the_platform_cache_dir_without_an_override() {
        // Whatever the platform dir is, it must end in "pages" and must not
        // be the override path from the test above.
        let dir = resolve_pages_dir(None);
        if let Some(dir) = dir {
            assert!(dir.ends_with("pages"));
        }
        // `None` is also a legitimate outcome in a sandboxed CI environment
        // with no resolvable home directory — `PageCache::open` degrades to
        // `disabled()` in that case, never a panic.
    }

    // -- Incognito cache tagging + wipe (PRD FR-PR-3) -----------------------

    #[test]
    fn entries_written_while_incognito_are_tagged_and_ordinary_entries_are_not() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Ordinary", "html", 1, None);
        cache.set_incognito(true);
        cache.put("", "en", "Secret", "html", 2, None);

        let ordinary_index =
            std::fs::read_to_string(dir.join("page").join("en").join("Ordinary.json")).unwrap();
        let secret_index =
            std::fs::read_to_string(dir.join("page").join("en").join("Secret.json")).unwrap();
        assert!(
            !serde_json::from_str::<IndexEntry>(&ordinary_index)
                .unwrap()
                .incognito
        );
        assert!(
            serde_json::from_str::<IndexEntry>(&secret_index)
                .unwrap()
                .incognito
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wipe_incognito_entries_removes_only_tagged_entries_and_their_blobs() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Ordinary", "kept content", 1, None);
        cache.set_incognito(true);
        cache.put("", "en", "Secret", "gone content", 2, None);
        cache.set_incognito(false);

        assert!(cache.get("", "en", "Ordinary").is_some());
        assert!(cache.get("", "en", "Secret").is_some());

        let report = cache.wipe_incognito_entries();
        assert_eq!(report.entries, 1, "exactly the one tagged entry");
        assert!(report.bytes > 0);

        assert!(
            cache.get("", "en", "Ordinary").is_some(),
            "a non-incognito entry must survive the wipe"
        );
        assert!(
            cache.get("", "en", "Secret").is_none(),
            "the incognito-tagged entry must be gone after the wipe"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wipe_incognito_entries_is_idempotent_and_harmless_on_a_clean_or_disabled_cache() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Ordinary", "html", 1, None);
        assert_eq!(cache.wipe_incognito_entries(), WipeReport::default());
        assert_eq!(
            cache.wipe_incognito_entries(),
            WipeReport::default(),
            "calling it again over an already-clean tree finds nothing new"
        );
        assert!(cache.get("", "en", "Ordinary").is_some());

        let disabled = PageCache::disabled();
        assert_eq!(disabled.wipe_incognito_entries(), WipeReport::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn touching_a_pre_incognito_entry_during_a_later_incognito_session_does_not_retag_it() {
        // A silent SWR touch (same revid confirmed) must not turn a
        // pre-existing, non-incognito entry into one that gets wiped later
        // just because the touch happened to occur while incognito was on —
        // `touch_fetched_at` never re-derives the tag from the current
        // session, only preserves whatever was already stored.
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "content", 1, None);
        cache.set_incognito(true);
        cache.touch_fetched_at("", "en", "Turing");

        let report = cache.wipe_incognito_entries();
        assert_eq!(report.entries, 0);
        assert!(cache.get("", "en", "Turing").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CORR-M1: re-putting a pre-existing *non*-incognito entry during a later
    /// incognito session (a revalidation-driven refetch, a ForceRefetch
    /// reopen) must carry its existing `incognito = false` forward, not
    /// re-derive `true` from this session — otherwise the session-end wipe
    /// would delete a cache entry the reader never chose to make private.
    #[test]
    fn re_putting_a_pre_incognito_entry_during_incognito_does_not_retag_or_wipe_it() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "content", 1, None); // written non-incognito
        cache.set_incognito(true);
        // A refetch of the same title (new revid) re-puts the index entry.
        cache.put("", "en", "Turing", "content v2", 2, None);

        let report = cache.wipe_incognito_entries();
        assert_eq!(
            report.entries, 0,
            "a pre-existing non-incognito entry must stay non-incognito across an incognito re-put"
        );
        assert!(
            cache.get("", "en", "Turing").is_some(),
            "and must survive the session-end wipe"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CORR-M1, the migration path: `migrate_legacy_entry` folds a pre-upgrade
    /// flat file into the new format (then deletes the source). Doing so during
    /// an incognito session must not tag the migrated entry incognito — the
    /// legacy file predates incognito, and because migration removes it, a wipe
    /// would be permanent loss of content the reader never made private.
    #[test]
    fn migrating_a_legacy_entry_during_incognito_does_not_tag_it_incognito() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        write_legacy_entry(&dir, "en", "Turing", now_unix(), "<html>legacy</html>");
        cache.set_incognito(true);
        // Opening it migrates it into the new format under incognito.
        assert_eq!(
            cache.get("", "en", "Turing").unwrap().html,
            "<html>legacy</html>"
        );

        let report = cache.wipe_incognito_entries();
        assert_eq!(
            report.entries, 0,
            "a migrated pre-upgrade entry must never be tagged incognito"
        );
        assert!(
            cache.get("", "en", "Turing").is_some(),
            "and must survive the wipe — the legacy source is already gone, so loss would be permanent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CORR-M8: `peek` (the reindex read path) serves the same content as
    /// `get` but touches nothing — no hit bump, no SLRU promotion, no mtime
    /// reset — so a full-cache reindex can't flatten the recency/segment state
    /// eviction depends on.
    #[test]
    fn peek_reads_content_without_touching_hits_segment_or_mtime() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Turing", "content", 1, None);
        let index_path = dir.join("page").join("en").join("Turing.json");
        let before_mtime = std::fs::metadata(&index_path).unwrap().modified().unwrap();
        let before: IndexEntry =
            serde_json::from_str(&std::fs::read_to_string(&index_path).unwrap()).unwrap();

        // Bulk-peek a few times (what reindex does over every cached entry).
        for _ in 0..3 {
            assert_eq!(cache.peek("", "en", "Turing").unwrap().html, "content");
        }

        let after: IndexEntry =
            serde_json::from_str(&std::fs::read_to_string(&index_path).unwrap()).unwrap();
        assert_eq!(after.hits, before.hits, "peek must not bump the hit count");
        assert_eq!(
            after.segment, before.segment,
            "peek must not promote the segment"
        );
        let after_mtime = std::fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_eq!(
            after_mtime, before_mtime,
            "peek must not reset the index mtime"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clones_share_the_same_incognito_flag() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        let clone = cache.clone();
        clone.set_incognito(true);
        cache.put("", "en", "SeenThroughAClone", "html", 1, None);

        let index =
            std::fs::read_to_string(dir.join("page").join("en").join("SeenThroughAClone.json"))
                .unwrap();
        assert!(
            serde_json::from_str::<IndexEntry>(&index)
                .unwrap()
                .incognito,
            "toggling incognito on a clone must be visible to every other clone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- list_entries (PRD FR-SR-7's reindex) --------------------------------

    #[test]
    fn list_entries_reports_every_cached_wiki_lang_title_triple() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Alan Turing", "html", 1, None);
        cache.put("wiktionary", "en", "Mercury", "html", 2, None);
        cache.put("", "de", "Turing", "html", 3, None);

        let mut entries = cache.list_entries();
        entries.sort();
        let mut expected = vec![
            ("".to_string(), "en".to_string(), "Alan Turing".to_string()),
            (
                "wiktionary".to_string(),
                "en".to_string(),
                "Mercury".to_string(),
            ),
            ("".to_string(), "de".to_string(), "Turing".to_string()),
        ];
        expected.sort();
        assert_eq!(entries, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_entries_is_empty_for_a_fresh_or_disabled_cache() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        assert!(cache.list_entries().is_empty());
        assert!(PageCache::disabled().list_entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_entries_skips_a_corrupt_index_file() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("", "en", "Good", "html", 1, None);
        std::fs::write(dir.join("page").join("en").join("Corrupt.json"), "not json").unwrap();
        let entries = cache.list_entries();
        assert_eq!(
            entries.len(),
            1,
            "the corrupt entry must be skipped, not panic"
        );
        assert_eq!(entries[0].2, "Good");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
