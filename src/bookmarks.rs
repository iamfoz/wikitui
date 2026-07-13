//! Bookmarks and the read-later queue (PRD §5.6, FR-BM-1/2/3/7): two more
//! plain-file JSONL stores in the same family as `research.rs`'s
//! bibliography — append-only adds, a safe delete/update via
//! `jsonl::rewrite_matching`, `#[serde(default)]` tolerance so an older or
//! newer file's records keep loading — plus the pure, network-free pieces
//! `main.rs`'s `ba`/`rl` key handling needs decided ahead of any I/O
//! (mirroring `app::resolve_hint_action`'s split between "what should
//! happen" and "make it happen").
//!
//! `(lang, title)` is the natural key for both stores: a bookmark or
//! read-later entry is "this article, in this wiki edition", not an
//! arbitrary opaque id — which is also what makes `m`'s bookmark/unbookmark
//! toggle possible (PRD FR-BM-1's documented idiom: bookmarking an
//! already-bookmarked article removes it, rather than erroring or needing a
//! separate unbookmark key).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One saved bookmark (PRD FR-BM-7's stored-fields list). `tags` and `note`
/// start empty/`None` on creation (`m` alone bookmarks with neither) and are
/// filled in later by the picker's `t` (tags) and `ba` (note, PRD FR-BM-2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bookmark {
    pub title: String,
    pub lang: String,
    /// The revid on screen at bookmark time, when known — `None` in
    /// degraded mode (PRD `Tab::current_revid`'s own doc comment: `0` there
    /// means "no real revid", which this field represents honestly as
    /// absent rather than as the sentinel `0`).
    #[serde(default)]
    pub revid_at_bookmark: Option<u64>,
    /// Reserved for a future "bookmark this section" (PRD FR-BM-1 mentions
    /// bookmarking "the current article (or focused section)"): always
    /// `None` today. Present now, with `#[serde(default)]`, so files this
    /// version writes don't need another migration once that lands.
    #[serde(default)]
    pub section_anchor: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// One read-later queue entry (PRD FR-BM-7). Ordering is FIFO by
/// construction: entries are only ever appended, so a store's own `Vec`
/// order already is the queue order — no separate sequence number needed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReadLaterEntry {
    pub title: String,
    pub lang: String,
    pub enqueued_at: String,
    /// Priority queue support (PRD FR-BM-3's "FIFO/priority"): `0` today —
    /// nothing yet reorders by it, but the field is here so a future
    /// priority-aware sort doesn't need another format migration.
    #[serde(default)]
    pub priority: i32,
}

/// `chrono::Local::now` as an RFC 3339 timestamp — bookmarks/read-later
/// record full timestamps (unlike `research::today`'s date-only
/// `saved_at`), since "annotated 3 minutes ago vs. this morning" is a
/// meaningful distinction a running reading queue actually has.
pub fn now_ts() -> String {
    chrono::Local::now().to_rfc3339()
}

/// The `YYYY-MM-DD` portion of one of this module's timestamps, for picker/
/// export display — falls back to the raw string if it's ever shorter than
/// that (hand-edited file, a future format change), never panics on a slice
/// that doesn't land on a char boundary because RFC 3339's first 10 bytes
/// are always plain ASCII digits and hyphens when the string is well-formed,
/// and `get` (not indexing) degrades gracefully when it isn't.
pub fn display_date(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

// ---- Bookmark store --------------------------------------------------

/// What `BookmarkStore::toggle` (PRD FR-BM-1's `m`) actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleOutcome {
    Added,
    Removed,
}

pub struct BookmarkStore {
    pub bookmarks: Vec<Bookmark>,
    /// `None` when no platform data directory could be found — bookmarking
    /// still works for the rest of the session, in-memory only, rather than
    /// refusing the feature outright (mirrors `ResearchStore`).
    path: Option<PathBuf>,
}

impl BookmarkStore {
    pub fn load() -> Self {
        match bookmarks_path() {
            Some(path) => Self::load_from(path),
            None => Self::in_memory(),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            bookmarks: Vec::new(),
            path: None,
        }
    }

    fn load_from(path: PathBuf) -> Self {
        Self {
            bookmarks: crate::jsonl::load(&path),
            path: Some(path),
        }
    }

