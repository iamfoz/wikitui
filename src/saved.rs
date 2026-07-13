//! Pinned, tiered **saved pages** (PRD FR-OFF-4..7, §5.7). This is the store
//! the PRD deliberately keeps *distinct* from the evictable page cache
//! (`cache.rs`): the cache is best-effort, LRU-evicted, and lives under
//! `$XDG_CACHE_HOME` ("safe to delete"); a saved page is **pinned** (never
//! evicted), **integrity-checked** (a sha256 of its HTML), **quota-visible**
//! (`total_bytes`), and lives under `$XDG_DATA_HOME` (§6.4's "saved pages
//! store + index"). Because it is a separate directory tree entirely, a saved
//! page is *inherently* excluded from the cache's eviction accounting
//! (`cache::PageCache::evict_to_cap` only ever walks the cache dir) — there is
//! no shared budget to opt out of, and `saved_survives_cache_eviction` locks
//! that in.
//!
//! ## Layout
//!
//! ```text
//! <data_dir>/saved/
//!   saved.jsonl                         -- the index (one SavedRecord per line)
//!   content/{lang}/{title}.html.zst     -- the pinned Parsoid HTML (zstd)
//!   thumbs/{lang}/{title}/{n}           -- T1+ thumbnails, raw fetched bytes
//! ```
//!
//! **Compression**: the pinned HTML is zstd-compressed, reusing the cache's
//! approach (`zstd::stream::{encode,decode}_all`). Pinned content is never
//! rewritten or evicted, so there is no immutability-by-revid dance like the
//! cache's — one saved copy per `(lang, title)`, replaced wholesale if the
//! user saves the same article again.
//!
//! ## Tiers (FR-OFF-4)
//!
//! - **T0** — article HTML only (the default).
//! - **T1** — + thumbnails at terminal resolution, fetched via the existing
//!   `api::fetch_image` path and stored under `thumbs/`, capped in count and
//!   (per image) bytes.
//! - **T2** — + link-target summaries (batched extracts) stored in the record
//!   as a sidecar, so link-peek works offline.
//!
//! ## Non-free image policy (§10)
//!
//! Thumbnails persisted for T1 are the same sanitized image sources already
//! shown transiently in the reading view; they are stored so the saved copy
//! renders offline (the "cached/saved thumbnails" §10 names). The stricter
//! "never written into exports" rule is enforced at *export* time
//! (`saved_export`), not here: since wikitui does not fetch per-image
//! `extmetadata` license data, an export cannot prove an image is free, so the
//! safe default is to omit image data from a redistributable export unless the
//! user opts in with `include_nonfree`. See `saved_export`'s module doc.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// zstd level for pinned HTML — same low, fetch-latency-favoring level the
/// cache uses (article HTML is text; even level 3 compresses it well, and a
/// save should feel instant).
const ZSTD_LEVEL: i32 = 3;

/// PRD FR-OFF-4 T1 cap: how many of an article's images are fetched and
/// pinned. A real article's lead + a few section images is plenty offline;
/// a gallery-heavy page shouldn't blow a saved-page budget.
pub const MAX_T1_THUMBS: usize = 8;

/// PRD FR-OFF-4 T2 cap: how many internal links get a stored summary. Kept at
/// Appendix A's `exlimit ≤ 20` batch ceiling so a future batched-extracts
/// implementation drops in without changing the contract.
pub const MAX_T2_SUMMARIES: usize = 20;

/// The three saved-page depths (PRD FR-OFF-4). Serializes as `"T0"`/`"T1"`/
/// `"T2"` (unit-variant default), which is what the index records store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    T0,
    T1,
    T2,
}

impl Tier {
    pub fn label(&self) -> &'static str {
        match self {
            Tier::T0 => "T0",
            Tier::T1 => "T1",
            Tier::T2 => "T2",
        }
    }

    /// Parse a `t0`/`t1`/`t2` command argument (case-insensitive).
    pub fn parse(s: &str) -> Option<Tier> {
        match s.to_ascii_lowercase().as_str() {
            "t0" => Some(Tier::T0),
            "t1" => Some(Tier::T1),
            "t2" => Some(Tier::T2),
            _ => None,
        }
    }

    /// PRD FR-OFF-9's per-article byte estimate, used for the bulk cost
    /// preview (FR-OFF-5) before anything is fetched: ~30 KB text-only (T0),
    /// ~300 KB with T1 thumbnails (mid of the stated 150–500 KB range), and a
    /// little more for T2's summaries.
    pub fn estimate_bytes(&self) -> u64 {
        match self {
            Tier::T0 => 30 * 1024,
            Tier::T1 => 300 * 1024,
            Tier::T2 => 340 * 1024,
        }
    }
}

