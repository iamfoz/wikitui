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
///
/// The line *and* its trailing newline are built into one buffer and emitted
/// in a single `write_all` (CORR-M8): `writeln!` would emit the body and the
/// `\n` as two separate `write` calls, so two `O_APPEND` writers racing on the
/// same file could interleave into `line_Aline_B\n\n` — one corrupt line plus
/// a blank, both then silently dropped by `load`'s `filter_map(ok)`, losing
/// *both* records. A single small `write_all` to an `O_APPEND` file is atomic
/// with respect to other appenders, so each record lands as one intact line.
pub fn append<T: Serialize>(path: &Path, record: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    file.write_all(line.as_bytes())
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

/// Writes `lines` (already newline-free) to `path` atomically — the
/// durability half of `rewrite_matching`, now a thin adapter over the shared
/// [`crate::atomicio::write_atomic`] helper (which owns the unique-temp +
/// fsync + rename + directory-fsync dance every store in the codebase relies
/// on). Split out from the generic parse/predicate machinery above because it
/// only needs the joined bytes.
fn atomic_rewrite(path: &Path, lines: &[String]) -> std::io::Result<()> {
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    crate::atomicio::write_atomic(path, out.as_bytes())
}

/// Reorders — in place, preserving every non-participating line
/// byte-for-byte — only the lines of `path` that parse to a `T` for which
/// `is_match` returns true, according to `reorder`. Unlike a whole-file
/// rewrite from the caller's in-memory snapshot (which re-serializes every
/// record from scratch and so silently drops any line that snapshot never held
/// — an unparseable line, another wiki/lang's record, or a concurrent append
/// by a second running instance), this reads the file *fresh* like
/// [`rewrite_matching`] and only touches the
/// matching records' *slots*: it collects the matching records in file order,
/// hands them to `reorder` (which must return the same records permuted, never
/// adding or dropping any), and writes them back into exactly the positions
/// they occupied, leaving every other line — matching or not, parseable or
/// not — untouched. This is what lets `BookmarkStore::reorder` re-rank one
/// `(wiki, lang)`'s bookmarks without erasing an unparseable line, another
/// language's bookmark, or one a second instance appended since load (CORR-M2).
///
/// Returns whether any matching record was found (and hence a rewrite
/// attempted). A `reorder` that returns a different number of records than it
/// was handed is a caller bug that would misalign the slots, so it is rejected
/// without writing (the file is left exactly as-is) rather than stranding an
/// empty slot and losing a line.
pub fn reorder_matching<T, M, R>(path: &Path, is_match: M, reorder: R) -> std::io::Result<bool>
where
    T: Serialize + DeserializeOwned,
    M: Fn(&T) -> bool,
    R: FnOnce(Vec<T>) -> Vec<T>,
{
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    // `slots` are the indices in `out` a matching record occupies (a `None`
    // placeholder to be filled by the reordered set); every other line is kept
    // verbatim as `Some(raw)` at its original position.
    let mut out: Vec<Option<String>> = Vec::new();
    let mut slots: Vec<usize> = Vec::new();
    let mut matching: Vec<T> = Vec::new();
    for line in content.lines() {
        if let Ok(parsed) = serde_json::from_str::<T>(line)
            && is_match(&parsed)
        {
            slots.push(out.len());
            matching.push(parsed);
            out.push(None);
        } else {
            out.push(Some(line.to_string()));
        }
    }
    if matching.is_empty() {
        return Ok(false);
    }

    let reordered = reorder(matching);
    if reordered.len() != slots.len() {
        return Ok(false);
    }
    for (&slot, record) in slots.iter().zip(reordered.iter()) {
        out[slot] = Some(serde_json::to_string(record).map_err(std::io::Error::other)?);
    }

    let lines: Vec<String> = out.into_iter().flatten().collect();
    atomic_rewrite(path, &lines)?;
    Ok(true)
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

    /// CORR-M8: each `append` emits its record and newline in one `write_all`,
    /// so two `O_APPEND` writers can't interleave a half-line. With the old
    /// `writeln!` (body and `\n` as separate writes) a race produced a
    /// concatenated, unparseable line plus a blank — `load` would drop both,
    /// and the count would fall short of what was appended.
    #[test]
    fn concurrent_appends_never_interleave_into_lost_lines() {
        let path = temp_path("concurrent-append");
        let threads: u32 = 8;
        let per_thread: u32 = 200;
        let mut handles = Vec::new();
        for t in 0..threads {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..per_thread {
                    append(
                        &path,
                        &Rec {
                            id: t * per_thread + i,
                            text: "x".repeat(40),
                        },
                    )
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let loaded: Vec<Rec> = load(&path);
        assert_eq!(
            loaded.len(),
            (threads * per_thread) as usize,
            "every concurrently-appended record must survive as one intact line"
        );
        let mut ids: Vec<u32> = loaded.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            (0..threads * per_thread).collect::<Vec<_>>(),
            "no record was interleaved into an unparseable line"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// CORR-M2's primitive: `reorder_matching` reorders only the matching
    /// records' slots and preserves every other line byte-for-byte — a
    /// parseable-but-non-matching record, an unparseable line, and a record a
    /// second instance appended after this store loaded all survive, where
    /// `rewrite_all` from a stale snapshot would have erased them.
    #[test]
    fn reorder_matching_reorders_only_matching_slots_preserving_everything_else() {
        let path = temp_path("reorder-matching");
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
                id: 99,
                text: "other".into(),
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
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"truncated\":\n");
        std::fs::write(&path, raw).unwrap();
        // A concurrent append after the (hypothetical) in-memory snapshot —
        // this is the record a `rewrite_all` from stale memory would drop.
        append(
            &path,
            &Rec {
                id: 3,
                text: "c".into(),
            },
        )
        .unwrap();

        let found = reorder_matching::<Rec, _, _>(
            &path,
            |r| r.id != 99,
            |mut recs: Vec<Rec>| {
                recs.reverse();
                recs
            },
        )
        .unwrap();
        assert!(found);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("{\"truncated\":"),
            "the unparseable line must survive byte-for-byte"
        );
        let loaded: Vec<Rec> = load(&path);
        assert_eq!(
            loaded.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![3, 99, 2, 1],
            "matching records reverse within their slots; id 99 keeps its slot"
        );
        let _ = std::fs::remove_file(&path);
    }
}
