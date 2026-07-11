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
    /// field existed still load; misclassifying an old self-citation as a
    /// reference only costs it style formatting, never data.
    #[default]
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

    /// Deletes the entry at `index`, rewriting the whole file — deletion is
    /// the one operation that can't be append-only. Returns the removed
    /// entry, or `None` if the index was out of range. The rewrite goes via
    /// a temp file + rename so a crash mid-write can't destroy the whole
    /// bibliography.
    pub fn remove(&mut self, index: usize) -> Option<SavedCitation> {
        if index >= self.citations.len() {
            return None;
        }
        let removed = self.citations.remove(index);
        if let Some(path) = &self.path {
            let _ = Self::rewrite(path, &self.citations);
        }
        Some(removed)
    }

    fn rewrite(path: &Path, citations: &[SavedCitation]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut content = String::new();
        for citation in citations {
            content.push_str(&serde_json::to_string(citation).map_err(std::io::Error::other)?);
            content.push('\n');
        }
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)
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
pub fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
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

        let removed = store.remove(1).expect("index 1 exists");
        assert_eq!(removed.text, "Delete me");
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