    pub fn find(&self, lang: &str, title: &str) -> Option<&Bookmark> {
        self.bookmarks
            .iter()
            .find(|b| b.lang == lang && b.title == title)
    }

    pub fn is_bookmarked(&self, lang: &str, title: &str) -> bool {
        self.find(lang, title).is_some()
    }

    /// `m` (PRD FR-BM-1): bookmarks `(lang, title)`, or un-bookmarks it if
    /// it's already saved — the toggle idiom means there's no separate
    /// "unbookmark" key to discover or forget.
    pub fn toggle(&mut self, lang: &str, title: &str, revid: Option<u64>) -> ToggleOutcome {
        if let Some(pos) = self
            .bookmarks
            .iter()
            .position(|b| b.lang == lang && b.title == title)
        {
            let removed = self.bookmarks.remove(pos);
            if let Some(path) = &self.path {
                let _ = crate::jsonl::rewrite_matching::<Bookmark, _>(
                    path,
                    |b| b.lang == removed.lang && b.title == removed.title,
                    None,
                );
            }
            ToggleOutcome::Removed
        } else {
            self.add(lang, title, revid);
            ToggleOutcome::Added
        }
    }

    fn add(&mut self, lang: &str, title: &str, revid: Option<u64>) {
        let now = now_ts();
        let bookmark = Bookmark {
            title: title.to_string(),
            lang: lang.to_string(),
            revid_at_bookmark: revid,
            section_anchor: None,
            created_at: now.clone(),
            updated_at: now,
            tags: Vec::new(),
            note: None,
        };
        if let Some(path) = &self.path {
            let _ = crate::jsonl::append(path, &bookmark);
        }
        self.bookmarks.push(bookmark);
    }

    /// Bookmarks `(lang, title)` unless it already is one (PRD FR-BM-2's
    /// `ba` auto-bookmark: annotating implies wanting to keep the article).
    /// Returns whether a new bookmark was actually created.
    pub fn ensure_bookmarked(&mut self, lang: &str, title: &str, revid: Option<u64>) -> bool {
        if self.is_bookmarked(lang, title) {
            return false;
        }
        self.add(lang, title, revid);
        true
    }

    /// Deletes the bookmark at `index` (the picker's `d`) — the one
    /// non-append-only operation, using the same safe-delete contract as
    /// `research::ResearchStore::remove` (see `jsonl::rewrite_matching`).
    pub fn remove(&mut self, index: usize) -> Option<(Bookmark, std::io::Result<()>)> {
        if index >= self.bookmarks.len() {
            return None;
        }
        let removed = self.bookmarks.remove(index);
        let persisted = match &self.path {
            Some(path) => crate::jsonl::rewrite_matching::<Bookmark, _>(
                path,
                |b| b.lang == removed.lang && b.title == removed.title,
                None,
            )
            .map(|_| ()),
            None => Ok(()),
        };
        Some((removed, persisted))
    }

    /// Replaces the tag set on `(lang, title)` (the picker's `t`). `false`
    /// if no such bookmark exists.
    pub fn set_tags(&mut self, lang: &str, title: &str, tags: Vec<String>) -> bool {
        self.update(lang, title, |b| b.tags = tags)
    }

    /// Replaces the note on `(lang, title)` (PRD FR-BM-2's `ba`, once the
    /// editor exits successfully). `false` if no such bookmark exists —
    /// `main.rs`'s annotate flow calls `ensure_bookmarked` first so this
    /// path is always reachable from `ba`.
    pub fn set_note(&mut self, lang: &str, title: &str, note: Option<String>) -> bool {
        self.update(lang, title, |b| b.note = note)
    }

    fn update(&mut self, lang: &str, title: &str, f: impl FnOnce(&mut Bookmark)) -> bool {
        let Some(pos) = self
            .bookmarks
            .iter()
            .position(|b| b.lang == lang && b.title == title)
        else {
            return false;
        };
        f(&mut self.bookmarks[pos]);
        self.bookmarks[pos].updated_at = now_ts();
        if let Some(path) = &self.path {
            let updated = self.bookmarks[pos].clone();
            let _ = crate::jsonl::rewrite_matching::<Bookmark, _>(
                path,
                |b| b.lang == lang && b.title == title,
                Some(&updated),
            );
        }
        true
    }
}

fn bookmarks_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    Some(dirs.data_dir().join("bookmarks.jsonl"))
}

