//! Crash-safe atomic file writes (PRD §6.4's "plain-file storage designed for
//! git/syncthing sync" durability contract), extracted here so every store in
//! the codebase shares one implementation instead of a per-module copy-paste.
//!
//! The single guarantee: a call to [`write_atomic`] leaves `path` holding
//! either exactly its previous contents or exactly the new `bytes` — never a
//! half-written, truncated, or byte-mixed hybrid — even across a crash or
//! (on typical filesystems) a power loss mid-write. It does this by writing to
//! a per-call-unique temp file in the *same directory* (so the final `rename`
//! is a same-filesystem atomic swap, not a cross-device copy), `fsync`ing that
//! temp before the rename, and best-effort `fsync`ing the parent directory
//! after so the rename entry itself is durable.
//!
//! This is `jsonl.rs`'s original private `atomic_rewrite` durability half,
//! promoted to a `pub(crate)` byte-oriented helper: `jsonl` (bookmarks,
//! read-later, saved-page index, research), `session`, `cache` (the title
//! index), `saved` (the pinned HTML blob), `auth` (the 0600 token file),
//! `interest`, and `account` (watchlist / reading-list-sync / watch-mirror
//! state) all route their whole-file writes through it. Before this, each
//! either duplicated the dance or used a bare, non-atomic `std::fs::write`
//! whose torn write could resurrect deleted reading-list entries, log a reader
//! out mid-refresh, or strand a `verify=Corrupt` saved page.
//!
//! Concurrency note: two callers renaming over the *same* `path` at the same
//! instant still race on which rename lands last (last-writer-wins); the
//! loser's write may not stick, but no *third* party's bytes are ever lost and
//! `path` is never observed half-written. The unique temp name (process id +
//! a per-call atomic counter) is what keeps those two writers from clobbering
//! each other's temp file between write and rename.

use std::io::Write;
use std::path::{Path, PathBuf};

/// A per-call-unique temp sibling of `path`, in the same directory so the
/// eventual `rename` stays on one filesystem (a cross-device rename would fail
/// with `EXDEV`, defeating the atomicity). Unique per *call*, not just per
/// process: two concurrent writers (different threads, or two stores sharing a
/// directory) must never pick the same temp name and clobber each other
/// between write and rename.
fn temp_sibling(path: &Path) -> PathBuf {
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("wikitui-atomic");
    path.with_file_name(format!(".{base}.{}.{unique}.tmp", std::process::id()))
}

/// Atomically replace `path` with `bytes` (creating it, and any missing parent
/// directories, if it doesn't exist yet). See the module doc comment for the
/// durability contract. A failure at any step removes the temp file rather
/// than leaving a stray `.tmp` sibling behind, and never touches `path`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_impl(path, bytes, None)
}

/// Like [`write_atomic`], but the temp file is set to `mode` (e.g. `0o600`)
/// *before* any content is written to it — so a secret (auth tokens) is never
/// present on disk at a laxer mode for even an instant, closing the window a
/// plain "write then `chmod`" leaves open (CORR-L6). Because the real `path`
/// only ever receives content via the atomic rename of the already-`mode`
/// temp, a pre-existing world-readable file at `path` is replaced outright,
/// its old inode (and old mode) never carrying the new bytes.
#[cfg(unix)]
pub(crate) fn write_atomic_mode(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    write_atomic_impl(path, bytes, Some(mode))
}

fn write_atomic_impl(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(path);
    if let Err(e) = write_temp(&tmp, bytes, mode) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
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

/// Create the temp file, set its mode (unix, when requested) *before* writing
/// any bytes, write the content, and `fsync` it — the durable-content half,
/// split out so the caller above owns the rename/cleanup half.
fn write_temp(tmp: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = std::fs::File::create(tmp)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        // The temp has no content yet, so this narrows the file's mode before a
        // single secret byte lands in it.
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-test-atomicio-{tag}-{}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn write_atomic_creates_and_round_trips_content() {
        let path = temp_path("create");
        write_atomic(&path, b"hello world").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_atomic_replaces_a_pre_existing_file_wholesale() {
        let path = temp_path("replace");
        std::fs::write(&path, b"the old contents, which are longer").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"new",
            "the new write fully replaces the old file, no trailing bytes survive"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_atomic_creates_missing_parent_directories() {
        let dir = temp_path("nested-dir");
        let path = dir.join("a").join("b").join("state.json");
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temp+rename dance must consume its own temp file — a leftover
    /// `.tmp` sibling would mean the rename never ran, exactly the failure
    /// atomicity is meant to rule out.
    #[test]
    fn write_atomic_leaves_no_temp_sibling_behind() {
        let dir = temp_path("no-temp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        write_atomic(&path, b"content").unwrap();
        let stray = std::fs::read_dir(&dir).unwrap().any(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            name.ends_with(".tmp")
        });
        assert!(!stray, "the rename must have consumed the temp file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CORR-L6: writing over a pre-existing world-readable (0644) file must
    /// leave the result at 0600 — and, structurally, the content only ever
    /// reaches `path` via the rename of a temp created at 0600, so the secret
    /// bytes are never present at the real path under the laxer mode.
    #[cfg(unix)]
    #[test]
    fn write_atomic_mode_replaces_a_0644_file_with_a_0600_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_path("private");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        // A pre-existing, world-readable file — the exact starting state the
        // old "write then chmod" left a secret momentarily exposed in.
        std::fs::write(&path, b"stale, world-readable").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_atomic_mode(&path, b"secret tokens", 0o600).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"secret tokens");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the rewritten private file must be 0600");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
