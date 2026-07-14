//! PRD FR-PR-3's single privacy chokepoint: "all persistence routes through
//! one gate (architectural requirement, do early)". Every store that writes
//! to disk (`history::History`, `cache::PageCache`, `bookmarks::
//! BookmarkStore`/`ReadLaterStore`, `research::ResearchStore`, `saved::
//! SavedPages`, `fetch_queue::FetchQueue`) is owned by `App`/`main`, which
//! already knows `App.incognito` — so the gate is not a wrapper *around*
//! those stores (that would mean two ways to reach the disk, one gated and
//! one not) but the one function every call site that's about to perform a
//! write consults *before* calling into its store: [`decide`]. A grep for
//! every `.record_visit(`/`.put(`/`.toggle(`/`.enqueue(`/`.save(`/`.add(`
//! call site in `app.rs`/`main.rs` should turn up a `privacy::decide` (or an
//! equivalent incognito check that predates this module and is documented
//! as such) immediately upstream of it — that is the audit this module
//! exists to make checkable, not a promise enforced by the type system.
//!
//! ## The passive/explicit split
//!
//! PRD FR-PR-3 lists what incognito suppresses: "no history, no stats, no
//! interest updates, no prefetch." Every item on that list is *passive*
//! tracking — a side effect of reading that the reader never asked for by
//! name. It says nothing about `m` (bookmark), `S` (save offline), or a
//! citation save (`s` in Research mode) — those are the reader explicitly
//! naming a piece of content and asking wikitui to keep it. Suppressing an
//! explicit save would be a worse surprise than warning about it: a reader
//! who presses `S` in incognito is not confused about whether they wanted
//! that page kept, but *would* be reasonably surprised, later, to learn
//! incognito quietly ate their save. So this module's policy is: passive
//! writes are **denied** outright; explicit writes are **allowed, with a
//! warning** the caller must surface (`AllowWithWarning`) — never silently.
//! `cache::PageCache`'s writes are their own case, folded into passive
//! (see [`Write::Cache`]'s doc comment): the *fetch* that lands a page
//! in L2 might be caused by an explicit action (opening a bookmarked
//! article) or a passive one (prefetch, revalidation) and the cache has no
//! way to tell which — so it is never denied (a cache miss must never be
//! the visible effect of incognito, that would be its own kind of leak via
//! timing) but every entry written while incognito is on is tagged and
//! wiped at session end (`cache::PageCache::wipe_incognito_entries`).
//!
//! ## Seams (not built yet, listed so the gate doesn't have to be rediscovered)
//!
//! - **Reading stats** (PRD FR-PC-3) and the **interest-affinity model**
//!   (FR-PF-3) don't exist yet ([`Write::Stats`]/[`Write::Interest`] exist
//!   here so `decide`'s table is already complete for them — a future
//!   `stats.rs`/`interest.rs` just has to call `decide` before its first
//!   write, not invent the policy).
//! - **Reading-position memory** (FR-NV-8) and **session save/restore**
//!   (FR-TB-5) are later chunks too; when they land, their writes are
//!   passive (nothing the reader explicitly asked to persist) and belong
//!   in the same deny bucket as history.

/// One persistence write this app can attempt. Named per call site (not per
/// underlying store) so `decide`'s policy table reads as "what is the
/// reader doing," which is what incognito's contract is actually about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// `history::History::record_visit` / dwell tracking
    /// (`app::record_history_visit`, `app::flush_tab_dwell`) — passive.
    History,
    /// Reading stats (PRD FR-PC-3) — not built yet; no real call site
    /// constructs this today (see the module doc comment's "Seams"), so a
    /// future `stats.rs` finds the policy already decided rather than having
    /// to invent it.
    #[allow(dead_code)]
    Stats,
    /// The interest-affinity model (PRD FR-PF-3) — not built yet; same seam
    /// posture as `Stats` above.
    #[allow(dead_code)]
    Interest,
    /// Scheduling a prefetch job (`app::prefetch_active`) — passive.
    Prefetch,
    /// `cache::PageCache::put_at` — see the module doc comment's "The
    /// passive/explicit split" for why this is never denied, only tagged
    /// (`cache.rs` `debug_assert!`s that this always resolves to `Allow`,
    /// rather than deciding cache-write policy a second, independent way).
    Cache,
    /// `m` / `BookmarkStore::toggle` (`app::toggle_bookmark`) — explicit.
    Bookmark,
    /// `rl` / `ReadLaterStore::enqueue` (`main::enqueue_read_later`) —
    /// explicit: the reader named this article and asked to read it later.
    ReadLater,
    /// `S` / the saved-pages pin (`main::fire_save_job`,
    /// `main::apply_save_outcome`) — explicit.
    OfflineSave,
    /// Research mode's `s`/Enter / `ResearchStore::add`
    /// (`app::save_selected_citation`) — explicit.
    Citation,
    /// The offline card's `f` / `FetchQueue::enqueue`
    /// (`app::queue_offline_target`) — explicit: the reader chose "fetch
    /// this specific thing when online" over "search saved pages instead".
    FetchQueue,
}

