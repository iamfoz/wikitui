//! Crash-safe session auto-restore (PRD FR-TB-5, v1.0 scope — continuous
//! auto-restore only; named sessions are v1.x). Persists the open tab set
//! to `$XDG_STATE_HOME/wikitui/session.json` (§6.4: sessions live in
//! *state*, not cache — this is "what was on screen", not evictable
//! content) so a `startpage = resume` launch (or `restore_session = true`)
//! can reopen it.
//!
//! ## The "at most the last action" guarantee
//!
//! `App::persist_session` is called synchronously from every "meaningful
//! change" chokepoint — a document installing (fresh open, following a
//! link, back/forward, a bookmark/history-picker reopen, the SWR "r to
//! reload"), a tab opening/closing/switching, a fold toggle, and a resumed
//! reading position — never on every scroll tick or keystroke (see
//! `App::persist_session`'s call sites). Each call is a complete snapshot
//! of every open tab, written via a unique temp file + fsync + rename
//! (mirroring `jsonl.rs`'s durability contract): a crash mid-write can
//! never leave `session.json` half-written, only ever the previous save or
//! the new one. The only thing a crash can lose is whatever changed
//! *between* the last meaningful-change save and the crash — a scroll that
//! hasn't yet been "locked in" by a subsequent switch/navigate/fold. That
//! is the documented tradeoff (§ the alternative, writing on every scroll
//! tick, means a disk write on every `j`/`k` keypress), not a bug.
//!
//! ## Privacy (PRD FR-PR-3)
//!
//! `App::persist_session` never writes while incognito. A restore still
//! reads whatever the *last non-incognito* run left behind — incognito
//! guarantees it adds nothing new, not that it erases what came before
//! (which would be a surprising side effect of a supposedly no-op mode).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tab::HistoryEntry;

/// One persisted tab (PRD FR-TB-5): everything `tab.rs` owns that restoring
/// a reading context needs. Built from a live `Tab` by
/// `App::session_snapshot`; consumed by `main::restore_session_tabs`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTab {
    pub lang: String,
    /// PRD FR-ML-4: the wiki scope (`api::wiki_scope`) this tab's article was
    /// read on, so a restored tab reopens — and caches — under the same wiki
    /// it belonged to, not whatever the active wiki happens to be at restore.
    /// `#[serde(default)]` (empty = default Wikipedia) so a `session.json`
    /// written before wiki scoping existed restores as the default wiki.
    #[serde(default)]
    pub wiki: String,
    /// `None` for a tab with nothing open (a blank `:tab new`, or a
    /// background load that failed and never landed) — restored as an
    /// empty tab, never a fetch of nothing.
    pub title: Option<String>,
    pub scroll: u16,
    /// `doc::Block` indices folded shut (PRD FR-NV-3). A `Vec`, not the
    /// live `HashSet`, both because `HashSet` iteration order isn't stable
    /// (a noisy on-disk diff between two saves of the same fold state) and
    /// because JSON has no native set type; sorted at snapshot time.
    pub folded_blocks: Vec<usize>,
    /// Carried for parity with what a `Tab` owns; not consulted by restore
    /// itself today (the fresh fetch on restore always installs its own
    /// live revid) — informational, and a documented seam for a future
    /// conditional-GET on restore.
    pub current_revid: u64,
    pub back_stack: Vec<HistoryEntry>,
    pub forward_stack: Vec<HistoryEntry>,
}

/// The whole persisted session: every tab, and which one was active.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub active: usize,
    pub tabs: Vec<SessionTab>,
}

/// The real on-disk location (PRD §6.4): `$XDG_STATE_HOME/wikitui/
/// session.json` — mirrors `history::history_path`'s state-dir resolution
/// (state dir on Linux/BSD, `<data_dir>/state` fallback on macOS/Windows,
/// `None` only when no platform directory can be determined at all, e.g. a
/// sandboxed CI environment with no resolvable home directory).
pub fn resolve_session_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("session.json"))
}

