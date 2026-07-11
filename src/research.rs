//! Research mode: a running bibliography built as you read. Two kinds of
//! entries get saved here — a proper citation for the Wikipedia article
//! you're reading (`self_citation`), and the sources *that article itself*
//! cites, extracted from its References section (`doc::extract_citations`).
//! Saved entries persist to a local, append-only, line-oriented JSONL file
//! (PRD §6.4's plain-file storage philosophy: git/syncthing-friendly, no
//! server, no account risk).

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::doc::Citation;

/// What a saved entry actually is, which determines how much a citation
/// style can do with it (see `cite`): an `Article` entry has known
/// structure (its subject is `source_article`, on `source_lang`.wikipedia,
/// retrieved `saved_at`) and can be re-formatted per style; a `Reference`
/// is raw text scraped from an article's References section and can only
/// be reproduced verbatim with provenance attached.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CitationKind {
    Article,
    /// The serde default so `citations.jsonl` files written before this
    /// field existed still load, and (via `serde(other)`) the catch-all
    /// for kinds written by future wikitui versions — misclassifying an
    /// entry as a reference only costs it style formatting, never data.
    #[default]
    #[serde(other)]
    Reference,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedCitation {
    /// The article you were reading when you saved this — not necessarily
    /// the citation's own subject (a saved reference's subject is whatever
    /// its own text describes; a saved self-citation's subject IS this
    /// article).
    pub source_article: String,
    pub source_lang: String,
    pub text: String,
    pub url: Option<String>,
    pub saved_at: String,
    #[serde(default)]
    pub kind: CitationKind,
}

pub struct ResearchStore {
    pub citations: Vec<SavedCitation>,
    /// `None` when no data directory could be found for this platform —
    /// saves still work for the rest of the session, just in-memory only,
    /// rather than silently failing the save entirely.
    path: Option<PathBuf>,
}

impl ResearchStore {
    /// Loads from the platform data directory (`wikitui/citations.jsonl`),
    /// degrading to an empty in-memory-only store if that directory can't
    /// be determined or the file can't be read — a missing bibliography
    /// file is not a reason to refuse to start the reader.
    pub fn load() -> Self {
        match citations_path() {
            Some(path) => Self::load_from(path),
            None => Self::in_memory(),
        }
    }

    /// A store with no on-disk persistence at all: saves only last the
    /// session. Used by tests elsewhere in the crate that want `App`'s
    /// citation-saving state machine exercised without touching the real
    /// platform data directory, and a natural fit for a future incognito
    /// mode (PRD FR-PR-3) that shouldn't write research state to disk.
    pub fn in_memory() -> Self {
        Self {
            citations: Vec::new(),
            path: None,
        }
    }

    fn load_from(path: PathBuf) -> Self {
        let citations = std::fs::read_to_string(&path)
            .ok()
            .map(|content| {
                content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            citations,
            path: Some(path),
        }
    }

    /// Appends to the in-memory list and, if persistence is available, the
    /// on-disk file. A write failure is swallowed rather than propagated —
    /// losing a citation from disk (but not from the current session) is
    /// preferable to crashing the reader over a permissions error.
    pub fn add(&mut self, citation: SavedCitation) {
        if let Some(path) = &self.path {
            let _ = Self::append(path, &citation);
        }
        self.citations.push(citation);
    }

    fn append(path: &Path, citation: &SavedCitation) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let line = serde_json::to_string(citation).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")
    }

    /// Deletes the entry at `index` — deletion is the one operation that
    /// can't be append-only. Returns the removed entry and the outcome of
    /// persisting the deletion (an in-memory-only store trivially
    /// succeeds), or `None` if the index was out of range. Callers must
    /// surface a persistence failure: reporting "deleted" for an entry
    /// that will resurrect on the next launch is worse than failing.
    pub fn remove(&mut self, index: usize) -> Option<(SavedCitation, std::io::Result<()>)> {
        if index >= self.citations.len() {
            return None;
        }
        let removed = self.citations.remove(index);
        let persisted = match &self.path {
            Some(path) => Self::remove_line_from_file(path, &removed),
            None => Ok(()),
        };
        Some((removed, persisted))
    }

