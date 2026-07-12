//! The on-disk page cache (PRD FR-OFF-1..3, MVP slice): raw article HTML
//! keyed by (lang, title), stored as one file per article under the
//! platform cache directory. Serving policy lives in the caller
//! (`main::fetch_page`): fresh-enough hits skip the network entirely, and
//! any hit — however stale — beats a network error, which is what makes
//! offline reading work.
//!
//! Each cache file is a one-line unix-seconds fetch timestamp followed by
//! the HTML. The file's *mtime* is deliberately separate state: it is
//! touched on every read and acts as the LRU clock for eviction, while
//! the embedded timestamp records when the content was actually fetched
//! (the "cached 3h ago" age). Compression and revid-aware keys are later
//! PRD phases (the full FR-OFF-1 design); this is the documented "MVP:
//! hard cap + crude LRU" cut.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// PRD FR-OFF-3: default cache cap 500 MB.
pub const DEFAULT_MAX_BYTES: u64 = 500 * 1024 * 1024;

/// PRD FR-OFF-2's backstop: content older than this is refetched when the
/// network is available (a stale hit still beats a network error).
pub const FRESH_TTL_SECS: u64 = 24 * 60 * 60;

pub struct PageCache {
    /// `None` when no cache directory could be determined — every lookup
    /// misses and every store is a no-op, but reading still works.
    dir: Option<PathBuf>,
    max_bytes: u64,
    /// PRD §6.7's `[cache] fresh_ttl_hours`, config-resolved. Storage
    /// settings are documented as restart-only (not part of `:config
    /// reload`), so this is fixed for the process's lifetime.
    fresh_ttl_secs: u64,
}

pub struct CachedPage {
    pub html: String,
    /// Seconds since this content was fetched from the network.
    pub age_secs: u64,
}

impl PageCache {
    /// `max_bytes`/`fresh_ttl_secs` come from the resolved config's
    /// `[cache]` section (§6.7); `cache::DEFAULT_MAX_BYTES`/`FRESH_TTL_SECS`
    /// are what that resolution falls back to absent a config file.
    pub fn open(max_bytes: u64, fresh_ttl_secs: u64) -> Self {
        match directories::ProjectDirs::from("", "", "wikitui") {
            Some(dirs) => Self::at(dirs.cache_dir().join("pages"), max_bytes, fresh_ttl_secs),
            None => Self::disabled(),
        }
    }

    /// A cache rooted at an explicit directory — tests, and `--config`'s
    /// `[cache]` override in production.
    pub fn at(dir: PathBuf, max_bytes: u64, fresh_ttl_secs: u64) -> Self {
        Self {
            dir: Some(dir),
            max_bytes,
            fresh_ttl_secs,
        }
    }

    /// A cache that never hits and never stores.
    pub fn disabled() -> Self {
        Self {
            dir: None,
            max_bytes: 0,
            fresh_ttl_secs: FRESH_TTL_SECS,
        }
    }

    /// Whether cached content this old still counts as fresh enough to
    /// skip the network entirely (PRD FR-OFF-2's serve policy).
    pub fn is_fresh(&self, age_secs: u64) -> bool {
        age_secs < self.fresh_ttl_secs
    }

    fn entry_path(&self, lang: &str, title: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        // Percent-encoding keeps names readable and filesystem-safe (no
        // '/', no ':'); very long titles could exceed the common 255-byte
        // filename limit once encoded, so those fall back to a stable
        // hash. Language codes are subdomain labels (safe as-is), but
        // encode defensively anyway.
        let encoded = urlencoding::encode(title).into_owned();
        let name = if encoded.len() > 200 {
            format!("h{:016x}", fnv1a(title.as_bytes()))
        } else {
            encoded
        };
        Some(
            dir.join(urlencoding::encode(lang).into_owned())
                .join(format!("{name}.html")),
        )
    }

    /// Looks up a page, returning its HTML and content age. A hit also
    /// touches the file's mtime so eviction treats it as recently used.
    pub fn get(&self, lang: &str, title: &str) -> Option<CachedPage> {
        let path = self.entry_path(lang, title)?;
        let raw = std::fs::read_to_string(&path).ok()?;
        let (first_line, html) = raw.split_once('\n')?;
        let fetched_at: u64 = first_line.trim().parse().ok()?;
        let now = now_unix();
        let age_secs = now.saturating_sub(fetched_at);

        // LRU touch; failure (e.g. read-only cache) only degrades
        // eviction ordering, never the hit itself.
        if let Ok(file) = std::fs::File::options().write(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }

        Some(CachedPage {
            html: html.to_string(),
            age_secs,
        })
    }

