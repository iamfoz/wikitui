//! `wikitui clear-data` (PRD FR-PR-4): a standalone subcommand, mirroring
//! `doctor.rs`'s posture — plain stdout, runs before any terminal, network,
//! or in-process store is initialized (`main` dispatches it before
//! `TerminalGuard::enter`, exactly like `config doctor`). Deleting a
//! reader's local data must never depend on the rest of the app starting up
//! successfully first.
//!
//! ## `--all`'s scope (a deliberate boundary, not an oversight)
//!
//! PRD FR-PR-4 names `--all` alongside `--history`/`--cache`/`--stats`/
//! `--auth` — the same list FR-PR-3's incognito gate suppresses (history,
//! stats/interest, and — read broadly — the cached record of what was
//! fetched) plus `--auth` (tokens, once login exists). Those are all
//! *tracking* surfaces: records of what the reader did, not things they
//! deliberately built. Bookmarks, the read-later queue, saved offline
//! pages, and the research bibliography are the opposite — **user-created
//! libraries** — so `--all` does **not** touch `bookmarks.jsonl`,
//! `readlater.jsonl`, the saved-pages store, or `research.jsonl`. A reader
//! who also wants those gone has to say so by name; no such flag exists yet
//! because no requirement calls for one, and adding it later is an
//! *addition* to this module, not a reclassification of what `--all`
//! already means.
//!
//! ## `--stats`/`--auth`
//!
//! `--stats` (FR-PC-3) deletes the **interest model** (`interest.json`, PRD
//! §6.4) — the persisted local personalization state whose top categories are
//! the reading-stats topic distribution. The reading stats *themselves* are
//! derived on demand from the history database (there is no separate stats
//! file), so `--history` already clears the numbers; `--stats` clears the
//! learned-topic model those numbers surface. `--auth` (FR-ACC-1/9) **is** also
//! wired: it deletes the
//! locally stored OAuth token file (`auth.json`, PRD SEC-4), the same tokens
//! `:logout` removes — and, like `:logout`, tells the reader to revoke
//! server-side at `Special:OAuthManageMyGrants` (this public client holds no
//! secret to revoke with itself). When the `keychain` feature is built, the
//! keychain entry is best-effort removed too.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::config::ResolvedConfig;

/// Which stores `clear-data` was asked to empty. Each accessor below folds
/// in `all` so call sites never have to remember to check both — see the
/// module doc comment for exactly what `all` does and doesn't cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scope {
    pub history: bool,
    pub cache: bool,
    pub stats: bool,
    pub auth: bool,
    pub all: bool,
}

impl Scope {
    fn wants_history(self) -> bool {
        self.history || self.all
    }
    fn wants_cache(self) -> bool {
        self.cache || self.all
    }
    fn wants_stats(self) -> bool {
        self.stats || self.all
    }
    fn wants_auth(self) -> bool {
        self.auth || self.all
    }

    /// Whether this scope names anything at all — an empty invocation
    /// (`clear-data` with no flags) is a usage error, not "delete nothing
    /// silently and exit 0".
    fn is_empty(self) -> bool {
        !(self.wants_history() || self.wants_cache() || self.wants_stats() || self.wants_auth())
    }
}

/// The concrete on-disk paths this run's `Scope` resolves to — split out
/// from `run` so tests can construct one directly, against tempdir paths,
/// without touching a real XDG directory (mirrors how `history.rs`/
/// `cache.rs`'s own tests bypass their `*_path`/`open` functions in favor of
/// `open_at`/`at`).
#[derive(Debug, Clone, Default)]
pub struct Targets {
    pub history: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    /// PRD FR-ACC-9 / SEC-4: the locally stored OAuth token file (`auth.json`).
    pub auth: Option<PathBuf>,
    /// PRD FR-PC-3 / FR-PF-3: the local interest model (`interest.json`),
    /// deleted by `--stats` (see the module doc comment's `--stats` note).
    pub interest: Option<PathBuf>,
}

/// Resolves `scope` against the real platform directories (`cache_dir_override`
/// is PRD FR-PR-5's `[cache] dir`, so `clear-data --cache` deletes exactly
/// what a real run would have written to).
pub fn resolve_targets(scope: Scope, cache_dir_override: Option<&Path>) -> Targets {
    Targets {
        history: scope
            .wants_history()
            .then(crate::history::history_path)
            .flatten(),
        cache_dir: scope
            .wants_cache()
            .then(|| crate::cache::resolve_pages_dir(cache_dir_override))
            .flatten(),
        auth: scope.wants_auth().then(crate::auth::auth_path).flatten(),
        interest: scope
            .wants_stats()
            .then(crate::interest::interest_path)
            .flatten(),
    }
}

