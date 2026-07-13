//! Vimium-style link hints (PRD FR-NV-1's second navigation model, alongside
//! Tab/Shift-Tab cycling): `f` overlays a short home-row label on every link
//! visible in the current viewport; typing narrows the set, and completing a
//! label follows it. `F` does the same but routes the resolved link into a
//! background tab (FR-TB-3 integration) instead of the current one.
//!
//! Three concerns live here, kept deliberately separate:
//!   * [`generate_hint_labels`] — pure label assignment, no knowledge of the
//!     document or screen at all.
//!   * [`visible_link_hints`] — which links are on screen right now, from
//!     `(layout, scroll, viewport_height)` alone. Nothing is cached across a
//!     resize: PRD's "hints survive reflow" is satisfied by recomputing this
//!     from scratch every time, not by trying to keep a stale assignment
//!     valid (see `App::refresh_hint_targets`).
//!   * [`resolve`] — the narrowing/completion state machine typed input
//!     drives, and [`overlay_hint_labels`] — splicing the resolved labels
//!     into laid-out lines for painting.
//!
//! Hints are transient interactive state, not part of the cacheable,
//! theme-independent [`crate::layout::Layout`] (its own doc comment: "spans
//! carry a semantic `SpanKind`, never a theme color or focus state"): nothing
//! here is stored on `Layout` itself, matching how focused-link/visited state
//! already work.

use unicode_segmentation::UnicodeSegmentation;

use crate::layout::{LaidLine, LaidSpan, Layout, SpanKind, display_width};

/// Home-row-first hint alphabet (vimium's own default is similar): the left
/// hand's home row (`asdfghjkl;`) first, since those are the fastest keys to
/// reach without looking, then the row above (`weruio`) for extra capacity.
/// Exactly 16 characters — documented, not tuned; a config-file override is
/// natural future work but out of scope here.
pub const HINT_ALPHABET: &str = "asdfghjkl;weruio";

/// Assigns `count` labels, deterministic and prefix-free: no assigned label
/// is ever a prefix of another, so a typed sequence resolves to exactly one
/// hint the instant it matches a label in full, with no need to wait for
/// "maybe more characters are coming" (PRD FR-NV-1, "hints survive reflow"
/// depends on this being a pure function of `count` alone, not on any state
/// object it can drift out of sync with).
///
/// The scheme is deliberately simpler than vimium's own (which mixes label
/// lengths in one set): every label assigned in a single call has the SAME
/// length, which alone guarantees prefix-freedom (a shorter string can never
/// equal-length-prefix a same-length one unless identical). One character
/// while `count` fits the alphabet; two characters (first char × second
/// char, alphabet-major order) once it doesn't, up to `alphabet.len()^2`
/// total capacity. A `count` beyond that capacity is clamped — no realistic
/// terminal viewport holds more than a few dozen links at once, so the
/// clamp is a documented safety backstop, not a real limit in practice.
pub fn generate_hint_labels(count: usize) -> Vec<String> {
    let alphabet: Vec<char> = HINT_ALPHABET.chars().collect();
    let a = alphabet.len();
    if count == 0 {
        return Vec::new();
    }
    if count <= a {
        return alphabet.iter().take(count).map(|c| c.to_string()).collect();
    }
    let capacity = a * a;
    let n = count.min(capacity);
    (0..n)
        .map(|i| format!("{}{}", alphabet[i / a], alphabet[i % a]))
        .collect()
}

/// One visible link ready to be labeled (PRD FR-NV-1): `link` is the
/// occurrence index into the active tab's `links`/the layout's `link_lines`;
/// `line`/`col` are where its label paints (line index, grapheme column into
/// that line — the same units as `crate::layout::Layout::link_cols`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintTarget {
    pub link: usize,
    pub line: usize,
    pub col: usize,
    pub label: String,
}

