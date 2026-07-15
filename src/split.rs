//! Vertical splits (PRD FR-TB-4) and the bilingual side-by-side view (PRD
//! FR-ML-3) — the biggest UI-structure change since tabs, kept deliberately
//! *additive* so single-pane reading stays byte-for-byte what it was.
//!
//! # The panes model
//!
//! `App` already models "one open reading context" as a [`crate::tab::Tab`]
//! (document, scroll, links, folds, find state) and keeps a `Vec<Tab>` with an
//! `active` index. A split does **not** introduce a second, parallel notion of
//! a view: a [`Split`] is a thin overlay that says *"show two of the existing
//! tabs side by side, and route keys to the focused one."* Its two panes are
//! referenced by stable [`crate::tab::TabId`] (never `Vec` index — indices
//! shift when a tab to the left closes), and the app maintains one invariant:
//!
//! > `App::tabs[App::active].id == split.panes[split.focused]`
//!
//! i.e. **the focused pane is always the active tab.** Because every existing
//! key handler already routes through `App::active_tab()`/`active_tab_mut()`,
//! scrolling, link focus, folding, find, and navigation all "just work" on the
//! focused pane with zero changes — the split only has to move `active` when
//! focus moves between panes. When `App::split` is `None` the app is in exactly
//! its pre-split single-pane state.
//!
//! # Tab-bar interaction
//!
//! Each pane is backed by a *real* tab in `App::tabs`, so the tab bar, `gt`/
//! `gT` cycling, and the tab picker all continue to see both panes' articles.
//! `:vsplit` duplicates the focused tab's document into a fresh tab (the second
//! pane); `:bilingual` opens the other-language article as a fresh tab. Closing
//! the split (`Ctrl-w c` / `:only`) drops the overlay and returns to the
//! focused tab full-width; a plain `:vsplit`'s throwaway duplicate is removed
//! then (no tab clutter), while a `:bilingual` split keeps the other-language
//! article as an ordinary background tab. Switching tabs with `gt`/`gT` or the
//! picker collapses the split first (you asked to look at a different tab).
//!
//! # Scope (v1.x)
//!
//! Exactly **two vertical panes**. Horizontal splits and N-way tiling/nesting
//! are explicitly future work — the model (`panes: [TabId; 2]`) is sized for
//! two and would need generalizing to a tree for more, which this chunk does
//! not attempt.

use crate::tab::TabId;

/// The narrowest a single pane may be laid out at (cells). Below this the
/// article renders as an unreadable sliver, so [`fits`] refuses the split
/// (PRD §6.3 terminal-size degradation: don't render broken sub-tier panes).
/// Chosen just under the `Minimal` tier's 40-col effective column so a pane is
/// never narrower than the narrowest single-pane view the reader would tolerate.
pub const MIN_PANE_WIDTH: u16 = 40;

/// The narrowest content area a two-pane split needs: two [`MIN_PANE_WIDTH`]
/// panes plus the one-column vertical divider between them.
pub const MIN_SPLIT_WIDTH: u16 = MIN_PANE_WIDTH * 2 + 1;

/// Whether a content area `width` cells wide can host a two-pane split (PRD
/// FR-TB-4 / §6.3). A refusal here is the "terminal too narrow to split"
/// notice, not a broken render.
pub fn fits(width: u16) -> bool {
    width >= MIN_SPLIT_WIDTH
}

/// How a split couples the two panes' scroll positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Each pane scrolls on its own, clamped to its own length. The default
    /// for `:vsplit`.
    Independent,
    /// PRD FR-TB-4 `:set scrollbind`: the same line delta drives both panes,
    /// each clamped to its own length (panes of different lengths clamp
    /// independently — best-effort alignment).
    Lockstep,
    /// PRD FR-ML-3's heuristic section-sync: the bound pane is aligned on the
    /// nearest section boundary of the same ordinal index (see
    /// [`section_synced_scroll`]). The default for `:bilingual`, since two
    /// language editions of an article have different lengths but comparable
    /// section structure.
    Section,
}

/// A live two-pane split overlaid on `App`'s tab set (PRD FR-TB-4 / FR-ML-3).
/// See the module doc comment for the panes model and its invariant.
pub struct Split {
    /// The two panes' tab ids, left pane first. Stable across tab-index shifts.
    pub panes: [TabId; 2],
    /// Which pane (`0` = left, `1` = right) currently has keyboard focus. The
    /// app keeps `App::active` pointing at this pane's tab.
    pub focused: usize,
    /// How the panes' scroll is coupled (`:set scrollbind`, bilingual sync).
    pub sync: SyncMode,
    /// Whether this split was opened by `:bilingual` (PRD FR-ML-3): drives the
    /// "interwiki articles are not translations" banner and the default
    /// [`SyncMode::Section`] coupling.
    pub bilingual: bool,
    /// Each pane's section-boundary line positions as of the most recent draw
    /// (one-frame-stale, exactly like `App::last_content_area`), indexed by
    /// pane slot. [`SyncMode::Section`] reads these to align the bound pane
    /// without re-laying-out anything in the input path.
    pub pane_section_lines: [Vec<u16>; 2],
}