/// One pinned thumbnail's bookkeeping (T1+): the original sanitized source URL
/// (so an export or offline render can match it back to a `doc::Block::Image`),
/// the on-disk filename under `thumbs/{lang}/{title}/`, and its byte size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedThumb {
    pub src: String,
    pub file: String,
    pub bytes: u64,
}

/// One stored link-target summary (T2): the internal article title and its
/// extract text, so link-peek resolves offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSummary {
    pub title: String,
    pub extract: String,
}

/// One saved-page index record (the `saved.jsonl` line). `#[serde(default)]`
/// on the tier-specific and forward-looking fields so an older or newer
/// file's records keep loading (the same tolerance `bookmarks.rs` documents).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRecord {
    pub lang: String,
    pub title: String,
    /// The revid the copy was pinned at (`0` when the server never told us —
    /// see `cache`'s degraded mode).
    #[serde(default)]
    pub revid: u64,
    pub tier: Tier,
    /// RFC 3339 save time.
    pub saved_at: String,
    pub html_bytes: u64,
    #[serde(default)]
    pub thumb_bytes: u64,
    pub size_total: u64,
    /// sha256 (hex) of the *uncompressed* HTML — the integrity check
    /// `verify` recomputes.
    pub sha256: String,
    /// How this page came to be saved ("S save", "bulk: Category:Physics", …)
    /// — surfaced in the `:saved` browser and useful for debugging a bulk run.
    #[serde(default)]
    pub source_note: String,
    /// Relative path of the pinned HTML under `saved/`, stored so a later
    /// naming-scheme change can't strand existing content.
    pub content_file: String,
    #[serde(default)]
    pub thumbs: Vec<SavedThumb>,
    #[serde(default)]
    pub summaries: Vec<LinkSummary>,
}

/// The content served back for an offline read (`get`): the decompressed HTML
/// plus the identity/age the reading view needs for the ▣ saved indicator. The
/// record's own `tier`/`thumbs`/`summaries` stay on `SavedRecord` (read by the
/// browser and export); wiring offline link-peek to a T2 record's summaries is
/// a documented seam for the peek (`K`) feature, so they aren't surfaced here.
pub struct SavedContent {
    pub html: String,
    pub revid: u64,
    /// Seconds since the page was saved (for the "▣ saved Nago" glyph).
    pub age_secs: u64,
}

/// The outcome of an integrity check (`verify`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    /// The stored HTML decompresses and its sha256 matches the record.
    Ok,
    /// The content file is missing, won't decompress, or its sha256 no longer
    /// matches — the pinned copy is corrupt (§7's "last-cached copy offered"
    /// class, surfaced honestly in the browser rather than silently served).
    Corrupt,
}

/// The pinned saved-pages store. Mirrors `BookmarkStore`'s shape (an in-memory
/// `Vec` of records plus the on-disk root), but adds the content/thumb blob
/// tree and integrity checking the cache-adjacent `jsonl` stores don't need.
pub struct SavedPages {
    pub records: Vec<SavedRecord>,
    /// The `saved/` root, or `None` when no data directory could be resolved —
    /// saving then works in-memory for the session (never refuses the feature),
    /// exactly like `BookmarkStore`.
    root: Option<PathBuf>,
}

impl SavedPages {
    pub fn load() -> Self {
        match saved_root() {
            Some(root) => Self::at(root),
            None => Self::in_memory(),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            records: Vec::new(),
            root: None,
        }
    }

    /// A store rooted at an explicit `saved/` directory — tests, and the
    /// isolated-XDG pty runs.
    pub fn at(root: PathBuf) -> Self {
        let index = root.join("saved.jsonl");
        Self {
            records: crate::jsonl::load(&index),
            root: Some(root),
        }
    }