/// Every link occurrence whose first line falls within the viewport
/// `[scroll, scroll + viewport_height)`, labeled in reading order (top to
/// bottom, matching `Layout::link_lines`'s own documented non-decreasing
/// order) — the sole source of "which links are hinted right now." Called
/// fresh on hint-mode entry and on every draw while hinting (PRD's "hints
/// survive reflow"): nothing about a previous call's assignment is reused,
/// so a resize that changes the visible set simply produces a different
/// (still internally consistent) label assignment next time this runs.
pub fn visible_link_hints(layout: &Layout, scroll: u16, viewport_height: u16) -> Vec<HintTarget> {
    let top = scroll as usize;
    let bottom = top + (viewport_height.max(1) as usize);
    let visible: Vec<usize> = (0..layout.link_lines.len())
        .filter(|&occ| {
            let line = layout.link_lines[occ];
            line >= top && line < bottom
        })
        .collect();
    let labels = generate_hint_labels(visible.len());
    visible
        .into_iter()
        .zip(labels)
        .map(|(occ, label)| HintTarget {
            link: occ,
            line: layout.link_lines[occ],
            col: layout.link_cols[occ].start,
            label,
        })
        .collect()
}

/// What one more typed character does to hint narrowing (PRD FR-NV-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintOutcome {
    /// More than one hint still matches the typed prefix; keep typing.
    Pending,
    /// Exactly one hint's label equals the typed text — follow it.
    Resolved(usize),
    /// No hint's label starts with the typed text at all: this keystroke is
    /// nonsense given what's on screen. Chosen over exiting hint mode
    /// outright (the brief's other documented option) so a single mistyped
    /// key doesn't throw the reader back to Reading mid-hint; Esc and
    /// backspace remain the explicit ways out/back.
    Ignored,
}

/// The targets whose label still starts with `typed` — the actual "narrowing"
/// a keystroke performs, factored out so both `resolve` and
/// `overlay_hint_labels` share one filter instead of two copies that could
/// drift apart.
fn matching<'a>(targets: &'a [HintTarget], typed: &str) -> Vec<&'a HintTarget> {
    targets
        .iter()
        .filter(|t| t.label.starts_with(typed))
        .collect()
}

/// Resolves `typed` against `targets` (see [`HintOutcome`]). Prefix-freedom
/// (guaranteed by [`generate_hint_labels`]) is what makes `Resolved` safe to
/// return the instant exactly one match's label equals `typed` in full: no
/// other still-matching label can also equal it, and none can be a longer
/// string that merely starts with it while itself remaining unresolved.
pub fn resolve(targets: &[HintTarget], typed: &str) -> HintOutcome {
    let matches = matching(targets, typed);
    match matches.as_slice() {
        [] => HintOutcome::Ignored,
        [only] if only.label == typed => HintOutcome::Resolved(only.link),
        _ => HintOutcome::Pending,
    }
}

/// Splices each hint in `targets` whose label still starts with `typed` over
/// the first display cells of its link, on a fresh copy of `lines` — the
/// cached `Layout` this came from is never touched (see the module doc
/// comment). A target whose label doesn't match `typed` is left alone
/// entirely: its link keeps its ordinary text and style, which is exactly
/// "narrowing hides the hints that no longer apply."
pub fn overlay_hint_labels(
    lines: &[LaidLine],
    targets: &[HintTarget],
    typed: &str,
    ambiguous_wide: bool,
) -> Vec<LaidLine> {
    let mut out = lines.to_vec();
    for target in matching(targets, typed) {
        if let Some(line) = out.get_mut(target.line) {
            splice_label(line, target, ambiguous_wide);
        }
    }
    out
}

/// Finds the span at `target`'s recorded column (always the exact start of
/// its link's own span — `finalize` never merges a `Link` span with its
/// neighbors) and overwrites its leading cells with the label.
fn splice_label(line: &mut LaidLine, target: &HintTarget, ambiguous_wide: bool) {
    let mut col = 0usize;
    for i in 0..line.spans.len() {
        let span_graphemes = line.spans[i].text.graphemes(true).count();
        if col == target.col
            && matches!(&line.spans[i].kind, SpanKind::Link(occ) if *occ == target.link)
        {
            replace_leading_cells(&mut line.spans, i, &target.label, ambiguous_wide);
            return;
        }
        col += span_graphemes;
    }
}