impl Split {
    /// A plain vertical split of two tabs, focus on the left pane, independent
    /// scroll (`:vsplit`).
    pub fn new(panes: [TabId; 2]) -> Self {
        Self {
            panes,
            focused: 0,
            sync: SyncMode::Independent,
            bilingual: false,
            pane_section_lines: [Vec::new(), Vec::new()],
        }
    }

    /// A bilingual split (`:bilingual`, PRD FR-ML-3): focus on the left
    /// (original-language) pane, heuristic section-sync coupling on by default.
    pub fn bilingual(panes: [TabId; 2]) -> Self {
        Self {
            panes,
            focused: 0,
            sync: SyncMode::Section,
            bilingual: true,
            pane_section_lines: [Vec::new(), Vec::new()],
        }
    }

    /// The focused pane's tab id (== the active tab, by the module invariant).
    pub fn focused_id(&self) -> TabId {
        self.panes[self.focused]
    }

    /// The *un*focused pane's tab id — the one scroll-sync mirrors into.
    pub fn other_id(&self) -> TabId {
        self.panes[1 - self.focused]
    }
}

/// PRD FR-ML-3's heuristic section-sync (pure, so it is unit-testable without
/// a live layout): given the focused pane scrolled to line `focused_scroll`,
/// the focused document's sorted section-boundary lines `focused_sections`, and
/// the other document's sorted section-boundary lines `other_sections`, returns
/// the line the *other* pane should scroll to so the two stay aligned on
/// section structure.
///
/// The heuristic: find which section the focused pane is currently inside (the
/// last boundary at or before `focused_scroll`), carry the intra-section offset
/// over, and land at the *same ordinal* section in the other pane. Because two
/// language editions rarely have the same number of sections, the ordinal index
/// is clamped to the other document's section count — this is explicitly
/// best-effort alignment, not a promise the two texts line up paragraph for
/// paragraph (interwiki articles are independent, not translations). The caller
/// still clamps the result to the other pane's own `max_scroll`.
pub fn section_synced_scroll(
    focused_scroll: u16,
    focused_sections: &[u16],
    other_sections: &[u16],
) -> u16 {
    // With no section structure on either side there is nothing to align on;
    // fall back to mirroring the raw line (identity).
    if focused_sections.is_empty() || other_sections.is_empty() {
        return focused_scroll;
    }
    let idx = focused_sections
        .iter()
        .rposition(|&line| line <= focused_scroll)
        .unwrap_or(0);
    let section_start = focused_sections[idx];
    let offset = focused_scroll.saturating_sub(section_start);
    let other_idx = idx.min(other_sections.len() - 1);
    other_sections[other_idx].saturating_add(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_refuses_below_two_min_panes_plus_divider() {
        assert!(!fits(70), "70 cols can't host two 40-col panes + a divider");
        assert!(!fits(MIN_SPLIT_WIDTH - 1));
        assert!(
            fits(MIN_SPLIT_WIDTH),
            "exactly two min panes + divider fits"
        );
        assert!(fits(100), "a wide terminal splits fine");
    }

    #[test]
    fn other_id_is_the_unfocused_pane() {
        let mut s = Split::new([7, 9]);
        assert_eq!(s.focused_id(), 7);
        assert_eq!(s.other_id(), 9);
        s.focused = 1;
        assert_eq!(s.focused_id(), 9);
        assert_eq!(s.other_id(), 7);
    }

    #[test]
    fn bilingual_split_defaults_to_section_sync() {
        let s = Split::bilingual([1, 2]);
        assert!(s.bilingual);
        assert_eq!(s.sync, SyncMode::Section);
        assert_eq!(s.focused, 0, "focus starts on the original-language pane");
    }

    #[test]
    fn section_sync_aligns_on_the_same_ordinal_section() {
        // Focused doc: sections start at lines 0, 20, 60. Other doc: 0, 10, 40.
        let focused = [0u16, 20, 60];
        let other = [0u16, 10, 40];
        // Inside section 1 (line 25 = 5 past its start at 20): the other pane
        // aligns to its section 1 (start 10) + the same 5-line offset = 15.
        assert_eq!(section_synced_scroll(25, &focused, &other), 15);
        // Right at a boundary carries no offset.
        assert_eq!(section_synced_scroll(60, &focused, &other), 40);
        // Before the first boundary stays at the top.
        assert_eq!(section_synced_scroll(0, &focused, &other), 0);
    }

    #[test]
    fn section_sync_clamps_the_ordinal_to_the_shorter_document() {
        // Focused has 4 sections, other only 2: section 3 maps to the other's
        // last section (index 1), best-effort.
        let focused = [0u16, 10, 20, 30];
        let other = [0u16, 100];
        // Line 32 is 2 past focused section 3 (start 30) -> other last (100) + 2.
        assert_eq!(section_synced_scroll(32, &focused, &other), 102);
    }

    #[test]
    fn section_sync_with_no_sections_is_identity() {
        assert_eq!(section_synced_scroll(42, &[], &[0, 10]), 42);
        assert_eq!(section_synced_scroll(42, &[0, 10], &[]), 42);
    }
}