    fn index_path(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("saved.jsonl"))
    }

    /// The relative content path for `(lang, title)` — `content/{lang}/
    /// {title}.html.zst`, names filesystem-safe via the same rule the cache
    /// uses (`cache::safe_name`).
    fn content_rel(lang: &str, title: &str) -> String {
        format!(
            "content/{}/{}.html.zst",
            crate::cache::safe_name(lang),
            crate::cache::safe_name(title)
        )
    }

    fn thumb_dir_rel(lang: &str, title: &str) -> String {
        format!(
            "thumbs/{}/{}",
            crate::cache::safe_name(lang),
            crate::cache::safe_name(title)
        )
    }

    pub fn find(&self, lang: &str, title: &str) -> Option<&SavedRecord> {
        self.records
            .iter()
            .find(|r| r.lang == lang && r.title == title)
    }

    pub fn is_saved(&self, lang: &str, title: &str) -> bool {
        self.find(lang, title).is_some()
    }

    pub fn list(&self) -> &[SavedRecord] {
        &self.records
    }

    /// The summed pinned footprint (PRD's "quota-visible") — from the records,
    /// not a disk walk, so the `:saved` browser's total is cheap.
    pub fn total_bytes(&self) -> u64 {
        self.records.iter().map(|r| r.size_total).sum()
    }

    /// Save (pin) `(lang, title)` at `tier`. `html` is the raw Parsoid HTML;
    /// `thumbs` are `(src, bytes)` pairs already fetched for T1+ (empty for
    /// T0); `summaries` are the T2 link extracts (empty otherwise). Writes the
    /// zstd HTML blob and any thumbnails, then appends/replaces the index
    /// record. Re-saving an already-saved article replaces it wholesale
    /// (index line + content), so the tier/size/integrity all move forward
    /// together. Returns the stored record.
    #[allow(clippy::too_many_arguments)]
    pub fn save(
        &mut self,
        lang: &str,
        title: &str,
        revid: u64,
        tier: Tier,
        html: &str,
        thumbs: &[(String, Vec<u8>)],
        summaries: Vec<LinkSummary>,
        source_note: &str,
    ) -> std::io::Result<SavedRecord> {
        let html_bytes = html.len() as u64;
        let sha256 = sha256_hex(html.as_bytes());
        let content_rel = Self::content_rel(lang, title);

        // Write the blobs first; only once the content is durably on disk do
        // we commit the index line that promises it exists.
        let mut stored_thumbs = Vec::new();
        let mut thumb_bytes_total = 0u64;
        if let Some(root) = &self.root {
            let content_abs = root.join(&content_rel);
            if let Some(parent) = content_abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let compressed = zstd::stream::encode_all(html.as_bytes(), ZSTD_LEVEL)
                .map_err(std::io::Error::other)?;
            std::fs::write(&content_abs, &compressed)?;

            if !thumbs.is_empty() {
                let thumb_dir = root.join(Self::thumb_dir_rel(lang, title));
                // A re-save shouldn't leave stale thumbnails behind.
                let _ = std::fs::remove_dir_all(&thumb_dir);
                std::fs::create_dir_all(&thumb_dir)?;
                for (i, (src, bytes)) in thumbs.iter().take(MAX_T1_THUMBS).enumerate() {
                    let file = format!("{i}");
                    std::fs::write(thumb_dir.join(&file), bytes)?;
                    thumb_bytes_total += bytes.len() as u64;
                    stored_thumbs.push(SavedThumb {
                        src: src.clone(),
                        file,
                        bytes: bytes.len() as u64,
                    });
                }
            }
        } else {
            // In-memory fallback: still record the thumbnail metadata so tier
            // semantics (T1 has thumbs) hold in tests without a disk root.
            for (src, bytes) in thumbs.iter().take(MAX_T1_THUMBS) {
                thumb_bytes_total += bytes.len() as u64;
                stored_thumbs.push(SavedThumb {
                    src: src.clone(),
                    file: String::new(),
                    bytes: bytes.len() as u64,
                });
            }
        }

        let record = SavedRecord {
            lang: lang.to_string(),
            title: title.to_string(),
            revid,
            tier,
            saved_at: crate::bookmarks::now_ts(),
            html_bytes,
            thumb_bytes: thumb_bytes_total,
            size_total: html_bytes + thumb_bytes_total,
            sha256,
            source_note: source_note.to_string(),
            content_file: content_rel,
            thumbs: stored_thumbs,
            summaries,
        };

        // Replace any existing index line for this (lang, title), else append.
        if let Some(pos) = self
            .records
            .iter()
            .position(|r| r.lang == lang && r.title == title)
        {
            self.records[pos] = record.clone();
            if let Some(index) = self.index_path() {
                let _ = crate::jsonl::rewrite_matching::<SavedRecord, _>(
                    &index,
                    |r| r.lang == lang && r.title == title,
                    Some(&record),
                );
            }
        } else {
            self.records.push(record.clone());
            if let Some(index) = self.index_path() {
                crate::jsonl::append(&index, &record)?;
            }
        }
        Ok(record)
    }

    /// Serve the pinned copy for an offline read (§5.7): decompresses the HTML
    /// blob. `None` on a miss or unreadable content — a corrupt pinned copy is
    /// a miss here (the caller falls back to the cache/offline card), while
    /// `verify` reports the corruption explicitly for the browser.
    pub fn get(&self, lang: &str, title: &str) -> Option<SavedContent> {
        let record = self.find(lang, title)?;
        let root = self.root.as_ref()?;
        let compressed = std::fs::read(root.join(&record.content_file)).ok()?;
        let html_bytes = zstd::stream::decode_all(compressed.as_slice()).ok()?;
        let html = String::from_utf8(html_bytes).ok()?;
        let age_secs = record
            .saved_at
            .parse::<chrono::DateTime<chrono::FixedOffset>>()
            .ok()
            .map(|t| {
                let now = chrono::Local::now().timestamp();
                (now - t.timestamp()).max(0) as u64
            })
            .unwrap_or(0);
        Some(SavedContent {
            html,
            revid: record.revid,
            age_secs,
        })
    }

    /// The raw thumbnail bytes stored for `(lang, title)`, keyed by source URL
    /// — used by the HTML export to embed pinned images when the non-free
    /// opt-in allows it (`saved_export`).
    pub fn thumb_bytes(&self, lang: &str, title: &str, src: &str) -> Option<Vec<u8>> {
        let record = self.find(lang, title)?;
        let root = self.root.as_ref()?;
        let thumb = record.thumbs.iter().find(|t| t.src == src)?;
        let dir = root.join(Self::thumb_dir_rel(lang, title));
        std::fs::read(dir.join(&thumb.file)).ok()
    }

    /// PRD's integrity contract: recompute the sha256 of the stored HTML and
    /// compare it to the record. `Corrupt` when the content is missing, won't
    /// decompress, or its hash no longer matches (a tampered/bit-rotted blob).
    pub fn verify(&self, lang: &str, title: &str) -> Integrity {
        let Some(record) = self.find(lang, title) else {
            return Integrity::Corrupt;
        };
        let Some(root) = self.root.as_ref() else {
            // An in-memory store has nothing on disk to verify against; treat
            // the record as intact (it was never persisted).
            return Integrity::Ok;
        };
        let Ok(compressed) = std::fs::read(root.join(&record.content_file)) else {
            return Integrity::Corrupt;
        };
        let Ok(html_bytes) = zstd::stream::decode_all(compressed.as_slice()) else {
            return Integrity::Corrupt;
        };
        if sha256_hex(&html_bytes) == record.sha256 {
            Integrity::Ok
        } else {
            Integrity::Corrupt
        }
    }

    /// Verify every saved page, returning `(lang, title, integrity)` per
    /// record — the `:saved` browser's ok/corrupt column.
    pub fn verify_all(&self) -> Vec<(String, String, Integrity)> {
        self.records
            .iter()
            .map(|r| {
                (
                    r.lang.clone(),
                    r.title.clone(),
                    self.verify(&r.lang, &r.title),
                )
            })
            .collect()
    }

    /// Un-pin `(lang, title)`: drops the index line (via the jsonl safe-delete)
    /// and removes the content blob and any thumbnails. Returns the removed
    /// record and whether the index rewrite persisted.
    pub fn remove(
        &mut self,
        lang: &str,
        title: &str,
    ) -> Option<(SavedRecord, std::io::Result<()>)> {
        let pos = self
            .records
            .iter()
            .position(|r| r.lang == lang && r.title == title)?;
        let removed = self.records.remove(pos);
        if let Some(root) = &self.root {
            let _ = std::fs::remove_file(root.join(&removed.content_file));
            let _ = std::fs::remove_dir_all(root.join(Self::thumb_dir_rel(lang, title)));
        }
        let persisted = match self.index_path() {
            Some(index) => crate::jsonl::rewrite_matching::<SavedRecord, _>(
                &index,
                |r| r.lang == removed.lang && r.title == removed.title,
                None,
            )
            .map(|_| ()),
            None => Ok(()),
        };
        Some((removed, persisted))
    }
}