// ---- Read-later store --------------------------------------------------

pub struct ReadLaterStore {
    pub entries: Vec<ReadLaterEntry>,
    path: Option<PathBuf>,
}

impl ReadLaterStore {
    pub fn load() -> Self {
        match readlater_path() {
            Some(path) => Self::load_from(path),
            None => Self::in_memory(),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            path: None,
        }
    }

    fn load_from(path: PathBuf) -> Self {
        Self {
            entries: crate::jsonl::load(&path),
            path: Some(path),
        }
    }

    pub fn contains(&self, lang: &str, title: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.lang == lang && e.title == title)
    }

    /// Enqueues `entry` unless `(lang, title)` is already queued (PRD
    /// FR-BM-3: the same article doesn't need two competing slots in one
    /// reading queue). Returns whether it was actually added.
    pub fn enqueue(&mut self, entry: ReadLaterEntry) -> bool {
        if self.contains(&entry.lang, &entry.title) {
            return false;
        }
        if let Some(path) = &self.path {
            let _ = crate::jsonl::append(path, &entry);
        }
        self.entries.push(entry);
        true
    }

    /// Removes the entry at `index` — used both by the picker's `d` and by
    /// "opening a read-later entry auto-dequeues it" (PRD FR-BM-3, config
    /// `readlater_auto_dequeue`). Same safe-delete contract as
    /// `BookmarkStore::remove`.
    pub fn remove(&mut self, index: usize) -> Option<(ReadLaterEntry, std::io::Result<()>)> {
        if index >= self.entries.len() {
            return None;
        }
        let removed = self.entries.remove(index);
        let persisted = match &self.path {
            Some(path) => {
                crate::jsonl::rewrite_matching::<ReadLaterEntry, _>(path, |e| *e == removed, None)
                    .map(|_| ())
            }
            None => Ok(()),
        };
        Some((removed, persisted))
    }
}

fn readlater_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    Some(dirs.data_dir().join("readlater.jsonl"))
}

// ---- Bookmark picker filter grammar (PRD FR-BM-1) ----------------------
//
// Grammar: whitespace-separated tokens. A token starting with `#` names a
// required tag (case-insensitive, exact — "having ALL named tags", an AND,
// not an OR); every other token is fuzzy-matched against the title. `#crypto
// #ww2 turing` therefore means "tagged both crypto AND ww2, and the title
// fuzzy-matches turing". An empty filter (no input at all) matches
// everything.

/// One parsed `/` filter expression.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookmarkFilter {
    /// Lowercased, `#`-stripped required tags.
    tags: Vec<String>,
    /// The non-`#` tokens, rejoined with single spaces, fuzzy-matched
    /// against the title.
    text: String,
}

pub fn parse_filter(input: &str) -> BookmarkFilter {
    let mut tags = Vec::new();
    let mut text_terms = Vec::new();
    for token in input.split_whitespace() {
        match token.strip_prefix('#') {
            Some(tag) if !tag.is_empty() => tags.push(tag.to_lowercase()),
            Some(_) => {} // a bare "#" names no tag — ignored, not an error
            None => text_terms.push(token),
        }
    }
    BookmarkFilter {
        tags,
        text: text_terms.join(" "),
    }
}

/// A minimal subsequence "fuzzy" match (the fzf-lite idiom): every character
/// of `query` must appear in `text`, in that order, case-insensitively —
/// not necessarily contiguous. An empty query matches anything, which is
/// what makes an all-tags, no-text filter (`#crypto`) match by tag alone.
/// Delegates to `crate::fuzzy`, shared with the history picker's `/` filter
/// and ranked search (PRD FR-HS-1) — kept as a thin wrapper here so this
/// module's existing call sites and tests don't need to change.
pub fn fuzzy_matches(text: &str, query: &str) -> bool {
    crate::fuzzy::fuzzy_matches(text, query)
}

/// Whether `bookmark` satisfies `filter`: every named tag present (AND),
/// and the title fuzzy-matches whatever free text remains.
pub fn matches_filter(bookmark: &Bookmark, filter: &BookmarkFilter) -> bool {
    let has_all_tags = filter
        .tags
        .iter()
        .all(|t| bookmark.tags.iter().any(|bt| bt.eq_ignore_ascii_case(t)));
    has_all_tags && fuzzy_matches(&bookmark.title, &filter.text)
}

