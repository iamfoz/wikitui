//! Shared platform-directory resolution (PRD §6.4's storage table: cache,
//! data, state, and config each have their own directory and sync posture).
//! Before this module existed, `directories::ProjectDirs::from("", "",
//! "wikitui")` was constructed at ~18 separate call sites across the crate
//! (`session.rs`, `history.rs`, `auth.rs`, `account.rs` x3, `crashguard.rs`,
//! `interest.rs`, `fetch_queue.rs`, `research.rs`, `bookmarks.rs` x2,
//! `saved.rs`, `bookmark_export.rs`, `cache.rs`, `offline_search.rs`,
//! `config.rs`), nine of them re-deriving the identical state-dir-with-
//! `data_dir().join("state")`-fallback block by hand. One of those nine had
//! drifted: `fetch_queue.rs`'s fallback landed in the bare data dir instead
//! of `data_dir/state`, so on a platform with no native state-dir concept
//! (macOS, Windows) the fetch queue would sit in a different directory than
//! every sibling state file — invisible to anything that walks `state/` for
//! a backup or a `clear-data` sweep. Routing every site through the four
//! functions below fixes that divergence by construction (there is only one
//! fallback expression left to drift) and means a future change to the
//! resolution strategy (or a `WIKITUI_*` env override) is a one-place edit.
//!
//! `None` everywhere below means exactly what it meant at each call site
//! before this module existed: no platform directory could be resolved at
//! all (e.g. a sandboxed environment with no resolvable home directory) —
//! callers keep their existing degrade-to-in-memory/no-persistence behavior,
//! never a hard error.

use std::path::{Path, PathBuf};

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "wikitui")
}

/// The state-dir-or-fallback expression itself, pulled out of
/// [`wikitui_state_dir`] so it's testable without depending on the host
/// platform's actual `directories` behavior (`state_dir()` is always `Some`
/// on Linux, so a test running on Linux CI could never otherwise exercise
/// the fallback arm that `fetch_queue.rs` got wrong).
fn state_dir_or_fallback(native_state_dir: Option<&Path>, data_dir: &Path) -> PathBuf {
    native_state_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join("state"))
}

/// `$XDG_STATE_HOME/wikitui` on Linux/BSD (`directories` has a native
/// state-dir concept there); `<data_dir>/state` on macOS/Windows, where it
/// doesn't. Backs `session.rs` (both the auto-restore path and named
/// sessions), `history.rs`, `auth.rs`, `account.rs` (watchlist,
/// reading-list-sync, and watch-mirror state), `crashguard.rs`,
/// `interest.rs`, and `fetch_queue.rs` — all local, not-meant-to-sync-as-
/// user-content state (PRD §6.4).
pub fn wikitui_state_dir() -> Option<PathBuf> {
    let dirs = project_dirs()?;
    Some(state_dir_or_fallback(dirs.state_dir(), dirs.data_dir()))
}

/// `$XDG_DATA_HOME/wikitui` — user content meant to ride the same
/// git/syncthing sync the reader's other data does (PRD §6.4). Backs
/// `research.rs`'s citations, `bookmarks.rs`'s bookmarks and read-later
/// queue, and `saved.rs`'s pinned pages.
pub fn wikitui_data_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_dir().to_path_buf())
}

/// `$XDG_CACHE_HOME/wikitui` — freely re-derivable, evictable content: the
/// page cache (`cache.rs`) and the offline full-text search index
/// (`offline_search.rs`).
pub fn wikitui_cache_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.cache_dir().to_path_buf())
}

/// `$XDG_CONFIG_HOME/wikitui` — `config.toml` (`config.rs`).
pub fn wikitui_config_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_or_fallback_prefers_the_native_state_dir_when_present() {
        let native = Path::new("/home/reader/.local/state/wikitui");
        let data = Path::new("/home/reader/.local/share/wikitui");
        assert_eq!(state_dir_or_fallback(Some(native), data), native);
    }

    /// The exact case `fetch_queue.rs`'s `queue_path` got wrong: with no
    /// native state dir (macOS/Windows), the fallback must be `data_dir`'s
    /// own `state` subdirectory, not `data_dir` itself — otherwise a state
    /// file lands somewhere a `data/state`-walking backup or `clear-data`
    /// sweep would never look.
    #[test]
    fn state_dir_or_fallback_falls_back_to_a_state_subdir_of_data_dir() {
        let data = Path::new("/Users/reader/Library/Application Support/wikitui");
        assert_eq!(
            state_dir_or_fallback(None, data),
            data.join("state"),
            "must never fall back to the bare data dir"
        );
    }

    #[test]
    fn wikitui_state_dir_ends_in_wikitui_when_a_platform_dir_resolves() {
        if let Some(dir) = wikitui_state_dir() {
            assert!(dir.ends_with("wikitui") || dir.ends_with("state"));
        }
        // `None` is a legitimate outcome in a sandboxed environment with no
        // resolvable home directory — never a panic either way.
    }

    #[test]
    fn wikitui_data_dir_and_cache_dir_and_config_dir_all_resolve_together() {
        // `ProjectDirs::from` either resolves every directory or none of
        // them (they all come from the same platform lookup) — so these
        // three either agree on `Some` or all agree on `None`.
        let data = wikitui_data_dir();
        let cache = wikitui_cache_dir();
        let config = wikitui_config_dir();
        assert_eq!(data.is_some(), cache.is_some());
        assert_eq!(data.is_some(), config.is_some());
    }
}