/// Writes `state` to `path` via a unique temp file, fsynced, then
/// atomically renamed over the original — mirrors `jsonl::atomic_rewrite`'s
/// durability contract (see that function's doc comment): a crash or power
/// loss mid-write can never leave `path` holding a truncated or mixed file,
/// only ever the previous save or the complete new one. Best-effort like
/// every other store in this codebase: the caller (`App::persist_session`)
/// discards the `Result`, since a failed session save must never interrupt
/// reading.
pub fn save(state: &SessionState, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    // Unique per call (not just per process), same reasoning as
    // `jsonl::atomic_rewrite`: concurrent saves (a second wikitui instance,
    // or two rapid saves racing a slow disk) must never clobber each
    // other's temp file between write and rename.
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session.json");
    let tmp = path.with_file_name(format!(".{base}.{}.{unique}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Best-effort directory sync so the rename itself is durable; opening a
    // directory for sync only works on Unix, and its failure shouldn't fail
    // the (already-visible) rename.
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Loads a previously-saved session. A missing file, an unreadable file, or
/// unparseable/partial JSON (the crash-mid-write case the atomic rename in
/// `save` is meant to make impossible in practice, but a hand-edited or
/// externally-truncated file is still worth surviving) is simply "nothing
/// to restore" (`None`) — never a crash, and never surfaced as an error the
/// reader has to deal with; the caller falls back to the normal start page
/// exactly as if `resume`/`restore_session` had never been set.
pub fn load(path: &Path) -> Option<SessionState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ---- Named sessions (PRD FR-TB-5, v1.x half) --------------------------
//
// The auto-restore session above is one continuous, unnamed snapshot at a
// fixed path. `:mksession <name>` saves a second, independent snapshot
// under a reader-chosen name; `wikitui --session <name>` / `:session
// <name>` load it back. Both live in the same `$XDG_STATE_HOME/wikitui/`
// tree (§6.4: sessions are state, not cache) but in their own `sessions/`
// subdirectory, so `ls`-ing state doesn't mix "what was on screen last
// time" with "the reading lists the reader explicitly named and saved" —
// and so a stray file matching `session.json` itself can never collide
// with a same-named saved session. `save`/`load` above are reused as-is
// for the named-file I/O; only path resolution is new here.

/// Whether `name` is safe to use as a session file's stem: non-empty,
/// reasonably short, and restricted to characters that can never spell a
/// path traversal or a hidden/relative segment (`..`, `/`, a leading `.`) —
/// the file lives at `sessions/<name>.json` with no further sanitization
/// downstream, so this is the one gate standing between a reader-typed
/// `:mksession <name>` and writing outside the sessions directory.
pub fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The `sessions/` directory alongside the auto-restore `session.json` —
/// shared plumbing behind [`resolve_named_session_path`] and
/// [`list_named_sessions`], mirroring [`resolve_session_path`]'s own
/// state-dir resolution (so a sandboxed environment with no resolvable
/// platform directory degrades to `None` for named sessions exactly the
/// way it already does for the auto-restore one).
fn resolve_named_sessions_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("sessions"))
}

/// Where `:mksession <name>` writes and `:session <name>`/`--session <name>`
/// read: `$XDG_STATE_HOME/wikitui/sessions/<name>.json`. `None` for an
/// invalid `name` (see [`is_valid_session_name`]) or when no platform state
/// directory could be resolved at all — both are the caller's cue to report
/// a notice rather than attempt the read/write.
pub fn resolve_named_session_path(name: &str) -> Option<PathBuf> {
    if !is_valid_session_name(name) {
        return None;
    }
    Some(resolve_named_sessions_dir()?.join(format!("{name}.json")))
}

/// Every saved session name found directly under `dir` (its `*.json` files'
/// stems), sorted for a stable `:sessions` listing. A separate,
/// directory-parameterized function from [`list_named_sessions`] so tests
/// can exercise the real listing logic against an isolated temp directory
/// rather than the process's real (and possibly nonexistent, possibly
/// reader-owned) state directory.
pub fn list_named_sessions_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// `:sessions`: every session saved via `:mksession`, by name. Empty (never
/// an error) when the sessions directory doesn't exist yet — a fresh
/// install has saved none — or no platform state directory resolves at all.
pub fn list_named_sessions() -> Vec<String> {
    resolve_named_sessions_dir()
        .map(|dir| list_named_sessions_in(&dir))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-test-session-{}-{n}.json",
            std::process::id()
        ))
    }

    fn sample_state() -> SessionState {
        SessionState {
            active: 1,
            tabs: vec![
                SessionTab {
                    lang: "en".to_string(),
                    wiki: String::new(),
                    title: Some("Alan Turing".to_string()),
                    scroll: 12,
                    folded_blocks: vec![2, 5],
                    current_revid: 1001,
                    back_stack: vec![HistoryEntry {
                        wiki: String::new(),
                        lang: "en".to_string(),
                        title: "Start page".to_string(),
                        scroll: 0,
                    }],
                    forward_stack: Vec::new(),
                },
                SessionTab {
                    lang: "en".to_string(),
                    wiki: "wiktionary".to_string(),
                    title: Some("Enigma machine".to_string()),
                    scroll: 0,
                    folded_blocks: Vec::new(),
                    current_revid: 1002,
                    back_stack: Vec::new(),
                    forward_stack: vec![HistoryEntry {
                        wiki: "wiktionary".to_string(),
                        lang: "en".to_string(),
                        title: "Bletchley Park".to_string(),
                        scroll: 40,
                    }],
                },
            ],
        }
    }

    #[test]
    fn save_then_load_round_trips_every_field() {
        let path = temp_path();
        let state = sample_state();
        save(&state, &path).unwrap();
        let loaded = load(&path).expect("just-saved session must load back");
        assert_eq!(loaded, state);
        let _ = std::fs::remove_file(&path);
    }

    /// The temp-then-rename dance must leave no `.tmp` sibling behind — a
    /// leftover here would mean the rename step never ran (or failed
    /// silently), which is exactly the failure mode atomicity is supposed
    /// to rule out.
    #[test]
    fn save_leaves_no_temp_file_behind() {
        let path = temp_path();
        let base = path.file_name().unwrap().to_string_lossy().into_owned();
        save(&sample_state(), &path).unwrap();
        let parent = path.parent().unwrap();
        let stray_tmp = std::fs::read_dir(parent).unwrap().any(|entry| {
            let Ok(entry) = entry else { return false };
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(&format!(".{base}.")) && name.ends_with(".tmp")
        });
        assert!(!stray_tmp, "the rename must have consumed the temp file");
        let _ = std::fs::remove_file(&path);
    }

    /// PRD FR-TB-5's atomic-write contract: a save fully replaces whatever
    /// was there before — a reader restoring never sees a hybrid of an old
    /// and a new session, and an existing file's presence is not required
    /// for a fresh save to work.
    #[test]
    fn a_second_save_completely_replaces_the_first() {
        let path = temp_path();
        let mut first = sample_state();
        save(&first, &path).unwrap();

        first.active = 0;
        first.tabs.truncate(1);
        first.tabs[0].scroll = 99;
        save(&first, &path).unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.active, 0);
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].scroll, 99);
        let _ = std::fs::remove_file(&path);
    }

    /// Simulates the one scenario the atomic temp+rename dance is meant to
    /// make impossible in practice: a file at the target path that is only
    /// partially written (as a crash mid-write, without the rename's
    /// atomicity, could otherwise leave behind). `load` must degrade to
    /// "nothing to restore", never panic.
    #[test]
    fn a_truncated_or_corrupt_file_loads_as_none_not_a_panic() {
        let path = temp_path();
        std::fs::write(&path, b"{\"active\":0,\"tabs\":[{\"lang\":\"en\"").unwrap();
        assert!(load(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_of_a_missing_file_is_none_not_an_error() {
        let path = temp_path();
        assert!(load(&path).is_none());
    }

    #[test]
    fn resolve_session_path_ends_in_session_json_when_a_platform_dir_resolves() {
        if let Some(path) = resolve_session_path() {
            assert!(path.ends_with("session.json"));
        }
        // `None` is a legitimate outcome in a sandboxed environment with no
        // resolvable home directory — never a panic either way.
    }

    #[test]
    fn empty_session_round_trips() {
        let path = temp_path();
        let empty = SessionState::default();
        save(&empty, &path).unwrap();
        assert_eq!(load(&path), Some(empty));
        let _ = std::fs::remove_file(&path);
    }

    // ---- Named sessions (PRD FR-TB-5) --------------------------------

    fn temp_dir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("wikitui-test-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn is_valid_session_name_accepts_alnum_dash_underscore_only() {
        assert!(is_valid_session_name("research"));
        assert!(is_valid_session_name("ww2-research"));
        assert!(is_valid_session_name("ww2_research_2"));
        assert!(!is_valid_session_name(""));
        assert!(!is_valid_session_name(".."));
        assert!(!is_valid_session_name("../escape"));
        assert!(!is_valid_session_name("has space"));
        assert!(!is_valid_session_name("has/slash"));
        assert!(
            !is_valid_session_name(&"x".repeat(101)),
            "over the length cap"
        );
        assert!(
            is_valid_session_name(&"x".repeat(100)),
            "exactly at the cap"
        );
    }

    /// PRD FR-TB-5: a named session round-trips tabs, wiki scope, and scroll
    /// exactly like the auto-restore session (same `save`/`load`, same
    /// atomic-write contract already exercised above) — proving
    /// `:mksession <name>` / `--session <name>` have a real, working file
    /// format to read and write at their own `sessions/<name>.json` shape;
    /// only the *path* differs from the auto-restore session, not the I/O.
    #[test]
    fn named_session_save_then_load_round_trips_tabs_wiki_and_scroll() {
        let dir = temp_dir("named-session-roundtrip");
        let path = dir.join("research.json");
        let state = sample_state();
        save(&state, &path).unwrap();
        let loaded = load(&path).expect("just-saved named session must load back");
        assert_eq!(loaded, state);
        assert_eq!(
            loaded.tabs[1].wiki, "wiktionary",
            "wiki scope survives (C6b)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_named_session_path_rejects_an_unsafe_name() {
        assert_eq!(resolve_named_session_path(""), None);
        assert_eq!(resolve_named_session_path(".."), None);
        assert_eq!(resolve_named_session_path("../escape"), None);
        assert_eq!(resolve_named_session_path("has space"), None);
    }

    #[test]
    fn resolve_named_session_path_lands_under_a_sessions_subdirectory() {
        if let Some(path) = resolve_named_session_path("research") {
            assert!(path.ends_with("sessions/research.json"));
        }
        // `None` is legitimate with no resolvable platform directory.
    }

    #[test]
    fn list_named_sessions_in_lists_json_stems_sorted_and_ignores_other_files() {
        let dir = temp_dir("list-sessions");
        save(&sample_state(), &dir.join("ww2.json")).unwrap();
        save(&sample_state(), &dir.join("morning.json")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"not a session").unwrap();

        let names = list_named_sessions_in(&dir);
        assert_eq!(names, vec!["morning".to_string(), "ww2".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_named_sessions_in_on_a_missing_directory_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join("wikitui-test-sessions-does-not-exist");
        assert_eq!(list_named_sessions_in(&dir), Vec::<String>::new());
    }
}