    /// Removes from the file the first line that parses to a citation
    /// equal to `target`, keeping every other line byte-for-byte. Working
    /// from a fresh read of the file — never from this instance's
    /// in-memory snapshot — means lines this version can't parse (a
    /// corrupt byte, an entry written by a newer wikitui) and citations
    /// appended by another running instance since we loaded all survive a
    /// delete instead of being silently erased with it.
    ///
    /// The write goes through a unique (per-call) temp file in the same
    /// directory, fsynced before an atomic rename, so neither a process
    /// crash nor — on typical filesystems — a power loss can destroy the
    /// bibliography. Two instances deleting at the same moment can still
    /// race on the final rename; the loser's *deletion* may not stick
    /// (its entry reappears), but no other entry is ever lost.
    fn remove_line_from_file(path: &Path, target: &SavedCitation) -> std::io::Result<()> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            // No file yet (nothing this store added ever persisted):
            // there's nothing the deletion needs to update.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut kept: Vec<&str> = Vec::new();
        let mut removed_one = false;
        for line in content.lines() {
            if !removed_one
                && serde_json::from_str::<SavedCitation>(line).ok().as_ref() == Some(target)
            {
                removed_one = true;
                continue;
            }
            kept.push(line);
        }
        if !removed_one {
            // Not on disk (already removed externally, or its add() never
            // persisted) — nothing to rewrite.
            return Ok(());
        }

        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        // The temp name must be unique per *call*, not just per process:
        // two concurrent removes (different threads, or different stores
        // sharing a directory) would otherwise clobber each other's temp
        // file between write and rename.
        static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("citations.jsonl");
        let tmp = path.with_file_name(format!(".{base}.{}.{unique}.tmp", std::process::id()));
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(out.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        // Best-effort directory sync so the rename itself is durable;
        // opening a directory for sync only works on Unix, and its
        // failure shouldn't fail the (already-visible) rename.
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

fn citations_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    Some(dirs.data_dir().join("citations.jsonl"))
}

/// A ready-to-save citation for the Wikipedia article currently being read
/// — the "cite this entry" half of Research mode, formatted close to a
/// plain-text MLA-style web citation.
pub fn self_citation(title: &str, lang: &str) -> Citation {
    let url = format!(
        "https://{lang}.wikipedia.org/wiki/{}",
        title.replace(' ', "_")
    );
    Citation {
        id: "self".to_string(),
        text: format!("\"{title}.\" Wikipedia, The Free Encyclopedia. Wikimedia Foundation."),
        url: Some(url),
    }
}

/// Today's date as `YYYY-MM-DD`, for a citation's "retrieved on" field.
/// Local time, not UTC: an access date is the date on the researcher's
/// own calendar (UTC would date an evening US save "tomorrow").
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique path per test (tests run in parallel by default), cleaned
    /// up at the end of each test that uses one.
    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-test-citations-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    fn sample(text: &str) -> SavedCitation {
        SavedCitation {
            source_article: "Alan Turing".to_string(),
            source_lang: "en".to_string(),
            text: text.to_string(),
            url: Some("https://example.com".to_string()),
            saved_at: "2026-01-01".to_string(),
            kind: CitationKind::Reference,
        }
    }

    #[test]
    fn self_citation_includes_title_and_wiki_url() {
        let citation = self_citation("Alan Turing", "en");
        assert!(citation.text.contains("Alan Turing"));
        assert_eq!(
            citation.url.as_deref(),
            Some("https://en.wikipedia.org/wiki/Alan_Turing")
        );
    }

    #[test]
    fn self_citation_underscores_spaces_in_the_url_only() {
        let citation = self_citation("New York City", "en");
        assert!(
            citation.text.contains("New York City"),
            "display text keeps real spaces"
        );
        assert!(citation.url.unwrap().ends_with("New_York_City"));
    }

    #[test]
    fn today_produces_a_plausible_iso_date() {
        let date = today();
        assert_eq!(date.len(), 10, "expected YYYY-MM-DD, got {date:?}");
        assert_eq!(date.chars().filter(|c| *c == '-').count(), 2);
        assert!(
            date.starts_with("20"),
            "sanity check: this runs in the 2000s, got {date:?}"
        );
    }

