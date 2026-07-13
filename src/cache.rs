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
//!   blob/{lang}/{revid}-{title_hash:016x}.zst   -- content, immutable per revid
//!   page/{lang}/{title_or_hash}.json            -- {revid, fetched_at, etag}
//!   {lang}/{title_or_hash}.html                 -- pre-upgrade format (see below)
//! ```
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

pub struct PageCache {
    /// `None` when no cache directory could be determined — every lookup
    /// misses and every store is a no-op, but reading still works.
    dir: Option<PathBuf>,
    max_bytes: u64,
    fresh_ttl_secs: u64,
    force_refetch_secs: u64,
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

/// The on-disk shape of a `page/{lang}/{title}.json` title-index entry.
#[derive(Debug, Serialize, Deserialize)]
struct IndexEntry {
    revid: u64,
    fetched_at: u64,
    etag: Option<String>,
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
    /// absent a config file.
    pub fn open(max_bytes: u64, fresh_ttl_secs: u64, force_refetch_secs: u64) -> Self {
        match directories::ProjectDirs::from("", "", "wikitui") {
            Some(dirs) => Self::at(
                dirs.cache_dir().join("pages"),
                max_bytes,
                fresh_ttl_secs,
                force_refetch_secs,
            ),
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
        }
    }

    /// A cache that never hits and never stores.
    pub fn disabled() -> Self {
        Self {
            dir: None,
            max_bytes: 0,
            fresh_ttl_secs: FRESH_TTL_SECS,
            force_refetch_secs: DEFAULT_FORCE_REFETCH_SECS,
        }
    }

    /// This cache's configured on-open staleness decision for content this
    /// old — see the free function [`swr_decision`] for the pure logic.
    pub fn swr_decision(&self, age_secs: u64) -> SwrDecision {
        swr_decision(age_secs, self.fresh_ttl_secs, self.force_refetch_secs)
    }

    fn blob_path(&self, lang: &str, title: &str, revid: u64) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let title_hash = fnv1a(title.as_bytes());
        Some(
            dir.join("blob")
                .join(safe_name(lang))
                .join(format!("{revid}-{title_hash:016x}.zst")),
        )
    }

