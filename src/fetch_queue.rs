//! The offline fetch queue (PRD FR-OFF-6 / §7's "Offline, uncached link"
//! row): when the network is down and the reader follows a link that is
//! neither cached nor saved, the target can be queued here to fetch "when
//! online". Persisted as a JSONL store (the same `jsonl` family as bookmarks/
//! read-later) under `$XDG_STATE_HOME/wikitui/` — this is transient local
//! state (§6.4's state dir: "sessions… local; optionally synced"), not saved
//! content, so it lives with history/sessions rather than the data dir.
//!
//! **Draining**: `:fetch-queue` drains the whole queue on demand — every
//! entry is fetched into the cache and dropped on success. *Automatic* drain
//! the moment connectivity returns is a documented seam: it needs a
//! connectivity signal (NetworkManager D-Bus, or a cheap probe) wikitui does
//! not yet have, and this environment can't exercise a real reconnect anyway.
//! Until that lands, the manual trigger is the drain path.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One queued fetch target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedFetch {
    pub lang: String,
    pub title: String,
    pub queued_at: String,
}

pub struct FetchQueue {
    pub entries: Vec<QueuedFetch>,
    path: Option<PathBuf>,
}

impl FetchQueue {
    pub fn load() -> Self {
        match queue_path() {
            Some(path) => Self::at(path),
            None => Self::in_memory(),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            path: None,
        }
    }

    pub fn at(path: PathBuf) -> Self {
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

    /// Enqueue `(lang, title)` unless already queued. Returns whether it was
    /// actually added.
    pub fn enqueue(&mut self, lang: &str, title: &str) -> bool {
        if self.contains(lang, title) {
            return false;
        }
        let entry = QueuedFetch {
            lang: lang.to_string(),
            title: title.to_string(),
            queued_at: crate::bookmarks::now_ts(),
        };
        if let Some(path) = &self.path {
            let _ = crate::jsonl::append(path, &entry);
        }
        self.entries.push(entry);
        true
    }

    /// Remove `(lang, title)` from the queue (a successful drain, or a manual
    /// dismissal). Returns whether it was present.
    pub fn remove(&mut self, lang: &str, title: &str) -> bool {
        let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.lang == lang && e.title == title)
        else {
            return false;
        };
        self.entries.remove(pos);
        if let Some(path) = &self.path {
            let _ = crate::jsonl::rewrite_matching::<QueuedFetch, _>(
                path,
                |e| e.lang == lang && e.title == title,
                None,
            );
        }
        true
    }

    /// A snapshot of the queued targets, for the drain trigger to iterate
    /// without holding a borrow on the queue while it fetches + removes.
    pub fn snapshot(&self) -> Vec<QueuedFetch> {
        self.entries.clone()
    }

    /// Test-only: see `bookmarks::BookmarkStore::is_in_memory` — lets
    /// `app.rs`'s H3 regression test confirm `App::new` never resolves the
    /// real platform state directory.
    #[cfg(test)]
    pub(crate) fn is_in_memory(&self) -> bool {
        self.path.is_none()
    }
}

/// `$XDG_STATE_HOME/wikitui/fetch_queue.jsonl` where a state dir exists
/// (Linux), else `<data_dir>/state` as a fallback (macOS/Windows have no
/// distinct state dir in the `directories` crate) — see `paths::
/// wikitui_state_dir`, shared with every other state file (session, history,
/// auth, interest, watchlist/reading-list-sync/watch-mirror, crash reports)
/// so this queue can never again land somewhere a `data/state`-walking
/// backup or `clear-data` sweep would miss it.
fn queue_path() -> Option<PathBuf> {
    Some(crate::paths::wikitui_state_dir()?.join("fetch_queue.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-fetchq-test-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn enqueue_dedups_and_persists() {
        let path = temp_path();
        let mut q = FetchQueue::at(path.clone());
        assert!(q.enqueue("en", "Alan Turing"));
        assert!(!q.enqueue("en", "Alan Turing"), "duplicate rejected");
        assert!(q.enqueue("en", "Enigma machine"));
        assert_eq!(q.entries.len(), 2);

        let reloaded = FetchQueue::at(path.clone());
        assert_eq!(reloaded.entries.len(), 2);
        assert!(reloaded.contains("en", "Alan Turing"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_drops_the_entry_and_survives_reload() {
        let path = temp_path();
        let mut q = FetchQueue::at(path.clone());
        q.enqueue("en", "A");
        q.enqueue("en", "B");
        assert!(q.remove("en", "A"));
        assert!(!q.remove("en", "A"), "already gone");

        let reloaded = FetchQueue::at(path.clone());
        let titles: Vec<_> = reloaded.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["B"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn snapshot_lists_queued_targets() {
        let mut q = FetchQueue::in_memory();
        q.enqueue("en", "A");
        q.enqueue("de", "B");
        let snap = q.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].title, "A");
        assert_eq!(snap[1].lang, "de");
    }

    /// quality-M5's divergence bug: `queue_path` used to fall back to the
    /// bare data dir instead of `<data_dir>/state` when `directories` has no
    /// native state-dir concept (macOS/Windows) — a different directory than
    /// every sibling state file (`session.json`, `history.sqlite`,
    /// `auth.json`, `interest.json`, `watchlist.json`, …), invisible to a
    /// `data/state`-walking backup or `clear-data` sweep. Now that
    /// `queue_path` routes through the same `paths::wikitui_state_dir` every
    /// sibling does, its parent directory must be exactly that shared
    /// directory — not merely "a" directory under the data dir.
    #[test]
    fn queue_path_parent_is_the_shared_state_dir_not_the_bare_data_dir() {
        if let (Some(q), Some(state)) = (queue_path(), crate::paths::wikitui_state_dir()) {
            assert_eq!(
                q.parent(),
                Some(state.as_path()),
                "fetch_queue.jsonl must land directly under the shared state dir"
            );
        }
        // `None` is a legitimate outcome in a sandboxed environment with no
        // resolvable home directory — never a panic either way.
    }
}