    #[test]
    fn add_persists_across_a_fresh_load_from_the_same_path() {
        let path = temp_path();
        let mut store = ResearchStore::load_from(path.clone());
        assert!(store.citations.is_empty());

        store.add(sample("First citation"));
        store.add(sample("Second citation"));
        assert_eq!(store.citations.len(), 2);

        // A brand new store reading the same file should see both saves.
        let reloaded = ResearchStore::load_from(path.clone());
        assert_eq!(reloaded.citations.len(), 2);
        assert_eq!(reloaded.citations[0].text, "First citation");
        assert_eq!(reloaded.citations[1].text, "Second citation");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_nonexistent_file_starts_empty_without_erroring() {
        let path = temp_path();
        assert!(!path.exists());
        let store = ResearchStore::load_from(path);
        assert!(store.citations.is_empty());
    }

    #[test]
    fn add_without_a_persistence_path_still_updates_in_memory() {
        let mut store = ResearchStore::in_memory();
        store.add(sample("In-memory only"));
        assert_eq!(store.citations.len(), 1);
    }

    #[test]
    fn remove_persists_the_deletion_across_a_fresh_load() {
        let path = temp_path();
        let mut store = ResearchStore::load_from(path.clone());
        store.add(sample("Keep me"));
        store.add(sample("Delete me"));
        store.add(sample("Keep me too"));

        let (removed, persisted) = store.remove(1).expect("index 1 exists");
        assert_eq!(removed.text, "Delete me");
        assert!(persisted.is_ok());
        assert_eq!(store.citations.len(), 2);

        let reloaded = ResearchStore::load_from(path.clone());
        let texts: Vec<_> = reloaded.citations.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["Keep me", "Keep me too"]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_out_of_range_returns_none_and_changes_nothing() {
        let mut store = ResearchStore::in_memory();
        store.add(sample("Only entry"));
        assert!(store.remove(5).is_none());
        assert_eq!(store.citations.len(), 1);
    }

    /// The data-loss scenario the code-review reproduced against the old
    /// implementation: a corrupt line and a line from a future file format
    /// must survive an unrelated delete, not be silently erased with it.
    #[test]
    fn remove_preserves_lines_it_cannot_parse() {
        let path = temp_path();
        let mut store = ResearchStore::load_from(path.clone());
        store.add(sample("Delete me"));
        store.add(sample("Keep me"));

        // Simulate corruption and a future format version, appended
        // directly to the file behind the store's back.
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"truncated\":\n");
        raw.push_str("{\"source_article\":\"X\",\"source_lang\":\"en\",\"text\":\"From the future\",\"url\":null,\"saved_at\":\"2027-01-01\",\"kind\":\"book\",\"new_field\":42}\n");
        std::fs::write(&path, raw).unwrap();

        let (_, persisted) = store.remove(0).expect("index 0 exists");
        assert!(persisted.is_ok());

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("Delete me"));
        assert!(after.contains("Keep me"));
        assert!(
            after.contains("{\"truncated\":"),
            "corrupt line must survive a delete byte-for-byte"
        );
        assert!(
            after.contains("\"new_field\":42"),
            "future-format line must survive with unknown fields intact"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Citations appended by another instance after this one loaded must
    /// survive a delete — remove works from a fresh read of the file, not
    /// this instance's stale snapshot.
    #[test]
    fn remove_preserves_citations_appended_by_another_instance() {
        let path = temp_path();
        let mut ours = ResearchStore::load_from(path.clone());
        ours.add(sample("Ours: delete me"));

        // A second instance appends after we loaded.
        let mut theirs = ResearchStore::load_from(path.clone());
        theirs.add(sample("Theirs: must survive"));

        let (_, persisted) = ours.remove(0).expect("our entry exists");
        assert!(persisted.is_ok());

        let reloaded = ResearchStore::load_from(path.clone());
        let texts: Vec<_> = reloaded.citations.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["Theirs: must survive"]);

        let _ = std::fs::remove_file(&path);
    }

    /// `kind` values written by future versions degrade to Reference
    /// instead of failing the whole line (serde(other)).
    #[test]
    fn unknown_citation_kind_degrades_to_reference() {
        let line = r#"{"source_article":"X","source_lang":"en","text":"T","url":null,"saved_at":"2027-01-01","kind":"holotape"}"#;
        let parsed: SavedCitation =
            serde_json::from_str(line).expect("unknown kind must still parse");
        assert_eq!(parsed.kind, CitationKind::Reference);
    }

    /// Deleting an entry whose add() never reached the disk (or that was
    /// already removed externally) reports success without rewriting.
    #[test]
    fn remove_of_an_entry_missing_from_disk_is_ok() {
        let path = temp_path();
        let mut store = ResearchStore::load_from(path.clone());
        store.add(sample("On disk"));
        // Entry present in memory only:
        store.citations.push(sample("Memory only"));

        let (removed, persisted) = store.remove(1).expect("index 1 exists in memory");
        assert_eq!(removed.text, "Memory only");
        assert!(persisted.is_ok());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("On disk"));

        let _ = std::fs::remove_file(&path);
    }

    /// A line written by the previous version of this file format (no
    /// `kind` field) must still load, defaulting to Reference — upgrading
    /// wikitui must never eat an existing bibliography.
    #[test]
    fn pre_kind_jsonl_lines_still_deserialize() {
        let old_line = r#"{"source_article":"Alan Turing","source_lang":"en","text":"Old entry","url":null,"saved_at":"2026-07-11"}"#;
        let parsed: SavedCitation = serde_json::from_str(old_line).expect("old format must parse");
        assert_eq!(parsed.kind, CitationKind::Reference);
        assert_eq!(parsed.text, "Old entry");
    }
}