/// What a caller must do about a [`Write`] it's about to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Proceed normally — either not incognito, or (`Cache`) a write
    /// this policy never blocks outright.
    Allow,
    /// Do not perform the write at all.
    Deny,
    /// Proceed, but the caller must surface [`EXPLICIT_SAVE_WARNING`] (or an
    /// equivalent, write-specific wording) to the reader — never a silent
    /// persist.
    AllowWithWarning,
}

/// The one function every persistence write in the app consults before
/// touching disk (PRD FR-PR-3's "architectural requirement"). Pure and
/// total: every [`Write`] variant has a defined outcome for both incognito
/// states, so adding a new write kind means extending [`Write`] and this
/// function's exhaustive match together, never leaving a write undecided.
pub fn decide(incognito: bool, write: Write) -> Verdict {
    if !incognito {
        return Verdict::Allow;
    }
    match write {
        // Passive tracking (PRD FR-PR-3's explicit list, plus the two seams
        // it implies): suppressed outright.
        Write::History | Write::Stats | Write::Interest | Write::Prefetch => Verdict::Deny,
        // See the module doc comment's "The passive/explicit split": never
        // denied, only tagged for wipe by `cache::PageCache` itself.
        Write::Cache => Verdict::Allow,
        // Explicit saves: the reader named this content — permitted, but
        // every caller must surface the warning.
        Write::Bookmark
        | Write::ReadLater
        | Write::OfflineSave
        | Write::Citation
        | Write::FetchQueue => Verdict::AllowWithWarning,
    }
}

/// The warning text every explicit-save call site appends to its own
/// success message under `Verdict::AllowWithWarning` — one wording, so the
/// reader learns to recognize it regardless of which key they pressed.
pub const EXPLICIT_SAVE_WARNING: &str =
    "incognito: explicit saves aren't private — this persists to disk";

/// Appends [`EXPLICIT_SAVE_WARNING`] to `base` exactly when `write` resolves
/// to `AllowWithWarning` under `incognito`; returns `base` untouched
/// otherwise (including when `write` would be `Deny` — no explicit-save call
/// site is ever denied, so that branch never actually fires today, but the
/// function stays total rather than assuming its own caller's write kind).
/// The one helper every explicit-save call site (`app::toggle_bookmark`,
/// `app::save_selected_citation`, `app::queue_offline_target`,
/// `main::enqueue_read_later`, `main::fire_save_job`) uses instead of each
/// formatting the warning itself, so the wording can't drift between them.
pub fn append_warning_if_needed(incognito: bool, write: Write, base: String) -> String {
    match decide(incognito, write) {
        Verdict::AllowWithWarning => format!("{base} ({EXPLICIT_SAVE_WARNING})"),
        Verdict::Allow | Verdict::Deny => base,
    }
}

// ---- PRD FR-PR-1's CI check: no analytics/telemetry endpoints in source ---
//
// "CI check asserts no analytics endpoints exist in the binary." Scanning
// the built binary directly is possible but slower and less legible than
// scanning the source that produced it — anything absent from every `.rs`
// file cannot have been linked in without a dependency smuggling its own
// call to one of these hosts, which `cargo deny`/`cargo audit` (§6.6 SEC-6)
// covers from the other direction (auditing what dependencies *are*, not
// what strings the binary contains). This test is the source-side half of
// that promise.

/// Known analytics/monitoring service domains — fully host-qualified (not
/// bare brand words like "segment" or "plausible", which collide with
/// ordinary English and this project's own identifiers, e.g.
/// `unicode_segmentation`, `today_produces_a_plausible_iso_date`) so the
/// scan below can do plain substring matching without a false positive on
/// existing code. `"://telemetry."` matches an actual URL
/// (`https://telemetry.example.com`) rather than the *word* "telemetry",
/// which this module's own doc comments and PRD-linked comments elsewhere
/// legitimately use when describing FR-PR-1.
///
/// `#[cfg(test)]`: this denylist and the scan it drives are a CI-time check
/// (PRD §9), not shipped runtime behavior — there is no code path in the
/// actual reader that ever needs to ask "is this string an analytics
/// endpoint", so gating it to test builds is honest about what it is,
/// rather than reaching for `#[allow(dead_code)]` on something that isn't a
/// documented future seam like `privacy::Write::Stats`.
#[cfg(test)]
const BANNED_ANALYTICS_SUBSTRINGS: &[&str] = &[
    "google-analytics.com",
    "googletagmanager.com",
    "sentry.io",
    "mixpanel.com",
    "segment.io",
    "segment.com",
    "plausible.io",
    "amplitude.com",
    "posthog.com",
    "fullstory.com",
    "hotjar.com",
    "bugsnag.com",
    "://telemetry.",
];