/// How much of one target was actually freed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Freed {
    pub existed: bool,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub history: Option<Freed>,
    pub cache: Option<Freed>,
    pub auth: Option<Freed>,
    pub interest: Option<Freed>,
}

/// Recursively sums file sizes under `path` (0 for a missing path or a
/// plain file that isn't there — callers pass either a known file or a
/// known directory, never an arbitrary unknown kind).
fn dir_size(path: &Path) -> u64 {
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in read_dir.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

fn delete_file(path: &Path) -> Freed {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let existed = path.exists();
    let _ = std::fs::remove_file(path);
    Freed { existed, bytes }
}

fn delete_dir(path: &Path) -> Freed {
    let bytes = dir_size(path);
    let existed = path.exists();
    let _ = std::fs::remove_dir_all(path);
    Freed { existed, bytes }
}

/// Performs the actual deletion (the only I/O-mutating function in this
/// module besides `run`'s stdin confirmation prompt) — `dry_run` skips the
/// filesystem calls but still reports sizes, for the confirmation preview.
pub fn delete(targets: &Targets, dry_run: bool) -> Report {
    Report {
        history: targets.history.as_deref().map(|p| {
            if dry_run {
                Freed {
                    existed: p.exists(),
                    bytes: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
                }
            } else {
                delete_file(p)
            }
        }),
        cache: targets.cache_dir.as_deref().map(|p| {
            if dry_run {
                Freed {
                    existed: p.exists(),
                    bytes: dir_size(p),
                }
            } else {
                delete_dir(p)
            }
        }),
        auth: targets.auth.as_deref().map(|p| {
            if dry_run {
                Freed {
                    existed: p.exists(),
                    bytes: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
                }
            } else {
                // PRD SEC-4: also drop the keychain entry when that backend is
                // compiled — the file is the fallback, not the only store.
                #[cfg(feature = "keychain")]
                {
                    use crate::auth::TokenStore as _;
                    let _ = crate::auth::KeyringTokenStore::new("wikitui", "oauth-tokens").delete();
                }
                delete_file(p)
            }
        }),
        interest: targets.interest.as_deref().map(|p| {
            if dry_run {
                Freed {
                    existed: p.exists(),
                    bytes: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
                }
            } else {
                delete_file(p)
            }
        }),
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Runs `wikitui clear-data` end to end: validates `scope`, previews what
/// would be deleted (with byte counts), confirms (unless `assume_yes`), then
/// deletes. Returns the process exit code.
pub fn run(resolved: &ResolvedConfig, scope: Scope, assume_yes: bool) -> i32 {
    if scope.is_empty() {
        println!(
            "wikitui clear-data: specify at least one of --history, --cache, --stats, --auth, --all"
        );
        return 1;
    }

    let targets = resolve_targets(scope, resolved.cache_dir.value.as_deref());
    let preview = delete(&targets, true);

    println!("This will delete:");
    let mut any_real_target = false;
    if let (Some(path), Some(freed)) = (&targets.history, preview.history) {
        any_real_target = true;
        println!(
            "  history: {} ({}{})",
            path.display(),
            human_bytes(freed.bytes),
            if freed.existed {
                ""
            } else {
                " — not present"
            }
        );
    }
    if let (Some(path), Some(freed)) = (&targets.cache_dir, preview.cache) {
        any_real_target = true;
        println!(
            "  cache: {} ({}{})",
            path.display(),
            human_bytes(freed.bytes),
            if freed.existed {
                ""
            } else {
                " — not present"
            }
        );
    }
    if let (Some(path), Some(freed)) = (&targets.auth, preview.auth) {
        any_real_target = true;
        println!(
            "  auth: {} ({}{})",
            path.display(),
            human_bytes(freed.bytes),
            if freed.existed {
                ""
            } else {
                " — not present"
            }
        );
    }
    if let (Some(path), Some(freed)) = (&targets.interest, preview.interest) {
        any_real_target = true;
        println!(
            "  interest model: {} ({}{})",
            path.display(),
            human_bytes(freed.bytes),
            if freed.existed {
                ""
            } else {
                " — not present"
            }
        );
    }
    if scope.wants_stats() && targets.interest.is_none() {
        println!("  stats: no platform state directory — nothing to delete");
    }
    if !any_real_target {
        println!("  (nothing resolvable — no platform directory could be determined)");
    }

    if !assume_yes && any_real_target && !confirm("Proceed? (y/N) ") {
        println!("Aborted — nothing deleted.");
        return 0;
    }

    let report = delete(&targets, false);
    let mut total_bytes = 0u64;
    if let (Some(path), Some(freed)) = (&targets.history, report.history) {
        if freed.existed {
            println!(
                "  deleted history ({}): {}",
                human_bytes(freed.bytes),
                path.display()
            );
            total_bytes += freed.bytes;
        } else {
            println!("  history already absent: {}", path.display());
        }
    }
    if let (Some(path), Some(freed)) = (&targets.cache_dir, report.cache) {
        if freed.existed {
            println!(
                "  deleted cache ({}): {}",
                human_bytes(freed.bytes),
                path.display()
            );
            total_bytes += freed.bytes;
        } else {
            println!("  cache already absent: {}", path.display());
        }
    }
    if let (Some(path), Some(freed)) = (&targets.auth, report.auth) {
        if freed.existed {
            println!(
                "  deleted auth tokens ({}): {}",
                human_bytes(freed.bytes),
                path.display()
            );
            total_bytes += freed.bytes;
        } else {
            println!("  auth tokens already absent: {}", path.display());
        }
        // PRD FR-ACC-9: local deletion doesn't revoke the grant server-side.
        println!(
            "  note: also revoke server-side at {}",
            crate::auth::MANAGE_GRANTS_URL
        );
    }
    if let (Some(path), Some(freed)) = (&targets.interest, report.interest) {
        if freed.existed {
            println!(
                "  deleted interest model ({}): {}",
                human_bytes(freed.bytes),
                path.display()
            );
            total_bytes += freed.bytes;
        } else {
            println!("  interest model already absent: {}", path.display());
        }
    }
    println!("Total freed: {}", human_bytes(total_bytes));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wikitui-cleardata-test-{tag}-{}-{n}",
            std::process::id()
        ))
    }

    // ---- Scope --------------------------------------------------------

    #[test]
    fn empty_scope_is_empty() {
        assert!(Scope::default().is_empty());
    }

    #[test]
    fn a_single_flag_is_not_empty_and_only_reaches_its_own_target() {
        let scope = Scope {
            history: true,
            ..Default::default()
        };
        assert!(!scope.is_empty());
        assert!(scope.wants_history());
        assert!(!scope.wants_cache());
        assert!(!scope.wants_stats());
        assert!(!scope.wants_auth());
    }

    #[test]
    fn all_reaches_history_cache_stats_and_auth() {
        let scope = Scope {
            all: true,
            ..Default::default()
        };
        assert!(scope.wants_history());
        assert!(scope.wants_cache());
        assert!(scope.wants_stats());
        assert!(scope.wants_auth());
    }

    // ---- resolve_targets: --all never names user-content stores --------

    #[test]
    fn resolve_targets_never_names_bookmarks_saved_or_research_paths() {
        // `Targets` only has `history`/`cache_dir` fields — the type itself
        // is the guarantee that `--all`'s scope can't quietly grow to cover
        // user-created libraries (see the module doc comment). Destructuring
        // exhaustively (no `..`) means adding a third field to `Targets`
        // without updating this test fails to *compile*, not just fails to
        // warn.
        let Targets {
            history: _h,
            cache_dir: _c,
            auth: _a,
            interest: _i,
        } = resolve_targets(
            Scope {
                all: true,
                ..Default::default()
            },
            None,
        );
    }

    // ---- --stats deletes the interest model (PRD FR-PC-3 / FR-PF-3) --------

    #[test]
    fn delete_removes_the_interest_model_file_and_reports_its_size() {
        let path = temp_dir("interest").with_extension("json");
        std::fs::write(&path, b"{\"categories\":{\"Cryptography\":4.0}}").unwrap();
        let targets = Targets {
            history: None,
            cache_dir: None,
            auth: None,
            interest: Some(path.clone()),
        };
        let report = delete(&targets, false);
        assert_eq!(report.interest.map(|f| f.existed), Some(true));
        assert!(!path.exists(), "interest.json must be gone after --stats");
    }

    #[test]
    fn stats_scope_resolves_the_interest_model_path_not_history_or_cache() {
        // `--stats` names interest.json and nothing else (history/cache have
        // their own flags) — the type guards that, destructured exhaustively.
        let Targets {
            history,
            cache_dir,
            auth,
            interest,
        } = resolve_targets(
            Scope {
                stats: true,
                ..Default::default()
            },
            None,
        );
        assert!(history.is_none() && cache_dir.is_none() && auth.is_none());
        // `interest` is Some iff a platform state dir exists (None in a
        // sandbox without one) — either way it never leaks a history/cache path.
        let _ = interest;
    }

    // ---- --auth deletes the token file (PRD FR-ACC-9 / SEC-4) ----------

    #[test]
    fn delete_removes_the_auth_token_file_and_reports_its_size() {
        let path = temp_dir("auth").with_extension("json");
        std::fs::write(&path, b"{\"access_token\":\"x\"}").unwrap();
        let targets = Targets {
            history: None,
            cache_dir: None,
            auth: Some(path.clone()),
            interest: None,
        };
        let report = delete(&targets, false);
        assert_eq!(report.auth.map(|f| f.existed), Some(true));
        assert!(!path.exists(), "auth token file must be gone");
    }

    #[test]
    fn dry_run_leaves_the_auth_token_file_in_place() {
        let path = temp_dir("auth-dry").with_extension("json");
        std::fs::write(&path, b"{}").unwrap();
        let targets = Targets {
            history: None,
            cache_dir: None,
            auth: Some(path.clone()),
            interest: None,
        };
        let report = delete(&targets, true);
        assert!(report.auth.unwrap().existed);
        assert!(path.exists(), "a dry run must not delete tokens");
        let _ = std::fs::remove_file(&path);
    }

    // ---- delete/dir_size mechanics (against real tempdirs) --------------

    #[test]
    fn delete_removes_a_history_file_and_reports_its_size() {
        let path = temp_dir("history").with_extension("sqlite");
        std::fs::write(&path, b"0123456789").unwrap();
        let targets = Targets {
            history: Some(path.clone()),
            cache_dir: None,
            auth: None,
            interest: None,
        };

        let report = delete(&targets, false);
        assert_eq!(
            report.history,
            Some(Freed {
                existed: true,
                bytes: 10
            })
        );
        assert!(!path.exists());
    }

    #[test]
    fn delete_removes_a_cache_directory_recursively_and_sums_its_bytes() {
        let dir = temp_dir("cache");
        std::fs::create_dir_all(dir.join("blob").join("en")).unwrap();
        std::fs::write(dir.join("blob").join("en").join("a.zst"), vec![0u8; 100]).unwrap();
        std::fs::create_dir_all(dir.join("page").join("en")).unwrap();
        std::fs::write(dir.join("page").join("en").join("a.json"), vec![0u8; 50]).unwrap();
        let targets = Targets {
            history: None,
            cache_dir: Some(dir.clone()),
            auth: None,
            interest: None,
        };

        let report = delete(&targets, false);
        assert_eq!(
            report.cache,
            Some(Freed {
                existed: true,
                bytes: 150
            })
        );
        assert!(!dir.exists());
    }

    #[test]
    fn delete_on_a_missing_target_reports_not_existed_and_does_not_panic() {
        let dir = temp_dir("missing");
        let targets = Targets {
            history: Some(dir.join("nope.sqlite")),
            cache_dir: Some(dir.join("nope-cache")),
            auth: None,
            interest: None,
        };
        let report = delete(&targets, false);
        assert_eq!(
            report.history,
            Some(Freed {
                existed: false,
                bytes: 0
            })
        );
        assert_eq!(
            report.cache,
            Some(Freed {
                existed: false,
                bytes: 0
            })
        );
    }

    #[test]
    fn dry_run_reports_sizes_but_deletes_nothing() {
        let path = temp_dir("dry-run-history").with_extension("sqlite");
        std::fs::write(&path, b"0123456789").unwrap();
        let targets = Targets {
            history: Some(path.clone()),
            cache_dir: None,
            auth: None,
            interest: None,
        };

        let report = delete(&targets, true);
        assert_eq!(report.history.unwrap().bytes, 10);
        assert!(path.exists(), "a dry run must not actually delete anything");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn human_bytes_scales_sensibly() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(2048), "2 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }
}