    fn index_path(&self, lang: &str, title: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        Some(
            dir.join("page")
                .join(safe_name(lang))
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

    /// Looks up a page. Tries the current two-layer format first; on a
    /// miss, falls back to a pre-upgrade flat file and migrates it in
    /// place (see the module doc comment) rather than refusing content
    /// that's still perfectly good. Either path touches mtimes as the LRU
    /// signal that this entry is still wanted.
    pub fn get(&self, lang: &str, title: &str) -> Option<CachedPage> {
        self.get_current_format(lang, title)
            .or_else(|| self.migrate_legacy_entry(lang, title))
    }

    fn get_current_format(&self, lang: &str, title: &str) -> Option<CachedPage> {
        let index_path = self.index_path(lang, title)?;
        let text = std::fs::read_to_string(&index_path).ok()?;
        let entry: IndexEntry = serde_json::from_str(&text).ok()?;
        let blob_path = self.blob_path(lang, title, entry.revid)?;
        let compressed = std::fs::read(&blob_path).ok()?;
        let html_bytes = zstd::stream::decode_all(compressed.as_slice()).ok()?;
        let html = String::from_utf8(html_bytes).ok()?;

        touch_mtime(&index_path);
        touch_mtime(&blob_path);

        let age_secs = now_unix().saturating_sub(entry.fetched_at);
        Some(CachedPage {
            html,
            age_secs,
            revid: entry.revid,
            etag: entry.etag,
        })
    }

    fn migrate_legacy_entry(&self, lang: &str, title: &str) -> Option<CachedPage> {
        let old_path = self.legacy_path(lang, title)?;
        let raw = std::fs::read_to_string(&old_path).ok()?;
        let (first_line, html) = raw.split_once('\n')?;
        let fetched_at: u64 = first_line.trim().parse().ok()?;

        // Fold it into the new format at its original fetched_at (an
        // honest age, not a falsely-fresh "just fetched now") before ever
        // returning success, then remove the old file — the "read once,
        // then rewritten" migration this module documents.
        self.put_at(lang, title, html, 0, None, fetched_at);
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
    pub fn put(&self, lang: &str, title: &str, html: &str, revid: u64, etag: Option<&str>) {
        self.put_at(lang, title, html, revid, etag, now_unix());
    }

    fn put_at(
        &self,
        lang: &str,
        title: &str,
        html: &str,
        revid: u64,
        etag: Option<&str>,
        fetched_at: u64,
    ) {
        let (Some(blob_path), Some(index_path)) = (
            self.blob_path(lang, title, revid),
            self.index_path(lang, title),
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

        let entry = IndexEntry {
            revid,
            fetched_at,
            etag: etag.map(str::to_string),
        };
        let Ok(json) = serde_json::to_string(&entry) else {
            return;
        };
        if let Some(parent) = index_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&index_path, json).is_err() {
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
    pub fn touch_fetched_at(&self, lang: &str, title: &str) {
        let Some(index_path) = self.index_path(lang, title) else {
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
            let _ = std::fs::write(&index_path, json);
        }
        if let Some(blob_path) = self.blob_path(lang, title, entry.revid) {
            touch_mtime(&blob_path);
        }
    }

    /// Deletes least-recently-read entries (oldest mtime first) until the
    /// `blob/`+`page/` tree fits its cap again (PRD FR-OFF-3's hard cap +
    /// LRU) — both trees are summed into one pool of eviction candidates,
    /// so a title index and its blob compete on equal footing with every
    /// other entry, not two separate budgets.
    fn evict_to_cap(&self) {
        let Some(dir) = self.dir.as_ref() else {
            return;
        };
        let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        collect_files(&dir.join("blob"), &mut entries);
        collect_files(&dir.join("page"), &mut entries);
        let total: u64 = entries.iter().map(|(_, size, _)| size).sum();
        if total <= self.max_bytes {
            return;
        }
        entries.sort_by_key(|(_, _, mtime)| *mtime);
        let mut excess = total - self.max_bytes;
        for (path, size, _) in entries {
            if std::fs::remove_file(&path).is_ok() {
                excess = excess.saturating_sub(size);
                if excess == 0 {
                    break;
                }
            }
        }
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
/// once encoded, so those fall back to a stable hash.
fn safe_name(s: &str) -> String {
    let encoded = urlencoding::encode(s).into_owned();
    if encoded.len() > 200 {
        format!("h{:016x}", fnv1a(s.as_bytes()))
    } else {
        encoded
    }
}

/// FNV-1a: tiny, dependency-free, and stable across runs and Rust
/// versions (unlike `DefaultHasher`), which cache filenames require.
fn fnv1a(bytes: &[u8]) -> u64 {
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
            "en",
            "Alan Turing",
            "<html>body</html>",
            100,
            Some("W/\"100/abc\""),
        );
        let hit = cache.get("en", "Alan Turing").expect("hit");
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
        cache.put("en", "Empty", "", 1, None);
        assert_eq!(cache.get("en", "Empty").unwrap().html, "");

        let multibyte = "<p>チューリング — café — 🎉</p>".repeat(200);
        cache.put("en", "Multibyte", &multibyte, 2, None);
        assert_eq!(cache.get("en", "Multibyte").unwrap().html, multibyte);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The index header's fields (revid, fetched_at, etag) must round-trip
    /// exactly, including the `etag: None` case (older/degraded fetches).
    #[test]
    fn index_entry_header_round_trips_with_and_without_an_etag() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "WithEtag", "html", 42, Some("W/\"42/xyz\""));
        cache.put("en", "NoEtag", "html", 0, None);

        let with = cache.get("en", "WithEtag").unwrap();
        assert_eq!(with.revid, 42);
        assert_eq!(with.etag.as_deref(), Some("W/\"42/xyz\""));

        let without = cache.get("en", "NoEtag").unwrap();
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
        cache.put("en", "Turing", "version one", 7, Some("W/\"7/aaa\""));
        cache.put(
            "en",
            "Turing",
            "version two — must not land",
            7,
            Some("W/\"7/bbb\""),
        );

        let hit = cache.get("en", "Turing").unwrap();
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
        cache.put("en", "Turing", "old content", 1, None);
        cache.put("en", "Turing", "new content", 2, None);
        assert_eq!(cache.get("en", "Turing").unwrap().html, "new content");
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
        assert!(cache.get("en", "Nonexistent").is_none());
        let disabled = PageCache::disabled();
        disabled.put("en", "X", "<html/>", 1, None); // must not panic
        assert!(disabled.get("en", "X").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_are_isolated_per_language_and_from_each_other() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Turing", "english", 1, None);
        cache.put("de", "Turing", "german", 1, None);
        cache.put("en", "Church", "church", 2, None);
        assert_eq!(cache.get("en", "Turing").unwrap().html, "english");
        assert_eq!(cache.get("de", "Turing").unwrap().html, "german");
        assert_eq!(cache.get("en", "Church").unwrap().html, "church");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slashes_and_spaces_in_titles_are_safe() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "AC/DC", "band", 1, None);
        cache.put("en", "New York City", "city", 2, None);
        assert_eq!(cache.get("en", "AC/DC").unwrap().html, "band");
        assert_eq!(cache.get("en", "New York City").unwrap().html, "city");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn very_long_titles_fall_back_to_a_hash_and_still_round_trip() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        let long_a = "統合デジタル通信網".repeat(15); // percent-encodes far past 200 bytes
        let long_b = format!("{long_a}違"); // near-identical sibling must not collide
        cache.put("en", &long_a, "content a", 1, None);
        cache.put("en", &long_b, "content b", 2, None);
        assert_eq!(cache.get("en", &long_a).unwrap().html, "content a");
        assert_eq!(cache.get("en", &long_b).unwrap().html, "content b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_index_entry_is_a_miss_not_a_panic() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Turing", "good", 1, None);
        // Overwrite the index (not the blob) with garbage JSON.
        let index_path = dir.join("page").join("en").join("Turing.json");
        std::fs::write(&index_path, "not json").unwrap();
        assert!(cache.get("en", "Turing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_blob_is_a_miss_not_a_panic() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Turing", "good", 5, None);
        let blob_path = dir
            .join("blob")
            .join("en")
            .join(format!("5-{:016x}.zst", fnv1a(b"Turing")));
        std::fs::write(&blob_path, b"not zstd data at all").unwrap();
        assert!(cache.get("en", "Turing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PRD FR-OFF-2's silent-touch path: `fetched_at` moves forward without
    /// touching the stored content at all.
    #[test]
    fn touch_fetched_at_refreshes_age_without_changing_content() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Turing", "content", 3, None);
        // Force an old fetched_at directly so the "before" age is visibly
        // large, then touch and confirm it collapses back near zero.
        let index_path = dir.join("page").join("en").join("Turing.json");
        let stale = IndexEntry {
            revid: 3,
            fetched_at: now_unix() - 100_000,
            etag: None,
        };
        std::fs::write(&index_path, serde_json::to_string(&stale).unwrap()).unwrap();
        assert!(cache.get("en", "Turing").unwrap().age_secs >= 100_000 - 5);

        cache.touch_fetched_at("en", "Turing");
        let touched = cache.get("en", "Turing").unwrap();
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
        cache.touch_fetched_at("en", "Nonexistent"); // must not panic
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
            .get("en", "Turing")
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
            .get("en", "Turing")
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
        assert!(cache.get("en", "Turing").is_none());
        // And a file with no newline at all:
        std::fs::write(&path, "no newline here").unwrap();
        assert!(cache.get("en", "Turing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_format_entry_is_preferred_over_a_stale_leftover_old_format_file() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Turing", "new content", 9, None);
        // A leftover old-format file for the same title must never be
        // consulted once the new format has an entry.
        write_legacy_entry(&dir, "en", "Turing", now_unix(), "stale old content");
        assert_eq!(cache.get("en", "Turing").unwrap().html, "new content");
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

    #[test]
    fn evicts_least_recently_read_entries_once_over_cap_counting_blob_and_index_bytes() {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wikitui-cache-evict-test-{}-{n}",
            std::process::id()
        ));

        // Write two entries with a generous cap (no eviction pressure),
        // then measure their real combined blob+index footprint so the
        // tightened cap below is exact — not a guess about zstd's ratio.
        let roomy = PageCache::at(
            dir.clone(),
            DEFAULT_MAX_BYTES,
            FRESH_TTL_SECS,
            DEFAULT_FORCE_REFETCH_SECS,
        );
        roomy.put("en", "First", &pseudo_random_content(1, 400), 1, None);
        roomy.put("en", "Second", &pseudo_random_content(2, 400), 2, None);
        // Small safety margin: at this cap, adding a same-sized third entry
        // forces eviction of exactly one LRU victim, never two.
        let cap = dir_size(&dir) + 20;

        let cache = PageCache::at(dir.clone(), cap, FRESH_TTL_SECS, DEFAULT_FORCE_REFETCH_SECS);
        // Read First so Second becomes the least-recently-used entry —
        // proving eviction follows reads, not just insertion order.
        assert!(cache.get("en", "First").is_some());

        cache.put("en", "Third", &pseudo_random_content(3, 400), 3, None); // pushes total past cap

        assert!(cache.get("en", "First").is_some(), "recently read: kept");
        assert!(
            cache.get("en", "Second").is_none(),
            "least recently used: evicted (both its blob and index)"
        );
        assert!(cache.get("en", "Third").is_some(), "just written: kept");
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
}
