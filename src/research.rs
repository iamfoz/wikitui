//! Research mode: a running bibliography built as you read. Two kinds of
//! entries get saved here — a proper citation for the Wikipedia article
//! you're reading (`self_citation`), and the sources *that article itself*
//! cites, extracted from its References section (`doc::extract_citations`).
//! Saved entries persist to a local, append-only, line-oriented JSONL file
//! (PRD §6.4's plain-file storage philosophy: git/syncthing-friendly, no
//! server, no account risk).

use serde::{Deserialize, Serialize};
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
    /// session. `App::new`'s own default (see `App.research`'s doc comment)
    /// so every test that builds an `App` gets this rather than the real
    /// platform data directory, and a natural fit for a future incognito
    /// mode (PRD FR-PR-3) that shouldn't write research state to disk.
    pub fn in_memory() -> Self {
        Self {
            citations: Vec::new(),
            path: None,
        }
    }

    fn load_from(path: PathBuf) -> Self {
        Self {
            citations: crate::jsonl::load(&path),
            path: Some(path),
        }
    }

    /// Whether this store has no on-disk backing — i.e. came from
    /// [`Self::in_memory`] rather than [`Self::load`] finding a real
    /// directory. Test-only: lets `app.rs`'s H3 regression test confirm
    /// `App::new` never resolves the real platform data directory without
    /// exposing the private `path` field itself.
    #[cfg(test)]
    pub(crate) fn is_in_memory(&self) -> bool {
        self.path.is_none()
    }

    /// Appends to the in-memory list and, if persistence is available, the
    /// on-disk file. A write failure is swallowed rather than propagated —
    /// losing a citation from disk (but not from the current session) is
    /// preferable to crashing the reader over a permissions error.
    pub fn add(&mut self, citation: SavedCitation) {
        if let Some(path) = &self.path {
            let _ = crate::jsonl::append(path, &citation);
        }
        self.citations.push(citation);
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
    /// equal to `target`, keeping every other line byte-for-byte — see
    /// `jsonl::rewrite_matching`'s doc comment for the full safety
    /// contract (fresh read, unique-temp-file + fsync + rename) this
    /// delegates to.
    fn remove_line_from_file(path: &Path, target: &SavedCitation) -> std::io::Result<()> {
        crate::jsonl::rewrite_matching(path, |c: &SavedCitation| c == target, None).map(|_| ())
    }
}

fn citations_path() -> Option<PathBuf> {
    Some(crate::paths::wikitui_data_dir()?.join("citations.jsonl"))
}

/// A ready-to-save citation for the Wikipedia article currently being read
/// — the "cite this entry" half of Research mode, formatted close to a
/// plain-text MLA-style web citation.
pub fn self_citation(title: &str, lang: &str) -> Citation {
    Citation {
        id: "self".to_string(),
        text: format!("\"{title}.\" Wikipedia, The Free Encyclopedia. Wikimedia Foundation."),
        url: Some(article_url(title, lang)),
    }
}

/// The canonical web URL for an article — shared by citations and the
/// clipboard-yank keys.
pub fn article_url(title: &str, lang: &str) -> String {
    format!(
        "https://{lang}.wikipedia.org/wiki/{}",
        title.replace(' ', "_")
    )
}

/// PRD FR-DL-5 / §7's "Redlink followed" card: the wiki's own "create this
/// page" URL — `action=edit` on a nonexistent title opens MediaWiki's page
/// creation editor directly, the same link a reader would reach by clicking
/// a live redlink on wikipedia.org itself. Yankable via the redlink card's
/// `y` (PRD's "yankable create-URL").
pub fn create_page_url(title: &str, lang: &str) -> String {
    format!(
        "https://{lang}.wikipedia.org/w/index.php?title={}&action=edit",
        title.replace(' ', "_")
    )
}

/// Today's date as `YYYY-MM-DD`, for a citation's "retrieved on" field.
/// Local time, not UTC: an access date is the date on the researcher's
/// own calendar (UTC would date an evening US save "tomorrow").
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// PRD §10's revision-specific permalink — the `oldid=` form Wikipedia's own
/// sidebar calls "Permanent link": it pins the exact revision a reader saw,
/// stable even after the article is later edited, which is what an
/// attribution reference should point at (the version the ShareAlike terms
/// actually cover, not whatever the title currently resolves to). Shares
/// `article_url`/`create_page_url`'s underscore-for-space convention.
pub fn permalink_url(title: &str, lang: &str, revid: u64) -> String {
    format!(
        "https://{lang}.wikipedia.org/w/index.php?title={}&oldid={revid}",
        title.replace(' ', "_")
    )
}

/// PRD §10's "permalink to the article's history": the full revision-history
/// listing (`action=history`) — every editor who has ever touched the
/// article, which is the authorship record Wikimedia's reuse terms point
/// reusers at. Distinct from [`permalink_url`]'s single pinned revision.
pub fn history_url(title: &str, lang: &str) -> String {
    format!(
        "https://{lang}.wikipedia.org/w/index.php?title={}&action=history",
        title.replace(' ', "_")
    )
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

    /// PRD §10's `oldid=` permalink form: pins the exact revision, not just
    /// the title, and underscores spaces the same way `article_url` does.
    #[test]
    fn permalink_url_uses_the_oldid_form() {
        assert_eq!(
            permalink_url("Alan Turing", "en", 123456),
            "https://en.wikipedia.org/w/index.php?title=Alan_Turing&oldid=123456"
        );
    }

    /// A revid of 0 (no revision loaded yet) is still a well-formed URL —
    /// callers decide whether 0 is meaningful to show, not this constructor.
    #[test]
    fn permalink_url_does_not_special_case_a_zero_revid() {
        assert!(permalink_url("Stub", "en", 0).ends_with("&oldid=0"));
    }

    /// PRD §10's "permalink to the article's history": `action=history`,
    /// distinct from the single-revision `oldid=` permalink above.
    #[test]
    fn history_url_uses_the_action_history_form() {
        assert_eq!(
            history_url("Alan Turing", "en"),
            "https://en.wikipedia.org/w/index.php?title=Alan_Turing&action=history"
        );
    }

    /// Non-English editions and multi-word titles both underscore correctly
    /// in the history form (mirrors `create_page_url`'s own coverage).
    #[test]
    fn history_url_handles_non_english_lang_and_multiword_titles() {
        assert_eq!(
            history_url("New York City", "de"),
            "https://de.wikipedia.org/w/index.php?title=New_York_City&action=history"
        );
    }
}