/// Which banned substring (if any) appears in `text` — a free function so
/// both the real source-tree scan and this module's own "the detector
/// actually detects something" test share one implementation, per PRD §9's
/// general preference for testing the pure decision separately from the
/// I/O around it.
#[cfg(test)]
fn find_analytics_reference(text: &str) -> Option<&'static str> {
    BANNED_ANALYTICS_SUBSTRINGS
        .iter()
        .copied()
        .find(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- decide/append_warning_if_needed: the policy table ----------------

    #[test]
    fn not_incognito_allows_every_write_kind() {
        for write in [
            Write::History,
            Write::Stats,
            Write::Interest,
            Write::Prefetch,
            Write::Cache,
            Write::Bookmark,
            Write::ReadLater,
            Write::OfflineSave,
            Write::Citation,
            Write::FetchQueue,
        ] {
            assert_eq!(
                decide(false, write),
                Verdict::Allow,
                "{write:?} must be unrestricted outside incognito"
            );
        }
    }

    #[test]
    fn incognito_denies_every_passive_write() {
        for write in [
            Write::History,
            Write::Stats,
            Write::Interest,
            Write::Prefetch,
        ] {
            assert_eq!(
                decide(true, write),
                Verdict::Deny,
                "{write:?} is passive tracking — FR-PR-3 requires it suppressed"
            );
        }
    }

    #[test]
    fn incognito_allows_cache_writes_outright_tagging_is_a_separate_mechanism() {
        // A cache miss must never be an incognito side effect (that would
        // leak incognito state via timing/behavior) — see the module doc
        // comment. Tagging-for-wipe lives in `cache::PageCache` itself.
        assert_eq!(decide(true, Write::Cache), Verdict::Allow);
    }

    #[test]
    fn incognito_allows_every_explicit_save_but_warns() {
        for write in [
            Write::Bookmark,
            Write::ReadLater,
            Write::OfflineSave,
            Write::Citation,
            Write::FetchQueue,
        ] {
            assert_eq!(
                decide(true, write),
                Verdict::AllowWithWarning,
                "{write:?} is an explicit save — FR-PR-3 permits it, with a warning"
            );
        }
    }

    #[test]
    fn append_warning_if_needed_is_a_no_op_outside_incognito() {
        assert_eq!(
            append_warning_if_needed(false, Write::Bookmark, "Bookmarked \"X\"".to_string()),
            "Bookmarked \"X\""
        );
    }

    #[test]
    fn append_warning_if_needed_appends_the_shared_wording_in_incognito() {
        let out = append_warning_if_needed(true, Write::Bookmark, "Bookmarked \"X\"".to_string());
        assert!(out.starts_with("Bookmarked \"X\" ("));
        assert!(out.contains(EXPLICIT_SAVE_WARNING));
    }

    #[test]
    fn append_warning_if_needed_leaves_a_denied_write_untouched() {
        // No real call site passes a passive `Write` here (that would be a
        // logic error at the call site), but the helper must not invent a
        // warning for a write it would have denied outright.
        assert_eq!(
            append_warning_if_needed(true, Write::History, "recorded".to_string()),
            "recorded"
        );
    }

    // ---- The no-telemetry source scan (PRD FR-PR-1) ------------------------

    /// Proves the detector itself actually detects something, before trusting
    /// it to certify the real source tree clean. The fixture is assembled at
    /// runtime from two literals that are each innocuous alone — neither
    /// `"sentry"` nor `".io"` is itself banned — so the *contiguous* banned
    /// substring `"sentry.io"` exists only in memory once this test runs,
    /// never as a matching literal anywhere `no_analytics_endpoints_in_the_
    /// real_source_tree` (or any other file it scans) could trip on.
    #[test]
    fn detector_catches_a_known_analytics_reference() {
        let fixture = format!("let endpoint = \"https://{}{}/collect\";", "sentry", ".io");
        assert_eq!(find_analytics_reference(&fixture), Some("sentry.io"));
    }

    #[test]
    fn detector_is_clean_on_ordinary_privacy_policy_prose() {
        let prose = "wikitui sends no telemetry, ever. The only network \
                      calls are to the wikis you read, plus prefetch.";
        assert_eq!(find_analytics_reference(prose), None);
    }

    /// PRD FR-PR-1: "CI check asserts no analytics endpoints exist" — scans
    /// every `.rs` file directly under `src/` (this project's whole source
    /// tree; there are no subdirectories under `src/` today, see `Cargo.toml`'s
    /// single-binary layout) for the banned list above. `privacy.rs` itself is
    /// excluded: it is the denylist's own definition, so it necessarily spells
    /// out every banned substring literally — the one place that's allowed to,
    /// as data, not as a call site.
    #[test]
    fn no_analytics_endpoints_in_the_real_source_tree() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let read_dir =
            std::fs::read_dir(&src_dir).unwrap_or_else(|e| panic!("src dir must be readable: {e}"));
        let mut scanned = 0u32;
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("privacy.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} must be readable: {e}", path.display()));
            if let Some(hit) = find_analytics_reference(&text) {
                panic!(
                    "found banned analytics reference {hit:?} in {} — wikitui ships zero telemetry (PRD FR-PR-1)",
                    path.display()
                );
            }
            scanned += 1;
        }
        assert!(
            scanned > 10,
            "sanity check: the scan should have covered dozens of files, only saw {scanned} — \
             is CARGO_MANIFEST_DIR/src resolving correctly?"
        );
    }
}
