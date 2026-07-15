//! Shared line-oriented JSONL persistence (PRD §6.4: "plain-file storage...
//! designed for git/syncthing sync"). `research.rs` established the pattern
//! this extracts: append-only adds, a delete/update that re-reads the file
//! fresh (never a caller's possibly-stale in-memory snapshot) so lines from
//! another running instance or a newer/older file format survive untouched,
//! and a per-call-unique temp file + fsync + rename so a crash or power loss
//! mid-write can never corrupt the file. `bookmarks.rs` needs the same
//! contract for two more stores (bookmarks, read-later) plus one operation
//! `research.rs` never needed — replacing a line in place (tag/note edits),
//! not just deleting it — so the shared logic lives here instead of a third
//! copy-paste.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::Path;

/// Parses every line of `path` as `T`, silently dropping lines that don't
/// parse (a corrupt byte, a future format this build doesn't understand —
/// PRD FR-BM-7's "documented merge behavior" is "ignore what you can't
/// read, never crash on it") and blank lines. A missing file (or any other
/// read failure) is simply an empty store — a fresh install with nothing
/// saved yet is the common case, not an error.
pub fn load<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    std::fs::read_to_string(path)
        .ok()
        .map(|content| {
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Appends one record as its own line, creating the file (and parent
/// directory) if needed. The whole point of append-only adds: this never
/// reads the file first, so it can't race with a concurrent reader/writer
/// on the read half — only `rewrite_matching`'s delete/replace path needs
/// the fresh-read-then-atomic-rename dance.
pub fn append<T: Serialize>(path: &Path, record: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")
}

/// Rewrites the first line in `path` that parses to a `T` for which
/// `predicate` returns true: replaced with `replacement` if given, or
/// deleted outright if `None`. Every other line is preserved byte-for-byte,
/// including ones this build can't parse at all — the same data-loss
/// concern `research.rs`'s original `remove_line_from_file` was written to
/// close (a corrupt line, or a field from a newer wikitui, surviving an
/// unrelated edit). Returns whether a matching line was actually found.
///
/// Always works from a fresh `read_to_string`, never a caller's in-memory
/// snapshot: a second running instance's append since this store loaded, or
/// its own concurrent edit, is only visible by re-reading, and only by
/// re-reading does it survive this rewrite instead of being silently
/// dropped with it.
///
/// The write itself goes through a unique (per-call) temp file in the same
/// directory, fsynced before an atomic rename, so neither a crash nor —
/// on typical filesystems — a power loss mid-write can corrupt the store.
/// Two instances rewriting at the same moment can still race on the final
/// rename; the loser's edit may not stick, but no *other* line is ever
/// lost, matching `research.rs`'s original documented tradeoff.
pub fn rewrite_matching<T, F>(
    path: &Path,
    predicate: F,
    replacement: Option<&T>,
) -> std::io::Result<bool>
where
    T: Serialize + DeserializeOwned,
    F: Fn(&T) -> bool,
{
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        // Nothing on disk yet (whatever this store added never persisted,
        // or the file was removed externally) — nothing to rewrite.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    let mut kept: Vec<String> = Vec::new();
    let mut found = false;
    for line in content.lines() {
        if !found
            && let Ok(parsed) = serde_json::from_str::<T>(line)
            && predicate(&parsed)
        {
            found = true;
            if let Some(replacement) = replacement {
                kept.push(serde_json::to_string(replacement).map_err(std::io::Error::other)?);
            }
            continue; // deletion: the line is dropped, not kept.
        }
        kept.push(line.to_string());
    }
    if !found {
        return Ok(false);
    }

    atomic_rewrite(path, &kept)?;
    Ok(true)
}

/// Writes `lines` (already newline-free) to `path` via a unique temp file,
/// fsynced, then atomically renamed over the original — the durability half
/// of `rewrite_matching`, split out because it has no need for the generic
/// parse/predicate machinery above.
fn atomic_rewrite(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    // The temp name must be unique per *call*, not just per process: two
    // concurrent rewrites (different threads, or different stores sharing a
    // directory) would otherwise clobber each other's temp file between
    // write and rename.
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("wikitui-store.jsonl");
    let tmp = path.with_file_name(format!(".{base}.{}.{unique}.tmp", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(out.as_bytes())?;
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

/// Rewrites the *entire* file to `records`, in the given order — unlike
/// `rewrite_matching`'s single-line replace/delete, every record is
/// re-serialized from scratch, so a line this build couldn't parse does NOT
/// survive a whole-file reorder the way it survives `rewrite_matching` (there
/// is nothing to preserve byte-for-byte once every record has already been
/// read into memory and it wasn't one of them). Used by `bookmarks::
/// BookmarkStore::reorder` (PRD FR-BM-5's "server wins on order" conflict
/// policy) — the one caller whose contract is "line order is itself
/// meaningful data," not just "one line's content changed."
pub fn rewrite_all<T: Serialize>(path: &Path, records: &[T]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lines: Vec<String> = records
        .iter()
        .map(|r| serde_json::to_string(r).map_err(std::io::Error::other))
        .collect::<std::io::Result<Vec<_>>>()?;
    atomic_rewrite(path, &lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-test-jsonl-{tag}-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Rec {
        id: u32,
        text: String,
    }

    #[test]
    fn append_then_load_round_trips() {
        let path = temp_path("roundtrip");
        append(
            &path,
            &Rec {
                id: 1,
                text: "a".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 2,
                text: "b".into(),
            },
        )
        .unwrap();

        let loaded: Vec<Rec> = load(&path);
        assert_eq!(
            loaded,
            vec![
                Rec {
                    id: 1,
                    text: "a".into()
                },
                Rec {
                    id: 2,
                    text: "b".into()
                },
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_of_a_missing_file_is_empty_not_an_error() {
        let path = temp_path("missing");
        let loaded: Vec<Rec> = load(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn rewrite_matching_deletes_exactly_one_matching_line() {
        let path = temp_path("delete");
        append(
            &path,
            &Rec {
                id: 1,
                text: "keep".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 2,
                text: "delete-me".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 3,
                text: "keep too".into(),
            },
        )
        .unwrap();

        let found = rewrite_matching::<Rec, _>(&path, |r| r.id == 2, None).unwrap();
        assert!(found);

        let loaded: Vec<Rec> = load(&path);
        assert_eq!(loaded.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 3]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rewrite_matching_replaces_in_place_preserving_order() {
        let path = temp_path("replace");
        append(
            &path,
            &Rec {
                id: 1,
                text: "a".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 2,
                text: "b".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 3,
                text: "c".into(),
            },
        )
        .unwrap();

        let replacement = Rec {
            id: 2,
            text: "b-edited".into(),
        };
        let found = rewrite_matching(&path, |r: &Rec| r.id == 2, Some(&replacement)).unwrap();
        assert!(found);

        let loaded: Vec<Rec> = load(&path);
        assert_eq!(
            loaded,
            vec![
                Rec {
                    id: 1,
                    text: "a".into()
                },
                Rec {
                    id: 2,
                    text: "b-edited".into()
                },
                Rec {
                    id: 3,
                    text: "c".into()
                },
            ],
            "the edited record must keep its original position"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rewrite_matching_preserves_lines_it_cannot_parse() {
        let path = temp_path("corrupt");
        append(
            &path,
            &Rec {
                id: 1,
                text: "delete-me".into(),
            },
        )
        .unwrap();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"truncated\":\n");
        raw.push_str("{\"id\":99,\"text\":\"future\",\"new_field\":true}\n");
        std::fs::write(&path, raw).unwrap();

        let found = rewrite_matching::<Rec, _>(&path, |r| r.id == 1, None).unwrap();
        assert!(found);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("delete-me"));
        assert!(
            after.contains("{\"truncated\":"),
            "an unparseable line must survive byte-for-byte"
        );
        assert!(
            after.contains("\"new_field\":true"),
            "a line with fields this build doesn't know must survive intact"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rewrite_matching_on_a_missing_file_reports_not_found() {
        let path = temp_path("nofile");
        let found = rewrite_matching::<Rec, _>(&path, |_| true, None).unwrap();
        assert!(!found);
    }

    #[test]
    fn rewrite_all_replaces_the_whole_file_in_the_given_order() {
        let path = temp_path("rewrite-all");
        append(
            &path,
            &Rec {
                id: 1,
                text: "a".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 2,
                text: "b".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 3,
                text: "c".into(),
            },
        )
        .unwrap();

        // Reordered AND missing id 2 — a whole-file rewrite, not an edit.
        let reordered = vec![
            Rec {
                id: 3,
                text: "c".into(),
            },
            Rec {
                id: 1,
                text: "a".into(),
            },
        ];
        rewrite_all(&path, &reordered).unwrap();

        let loaded: Vec<Rec> = load(&path);
        assert_eq!(loaded, reordered);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rewrite_all_on_a_fresh_path_creates_the_file_and_parent_dir() {
        let dir = std::env::temp_dir().join(format!(
            "wikitui-test-jsonl-rewrite-all-fresh-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("nested").join("store.jsonl");
        rewrite_all(
            &path,
            &[Rec {
                id: 1,
                text: "x".into(),
            }],
        )
        .unwrap();
        let loaded: Vec<Rec> = load(&path);
        assert_eq!(
            loaded,
            vec![Rec {
                id: 1,
                text: "x".into()
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_matching_only_touches_the_first_match() {
        let path = temp_path("firstonly");
        append(
            &path,
            &Rec {
                id: 1,
                text: "dup".into(),
            },
        )
        .unwrap();
        append(
            &path,
            &Rec {
                id: 1,
                text: "dup".into(),
            },
        )
        .unwrap();

        rewrite_matching::<Rec, _>(&path, |r| r.id == 1, None).unwrap();
        let loaded: Vec<Rec> = load(&path);
        assert_eq!(loaded.len(), 1, "only the first matching line is removed");
        let _ = std::fs::remove_file(&path);
    }
}