    /// Stores a freshly fetched page, then evicts least-recently-used
    /// entries if the cache has grown past its cap. Failures are
    /// swallowed: a full disk must not break reading, and the entry
    /// simply won't be there next time.
    pub fn put(&self, lang: &str, title: &str, html: &str) {
        let Some(path) = self.entry_path(lang, title) else {
            return;
        };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, format!("{}\n{html}", now_unix()))
        };
        if write().is_err() {
            return;
        }
        self.evict_to_cap();
    }

    /// Deletes least-recently-read entries (oldest mtime first) until the
    /// cache fits its cap again (PRD FR-OFF-3's MVP "hard cap + crude
    /// LRU").
    fn evict_to_cap(&self) {
        let Some(dir) = self.dir.as_ref() else {
            return;
        };
        let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        collect_files(dir, &mut entries);
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
        (PageCache::at(dir.clone(), max_bytes, FRESH_TTL_SECS), dir)
    }

    #[test]
    fn round_trips_a_page_and_reports_a_small_age() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Alan Turing", "<html>body</html>");
        let hit = cache.get("en", "Alan Turing").expect("hit");
        assert_eq!(hit.html, "<html>body</html>");
        assert!(
            hit.age_secs < 5,
            "freshly written, age was {}",
            hit.age_secs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_fresh_honors_the_configured_ttl_not_the_hardcoded_default() {
        let short_ttl = PageCache::at(std::env::temp_dir(), DEFAULT_MAX_BYTES, 10);
        assert!(short_ttl.is_fresh(5));
        assert!(
            !short_ttl.is_fresh(15),
            "config TTL of 10s must be honored, not the 24h default"
        );
    }

    #[test]
    fn miss_on_unknown_title_and_on_disabled_cache() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        assert!(cache.get("en", "Nonexistent").is_none());
        let disabled = PageCache::disabled();
        disabled.put("en", "X", "<html/>"); // must not panic
        assert!(disabled.get("en", "X").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_are_isolated_per_language_and_from_each_other() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Turing", "english");
        cache.put("de", "Turing", "german");
        cache.put("en", "Church", "church");
        assert_eq!(cache.get("en", "Turing").unwrap().html, "english");
        assert_eq!(cache.get("de", "Turing").unwrap().html, "german");
        assert_eq!(cache.get("en", "Church").unwrap().html, "church");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slashes_and_spaces_in_titles_are_safe() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "AC/DC", "band");
        cache.put("en", "New York City", "city");
        assert_eq!(cache.get("en", "AC/DC").unwrap().html, "band");
        assert_eq!(cache.get("en", "New York City").unwrap().html, "city");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn very_long_titles_fall_back_to_a_hash_and_still_round_trip() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        let long_a = "統合デジタル通信網".repeat(15); // percent-encodes far past 200 bytes
        let long_b = format!("{long_a}違"); // near-identical sibling must not collide
        cache.put("en", &long_a, "content a");
        cache.put("en", &long_b, "content b");
        assert_eq!(cache.get("en", &long_a).unwrap().html, "content a");
        assert_eq!(cache.get("en", &long_b).unwrap().html, "content b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cache_file_is_a_miss_not_a_panic() {
        let (cache, dir) = temp_cache(DEFAULT_MAX_BYTES);
        cache.put("en", "Turing", "good");
        // Overwrite with garbage lacking a valid timestamp header.
        let path = dir.join("en").join("Turing.html");
        std::fs::write(&path, "not-a-timestamp\nhtml").unwrap();
        assert!(cache.get("en", "Turing").is_none());
        // And a file with no newline at all:
        std::fs::write(&path, "no newline here").unwrap();
        assert!(cache.get("en", "Turing").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicts_least_recently_read_entries_once_over_cap() {
        // Each entry is ~51 bytes on disk (11-byte timestamp header + 40
        // bytes of content); a 110-byte cap fits two entries but not three.
        let (cache, dir) = temp_cache(110);
        cache.put("en", "First", &"a".repeat(40));
        cache.put("en", "Second", &"b".repeat(40));
        // Read First so Second becomes the least-recently-used entry —
        // proving eviction follows reads, not just insertion order. The
        // explicit mtimes make the ordering deterministic (filesystem
        // mtime granularity can be coarser than this test runs).
        set_mtime(&dir.join("en").join("First.html"), 1000);
        set_mtime(&dir.join("en").join("Second.html"), 500);

        cache.put("en", "Third", &"c".repeat(40)); // pushes total past 100 bytes

        assert!(cache.get("en", "First").is_some(), "recently read: kept");
        assert!(
            cache.get("en", "Second").is_none(),
            "least recently used: evicted"
        );
        assert!(cache.get("en", "Third").is_some(), "just written: kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_mtime(path: &Path, unix_secs: u64) {
        let time = UNIX_EPOCH + std::time::Duration::from_secs(unix_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
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