/// `$XDG_DATA_HOME/wikitui/saved/` (PRD §6.4) — the pinned-store root, deliberately
/// under the *data* dir, never the cache dir.
fn saved_root() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "wikitui").map(|d| d.data_dir().join("saved"))
}

/// sha256 as a lowercase hex string.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_store() -> (SavedPages, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("wikitui-saved-test-{}-{n}", std::process::id()));
        (SavedPages::at(root.clone()), root)
    }

    fn thumb(bytes: usize) -> (String, Vec<u8>) {
        ("http://example/img.png".to_string(), vec![0xAB; bytes])
    }

    #[test]
    fn tier_parses_and_labels_round_trip() {
        assert_eq!(Tier::parse("t0"), Some(Tier::T0));
        assert_eq!(Tier::parse("T1"), Some(Tier::T1));
        assert_eq!(Tier::parse("t2"), Some(Tier::T2));
        assert_eq!(Tier::parse("t3"), None);
        assert_eq!(Tier::T0.label(), "T0");
        // Serde uses the unit-variant name, matching the label.
        let json = serde_json::to_string(&Tier::T2).unwrap();
        assert_eq!(json, "\"T2\"");
    }

    #[test]
    fn save_get_remove_round_trip() {
        let (mut store, root) = temp_store();
        assert!(!store.is_saved("en", "Alan Turing"));

        let rec = store
            .save(
                "en",
                "Alan Turing",
                1001,
                Tier::T0,
                "<html><body><p>hi</p></body></html>",
                &[],
                vec![],
                "S save",
            )
            .unwrap();
        assert_eq!(rec.tier, Tier::T0);
        assert!(store.is_saved("en", "Alan Turing"));

        let got = store.get("en", "Alan Turing").expect("served offline");
        assert_eq!(got.html, "<html><body><p>hi</p></body></html>");
        assert_eq!(got.revid, 1001);

        // A fresh load from the same root sees the persisted record.
        let reloaded = SavedPages::at(root.clone());
        assert!(reloaded.is_saved("en", "Alan Turing"));

        let (removed, persisted) = store.remove("en", "Alan Turing").unwrap();
        assert_eq!(removed.title, "Alan Turing");
        assert!(persisted.is_ok());
        assert!(!store.is_saved("en", "Alan Turing"));
        assert!(store.get("en", "Alan Turing").is_none());
        // The content blob is gone from disk too.
        assert!(!root.join(&removed.content_file).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn integrity_ok_when_intact_and_corrupt_when_tampered() {
        let (mut store, root) = temp_store();
        let rec = store
            .save(
                "en",
                "Turing",
                1,
                Tier::T0,
                "<p>original</p>",
                &[],
                vec![],
                "",
            )
            .unwrap();
        assert_eq!(store.verify("en", "Turing"), Integrity::Ok);

        // Tamper with the on-disk content: overwrite with different (valid
        // zstd) bytes so it decompresses but no longer matches the sha256.
        let tampered = zstd::stream::encode_all("<p>TAMPERED</p>".as_bytes(), ZSTD_LEVEL).unwrap();
        std::fs::write(root.join(&rec.content_file), &tampered).unwrap();
        assert_eq!(
            store.verify("en", "Turing"),
            Integrity::Corrupt,
            "a changed HTML blob must fail the sha256 check"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn integrity_corrupt_when_content_unreadable() {
        let (mut store, root) = temp_store();
        let rec = store
            .save("en", "Turing", 1, Tier::T0, "<p>x</p>", &[], vec![], "")
            .unwrap();
        // Non-zstd garbage → decode fails → corrupt.
        std::fs::write(root.join(&rec.content_file), b"not zstd at all").unwrap();
        assert_eq!(store.verify("en", "Turing"), Integrity::Corrupt);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tier_semantics_t0_has_no_thumbs_t1_does_t2_has_summaries() {
        let (mut store, root) = temp_store();

        let t0 = store
            .save("en", "A", 1, Tier::T0, "<p>a</p>", &[], vec![], "")
            .unwrap();
        assert!(t0.thumbs.is_empty(), "T0 stores HTML only");
        assert!(t0.summaries.is_empty());
        assert_eq!(t0.thumb_bytes, 0);

        let t1 = store
            .save(
                "en",
                "B",
                1,
                Tier::T1,
                "<p>b</p>",
                &[thumb(100), thumb(50)],
                vec![],
                "",
            )
            .unwrap();
        assert_eq!(t1.thumbs.len(), 2, "T1 pins thumbnails");
        assert_eq!(t1.thumb_bytes, 150);
        assert!(t1.summaries.is_empty());
        // The thumbnail bytes are actually on disk and retrievable by src.
        assert_eq!(
            store
                .thumb_bytes("en", "B", "http://example/img.png")
                .map(|b| b.len()),
            Some(100)
        );

        let summaries = vec![LinkSummary {
            title: "Linked".to_string(),
            extract: "A short extract.".to_string(),
        }];
        let t2 = store
            .save("en", "C", 1, Tier::T2, "<p>c</p>", &[], summaries, "")
            .unwrap();
        assert_eq!(t2.summaries.len(), 1, "T2 stores link summaries");
        assert_eq!(t2.summaries[0].title, "Linked");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn t1_thumbnails_are_capped() {
        let (mut store, root) = temp_store();
        let many: Vec<(String, Vec<u8>)> = (0..MAX_T1_THUMBS + 5)
            .map(|i| (format!("http://example/{i}.png"), vec![0u8; 10]))
            .collect();
        let rec = store
            .save("en", "Gallery", 1, Tier::T1, "<p>g</p>", &many, vec![], "")
            .unwrap();
        assert_eq!(
            rec.thumbs.len(),
            MAX_T1_THUMBS,
            "thumbnail count is capped at MAX_T1_THUMBS"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn total_bytes_sums_html_and_thumbs() {
        let (mut store, root) = temp_store();
        store
            .save("en", "A", 1, Tier::T0, "12345", &[], vec![], "")
            .unwrap();
        store
            .save("en", "B", 1, Tier::T1, "12345", &[thumb(20)], vec![], "")
            .unwrap();
        // 5 + 5 HTML bytes + 20 thumb bytes.
        assert_eq!(store.total_bytes(), 5 + 5 + 20);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn re_saving_replaces_rather_than_duplicates() {
        let (mut store, root) = temp_store();
        store
            .save("en", "A", 1, Tier::T0, "<p>v1</p>", &[], vec![], "")
            .unwrap();
        store
            .save(
                "en",
                "A",
                2,
                Tier::T1,
                "<p>v2</p>",
                &[thumb(10)],
                vec![],
                "",
            )
            .unwrap();
        assert_eq!(
            store.records.len(),
            1,
            "same (lang,title) is not duplicated"
        );
        let got = store.get("en", "A").unwrap();
        assert_eq!(got.html, "<p>v2</p>");
        assert_eq!(got.revid, 2);
        assert_eq!(store.find("en", "A").unwrap().tier, Tier::T1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PRD §5.7's headline distinction: a saved page lives in its own tree and
    /// is *never* touched by the cache's eviction. Fill a tiny-capped cache
    /// well past its cap (forcing eviction) and confirm the saved page — in a
    /// wholly separate directory — still serves.
    #[test]
    fn saved_survives_cache_eviction() {
        use crate::cache::PageCache;

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("wikitui-saved-vs-cache-{}-{n}", std::process::id()));
        let saved_root = base.join("data").join("saved");
        let cache_dir = base.join("cache").join("pages");

        let mut store = SavedPages::at(saved_root.clone());
        store
            .save(
                "en",
                "Pinned",
                1,
                Tier::T0,
                "<p>pinned content that must never be evicted</p>",
                &[],
                vec![],
                "",
            )
            .unwrap();

        // A cache with an absurdly small cap; writing two entries forces the
        // LRU eviction path to run.
        let cache = PageCache::at(cache_dir, 64, crate::cache::FRESH_TTL_SECS, u64::MAX);
        cache.put("en", "One", &"x".repeat(4096), 1, None);
        cache.put("en", "Two", &"y".repeat(4096), 2, None);

        // The saved page is untouched by any of that.
        assert!(
            store.get("en", "Pinned").is_some(),
            "cache eviction must never reach the pinned saved store"
        );
        assert_eq!(store.verify("en", "Pinned"), Integrity::Ok);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn get_on_missing_and_disabled_store() {
        let (store, root) = temp_store();
        assert!(store.get("en", "Nope").is_none());
        let mem = SavedPages::in_memory();
        assert!(mem.get("en", "Nope").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