/// Overwrites the leading display cells of `spans[idx]` with `label`,
/// preserving the line's total width whenever the link has room: trailing
/// spaces pad out any cell the label's (always-ASCII, always-narrow) text is
/// narrower than the wide grapheme(s) it displaced. This is the "column math
/// must use display width, not char count" requirement: a CJK link's first
/// character is 2 display cells wide but 1 grapheme, so a 2-character label
/// consumes that ONE grapheme (2 cells) rather than two graphemes (4 cells),
/// or the label would eat into text the link never owned.
///
/// A link shorter (in cells) than the label itself — pathological; real
/// article link text is always at least a word — is fully consumed and the
/// label still painted at its natural width regardless, which can shift that
/// one row by the shortfall. Documented, not fixed: correctly extending the
/// overlay into a neighboring span's text isn't worth the complexity for a
/// case normal content never produces.
fn replace_leading_cells(spans: &mut Vec<LaidSpan>, idx: usize, label: &str, ambiguous_wide: bool) {
    let original = spans[idx].text.clone();
    let kind = spans[idx].kind.clone();
    let label_width = display_width(label, ambiguous_wide);

    let mut consumed_width = 0usize;
    let mut consumed_bytes = 0usize;
    for g in original.graphemes(true) {
        if consumed_width >= label_width {
            break;
        }
        consumed_width += display_width(g, ambiguous_wide);
        consumed_bytes += g.len();
    }
    let remainder = original[consumed_bytes..].to_string();
    let pad = consumed_width.saturating_sub(label_width);

    let mut replacement = Vec::with_capacity(2);
    replacement.push(LaidSpan {
        text: format!("{label}{}", " ".repeat(pad)),
        kind: SpanKind::Hint,
    });
    if !remainder.is_empty() {
        replacement.push(LaidSpan {
            text: remainder,
            kind,
        });
    }
    spans.splice(idx..=idx, replacement);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{collect_links, parse_article_html};
    use crate::layout::{LayoutOptions, layout_document};

    // ---- Label generation ---------------------------------------------

    #[test]
    fn zero_count_produces_no_labels() {
        assert!(generate_hint_labels(0).is_empty());
    }

    #[test]
    fn one_char_labels_while_count_fits_the_alphabet() {
        let a = HINT_ALPHABET.chars().count();
        for count in [1, 2, a / 2, a] {
            let labels = generate_hint_labels(count);
            assert_eq!(labels.len(), count);
            assert!(
                labels.iter().all(|l| l.chars().count() == 1),
                "every label must be 1 char at count {count}: {labels:?}"
            );
        }
    }

    /// The exact boundary the brief calls out: one more link than the
    /// alphabet holds flips EVERY label (not just the new one) to 2 chars,
    /// since all labels from one call share a length.
    #[test]
    fn two_char_labels_once_count_exceeds_the_alphabet() {
        let a = HINT_ALPHABET.chars().count();
        let labels = generate_hint_labels(a + 1);
        assert_eq!(labels.len(), a + 1);
        assert!(
            labels.iter().all(|l| l.chars().count() == 2),
            "every label must be 2 chars once count > alphabet size: {labels:?}"
        );
    }

    #[test]
    fn labels_are_always_unique() {
        let a = HINT_ALPHABET.chars().count();
        for count in [1, a - 1, a, a + 1, a + 5, a * a, a * a + 10] {
            let labels = generate_hint_labels(count);
            let unique: std::collections::HashSet<&String> = labels.iter().collect();
            assert_eq!(
                unique.len(),
                labels.len(),
                "duplicate label at count {count}: {labels:?}"
            );
        }
    }

    /// No label may be a literal prefix of a different label — the property
    /// that makes `resolve` safe to declare victory the instant one match
    /// equals the typed text exactly.
    #[test]
    fn no_label_is_a_prefix_of_another() {
        let a = HINT_ALPHABET.chars().count();
        for count in [1, a - 1, a, a + 1, a + 7, a * a] {
            let labels = generate_hint_labels(count);
            for (i, x) in labels.iter().enumerate() {
                for (j, y) in labels.iter().enumerate() {
                    if i != j {
                        assert!(
                            !y.starts_with(x.as_str()),
                            "at count {count}: {x:?} is a prefix of {y:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(generate_hint_labels(30), generate_hint_labels(30));
    }

    #[test]
    fn count_beyond_capacity_clamps_without_panicking() {
        let a = HINT_ALPHABET.chars().count();
        let labels = generate_hint_labels(a * a + 500);
        assert_eq!(labels.len(), a * a, "clamped to two-char capacity");
    }

    // ---- Viewport visibility -------------------------------------------

    /// Several paragraphs, each holding one link, so each link lands on a
    /// distinct, predictable line for scroll-window tests.
    const MANY_LINKS_FIXTURE: &str = r##"<html><body>
        <p>one <a href="./A">Alpha</a></p>
        <p>two <a href="./B">Bravo</a></p>
        <p>three <a href="./C">Charlie</a></p>
        <p>four <a href="./D">Delta</a></p>
        <p>five <a href="./E">Echo</a></p>
    </body></html>"##;

    #[test]
    fn links_above_the_scroll_offset_are_excluded() {
        let doc = parse_article_html("T", MANY_LINKS_FIXTURE);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let links = collect_links(&doc);
        assert_eq!(links.len(), 5);

        // Scroll far enough that only the last link's line remains on-screen.
        let last_line = *layout.link_lines.last().unwrap() as u16;
        let targets = visible_link_hints(&layout, last_line, 1);
        assert_eq!(targets.len(), 1, "only the last link's line is in view");
        assert_eq!(targets[0].link, 4);
    }

    #[test]
    fn links_below_the_viewport_are_excluded() {
        let doc = parse_article_html("T", MANY_LINKS_FIXTURE);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        // A viewport tall enough to show only the first link's line.
        let first_line = layout.link_lines[0] as u16;
        let targets = visible_link_hints(&layout, 0, first_line + 1);
        assert_eq!(
            targets.len(),
            1,
            "later links' lines are below the viewport"
        );
        assert_eq!(targets[0].link, 0);
    }

    /// The boundary case: a link whose line is exactly the LAST row still
    /// inside the viewport (not one row past it) must still be hinted.
    #[test]
    fn a_link_on_the_last_visible_row_is_included() {
        let doc = parse_article_html("T", MANY_LINKS_FIXTURE);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let third_line = layout.link_lines[2] as u16;
        // Viewport exactly tall enough to reach the third link's line and no
        // further.
        let targets = visible_link_hints(&layout, 0, third_line + 1);
        let occs: Vec<usize> = targets.iter().map(|t| t.link).collect();
        assert!(occs.contains(&2), "the boundary row itself must be hinted");
        assert!(
            !occs.contains(&3),
            "one row past the viewport must not be hinted"
        );
    }

    #[test]
    fn visible_links_are_labeled_in_reading_order() {
        let doc = parse_article_html("T", MANY_LINKS_FIXTURE);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let targets = visible_link_hints(&layout, 0, layout.lines.len() as u16);
        let occs: Vec<usize> = targets.iter().map(|t| t.link).collect();
        assert_eq!(occs, vec![0, 1, 2, 3, 4], "reading order, top to bottom");
        // With <= 16 visible links every label is distinct and 1 char.
        let labels: std::collections::HashSet<&str> =
            targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels.len(), 5);
    }

    // ---- Narrowing / resolution -----------------------------------------

    fn target(link: usize, label: &str) -> HintTarget {
        HintTarget {
            link,
            line: 0,
            col: 0,
            label: label.to_string(),
        }
    }

    #[test]
    fn typed_prefix_narrows_the_matching_set() {
        let targets = vec![target(0, "as"), target(1, "ad"), target(2, "sf")];
        assert_eq!(matching(&targets, "").len(), 3);
        assert_eq!(matching(&targets, "a").len(), 2);
        assert_eq!(matching(&targets, "s").len(), 1);
    }

    #[test]
    fn completing_a_label_resolves_to_its_own_link_index() {
        let targets = vec![target(0, "as"), target(1, "ad"), target(2, "sf")];
        assert_eq!(resolve(&targets, "a"), HintOutcome::Pending);
        assert_eq!(resolve(&targets, "ad"), HintOutcome::Resolved(1));
        assert_eq!(resolve(&targets, "sf"), HintOutcome::Resolved(2));
    }

    #[test]
    fn a_char_matching_no_label_is_ignored() {
        let targets = vec![target(0, "as"), target(1, "df")];
        assert_eq!(resolve(&targets, "z"), HintOutcome::Ignored);
        assert_eq!(resolve(&targets, "ax"), HintOutcome::Ignored);
    }

    #[test]
    fn single_char_labels_resolve_on_the_first_keystroke() {
        let targets = vec![target(0, "a"), target(1, "s")];
        assert_eq!(resolve(&targets, "a"), HintOutcome::Resolved(0));
    }

    // ---- CJK overlay: display width, not grapheme count ------------------

    const JA_LINK_FIXTURE: &str = r##"<html><body>
        <p>彼はしばしば<a href="./計算機科学">計算機科学</a>の父と呼ばれている。</p>
    </body></html>"##;

    #[test]
    fn overlay_on_a_cjk_link_consumes_whole_wide_graphemes_and_pads_to_width() {
        let doc = parse_article_html("T", JA_LINK_FIXTURE);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let links = collect_links(&doc);
        assert_eq!(links.len(), 1);
        let line_idx = layout.link_lines[0];
        let original_width = layout.lines[line_idx].width(false);

        let targets = vec![HintTarget {
            link: 0,
            line: line_idx,
            col: layout.link_cols[0].start,
            label: "as".to_string(),
        }];
        let overlaid = overlay_hint_labels(&layout.lines, &targets, "", false);

        // The line's total display width is unchanged: the label displaced
        // exactly as many cells as it occupies, not more or fewer.
        assert_eq!(overlaid[line_idx].width(false), original_width);

        let overlaid_text: String = overlaid[line_idx]
            .spans
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            overlaid_text.contains("as"),
            "the label text must appear on the line: {overlaid_text:?}"
        );
        // Only the first CJK character (2 display cells, matching "as"'s 2
        // cells) was displaced — the rest of the link's own text survives.
        assert!(
            overlaid_text.contains("算機科学"),
            "only the first wide grapheme should be displaced: {overlaid_text:?}"
        );
    }

    /// Narrowing: a target whose label doesn't match the typed prefix keeps
    /// its link's ORIGINAL text untouched — "non-matching hints disappear"
    /// means the overlay reverts, not that the link vanishes.
    #[test]
    fn overlay_skips_targets_that_no_longer_match_the_typed_prefix() {
        let html = r##"<html><body><p>See <a href="./X">Alpha</a> and
            <a href="./Y">Delta</a>.</p></body></html>"##;
        let doc = parse_article_html("T", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let targets = visible_link_hints(&layout, 0, layout.lines.len() as u16);
        assert_eq!(targets.len(), 2);

        // Narrow to just the first target's label.
        let keep = &targets[0].label;
        let overlaid = overlay_hint_labels(&layout.lines, &targets, keep, false);
        let joined: String = overlaid
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.text.as_str())
            .collect();
        assert!(joined.contains("Delta"), "unmatched link's text survives");
    }
}