/// Parses the `t` tag-edit prompt's comma/space-separated input into a tag
/// set: trims each token, drops empties, and strips a leading `#` some
/// users will type out of habit (tags are stored bare; the picker adds the
/// `#` at display time).
pub fn parse_tags(input: &str) -> Vec<String> {
    input
        .split([',', ' '])
        .map(str::trim)
        .map(|t| t.strip_prefix('#').unwrap_or(t))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

// ---- `ba` annotate: pure editor-invocation helpers (PRD FR-BM-2, SEC-5) -

/// Splits a `$EDITOR` value into its program and leading arguments. `$EDITOR`
/// conventionally carries flags too (`"code --wait"`, `"vim -u NONE"`), so a
/// bare `Command::new(&editor)` would try to execute the whole string as one
/// (nonexistent) binary name. Deliberately simple whitespace splitting, not
/// full shell parsing: SEC-5 only requires that this run a *user-configured*
/// command, never a content-derived one — `$EDITOR` is the user's own
/// trusted setting, so there is no untrusted string here that needs
/// quoting/escaping semantics. `None` for an empty/whitespace-only spec.
pub fn parse_editor_command(spec: &str) -> Option<(String, Vec<String>)> {
    let mut parts = spec.split_whitespace();
    let program = parts.next()?.to_string();
    Some((program, parts.map(str::to_string).collect()))
}

/// The temp file's starting content for `ba`: the bookmark's existing note
/// verbatim, or empty for a fresh annotation. Never a placeholder comment —
/// saving without touching anything must reproduce exactly the note that
/// was there (including a legitimately empty one).
pub fn seed_note_content(existing: Option<&str>) -> String {
    existing.unwrap_or_default().to_string()
}

/// What the editor left behind becomes the new note: trimmed of the
/// trailing newline(s) every editor adds on save, and `None` — "no note" —
/// when the user deleted everything, rather than a technically-non-empty
/// note some editors would otherwise leave (a lone `\n`).
pub fn note_from_editor_output(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-test-{tag}-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    // ---- BookmarkStore --------------------------------------------------

    #[test]
    fn toggle_adds_then_removes_the_same_article() {
        let mut store = BookmarkStore::in_memory();
        assert!(!store.is_bookmarked("en", "Alan Turing"));

        let outcome = store.toggle("en", "Alan Turing", Some(7));
        assert_eq!(outcome, ToggleOutcome::Added);
        assert!(store.is_bookmarked("en", "Alan Turing"));
        assert_eq!(store.bookmarks[0].revid_at_bookmark, Some(7));

        let outcome = store.toggle("en", "Alan Turing", Some(7));
        assert_eq!(outcome, ToggleOutcome::Removed);
        assert!(!store.is_bookmarked("en", "Alan Turing"));
    }

    #[test]
    fn toggle_persists_across_a_fresh_load_from_the_same_path() {
        let path = temp_path("bookmarks-roundtrip");
        let mut store = BookmarkStore::load_from(path.clone());
        store.toggle("en", "Alan Turing", None);
        store.toggle("de", "Berlin", Some(3));

        let reloaded = BookmarkStore::load_from(path.clone());
        assert_eq!(reloaded.bookmarks.len(), 2);
        assert!(reloaded.is_bookmarked("en", "Alan Turing"));
        assert!(reloaded.is_bookmarked("de", "Berlin"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn toggle_off_removes_exactly_that_line_and_survives_a_reload() {
        let path = temp_path("bookmarks-toggle-off");
        let mut store = BookmarkStore::load_from(path.clone());
        store.toggle("en", "A", None);
        store.toggle("en", "B", None);
        store.toggle("en", "A", None); // un-bookmark A

        let reloaded = BookmarkStore::load_from(path.clone());
        let titles: Vec<_> = reloaded
            .bookmarks
            .iter()
            .map(|b| b.title.as_str())
            .collect();
        assert_eq!(titles, vec!["B"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_preserves_lines_it_cannot_parse() {
        let path = temp_path("bookmarks-corrupt");
        let mut store = BookmarkStore::load_from(path.clone());
        store.toggle("en", "Delete me", None);
        store.toggle("en", "Keep me", None);

        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"not\":\"a bookmark\"}\n");
        std::fs::write(&path, raw).unwrap();

        store.remove(0);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("Delete me"));
        assert!(after.contains("Keep me"));
        assert!(after.contains("{\"not\":\"a bookmark\"}"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ensure_bookmarked_is_a_no_op_when_already_bookmarked() {
        let mut store = BookmarkStore::in_memory();
        assert!(store.ensure_bookmarked("en", "Alan Turing", None));
        assert_eq!(store.bookmarks.len(), 1);
        assert!(!store.ensure_bookmarked("en", "Alan Turing", None));
        assert_eq!(store.bookmarks.len(), 1, "must not create a duplicate");
    }

    #[test]
    fn set_tags_and_set_note_persist_and_touch_updated_at() {
        let path = temp_path("bookmarks-update");
        let mut store = BookmarkStore::load_from(path.clone());
        store.toggle("en", "Alan Turing", None);
        let created = store.bookmarks[0].created_at.clone();

        assert!(store.set_tags("en", "Alan Turing", vec!["crypto".into(), "ww2".into()]));
        assert!(store.set_note("en", "Alan Turing", Some("great article".into())));

        let reloaded = BookmarkStore::load_from(path.clone());
        let b = reloaded.find("en", "Alan Turing").unwrap();
        assert_eq!(b.tags, vec!["crypto", "ww2"]);
        assert_eq!(b.note.as_deref(), Some("great article"));
        assert_eq!(b.created_at, created, "created_at must never change");
        assert!(
            b.updated_at >= created,
            "an edit touches updated_at forward, and it round-trips through the file"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_tags_on_a_missing_bookmark_reports_false() {
        let mut store = BookmarkStore::in_memory();
        assert!(!store.set_tags("en", "Nope", vec!["x".into()]));
        assert!(!store.set_note("en", "Nope", Some("x".into())));
    }

    #[test]
    fn pre_tags_jsonl_lines_still_deserialize() {
        // A line written before `tags`/`note`/`section_anchor` existed.
        let old = r#"{"title":"Alan Turing","lang":"en","created_at":"2026-01-01T00:00:00-00:00","updated_at":"2026-01-01T00:00:00-00:00"}"#;
        let parsed: Bookmark = serde_json::from_str(old).expect("old format must parse");
        assert!(parsed.tags.is_empty());
        assert!(parsed.note.is_none());
        assert!(parsed.section_anchor.is_none());
        assert!(parsed.revid_at_bookmark.is_none());
    }

    // ---- ReadLaterStore ---------------------------------------------------

    fn entry(title: &str) -> ReadLaterEntry {
        ReadLaterEntry {
            title: title.to_string(),
            lang: "en".to_string(),
            enqueued_at: now_ts(),
            priority: 0,
        }
    }

    #[test]
    fn enqueue_is_fifo_ordered() {
        let mut store = ReadLaterStore::in_memory();
        assert!(store.enqueue(entry("First")));
        assert!(store.enqueue(entry("Second")));
        assert!(store.enqueue(entry("Third")));
        let titles: Vec<_> = store.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["First", "Second", "Third"]);
    }

    #[test]
    fn duplicate_enqueue_is_rejected() {
        let mut store = ReadLaterStore::in_memory();
        assert!(store.enqueue(entry("Alan Turing")));
        assert!(
            !store.enqueue(entry("Alan Turing")),
            "the same (lang, title) must not get a second queue slot"
        );
        assert_eq!(store.entries.len(), 1);
    }

    #[test]
    fn enqueue_persists_and_remove_survives_a_reload() {
        let path = temp_path("readlater-roundtrip");
        let mut store = ReadLaterStore::load_from(path.clone());
        store.enqueue(entry("A"));
        store.enqueue(entry("B"));

        let (removed, persisted) = store.remove(0).unwrap();
        assert_eq!(removed.title, "A");
        assert!(persisted.is_ok());

        let reloaded = ReadLaterStore::load_from(path.clone());
        let titles: Vec<_> = reloaded.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["B"]);
        let _ = std::fs::remove_file(&path);
    }

    // ---- Filter grammar (PRD FR-BM-1) --------------------------------------

    fn tagged(title: &str, tags: &[&str]) -> Bookmark {
        Bookmark {
            title: title.to_string(),
            lang: "en".to_string(),
            revid_at_bookmark: None,
            section_anchor: None,
            created_at: now_ts(),
            updated_at: now_ts(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            note: None,
        }
    }

    #[test]
    fn empty_filter_matches_everything() {
        let filter = parse_filter("");
        assert!(matches_filter(&tagged("Anything", &[]), &filter));
        assert!(matches_filter(&tagged("Something else", &["x"]), &filter));
    }

    #[test]
    fn tag_terms_require_all_named_tags_present_an_and_not_an_or() {
        let filter = parse_filter("#crypto #ww2");
        assert!(matches_filter(
            &tagged("Enigma", &["crypto", "ww2"]),
            &filter
        ));
        assert!(
            !matches_filter(&tagged("Enigma", &["crypto"]), &filter),
            "missing one of the two named tags must not match"
        );
        assert!(!matches_filter(&tagged("Enigma", &[]), &filter));
    }

    #[test]
    fn free_text_fuzzy_matches_the_title() {
        let filter = parse_filter("turng"); // subsequence of "Turing", not contiguous
        assert!(matches_filter(&tagged("Alan Turing", &[]), &filter));
        assert!(!matches_filter(&tagged("Ada Lovelace", &[]), &filter));
    }

    #[test]
    fn mixed_fuzzy_and_tag_terms_apply_both() {
        let filter = parse_filter("enig #crypto");
        assert!(matches_filter(
            &tagged("Enigma machine", &["crypto"]),
            &filter
        ));
        assert!(
            !matches_filter(&tagged("Enigma machine", &["ww2"]), &filter),
            "title matches but the tag doesn't"
        );
        assert!(
            !matches_filter(&tagged("Bombe", &["crypto"]), &filter),
            "tag matches but the title doesn't"
        );
    }

    #[test]
    fn tag_matching_is_case_insensitive() {
        let filter = parse_filter("#CRYPTO");
        assert!(matches_filter(&tagged("Enigma", &["crypto"]), &filter));
    }

    #[test]
    fn fuzzy_matches_is_a_true_subsequence_not_a_substring_match() {
        assert!(fuzzy_matches("Alan Turing", "atn"));
        assert!(
            !fuzzy_matches("Alan Turing", "nat"),
            "wrong order must not match"
        );
        assert!(fuzzy_matches("Alan Turing", ""));
    }

    #[test]
    fn parse_tags_splits_on_commas_and_spaces_and_strips_hashes() {
        assert_eq!(
            parse_tags("crypto, ww2 #history"),
            vec!["crypto", "ww2", "history"]
        );
        assert_eq!(parse_tags(""), Vec::<String>::new());
        assert_eq!(parse_tags("  ,  ,"), Vec::<String>::new());
    }

    // ---- `ba` editor plumbing (pure parts only, PRD FR-BM-2 / SEC-5) ------

    #[test]
    fn parse_editor_command_splits_program_from_arguments() {
        assert_eq!(
            parse_editor_command("vim -u NONE"),
            Some((
                "vim".to_string(),
                vec!["-u".to_string(), "NONE".to_string()]
            ))
        );
        assert_eq!(
            parse_editor_command("nano"),
            Some(("nano".to_string(), vec![]))
        );
        assert_eq!(parse_editor_command(""), None);
        assert_eq!(parse_editor_command("   "), None);
    }

    #[test]
    fn seed_note_content_reproduces_the_existing_note_or_is_empty() {
        assert_eq!(seed_note_content(Some("existing note")), "existing note");
        assert_eq!(seed_note_content(None), "");
    }

    #[test]
    fn note_from_editor_output_trims_trailing_newlines_and_empties_to_none() {
        assert_eq!(
            note_from_editor_output("hello\n"),
            Some("hello".to_string())
        );
        assert_eq!(
            note_from_editor_output("line one\nline two\n"),
            Some("line one\nline two".to_string())
        );
        assert_eq!(note_from_editor_output("\n"), None);
        assert_eq!(note_from_editor_output(""), None);
        assert_eq!(note_from_editor_output("   \n"), Some("   ".to_string()));
    }

    #[test]
    fn display_date_takes_the_first_ten_characters() {
        assert_eq!(display_date("2026-07-13T10:00:00-07:00"), "2026-07-13");
        assert_eq!(display_date("short"), "short");
    }
}
