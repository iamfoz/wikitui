//! The width-aware layout engine (PRD FR-RD-10 + the measure half of
//! FR-RD-9). It turns a `doc::Document` into a flat list of laid-out lines
//! where **one logical line equals one screen row** — the property the
//! reading view depends on so that scroll offset, section jumps, find
//! matches, and focused-link auto-scroll all agree on a single line count.
//!
//! Why this exists: ratatui's runtime `Wrap` scrolls in *wrapped-line*
//! units, but every position the app tracks (`max_scroll`, section lines,
//! find matches, link occurrences) counts *logical* lines. On any page whose
//! paragraphs wrap — nearly all of them — those two counts diverge and jumps
//! land in the wrong place. Laying the text out ourselves makes the two the
//! same thing.
//!
//! Design constraints honored here (PRD §6.3 "width-bucketed,
//! theme-independent spans + semantic styles ... theme applied at paint
//! time"):
//!   * Spans carry a *semantic* [`SpanKind`], never a theme color or focus
//!     state — those are applied in `ui.rs` at paint time, so a layout is
//!     cacheable across theme/focus changes.
//!   * Width is measured per grapheme cluster with `unicode-width`, using the
//!     CJK/ambiguous-wide tables when [`LayoutOptions::ambiguous_wide`] is
//!     set. A grapheme cluster is never split across lines, so combining
//!     marks, ZWJ emoji, and IPA stay intact.
//!   * Line breaking happens at spaces and after hyphens for spaced scripts,
//!     and *between* CJK characters (per-character, FR-RD-10) with a minimal
//!     kinsoku rule set (see [`is_no_start`]/[`is_no_end`]).

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::doc::{Block, Document, SpanStyle};

/// The semantic style of a laid-out span. Deliberately theme-independent:
/// the paint step in `ui.rs` maps each variant to concrete `ratatui` styles
/// using the active theme, the focused-link index, and visited state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanKind {
    /// Ordinary body text.
    Plain,
    Bold,
    Italic,
    /// Dimmed structural text: superscripts, the blockquote gutter bar, and
    /// horizontal rules (all rendered in `theme.dim`).
    Dim,
    /// The article title line (bold, no color — like a `<h1>`).
    Title,
    Heading(u8),
    /// Body text inside a blockquote (rendered in `theme.quote`).
    Quote,
    Code,
    Table,
    Infobox,
    Image,
    /// A link occurrence. The index matches `doc::collect_links` ordering, so
    /// the paint step can look up focus/visited state by the same index the
    /// app cycles through.
    Link(usize),
    /// A link-hint label (PRD FR-NV-1), spliced over the leading cells of a
    /// link's own span at paint time (`hints::overlay_hint_labels`) — never
    /// produced by `layout_document` itself, and never stored on a cached
    /// `Layout`: hint state is as transient as focus/visited state, which
    /// also never becomes a `SpanKind` variant of its own (see this enum's
    /// doc comment).
    Hint,
}

/// One styled run of text within a laid-out line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaidSpan {
    pub text: String,
    pub kind: SpanKind,
}

/// One laid-out line — exactly one terminal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaidLine {
    pub spans: Vec<LaidSpan>,
}

impl LaidLine {
    /// The total display width of the line, measured the same way the layout
    /// measured it. The engine guarantees this never exceeds the width the
    /// layout was built for. Used by the width-invariant property tests.
    #[cfg(test)]
    pub fn width(&self, ambiguous_wide: bool) -> usize {
        self.spans
            .iter()
            .map(|s| display_width(&s.text, ambiguous_wide))
            .sum()
    }
}

/// Layout knobs a config file will eventually wire (FR-RD-9/FR-RD-10);
/// exposed now as `App` fields with the documented defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutOptions {
    /// Maximum line measure in cells (FR-RD-9, default 88). Content is capped
    /// at `min(available_width, measure)` and centered with a left pad when
    /// the terminal is wider.
    pub measure: u16,
    /// East-Asian-Ambiguous width (FR-RD-10, `ambiguous_width = 1|2`). When
    /// true, ambiguous-width characters (e.g. `·`, Greek letters) measure 2
    /// cells per the unicode-width CJK tables.
    pub ambiguous_wide: bool,
    /// PRD FR-ACS-6 (`ACCESSIBLE=1`): collapse tables to "Header: value"
    /// lists rather than drawing box-drawing grids. Part of `LayoutOptions`
    /// (and thus the L1 cache key) because it changes the laid-out lines.
    pub accessible: bool,
    /// PRD FR-RD-4's horizontal table scroll: how many columns to skip at the
    /// left of every horizontally-scrollable table in the article (the
    /// simplest coherent model — one shared offset shifts all wide tables'
    /// column windows uniformly, `[`/`]` in the reading view). It changes the
    /// laid-out lines, so it participates in the L1 cache key too: each scroll
    /// step is a cheap relayout, never a stale reuse.
    pub table_col_offset: u16,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            measure: 88,
            ambiguous_wide: false,
            accessible: false,
            table_col_offset: 0,
        }
    }
}

/// A fully laid-out document plus the line mappings the app needs. All
/// mappings are computed *from* the emitted lines, so there is one source of
/// line truth.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The available terminal width this layout was built for — the cache
    /// key (with `options`) the reading view compares against on resize.
    pub width: u16,
    pub options: LayoutOptions,
    pub lines: Vec<LaidLine>,
    /// The scroll target line for each block, in `doc.blocks` order: a
    /// heading's own text line, or a block's first line otherwise. Drives
    /// section jump and find-match scrolling.
    pub block_lines: Vec<usize>,
    /// The first line each link occurrence appears on, indexed identically to
    /// `doc::collect_links`, so cycling links can scroll the focused link
    /// into view.
    pub link_lines: Vec<usize>,
    /// The grapheme-column range `[start, end)` of each link occurrence's own
    /// text on its `link_lines` line — same units as [`MatchSpan`] (grapheme
    /// clusters from the line's own start, never display cells or bytes) and
    /// indexed identically to `link_lines`/`doc::collect_links`. A link that
    /// wraps across lines is only recorded here for its first line, matching
    /// `link_lines`. This is link-hint painting's (PRD FR-NV-1) only geometry
    /// dependency on the layout: it locates a link's own span within its line
    /// so a hint label can be spliced in without re-deriving column offsets
    /// from `lines` at paint time.
    pub link_cols: Vec<MatchSpan>,
    /// `continuation[i]` is true when `lines[i]` is a soft-wrapped
    /// continuation of the very same content run as `lines[i + 1]` — no
    /// block boundary (paragraph break, list-item edge, table row, ...)
    /// separates them, only a visual wrap. Length `lines.len() - 1` (empty
    /// for a one-line document). The only consumer is `find_matches`, which
    /// uses it to glue adjacent lines back together so a query straddling a
    /// wrap point is still found (PRD FR-NV-6b) — nothing else may rely on
    /// this reconstructing the original unwrapped text exactly, since a
    /// space dropped at the wrap is spliced back in heuristically (see
    /// `find_matches`'s doc comment).
    pub continuation: Vec<bool>,
}

/// Display width of a string, one grapheme cluster is measured as a unit.
/// `unicode-width` sums per-character widths, so this is exactly additive
/// over clusters — the invariant the no-overflow property test relies on.
pub fn display_width(s: &str, ambiguous_wide: bool) -> usize {
    if ambiguous_wide {
        UnicodeWidthStr::width_cjk(s)
    } else {
        UnicodeWidthStr::width(s)
    }
}

/// One grapheme cluster with everything the line breaker needs.
#[derive(Debug, Clone)]
struct Cluster {
    text: String,
    width: usize,
    kind: SpanKind,
    is_space: bool,
    is_newline: bool,
    cjk: bool,
    first_char: char,
    last_char: char,
}

/// True for scripts wrapped per-character (FR-RD-10): Han, kana, Hangul, and
/// the CJK punctuation/fullwidth blocks. A break is allowed on either side of
/// such a character, subject to the kinsoku rules below.
fn is_cjk(c: char) -> bool {
    let u = c as u32;
    (0x3000..=0x303F).contains(&u)      // CJK symbols and punctuation
        || (0x3040..=0x309F).contains(&u) // Hiragana
        || (0x30A0..=0x30FF).contains(&u) // Katakana
        || (0x31F0..=0x31FF).contains(&u) // Katakana phonetic extensions
        || (0x3400..=0x4DBF).contains(&u) // CJK Unified Ideographs Ext A
        || (0x4E00..=0x9FFF).contains(&u) // CJK Unified Ideographs
        || (0xF900..=0xFAFF).contains(&u) // CJK Compatibility Ideographs
        || (0xAC00..=0xD7A3).contains(&u) // Hangul syllables
        || (0x1100..=0x11FF).contains(&u) // Hangul Jamo
        || (0xFF00..=0xFFEF).contains(&u) // Halfwidth and Fullwidth forms
}

/// Kinsoku: characters forbidden at the *start* of a line — closing
/// punctuation and small kana. Kept deliberately small and documented; the
/// breaker refuses a CJK break that would place one of these first.
fn is_no_start(c: char) -> bool {
    matches!(
        c,
        // Closing punctuation (full- and half-width).
        '。' | '、' | '．' | '，' | '：' | '；' | '！' | '？' | '・'
            | '）' | '」' | '』' | '】' | '｝' | '〕' | '〉' | '》' | '〙' | '〗'
            | '!' | '?' | ')' | ']' | '}' | ',' | '.'
            // Small kana and iteration marks.
            | 'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'っ' | 'ゃ' | 'ゅ' | 'ょ' | 'ゎ'
            | 'ァ' | 'ィ' | 'ゥ' | 'ェ' | 'ォ' | 'ッ' | 'ャ' | 'ュ' | 'ョ' | 'ヮ'
            | 'ー' | '々' | 'ゝ' | 'ゞ' | 'ヽ' | 'ヾ'
    )
}

/// Kinsoku: characters forbidden at the *end* of a line — opening brackets.
fn is_no_end(c: char) -> bool {
    matches!(
        c,
        '（' | '「'
            | '『'
            | '【'
            | '｛'
            | '〔'
            | '〈'
            | '《'
            | '〖'
            | '〘'
            | '｢'
            | '('
            | '['
            | '{'
    )
}

fn make_cluster(text: &str, kind: SpanKind, ambiguous_wide: bool) -> Cluster {
    let is_newline = text == "\n";
    let is_space = !is_newline && !text.is_empty() && text.chars().all(char::is_whitespace);
    let first_char = text.chars().next().unwrap_or(' ');
    let last_char = text.chars().next_back().unwrap_or(' ');
    Cluster {
        width: if is_newline || is_space {
            1
        } else {
            display_width(text, ambiguous_wide)
        },
        kind,
        is_space,
        is_newline,
        cjk: is_cjk(first_char),
        first_char,
        last_char,
        text: text.to_string(),
    }
}

fn clusters_from_str(s: &str, kind: SpanKind, ambiguous_wide: bool) -> Vec<Cluster> {
    s.graphemes(true)
        .map(|g| make_cluster(g, kind.clone(), ambiguous_wide))
        .collect()
}

/// The semantic kind of an inline span in the given block context. Plain text
/// takes the block's body kind (e.g. `Quote` inside a blockquote); links are
/// numbered in document order so paint can resolve focus/visited state. Link
/// numbering must match `doc::collect_links` exactly.
fn span_kind(style: &SpanStyle, plain_kind: &SpanKind, link_counter: &mut usize) -> SpanKind {
    match style {
        SpanStyle::Link(_) => {
            let occ = *link_counter;
            *link_counter += 1;
            SpanKind::Link(occ)
        }
        SpanStyle::Bold => SpanKind::Bold,
        SpanStyle::Italic => SpanKind::Italic,
        SpanStyle::Superscript => SpanKind::Dim,
        SpanStyle::Plain => plain_kind.clone(),
    }
}

fn flatten_spans(
    spans: &[crate::doc::Span],
    plain_kind: SpanKind,
    link_counter: &mut usize,
    ambiguous_wide: bool,
) -> Vec<Cluster> {
    let mut out = Vec::new();
    for span in spans {
        let kind = span_kind(&span.style, &plain_kind, link_counter);
        out.extend(clusters_from_str(&span.text, kind, ambiguous_wide));
    }
    out
}

fn flatten_plain(spans: &[crate::doc::Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// A maximal run of clusters that never breaks internally, plus what follows
/// it. Pieces are only ever separated by a real break opportunity, so the
/// greedy filler can treat every boundary as breakable.
struct Piece {
    clusters: Vec<Cluster>,
    sep: Sep,
}

#[derive(PartialEq)]
enum Sep {
    /// A collapsible space run follows (rendered as one space mid-line,
    /// dropped at a line break). Carries the original space's semantic kind
    /// so a space *inside* a multi-word link stays part of the link — the
    /// focused-link highlight must not have holes.
    Space(SpanKind),
    /// A zero-width break follows (after a hyphen, or between CJK characters).
    Break,
    /// End of input.
    End,
}

impl Piece {
    fn width(&self) -> usize {
        self.clusters.iter().map(|c| c.width).sum()
    }
}

/// Whether a line break is allowed between adjacent non-space clusters `a`
/// and `b`. `a_is_piece_first` guards against orphaning a leading hyphen.
fn breakable(a: &Cluster, b: &Cluster, a_is_piece_first: bool) -> bool {
    // Spaced scripts: break after a hyphen (which stays on the prior line).
    if (a.last_char == '-' || a.last_char == '\u{2010}') && !a_is_piece_first {
        return true;
    }
    // CJK per-character breaking, with kinsoku prohibitions.
    if a.cjk || b.cjk {
        if is_no_start(b.first_char) {
            return false;
        }
        if is_no_end(a.last_char) {
            return false;
        }
        return true;
    }
    false
}

fn build_pieces(clusters: &[Cluster]) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut cur: Vec<Cluster> = Vec::new();
    let mut i = 0;
    while i < clusters.len() {
        if clusters[i].is_space {
            if !cur.is_empty() {
                pieces.push(Piece {
                    clusters: std::mem::take(&mut cur),
                    sep: Sep::Space(clusters[i].kind.clone()),
                });
            }
            while i < clusters.len() && clusters[i].is_space {
                i += 1;
            }
            continue;
        }
        let a_is_first = cur.is_empty();
        cur.push(clusters[i].clone());
        if i + 1 < clusters.len() && !clusters[i + 1].is_space {
            let a = &clusters[i];
            let b = &clusters[i + 1];
            if breakable(a, b, a_is_first) {
                pieces.push(Piece {
                    clusters: std::mem::take(&mut cur),
                    sep: Sep::Break,
                });
            }
        }
        i += 1;
    }
    if !cur.is_empty() {
        pieces.push(Piece {
            clusters: cur,
            sep: Sep::End,
        });
    }
    pieces
}

/// Greedily pack pieces into lines no wider than `avail`. A piece wider than
/// `avail` is hard-split at cluster boundaries so nothing ever overflows.
fn fill(pieces: Vec<Piece>, avail: usize) -> Vec<Vec<Cluster>> {
    let avail = avail.max(1);
    let mut lines: Vec<Vec<Cluster>> = Vec::new();
    let mut cur: Vec<Cluster> = Vec::new();
    let mut cur_w = 0usize;
    let mut pending_space: Option<SpanKind> = None;

    for piece in pieces {
        let pw = piece.width();
        let sep_w = usize::from(pending_space.is_some() && !cur.is_empty());
        if !cur.is_empty() && cur_w + sep_w + pw > avail {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            pending_space = None;
        }
        if let Some(kind) = pending_space.take()
            && !cur.is_empty()
        {
            cur.push(make_cluster(" ", kind, false));
            cur_w += 1;
        }

        if cur_w + pw <= avail {
            cur.extend(piece.clusters);
            cur_w += pw;
        } else {
            // The piece itself is wider than a line: hard-split it.
            for cl in piece.clusters {
                if !cur.is_empty() && cur_w + cl.width > avail {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur_w += cl.width;
                cur.push(cl);
            }
        }
        if let Sep::Space(kind) = piece.sep {
            pending_space = Some(kind);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Wrap a run of clusters (with hard `\n` breaks honored) into visual lines.
fn wrap_content(clusters: Vec<Cluster>, avail: usize) -> Vec<Vec<Cluster>> {
    let mut out = Vec::new();
    let mut sub: Vec<Cluster> = Vec::new();
    for c in clusters {
        if c.is_newline {
            out.extend(fill(build_pieces(&sub), avail));
            sub = Vec::new();
        } else {
            sub.push(c);
        }
    }
    out.extend(fill(build_pieces(&sub), avail));
    out
}

/// Hard-wrap clusters at `avail` with no break logic and no whitespace
/// collapsing — used for code, where spacing is significant.
fn chunk_by_width(clusters: Vec<Cluster>, avail: usize) -> Vec<Vec<Cluster>> {
    let avail = avail.max(1);
    let mut lines: Vec<Vec<Cluster>> = Vec::new();
    let mut cur: Vec<Cluster> = Vec::new();
    let mut cur_w = 0usize;
    for c in clusters {
        if !cur.is_empty() && cur_w + c.width > avail {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur_w += c.width;
        cur.push(c);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn truncate_to_width(s: &str, width: usize, ambiguous_wide: bool) -> String {
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = display_width(g, ambiguous_wide);
        if w + gw > width {
            break;
        }
        w += gw;
        out.push_str(g);
    }
    out
}

/// Build a laid-out line from a left pad (for centering), an optional
/// structural prefix (list bullet, gutter), and the wrapped content. Adjacent
/// content clusters of the same kind are coalesced into one span, but content
/// is never merged into the pad or prefix — those stay distinct, stable spans.
fn finalize(pad_width: usize, prefix: &[LaidSpan], content: &[Cluster]) -> LaidLine {
    let mut spans: Vec<LaidSpan> = Vec::new();
    if pad_width > 0 {
        spans.push(LaidSpan {
            text: " ".repeat(pad_width),
            kind: SpanKind::Plain,
        });
    }
    spans.extend(prefix.iter().cloned());
    let mut content_spans: Vec<LaidSpan> = Vec::new();
    for c in content {
        match content_spans.last_mut() {
            Some(last) if last.kind == c.kind => last.text.push_str(&c.text),
            _ => content_spans.push(LaidSpan {
                text: c.text.clone(),
                kind: c.kind.clone(),
            }),
        }
    }
    spans.extend(content_spans);
    LaidLine { spans }
}

/// A helper carrying the emit state through the per-block builders.
struct Emitter<'a> {
    lines: &'a mut Vec<LaidLine>,
    /// Parallel to `lines`, one entry shorter (see `Layout::continuation`):
    /// `push_line`'s `continues_prev` argument for every line after the
    /// first.
    continuation: &'a mut Vec<bool>,
    pad_width: usize,
    content_width: usize,
    ambiguous_wide: bool,
}

impl Emitter<'_> {
    /// The single point every laid-out line passes through, so
    /// `continuation` always stays exactly one shorter than `lines`.
    /// `continues_prev` records whether this line is a soft-wrap
    /// continuation of the line just pushed (see `Layout::continuation`) —
    /// meaningless (and ignored) for the very first line of the document.
    fn push_line(&mut self, line: LaidLine, continues_prev: bool) {
        if !self.lines.is_empty() {
            self.continuation.push(continues_prev);
        }
        self.lines.push(line);
    }

    fn blank(&mut self) {
        self.push_line(finalize(self.pad_width, &[], &[]), false);
    }

    /// Emit a block whose content wraps under an optional hanging prefix
    /// (list bullet, blockquote gutter). Returns nothing; the caller records
    /// the anchor line before calling.
    fn emit_wrapped(
        &mut self,
        content: Vec<Cluster>,
        first_prefix: Vec<LaidSpan>,
        cont_prefix: Vec<LaidSpan>,
    ) {
        let prefix_width: usize = first_prefix
            .iter()
            .map(|s| display_width(&s.text, self.ambiguous_wide))
            .sum();
        // Drop the prefix entirely if it wouldn't leave room for content —
        // guarantees the emitted line never exceeds the content width.
        let (first_prefix, cont_prefix, prefix_width) = if prefix_width + 1 > self.content_width {
            (Vec::new(), Vec::new(), 0)
        } else {
            (first_prefix, cont_prefix, prefix_width)
        };
        let avail = self.content_width - prefix_width;
        let wrapped = wrap_content(content, avail);
        if wrapped.is_empty() {
            self.push_line(finalize(self.pad_width, &first_prefix, &[]), false);
            return;
        }
        for (i, line) in wrapped.iter().enumerate() {
            let prefix = if i == 0 { &first_prefix } else { &cont_prefix };
            self.push_line(finalize(self.pad_width, prefix, line), i > 0);
        }
    }

    fn emit_plain_wrapped(&mut self, content: Vec<Cluster>) {
        self.emit_wrapped(content, Vec::new(), Vec::new());
    }

    /// Emit one document block, pushing exactly one `block_lines` scroll
    /// anchor for it (so `block_lines` stays index-aligned with `doc.blocks`,
    /// which section jumps depend on). `link_counter` numbers link
    /// occurrences in document order — shared across every block so the
    /// numbering matches `doc::collect_links`. Factored out of the main loop
    /// so the infobox float can reuse it for the lead blocks it lays out
    /// beside the card.
    fn emit_block(
        &mut self,
        block: &Block,
        block_lines: &mut Vec<usize>,
        link_counter: &mut usize,
        accessible: bool,
        table_offset: usize,
    ) {
        let aw = self.ambiguous_wide;
        match block {
            Block::Heading { level, spans } => {
                self.blank();
                let anchor = self.lines.len();
                let text = flatten_plain(spans);
                self.emit_plain_wrapped(clusters_from_str(&text, SpanKind::Heading(*level), aw));
                block_lines.push(anchor);
            }
            Block::Paragraph(spans) => {
                let anchor = self.lines.len();
                let content = flatten_spans(spans, SpanKind::Plain, link_counter, aw);
                self.emit_plain_wrapped(content);
                self.blank();
                block_lines.push(anchor);
            }
            Block::ListItem {
                ordered,
                index,
                depth,
                spans,
            } => {
                let anchor = self.lines.len();
                let indent = "  ".repeat(*depth as usize);
                let bullet = if *ordered {
                    format!("{index}.")
                } else {
                    "•".to_string()
                };
                let prefix_text = format!("{indent}{bullet} ");
                let prefix_w = display_width(&prefix_text, aw);
                let first_prefix = vec![LaidSpan {
                    text: prefix_text,
                    kind: SpanKind::Plain,
                }];
                let cont_prefix = vec![LaidSpan {
                    text: " ".repeat(prefix_w),
                    kind: SpanKind::Plain,
                }];
                let content = flatten_spans(spans, SpanKind::Plain, link_counter, aw);
                self.emit_wrapped(content, first_prefix, cont_prefix);
                block_lines.push(anchor);
            }
            Block::Blockquote(spans) => {
                let anchor = self.lines.len();
                let gutter = vec![LaidSpan {
                    text: "▌ ".to_string(),
                    kind: SpanKind::Dim,
                }];
                let content = flatten_spans(spans, SpanKind::Quote, link_counter, aw);
                self.emit_wrapped(content, gutter.clone(), gutter);
                self.blank();
                block_lines.push(anchor);
            }
            Block::Code(text) => {
                let anchor = self.lines.len();
                let gutter = "    ";
                let avail = self.content_width.saturating_sub(4).max(1);
                for src in text.lines() {
                    let chunks = chunk_by_width(clusters_from_str(src, SpanKind::Code, aw), avail);
                    if chunks.is_empty() {
                        self.push_line(
                            finalize(
                                self.pad_width,
                                &[LaidSpan {
                                    text: gutter.to_string(),
                                    kind: SpanKind::Code,
                                }],
                                &[],
                            ),
                            false,
                        );
                    }
                    for (i, chunk) in chunks.into_iter().enumerate() {
                        self.push_line(
                            finalize(
                                self.pad_width,
                                &[LaidSpan {
                                    text: gutter.to_string(),
                                    kind: SpanKind::Code,
                                }],
                                &chunk,
                            ),
                            i > 0,
                        );
                    }
                }
                self.blank();
                block_lines.push(anchor);
            }
            Block::Rule => {
                let anchor = self.lines.len();
                let dash_w = display_width("─", aw).max(1);
                let n = self.content_width.min(40) / dash_w;
                self.push_line(
                    finalize(
                        self.pad_width,
                        &[LaidSpan {
                            text: "─".repeat(n),
                            kind: SpanKind::Dim,
                        }],
                        &[],
                    ),
                    false,
                );
                block_lines.push(anchor);
            }
            Block::Table(table) => {
                let anchor = self.lines.len();
                self.emit_table(table, accessible, table_offset);
                self.blank();
                block_lines.push(anchor);
            }
            Block::Infobox(rows) => {
                let anchor = self.lines.len();
                self.emit_infobox_topblock(rows);
                self.blank();
                block_lines.push(anchor);
            }
            Block::Image(alt) => {
                let anchor = self.lines.len();
                self.emit_plain_wrapped(clusters_from_str(
                    &format!("[image: {alt}]"),
                    SpanKind::Image,
                    aw,
                ));
                self.blank();
                block_lines.push(anchor);
            }
        }
    }

    /// Emit a table (PRD FR-RD-4): a box-drawing grid (with per-column
    /// sizing, cell wrapping, and a horizontally-scrollable column window for
    /// wide tables) or, when accessible/too narrow, its collapse-to-list
    /// form. See [`plan_table`] for the collapse-vs-scroll rule.
    fn emit_table(&mut self, table: &crate::doc::Table, accessible: bool, table_offset: usize) {
        let aw = self.ambiguous_wide;
        let v = display_width("│", aw).max(1);
        let natural = natural_col_widths(table, aw);
        match plan_table(&natural, self.content_width, accessible, table_offset, v) {
            TablePlan::Collapse => {
                for line in table.to_list_lines() {
                    if line.is_empty() {
                        self.push_line(finalize(self.pad_width, &[], &[]), false);
                        continue;
                    }
                    let clusters = clusters_from_str(&line, SpanKind::Table, aw);
                    for wl in wrap_content(clusters, self.content_width.max(1)) {
                        self.push_line(finalize(self.pad_width, &[], &wl), false);
                    }
                }
            }
            TablePlan::Grid { first_col, widths } => {
                for line in render_grid_lines(table, first_col, &widths, aw) {
                    let safe = truncate_to_width(&line, self.content_width, aw);
                    let clusters = clusters_from_str(&safe, SpanKind::Table, aw);
                    self.push_line(finalize(self.pad_width, &[], &clusters), false);
                }
            }
        }
    }

    /// Emit an infobox as a top-block card (PRD FR-RD-5, the compact/minimal
    /// tiers and the no-float fallback). Capped at [`INFOBOX_CARD_WIDTH`] so
    /// it reads as a card rather than a full-width banner.
    fn emit_infobox_topblock(&mut self, rows: &[(String, String)]) {
        let aw = self.ambiguous_wide;
        let min_w = 2 * display_width("│", aw).max(1) + 3;
        let box_w = self.content_width.min(INFOBOX_CARD_WIDTH).max(min_w);
        for line in infobox_card_lines(rows, box_w, aw) {
            let safe = truncate_to_width(&line, self.content_width, aw);
            let clusters = clusters_from_str(&safe, SpanKind::Infobox, aw);
            self.push_line(finalize(self.pad_width, &[], &clusters), false);
        }
    }

    /// Emit a right-floated infobox card (PRD FR-RD-5, the full tier): the
    /// card is drawn on the right and the lead blocks flow in the remaining
    /// left column beside it, merged row-by-row. Documented approximations:
    /// the whole lead is laid at the reduced left width (so the text column
    /// doesn't re-widen below the card), and merged rows are never glued for
    /// in-page find (each is searched independently). One `block_lines`
    /// anchor is pushed for the infobox and one for each lead block, in order.
    #[allow(clippy::too_many_arguments)]
    fn emit_infobox_float(
        &mut self,
        block_lines: &mut Vec<usize>,
        link_counter: &mut usize,
        rows: &[(String, String)],
        lead_blocks: &[Block],
        accessible: bool,
        table_offset: usize,
    ) {
        let aw = self.ambiguous_wide;
        let gap = INFOBOX_FLOAT_GAP;
        let box_w = INFOBOX_CARD_WIDTH.min(self.content_width.saturating_sub(gap + MIN_LEAD_WIDTH));
        let left_w = self.content_width.saturating_sub(box_w + gap).max(1);
        let box_lines = infobox_card_lines(rows, box_w, aw);

        let mut temp_lines: Vec<LaidLine> = Vec::new();
        let mut temp_cont: Vec<bool> = Vec::new();
        let mut temp_block_lines: Vec<usize> = Vec::new();
        {
            let mut tem = Emitter {
                lines: &mut temp_lines,
                continuation: &mut temp_cont,
                pad_width: 0,
                content_width: left_w,
                ambiguous_wide: aw,
            };
            for b in lead_blocks {
                tem.emit_block(
                    b,
                    &mut temp_block_lines,
                    link_counter,
                    accessible,
                    table_offset,
                );
            }
        }

        let base = self.lines.len();
        block_lines.push(base); // the infobox's own anchor = the card's first row

        let rows_total = temp_lines.len().max(box_lines.len());
        for j in 0..rows_total {
            let mut spans: Vec<LaidSpan> = Vec::new();
            if self.pad_width > 0 {
                spans.push(LaidSpan {
                    text: " ".repeat(self.pad_width),
                    kind: SpanKind::Plain,
                });
            }
            let left_used: usize = if let Some(tl) = temp_lines.get(j) {
                spans.extend(tl.spans.iter().cloned());
                tl.spans.iter().map(|s| display_width(&s.text, aw)).sum()
            } else {
                0
            };
            if let Some(bl) = box_lines.get(j) {
                let fill = left_w.saturating_sub(left_used) + gap;
                spans.push(LaidSpan {
                    text: " ".repeat(fill),
                    kind: SpanKind::Plain,
                });
                spans.push(LaidSpan {
                    text: bl.clone(),
                    kind: SpanKind::Infobox,
                });
            }
            self.push_line(LaidLine { spans }, false);
        }
        for anchor in temp_block_lines {
            block_lines.push(base + anchor);
        }
        self.blank();
    }
}

// -- Terminal-size degradation tiers (PRD §6.3) ---------------------------

/// The four terminal-size tiers of PRD §6.3, in a single documented,
/// table-tested pure function of the terminal's cell dimensions. The reading
/// UI keys the infobox placement (`Full` floats it beside the lead; the
/// narrower tiers stack it on top) and the "terminal too small" screen off
/// this — never off ad-hoc width checks scattered across the draw code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeTier {
    /// Below the terminal floor (PRD §8: any VT100-ish terminal ≥ 60×16):
    /// the reader shows a dedicated "terminal too small" screen instead of
    /// the article.
    Floor,
    /// `< 80` wide: single column, no infobox float (top block or inline).
    Minimal,
    /// `80..=99` wide: compact — infobox as a top block, TOC still available.
    Compact,
    /// `>= 100` wide: full layout — the infobox floats to the right of the
    /// lead section (`FR-RD-5`).
    Full,
}

/// PRD §8's terminal floor.
pub const FLOOR_MIN_WIDTH: u16 = 60;
pub const FLOOR_MIN_HEIGHT: u16 = 16;
/// PRD §6.3's tier width boundaries.
pub const COMPACT_TIER_MIN_WIDTH: u16 = 80;
pub const FULL_TIER_MIN_WIDTH: u16 = 100;

/// The tier for a terminal `width × height` cells (PRD §6.3), the single
/// source of truth for size-driven layout decisions. Height only ever
/// matters at the floor: the `< 60 wide OR < 16 tall` check comes first, so
/// a wide-but-short terminal is still `Floor` (there isn't room to read).
pub fn size_tier(width: u16, height: u16) -> SizeTier {
    if width < FLOOR_MIN_WIDTH || height < FLOOR_MIN_HEIGHT {
        SizeTier::Floor
    } else if width < COMPACT_TIER_MIN_WIDTH {
        SizeTier::Minimal
    } else if width < FULL_TIER_MIN_WIDTH {
        SizeTier::Compact
    } else {
        SizeTier::Full
    }
}

// -- Table rendering (PRD FR-RD-4) ----------------------------------------

/// Minimum readable column width when a table is shrunk or scrolled: if the
/// available width cannot give even one column this many cells, the table
/// collapses to a list instead of being drawn as a grid (the documented
/// collapse-vs-scroll rule — see [`plan_table`]).
const MIN_COL_WIDTH: usize = 8;
/// Per-column cap so one enormous cell can't make a column swallow the whole
/// table; content past this wraps within the column, growing row height.
const MAX_COL_WIDTH: usize = 40;

/// How a table will be rendered at a given width (PRD FR-RD-4), decided by
/// [`plan_table`]. `Grid` names the visible column window (`first_col` plus
/// the width of each visible column, in cells); `Collapse` means fall back
/// to the "Header: value" list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TablePlan {
    Collapse,
    Grid {
        first_col: usize,
        widths: Vec<usize>,
    },
}

/// The natural (pre-clamp) display width of each column: the widest cell in
/// it, capped at [`MAX_COL_WIDTH`], and never below 1.
fn natural_col_widths(table: &crate::doc::Table, aw: bool) -> Vec<usize> {
    let cols = table.cols();
    let mut widths = vec![1usize; cols];
    for row in &table.rows {
        for (c, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(c) {
                *w = (*w).max(display_width(&cell.text, aw).min(MAX_COL_WIDTH));
            }
        }
    }
    widths
}

/// Total grid width in cells for a set of visible column widths, given the
/// vertical-border glyph width `v` (2 under `ambiguous_wide` for the
/// East-Asian-Ambiguous box-drawing glyphs). A cell renders as
/// `│ {content} ` and the row closes with a final `│`, so each column costs
/// `content + 2 spaces + one border`, plus one leading border.
fn grid_width(widths: &[usize], v: usize) -> usize {
    v + widths.iter().map(|w| w + 2 + v).sum::<usize>()
}

/// PRD FR-RD-4's collapse-vs-scroll decision, as a pure function so the rule
/// is table-testable. The documented rule:
///   * **Accessible mode** (FR-ACS-6) always collapses.
///   * If the width can't fit even one column at [`MIN_COL_WIDTH`] → collapse.
///   * Else if all columns fit (after shrinking wide ones toward the width) →
///     a grid showing every column.
///   * Else → a grid showing a horizontally-scrollable window of columns
///     starting at `offset` (PRD FR-RD-4's horizontal scroll for wide tables).
fn plan_table(
    natural: &[usize],
    avail: usize,
    accessible: bool,
    offset: usize,
    v: usize,
) -> TablePlan {
    let n = natural.len();
    if n == 0 || accessible {
        return TablePlan::Collapse;
    }
    // Can't fit even one column at the minimum readable width → collapse.
    if avail < grid_width(&[MIN_COL_WIDTH], v) {
        return TablePlan::Collapse;
    }
    // Fit-all path: is there room for every column, each at least as wide as
    // min(its natural width, MIN_COL_WIDTH)?
    let per_col_overhead = 2 + v;
    let budget = avail.saturating_sub(v + n * per_col_overhead);
    let min_needed: usize = natural.iter().map(|&x| x.min(MIN_COL_WIDTH)).sum();
    if min_needed <= budget {
        return TablePlan::Grid {
            first_col: 0,
            widths: shrink_to_budget(natural, budget),
        };
    }
    // Scroll path: a contiguous window of columns from `offset`.
    let first = offset.min(n - 1);
    let mut widths = Vec::new();
    let mut used = v;
    for &nat in &natural[first..] {
        let need = nat + per_col_overhead;
        if used + need <= avail {
            widths.push(nat);
            used += need;
        } else if widths.is_empty() {
            // Shrink the first (offset) column to fill the row on its own.
            let w = avail.saturating_sub(2 + 2 * v);
            if w >= MIN_COL_WIDTH {
                widths.push(w);
            }
            break;
        } else {
            break;
        }
    }
    if widths.is_empty() {
        TablePlan::Collapse
    } else {
        TablePlan::Grid {
            first_col: first,
            widths,
        }
    }
}

/// Water-fill the columns to fit `budget` total content cells: raise a shared
/// cap from `MIN_COL_WIDTH` toward `MAX_COL_WIDTH` as far as the budget
/// allows, so short columns keep their natural width and only genuinely wide
/// ones get clamped (and then wrap). Precondition (`plan_table` guarantees
/// it): `Σ min(natural, MIN_COL_WIDTH) ≤ budget`.
fn shrink_to_budget(natural: &[usize], budget: usize) -> Vec<usize> {
    let mut cap = MAX_COL_WIDTH;
    while cap > 1 {
        let sum: usize = natural.iter().map(|&x| x.min(cap)).sum();
        if sum <= budget {
            break;
        }
        cap -= 1;
    }
    natural.iter().map(|&x| x.min(cap)).collect()
}

/// Truncate to `w` cells and right-pad with spaces to exactly `w` cells.
fn pad_to_width(s: &str, w: usize, aw: bool) -> String {
    let truncated = truncate_to_width(s, w, aw);
    let used = display_width(&truncated, aw);
    format!("{truncated}{}", " ".repeat(w.saturating_sub(used)))
}

/// Wrap `text` to `w` cells using the same word/CJK line-breaker the body
/// text uses (so a cell that overflows its column wraps and grows the row's
/// height, never overflows). Returns one string per visual line.
fn wrap_cell_text(text: &str, w: usize, aw: bool) -> Vec<String> {
    let clusters = clusters_from_str(text, SpanKind::Table, aw);
    let wrapped = wrap_content(clusters, w.max(1));
    let mut out: Vec<String> = wrapped
        .into_iter()
        .map(|line| line.iter().map(|c| c.text.as_str()).collect())
        .collect();
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// One horizontal grid border line (`┌─┬─┐`, `├─┼─┤`, or `└─┴─┘`) spanning
/// the visible columns, cell-accurate under `ambiguous_wide`.
fn grid_border(widths: &[usize], left: &str, mid: &str, right: &str, aw: bool) -> String {
    let dash = "─";
    let dw = display_width(dash, aw).max(1);
    let mut s = String::from(left);
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            s.push_str(mid);
        }
        let cells = w + 2; // the cell's own two padding spaces
        let d = cells / dw;
        s.push_str(&dash.repeat(d));
        let rem = cells - d * dw;
        if rem > 0 {
            s.push_str(&" ".repeat(rem));
        }
    }
    s.push_str(right);
    s
}

/// Render `table`'s visible column window as box-drawing grid lines
/// (`┌┬┐├┼┤└┴┘─│`). Each cell is wrapped to its column width, so a grid row
/// can occupy several screen rows; a header row (any `<th>`) is separated
/// from the body by a `├┼┤` rule.
fn render_grid_lines(
    table: &crate::doc::Table,
    first_col: usize,
    widths: &[usize],
    aw: bool,
) -> Vec<String> {
    let cols: Vec<usize> = (first_col..first_col + widths.len()).collect();
    let mut out = Vec::new();
    out.push(grid_border(widths, "┌", "┬", "┐", aw));
    let header = table.has_header_row();
    for (ri, row) in table.rows.iter().enumerate() {
        let cell_lines: Vec<Vec<String>> = cols
            .iter()
            .enumerate()
            .map(|(wi, &c)| {
                let text = row.get(c).map(|cell| cell.text.as_str()).unwrap_or("");
                wrap_cell_text(text, widths[wi], aw)
            })
            .collect();
        let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1);
        for h in 0..height {
            let mut line = String::from("│");
            for (wi, cl) in cell_lines.iter().enumerate() {
                let content = cl.get(h).map(String::as_str).unwrap_or("");
                line.push(' ');
                line.push_str(&pad_to_width(content, widths[wi], aw));
                line.push(' ');
                line.push('│');
            }
            out.push(line);
        }
        if ri == 0 && header && table.rows.len() > 1 {
            out.push(grid_border(widths, "├", "┼", "┤", aw));
        }
    }
    out.push(grid_border(widths, "└", "┴", "┘", aw));
    if table.truncated {
        out.push("… (table truncated)".to_string());
    }
    out
}

// -- Infobox card rendering (PRD FR-RD-5) ---------------------------------

/// Card width bounds (cells). The float target is `INFOBOX_CARD_WIDTH`; a
/// top-block card is capped at it too so it reads as a card, not a banner.
const INFOBOX_CARD_WIDTH: usize = 40;
/// Minimum lead-text column kept to the left of a floated infobox; below this
/// the float is abandoned for a top block (documented in `layout_document`).
const MIN_LEAD_WIDTH: usize = 30;
/// Gap in cells between the floated infobox and the lead text to its left.
const INFOBOX_FLOAT_GAP: usize = 2;

/// One bordered content line of an infobox card: `│ {content} │`, the content
/// truncated/padded so the whole line is exactly `box_w` cells (cell-accurate
/// under `ambiguous_wide`, where the border glyph is width 2).
fn card_content_line(content: &str, box_w: usize, aw: bool) -> String {
    let bar = "│";
    let bw = display_width(bar, aw).max(1);
    let inner = box_w.saturating_sub(2 * bw + 2);
    format!("{bar} {} {bar}", pad_to_width(content, inner, aw))
}

/// One horizontal card border line (`┌─┐`, `├─┤`, or `└─┘`) exactly `box_w`
/// cells wide.
fn card_border_line(left: &str, right: &str, box_w: usize, aw: bool) -> String {
    let cw = display_width(left, aw).max(1);
    let dash = "─";
    let dw = display_width(dash, aw).max(1);
    let cells = box_w.saturating_sub(2 * cw);
    let d = cells / dw;
    let rem = cells - d * dw;
    format!("{left}{}{}{right}", dash.repeat(d), " ".repeat(rem))
}

/// Render an infobox's (label, value) rows as a boxed card `box_w` cells wide
/// (PRD FR-RD-5): a top title border, an optional centered-ish title row (the
/// leading empty-label row), a `├─┤` rule under it, then `Label: value` rows
/// with wrapped values, closed by a bottom border. Returns the lines as
/// strings (all painted in the `Infobox` slot by the caller).
fn infobox_card_lines(rows: &[(String, String)], box_w: usize, aw: bool) -> Vec<String> {
    let bar = "│";
    let bw = display_width(bar, aw).max(1);
    let inner = box_w.saturating_sub(2 * bw + 2).max(1);
    let mut out = vec![card_border_line("┌", "┐", box_w, aw)];
    for (i, (label, value)) in rows.iter().enumerate() {
        let text = if label.is_empty() {
            value.clone()
        } else {
            format!("{label}: {value}")
        };
        for wl in wrap_cell_text(&text, inner, aw) {
            out.push(card_content_line(&wl, box_w, aw));
        }
        // A leading title row (empty label) gets a rule under it.
        if i == 0 && label.is_empty() && rows.len() > 1 {
            out.push(card_border_line("├", "┤", box_w, aw));
        }
    }
    out.push(card_border_line("└", "┘", box_w, aw));
    out
}

/// Lay `doc` out for a terminal `width` cells wide. See the module docs for
/// the guarantees this upholds.
pub fn layout_document(doc: &Document, width: u16, options: LayoutOptions) -> Layout {
    let available = (width.max(1)) as usize;
    let content_width = available.min((options.measure.max(1)) as usize);
    let pad_width = available.saturating_sub(content_width) / 2;
    let aw = options.ambiguous_wide;

    let mut lines: Vec<LaidLine> = Vec::new();
    let mut continuation: Vec<bool> = Vec::new();
    let mut block_lines: Vec<usize> = Vec::with_capacity(doc.blocks.len());
    let mut link_counter = 0usize;

    // PRD §6.3/FR-RD-5: on the Full tier (≥ 100 cols) the first infobox
    // floats to the right of the lead section, if there's room for a readable
    // card plus a `MIN_LEAD_WIDTH` text column beside it; otherwise it renders
    // as a top-block card (Compact/Minimal, or a too-narrow content column).
    let float_infobox =
        width >= FULL_TIER_MIN_WIDTH && content_width >= 24 + INFOBOX_FLOAT_GAP + MIN_LEAD_WIDTH;
    let infobox_idx = if float_infobox {
        doc.blocks
            .iter()
            .position(|b| matches!(b, Block::Infobox(_)))
    } else {
        None
    };
    // The lead section floated beside the card runs from just after the
    // infobox to the first heading (or the end of the article).
    let lead_end = infobox_idx.map(|idx| {
        doc.blocks[idx + 1..]
            .iter()
            .position(|b| matches!(b, Block::Heading { .. }))
            .map(|off| idx + 1 + off)
            .unwrap_or(doc.blocks.len())
    });

    {
        let mut em = Emitter {
            lines: &mut lines,
            continuation: &mut continuation,
            pad_width,
            content_width,
            ambiguous_wide: aw,
        };
        // Title, then a blank line — mirrors the previous renderer's header.
        em.emit_plain_wrapped(clusters_from_str(&doc.title, SpanKind::Title, aw));
        em.blank();

        let mut i = 0;
        while i < doc.blocks.len() {
            if Some(i) == infobox_idx
                && let (Some(idx), Some(end)) = (infobox_idx, lead_end)
                && let Block::Infobox(rows) = &doc.blocks[idx]
            {
                em.emit_infobox_float(
                    &mut block_lines,
                    &mut link_counter,
                    rows,
                    &doc.blocks[idx + 1..end],
                    options.accessible,
                    options.table_col_offset as usize,
                );
                i = end;
                continue;
            }
            em.emit_block(
                &doc.blocks[i],
                &mut block_lines,
                &mut link_counter,
                options.accessible,
                options.table_col_offset as usize,
            );
            i += 1;
        }
    }

    // Map each link occurrence to the first line it appears on, and the
    // grapheme-column range its own span occupies on that line. Occurrence
    // indices were assigned in document order (== collect_links order) during
    // flattening, so this fills every slot.
    let mut link_lines = vec![0usize; link_counter];
    let mut link_cols = vec![MatchSpan { start: 0, end: 0 }; link_counter];
    let mut seen = vec![false; link_counter];
    for (i, line) in lines.iter().enumerate() {
        let mut col = 0usize;
        for span in &line.spans {
            let span_graphemes = span.text.graphemes(true).count();
            if let SpanKind::Link(occ) = span.kind
                && occ < link_counter
                && !seen[occ]
            {
                seen[occ] = true;
                link_lines[occ] = i;
                link_cols[occ] = MatchSpan {
                    start: col,
                    end: col + span_graphemes,
                };
            }
            col += span_graphemes;
        }
    }

    Layout {
        width,
        options,
        lines,
        block_lines,
        link_lines,
        link_cols,
        continuation,
    }
}

/// PRD FR-NV-6's smart-case rule for in-page find: an all-lowercase query
/// (including one with no letters at all — punctuation, digits) matches
/// case-insensitively; a query with *any* uppercase letter matches exact
/// case only. Same rule real editors use (vim's `smartcase`, most browser
/// finders): typing a capital is read as intentional.
pub fn is_case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

/// One in-page-find match's grapheme-index range `[start, end)` on a single
/// rendered line — counted in grapheme clusters from the line's own start
/// (as `LaidLine::spans` concatenate), never bytes or code points, so a
/// caller can never slice inside a combining-mark cluster or a ZWJ sequence
/// (the same invariant the layout engine itself keeps, see the module
/// docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchSpan {
    pub start: usize,
    pub end: usize,
}

/// One occurrence of the find query, as the one or more per-line pieces a
/// visual line wrap split it into. `(line_index, MatchSpan)` per piece, top-
/// to-bottom. Almost always a single piece; exactly two when the query
/// straddled a wrap point (PRD FR-NV-6b) — never collapsed back into "one
/// entry per line" upstream of this type, because that would double-count
/// a single occurrence as two for the match counter and n/N cycling, and
/// only highlight half of it as "current."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub pieces: Vec<(usize, MatchSpan)>,
}

/// A laid-out line's own text, exactly as painted (pad/prefix included) —
/// concatenating `LaidSpan`s never splits a cluster since each span's text
/// is already whole clusters.
fn laid_line_text(line: &LaidLine) -> String {
    line.spans.iter().map(|s| s.text.as_str()).collect()
}

/// Finds every occurrence of `query` across laid-out `lines`, smart-case
/// (`is_case_sensitive`), grapheme-safe throughout, in reading order.
///
/// A run of lines chained by `continuation` (soft wraps within one content
/// block — see `Layout::continuation`) is searched as a single joined
/// string, so a query that happens to straddle a visual line break is still
/// found — as one `Occurrence` with two pieces (one per line it touches),
/// not two separate occurrences, so the match counter and "which one is
/// current" stay meaningful. The join glues in a single space at a wrap
/// point that dropped one (ordinary word-wrap) but nothing at all between
/// two CJK characters, which never had a space there to begin with
/// (FR-RD-10) — determined by inspecting the two characters immediately
/// either side of the break, not by recording the original separator, so
/// the rare case of an unbreakable non-CJK token that was hard-split purely
/// for width (no separator ever existed there either) is spliced with a
/// phantom space too. That's a narrow, documented miss (a query landing
/// exactly on such a cut goes unfound), not a wrong highlight — nothing is
/// ever painted that isn't a real, correctly-cased occurrence of `query`.
pub fn find_matches(lines: &[LaidLine], continuation: &[bool], query: &str) -> Vec<Occurrence> {
    let mut out: Vec<Occurrence> = Vec::new();
    if query.is_empty() || lines.is_empty() {
        return out;
    }
    let case_sensitive = is_case_sensitive(query);
    let normalize = |g: &str| -> String {
        if case_sensitive {
            g.to_string()
        } else {
            g.to_lowercase()
        }
    };
    let q_graphemes: Vec<String> = query.graphemes(true).map(&normalize).collect();
    if q_graphemes.is_empty() {
        return out;
    }

    let mut i = 0usize;
    while i < lines.len() {
        let mut end = i;
        while end + 1 < lines.len() && continuation.get(end).copied().unwrap_or(false) {
            end += 1;
        }
        find_in_run(lines, i, end, &q_graphemes, &normalize, &mut out);
        i = end + 1;
    }
    out
}

/// Searches one continuation-chained run of lines (`start..=end`) and
/// appends every hit to `out`, in left-to-right order within the run — runs
/// themselves are processed top-to-bottom by `find_matches`, so `out` stays
/// in overall reading order throughout. See `find_matches` for the glue
/// rule.
fn find_in_run(
    lines: &[LaidLine],
    start: usize,
    end: usize,
    q_graphemes: &[String],
    normalize: &impl Fn(&str) -> String,
    out: &mut Vec<Occurrence>,
) {
    // `origin[k]` is which (line, column) grapheme `graphemes[k]` came from;
    // `None` marks a synthetic glue grapheme (a spliced-in dropped space)
    // that exists on no rendered line and must never itself be reported as
    // part of a match.
    let mut graphemes: Vec<String> = Vec::new();
    let mut origin: Vec<Option<(usize, usize)>> = Vec::new();

    for li in start..=end {
        let text = laid_line_text(&lines[li]);
        for (col, g) in text.graphemes(true).enumerate() {
            graphemes.push(normalize(g));
            origin.push(Some((li, col)));
        }
        if li < end {
            let next_text = laid_line_text(&lines[li + 1]);
            let glue_is_space = matches!(
                (text.chars().next_back(), next_text.chars().next()),
                (Some(a), Some(b)) if !is_cjk(a) && !is_cjk(b)
            );
            if glue_is_space {
                graphemes.push(normalize(" "));
                origin.push(None);
            }
        }
    }

    let q_len = q_graphemes.len();
    if graphemes.len() < q_len {
        return;
    }
    let mut pos = 0usize;
    while pos + q_len <= graphemes.len() {
        if graphemes[pos..pos + q_len] == q_graphemes[..] {
            if let Some(occurrence) = occurrence_from_origins(&origin[pos..pos + q_len]) {
                out.push(occurrence);
            }
            pos += q_len; // non-overlapping, like every find-in-page implementation
        } else {
            pos += 1;
        }
    }
}

/// Folds one match's per-grapheme origins into an `Occurrence`, starting a
/// new piece at every point the match crosses from one line to another
/// (including across a glue grapheme, which contributes nothing itself but
/// still ends whatever piece came before it). `None` only when every
/// grapheme was glue — i.e. the query matched nothing that actually exists
/// on screen, which `find_in_run`'s search can't produce but this stays
/// total rather than assuming that.
fn occurrence_from_origins(origins: &[Option<(usize, usize)>]) -> Option<Occurrence> {
    let mut pieces: Vec<(usize, MatchSpan)> = Vec::new();
    let mut current: Option<(usize, usize, usize)> = None; // (line, start_col, end_col_exclusive)
    for o in origins {
        match (o, current) {
            (Some((line, col)), Some((cur_line, s, _))) if *line == cur_line => {
                current = Some((cur_line, s, col + 1));
            }
            (Some((line, col)), _) => {
                if let Some((cur_line, s, e)) = current {
                    pieces.push((cur_line, MatchSpan { start: s, end: e }));
                }
                current = Some((*line, *col, col + 1));
            }
            (None, _) => {} // glue grapheme: doesn't exist on screen, skip
        }
    }
    if let Some((cur_line, s, e)) = current {
        pieces.push((cur_line, MatchSpan { start: s, end: e }));
    }
    (!pieces.is_empty()).then_some(Occurrence { pieces })
}

/// Bump whenever `Layout`/`LaidLine`'s shape, or `layout_document`'s
/// wrapping/breaking semantics, change in a way that would make an old
/// cached `Layout` wrong to keep serving. Participates in
/// [`LayoutCacheKey`] (PRD FR-OFF-1's L1 layer) so a stale schema can never
/// be silently replayed across an upgrade — a version bump makes every
/// existing L1 entry a guaranteed miss instead.
pub const LAYOUT_SCHEMA_VERSION: u32 = 2;

/// PRD §6.8's L1 hit target (< 50 ms) only holds if the cache stays small
/// enough that a linear scan over it is free — 8 entries covers "the
/// article you're reading plus whatever you just came from or are about to
/// follow a link into," which is what actually gets reopened at an
/// unchanged width in a normal reading session.
pub const DEFAULT_L1_CAPACITY: usize = 8;

/// The L1 render-cache key (PRD FR-OFF-1): a laid-out `Layout` is only
/// reusable for the exact same document identity, at the exact same width
/// and render options, under the exact same layout engine version. Any
/// field differing is a cache miss, never a "close enough" reuse — a wrong
/// layout silently reused would misplace scroll offsets, section jumps,
/// and find matches, all of which trust line numbers absolutely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutCacheKey {
    pub lang: String,
    pub title: String,
    /// `0` when the content's revid is unknown (degraded mode — see
    /// `cache`'s module doc comment); still a valid, distinct key value,
    /// just one that can collide across different *content* at the same
    /// title if that content was never revid-tagged (an accepted
    /// degradation, matching L2's own).
    pub revid: u64,
    pub width: u16,
    pub options: LayoutOptions,
    pub schema_version: u32,
}

/// PRD FR-OFF-1's L1 layer: a small in-memory LRU of already-laid-out
/// documents, so reopening an article at an unchanged identity/width/
/// options doesn't repeat the layout pass (§6.8: L1 hit < 50 ms). Disk
/// persistence is explicitly out of scope: an in-memory cache alone meets
/// the target at today's article sizes, and every entry is trivially
/// rebuildable from L2 on a miss, so losing it on restart costs one
/// relayout, not correctness — revisit only if a future compliance audit
/// finds the target missed at realistic article sizes.
pub struct LayoutCache {
    capacity: usize,
    /// Least-recently-used at the front, most-recently-used at the back —
    /// a plain `Vec` rather than a `HashMap`+linked-list LRU because
    /// `DEFAULT_L1_CAPACITY` is tiny: a linear scan over 8 entries is
    /// cheaper than maintaining a fancier structure would ever recoup.
    entries: Vec<(LayoutCacheKey, Layout)>,
}

impl LayoutCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Vec::new(),
        }
    }

    /// A hit clones the stored `Layout` out (cheap relative to a relayout)
    /// and promotes the entry to most-recently-used; a miss leaves the
    /// cache untouched.
    pub fn get(&mut self, key: &LayoutCacheKey) -> Option<Layout> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let (k, v) = self.entries.remove(pos);
        let layout = v.clone();
        self.entries.push((k, v));
        Some(layout)
    }

    /// Inserts (or replaces, promoting to most-recently-used) an entry,
    /// evicting the least-recently-used one if this pushes the cache over
    /// capacity.
    pub fn put(&mut self, key: LayoutCacheKey, layout: Layout) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == &key) {
            self.entries.remove(pos);
        }
        self.entries.push((key, layout));
        while self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{collect_links, parse_article_html, section_outline};

    fn line_text(line: &LaidLine) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn assert_no_overflow(doc: &Document, width: u16, opts: LayoutOptions) {
        let layout = layout_document(doc, width, opts);
        for (i, line) in layout.lines.iter().enumerate() {
            let w = line.width(opts.ambiguous_wide);
            assert!(
                w <= width as usize,
                "line {i} width {w} exceeds {width}: {:?}",
                line_text(line)
            );
        }
    }

    const FIXTURE: &str = r##"
    <html><head><title>Test Article</title></head><body>
      <table class="infobox"><tbody>
        <tr><th colspan="2">Subject</th></tr>
        <tr><th>Field</th><td>Testing and a rather long value that will need to wrap</td></tr>
      </tbody></table>
      <p>This is <b>bold</b> and <i>italic</i> text with a <a href="./Other_Article">link</a>
      and a reference<sup class="reference"><a href="#cite_note-1">[1]</a></sup> plus enough
      words to force at least one wrap on a narrow terminal for good measure.</p>
      <h2>History</h2>
      <p>Some history text.</p>
      <ul><li>First item with a fairly long description that should wrap and hang</li>
      <li>Second item</li></ul>
      <blockquote><p>A quoted remark that is deliberately long enough to wrap across
      more than one line so the gutter bar is exercised on continuation lines.</p></blockquote>
      <pre>fn main() {
    println!("hello");
}</pre>
      <hr/>
      <table class="wikitable"><tbody>
        <tr><th>Year</th><th>Event</th></tr>
        <tr><td>1950</td><td>Something happened</td></tr>
      </tbody></table>
      <figure><img src="x.jpg" alt="A test picture"/></figure>
    </body></html>
    "##;

    /// A long Japanese paragraph, mixed CJK/latin, a heading, and internal
    /// links — the CJK-correctness fixture (FR-RD-10, ML-6).
    const JA_FIXTURE: &str = r##"
    <html><head><title>アラン・チューリング</title></head><body>
      <p>アラン・マティソン・チューリングは、イギリスの数学者、論理学者、暗号解読者、
      計算機科学者である。彼はしばしば<a href="./計算機科学">計算機科学</a>および
      <a href="./人工知能">人工知能</a>の父と呼ばれている。第二次世界大戦中、
      チューリングはドイツの暗号機エニグマの解読に大きく貢献した。</p>
      <h2>生涯</h2>
      <p>チューリングは1912年にロンドンで生まれた。The Turing machine は計算の
      数学的モデルであり、現代のコンピュータの理論的基礎となっている。</p>
    </body></html>
    "##;

    /// IPA with combining diacritics, a ZWJ emoji family, and a 300-character
    /// unbroken ASCII token — the grapheme-integrity fixtures.
    fn hard_cases_doc() -> Document {
        let long_token = "x".repeat(300);
        let html = format!(
            r##"<html><head><title>Edge Cases</title></head><body>
            <p>IPA sample: k&#688;a&#805; t&#865;s&#688;i&#331; with combining marks that must stay whole.</p>
            <p>Family emoji: &#128104;&#8205;&#128105;&#8205;&#128103;&#8205;&#128102; stays intact.</p>
            <p>{long_token}</p>
            </body></html>"##
        );
        parse_article_html("Edge Cases", &html)
    }

    #[test]
    fn no_line_exceeds_width_across_fixtures_and_widths() {
        let docs = [
            parse_article_html("Test Article", FIXTURE),
            parse_article_html("アラン・チューリング", JA_FIXTURE),
            hard_cases_doc(),
        ];
        for doc in &docs {
            for width in [20u16, 40, 60, 80, 100, 200] {
                for ambiguous_wide in [false, true] {
                    assert_no_overflow(
                        doc,
                        width,
                        LayoutOptions {
                            measure: 88,
                            ambiguous_wide,
                            ..LayoutOptions::default()
                        },
                    );
                }
            }
        }
    }

    #[test]
    fn grapheme_clusters_are_never_split() {
        let doc = hard_cases_doc();
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        // Even at a narrow width the ZWJ family must appear as one intact
        // substring on some line — never chopped between its scalars.
        let layout = layout_document(&doc, 20, LayoutOptions::default());
        let joined: String = layout.lines.iter().map(line_text).collect();
        assert!(
            joined.contains(family),
            "the ZWJ emoji family was split across a wrap"
        );

        // The IPA aspirated k with the combining accent below must also stay
        // as one grapheme cluster.
        let ipa = "k\u{2B0}"; // k + modifier letter small h
        assert!(joined.contains(ipa), "IPA cluster was split");
    }

    #[test]
    fn ambiguous_wide_changes_measured_width() {
        // East-Asian-Ambiguous characters measure 1 cell normally and 2
        // under the CJK tables (unicode-width's width_cjk): middle dot,
        // degree sign, section sign, circled digits.
        for s in ["·", "°", "§", "①"] {
            assert_eq!(display_width(s, false), 1, "{s} narrow by default");
            assert_eq!(display_width(s, true), 2, "{s} wide under CJK tables");
        }
        // A plain CJK ideograph is width 2 either way.
        assert_eq!(display_width("語", false), 2);
        assert_eq!(display_width("語", true), 2);
    }

    #[test]
    fn cjk_line_never_starts_with_forbidden_punctuation() {
        let doc = parse_article_html("アラン・チューリング", JA_FIXTURE);
        for width in [16u16, 24, 32, 40, 60] {
            let layout = layout_document(&doc, width, LayoutOptions::default());
            for line in &layout.lines {
                // The first visible glyph after any pad/gutter must not be a
                // kinsoku no-start character.
                let text = line_text(line);
                if let Some(first) = text.trim_start().chars().next() {
                    assert!(
                        !is_no_start(first),
                        "a line began with forbidden {first:?} at width {width}: {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn wide_terminal_caps_measure_and_centers() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let layout = layout_document(&doc, 200, LayoutOptions::default());
        // Every line's content beyond the left pad stays within the 88-cell
        // measure, and a centering pad is present.
        let pad = (200 - 88) / 2;
        let mut saw_pad = false;
        for line in &layout.lines {
            if let Some(first) = line.spans.first()
                && first.text.starts_with(' ')
                && first.text.chars().all(|c| c == ' ')
            {
                assert_eq!(
                    display_width(&first.text, false),
                    pad,
                    "left pad should center the 88-cell column in 200 cells"
                );
                saw_pad = true;
                let rest: usize = line.spans[1..]
                    .iter()
                    .map(|s| display_width(&s.text, false))
                    .sum();
                assert!(rest <= 88, "content column exceeded the 88-cell measure");
            }
        }
        assert!(saw_pad, "expected centered content with a left pad");
    }

    #[test]
    fn narrow_terminal_uses_full_width_without_pad() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let layout = layout_document(&doc, 50, LayoutOptions::default());
        // At 50 cells (< 88) there is no centering pad: the first content span
        // is never a pure-space pad the width of a margin.
        for line in &layout.lines {
            let w = line.width(false);
            assert!(w <= 50);
        }
        // Some line should actually use most of the width (proves we wrap to
        // 50, not to 88).
        assert!(layout.lines.iter().any(|l| l.width(false) > 40));
    }

    #[test]
    fn section_block_lines_land_on_the_rendered_heading() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let sections = section_outline(&doc);
        assert_eq!(sections.len(), 1, "fixture has one heading");
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        for section in &sections {
            let line = layout.block_lines[section.block];
            let rendered = line_text(&layout.lines[line]);
            assert_eq!(
                rendered.trim(),
                section.title,
                "block_lines must point at the heading's own text"
            );
        }
    }

    #[test]
    fn list_and_blockquote_continuation_lines_hang() {
        let doc = parse_article_html("Test Article", FIXTURE);
        // Narrow enough to force the long list item and blockquote to wrap.
        let layout = layout_document(&doc, 30, LayoutOptions::default());

        // The list bullet appears once; its continuation lines are indented
        // by the bullet width and carry no second bullet.
        let bullet_lines: Vec<usize> = layout
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| line_text(l).contains('•'))
            .map(|(i, _)| i)
            .collect();
        assert!(!bullet_lines.is_empty(), "expected a bulleted list item");
        let first_bullet = bullet_lines[0];
        let cont = &layout.lines[first_bullet + 1];
        let cont_text = line_text(cont);
        assert!(
            cont_text.starts_with("  ") && !cont_text.contains('•'),
            "list continuation should hang past the bullet: {cont_text:?}"
        );

        // The blockquote gutter bar runs down every one of its lines.
        let gutter_lines: Vec<&LaidLine> = layout
            .lines
            .iter()
            .filter(|l| line_text(l).contains('▌'))
            .collect();
        assert!(
            gutter_lines.len() >= 2,
            "blockquote should wrap onto continuation lines that keep the gutter"
        );
    }

    #[test]
    fn link_occurrence_order_matches_collect_links() {
        let doc = parse_article_html("アラン・チューリング", JA_FIXTURE);
        let links = collect_links(&doc);
        let layout = layout_document(&doc, 40, LayoutOptions::default());
        assert_eq!(
            layout.link_lines.len(),
            links.len(),
            "one laid link-line per collected link, in the same order"
        );
        // Link lines are non-decreasing in document order.
        for w in layout.link_lines.windows(2) {
            assert!(w[0] <= w[1], "link occurrences must be in document order");
        }
    }

    /// `link_cols` must slice out exactly the link's own rendered text on its
    /// `link_lines` line — the property link-hint painting (PRD FR-NV-1)
    /// depends on to splice a label over the right cells without re-deriving
    /// column offsets itself.
    #[test]
    fn link_cols_slice_out_the_links_own_rendered_text() {
        let doc = parse_article_html("アラン・チューリング", JA_FIXTURE);
        let links = collect_links(&doc);
        let layout = layout_document(&doc, 40, LayoutOptions::default());
        assert_eq!(layout.link_cols.len(), links.len());
        for (occ, link) in links.iter().enumerate() {
            let line = &layout.lines[layout.link_lines[occ]];
            let text = line_text(line);
            let graphemes: Vec<&str> = text.graphemes(true).collect();
            let span = layout.link_cols[occ];
            let sliced: String = graphemes[span.start..span.end].concat();
            assert_eq!(
                sliced, link.text,
                "link_cols[{occ}] must bound exactly the link's own text"
            );
        }
    }

    /// A multi-word link must lay out as one contiguous `Link` span (the
    /// space between its words keeps the link's kind) — otherwise the
    /// focused-link highlight would have unhighlighted holes at each space.
    #[test]
    fn multi_word_link_stays_one_contiguous_span() {
        let html = r##"<html><body><p>See <a href="./X">Alpha Beta</a> end.</p></body></html>"##;
        let doc = parse_article_html("T", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let link_span = layout
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.kind == SpanKind::Link(0))
            .expect("link span present");
        assert_eq!(link_span.text, "Alpha Beta");
    }

    #[test]
    fn long_ascii_token_hard_wraps_without_overflow() {
        let doc = hard_cases_doc();
        let layout = layout_document(&doc, 40, LayoutOptions::default());
        // The 300-x token spans multiple lines, none wider than 40.
        let x_lines: Vec<&LaidLine> = layout
            .lines
            .iter()
            .filter(|l| {
                line_text(l).chars().all(|c| c == 'x' || c == ' ') && line_text(l).contains('x')
            })
            .collect();
        assert!(x_lines.len() >= 7, "300 x's at width 40 need >=8 rows");
        for l in x_lines {
            assert!(l.width(false) <= 40);
        }
    }

    fn plain_line(text: &str) -> LaidLine {
        LaidLine {
            spans: vec![LaidSpan {
                text: text.to_string(),
                kind: SpanKind::Plain,
            }],
        }
    }

    /// PRD FR-NV-6's smart-case rule, exhaustively: any uppercase letter
    /// anywhere in the query flips it to case-sensitive; an all-lowercase
    /// query (including one with no letters at all) stays insensitive.
    #[test]
    fn is_case_sensitive_truth_table() {
        assert!(!is_case_sensitive("turing"));
        assert!(!is_case_sensitive(""));
        assert!(!is_case_sensitive("123"));
        assert!(
            !is_case_sensitive("über"),
            "lowercase diacritic stays insensitive"
        );
        assert!(is_case_sensitive("Turing"));
        assert!(is_case_sensitive("TURING"));
        assert!(
            is_case_sensitive("turinG"),
            "one trailing capital is enough"
        );
        assert!(is_case_sensitive("Über"));
    }

    /// A one-piece occurrence entirely on `line`, for asserting equality
    /// against `find_matches` output tersely.
    fn one_piece(line: usize, start: usize, end: usize) -> Occurrence {
        Occurrence {
            pieces: vec![(line, MatchSpan { start, end })],
        }
    }

    #[test]
    fn find_matches_is_case_insensitive_for_an_all_lowercase_query() {
        let lines = vec![plain_line("Alan Turing was here")];
        let matches = find_matches(&lines, &[], "turing");
        assert_eq!(matches, vec![one_piece(0, 5, 11)]);
    }

    #[test]
    fn find_matches_smart_case_rejects_wrong_case_when_query_has_uppercase() {
        let lines = vec![plain_line("alan turing was here")];
        let matches = find_matches(&lines, &[], "Turing");
        assert!(
            matches.is_empty(),
            "a capitalized query must not match lowercase text"
        );

        let lines = vec![plain_line("Alan Turing was here")];
        let matches = find_matches(&lines, &[], "Turing");
        assert_eq!(matches, vec![one_piece(0, 5, 11)]);
    }

    #[test]
    fn find_matches_reports_every_occurrence_non_overlapping() {
        let lines = vec![plain_line("turing turing turing")];
        let matches = find_matches(&lines, &[], "turing");
        assert_eq!(
            matches,
            vec![
                one_piece(0, 0, 6),
                one_piece(0, 7, 13),
                one_piece(0, 14, 20)
            ]
        );
    }

    /// A query straddling a soft wrap where a space was dropped (ordinary
    /// word-wrap) must be found as ONE occurrence with two pieces — not two
    /// separate occurrences, which would double-count it for the match
    /// counter and only let one half take the current-match emphasis.
    #[test]
    fn find_matches_spans_a_soft_wrap_where_a_space_was_dropped() {
        let lines = vec![plain_line("computer"), plain_line("science is fun")];
        let matches = find_matches(&lines, &[true], "computer science");
        assert_eq!(
            matches,
            vec![Occurrence {
                pieces: vec![
                    (0, MatchSpan { start: 0, end: 8 }),
                    (1, MatchSpan { start: 0, end: 7 }),
                ],
            }],
            "one occurrence, split into a piece per line it touches"
        );
    }

    /// Without a recorded continuation (a real block boundary — separate
    /// list items, table rows, ...), adjacent lines must never be glued:
    /// "cat" ending one block and "dog" starting the next is not "cat dog".
    #[test]
    fn find_matches_does_not_glue_across_a_block_boundary() {
        let lines = vec![plain_line("the cat"), plain_line("dog sat")];
        let matches = find_matches(&lines, &[false], "cat dog");
        assert!(matches.is_empty());
    }

    /// CJK per-character wrapping never had a space at the break — the glue
    /// must be empty, not a phantom space, or a genuine adjacency would go
    /// unfound. Still one occurrence, two pieces.
    #[test]
    fn find_matches_spans_a_cjk_wrap_with_no_glue_character() {
        let lines = vec![plain_line("計算機科"), plain_line("学は面白い")];
        let matches = find_matches(&lines, &[true], "科学");
        assert_eq!(
            matches,
            vec![Occurrence {
                pieces: vec![
                    (0, MatchSpan { start: 3, end: 4 }),
                    (1, MatchSpan { start: 0, end: 1 }),
                ],
            }]
        );
    }

    /// A combining-mark grapheme cluster (base + combining acute, two code
    /// points) must count and match as exactly one grapheme column, never
    /// split — the same invariant `grapheme_clusters_are_never_split`
    /// locks for the layout engine itself.
    #[test]
    fn find_matches_never_splits_a_combining_grapheme_cluster() {
        let combining_e = "e\u{0301}"; // "é" as base 'e' + combining acute accent
        let text = format!("caf{combining_e} borrowed word");
        let lines = vec![plain_line(&text)];
        let query = format!("caf{combining_e}");
        let matches = find_matches(&lines, &[], &query);
        assert_eq!(
            matches,
            vec![one_piece(0, 0, 4)],
            "c, a, f, and the combined e-acute cluster = 4 grapheme columns"
        );
    }

    #[test]
    fn find_matches_empty_query_or_lines_finds_nothing_and_does_not_panic() {
        let lines = vec![plain_line("some text")];
        assert!(find_matches(&lines, &[], "").is_empty());
        assert!(find_matches(&[], &[], "text").is_empty());
    }

    /// Wires `find_matches` to real `layout_document` output: a paragraph
    /// that actually wraps at a narrow width must mark at least one
    /// continuation, while two separate (unwrapped) list items must not be
    /// glued to each other.
    #[test]
    fn continuation_marks_wrapped_paragraph_lines_but_not_separate_list_items() {
        let html = r##"<html><body>
            <p>alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron</p>
            <ul><li>first item</li><li>second item</li></ul>
        </body></html>"##;
        let doc = parse_article_html("T", html);
        let layout = layout_document(&doc, 20, LayoutOptions::default());
        assert!(
            layout.continuation.iter().any(|&c| c),
            "the long paragraph must wrap and mark a continuation"
        );

        let bullet_lines: Vec<usize> = layout
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| line_text(l).contains('•'))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(bullet_lines.len(), 2, "two separate list items");
        assert!(
            !layout.continuation[bullet_lines[0]],
            "separate list items must not be glued for find purposes"
        );
    }

    #[test]
    fn continuation_length_is_always_one_less_than_lines() {
        for doc in [
            parse_article_html("Test Article", FIXTURE),
            parse_article_html("アラン・チューリング", JA_FIXTURE),
        ] {
            let layout = layout_document(&doc, 30, LayoutOptions::default());
            assert_eq!(layout.continuation.len(), layout.lines.len() - 1);
        }
    }

    // -- L1 render cache (PRD FR-OFF-1) -------------------------------------

    fn cache_key(title: &str, revid: u64, width: u16, schema_version: u32) -> LayoutCacheKey {
        LayoutCacheKey {
            lang: "en".to_string(),
            title: title.to_string(),
            revid,
            width,
            options: LayoutOptions::default(),
            schema_version,
        }
    }

    fn sample_layout(width: u16) -> Layout {
        layout_document(
            &parse_article_html("Test Article", FIXTURE),
            width,
            LayoutOptions::default(),
        )
    }

    #[test]
    fn layout_cache_hits_on_an_identical_key_and_misses_on_any_field_change() {
        let mut cache = LayoutCache::new(4);
        let key = cache_key("Article", 1, 80, LAYOUT_SCHEMA_VERSION);
        let layout = sample_layout(80);
        cache.put(key.clone(), layout.clone());

        let hit = cache.get(&key).expect("identical key must hit");
        assert_eq!(hit.lines, layout.lines);

        let mut different_schema = key.clone();
        different_schema.schema_version += 1;
        assert!(
            cache.get(&different_schema).is_none(),
            "a schema-version bump must miss, never replay a stale layout"
        );

        let mut different_revid = key.clone();
        different_revid.revid = 2;
        assert!(
            cache.get(&different_revid).is_none(),
            "different revid must miss"
        );

        let mut different_width = key.clone();
        different_width.width = 40;
        assert!(
            cache.get(&different_width).is_none(),
            "different width must miss"
        );

        let mut different_title = key.clone();
        different_title.title = "Other".to_string();
        assert!(
            cache.get(&different_title).is_none(),
            "different title must miss"
        );

        let mut different_lang = key;
        different_lang.lang = "de".to_string();
        assert!(
            cache.get(&different_lang).is_none(),
            "different lang must miss"
        );
    }

    #[test]
    fn layout_cache_get_on_empty_cache_is_a_miss_not_a_panic() {
        let mut cache = LayoutCache::new(4);
        assert!(
            cache
                .get(&cache_key("Nothing", 0, 80, LAYOUT_SCHEMA_VERSION))
                .is_none()
        );
    }

    #[test]
    fn layout_cache_evicts_least_recently_used_once_over_capacity() {
        let mut cache = LayoutCache::new(2);
        let a = cache_key("A", 1, 80, LAYOUT_SCHEMA_VERSION);
        let b = cache_key("B", 1, 80, LAYOUT_SCHEMA_VERSION);
        let c = cache_key("C", 1, 80, LAYOUT_SCHEMA_VERSION);
        let layout = sample_layout(80);

        cache.put(a.clone(), layout.clone());
        cache.put(b.clone(), layout.clone());
        assert_eq!(cache.len(), 2);

        // Touch A, making B the least-recently-used entry.
        assert!(cache.get(&a).is_some());
        cache.put(c.clone(), layout); // must evict B, not A

        assert_eq!(cache.len(), 2);
        assert!(cache.get(&a).is_some(), "recently touched: kept");
        assert!(cache.get(&b).is_none(), "least recently used: evicted");
        assert!(cache.get(&c).is_some(), "just inserted: kept");
    }

    #[test]
    fn layout_cache_put_again_for_the_same_key_replaces_not_duplicates() {
        let mut cache = LayoutCache::new(4);
        let key = cache_key("Article", 1, 80, LAYOUT_SCHEMA_VERSION);
        cache.put(key.clone(), sample_layout(80));
        cache.put(key.clone(), sample_layout(80));
        assert_eq!(
            cache.len(),
            1,
            "re-inserting the same key must not grow the cache"
        );
        assert!(cache.get(&key).is_some());
    }

    // -- Terminal-size degradation tiers (PRD §6.3) -------------------------

    #[test]
    fn size_tier_boundaries_are_exact() {
        // Width boundaries at a comfortable height.
        assert_eq!(size_tier(59, 40), SizeTier::Floor);
        assert_eq!(size_tier(60, 40), SizeTier::Minimal);
        assert_eq!(size_tier(79, 40), SizeTier::Minimal);
        assert_eq!(size_tier(80, 40), SizeTier::Compact);
        assert_eq!(size_tier(99, 40), SizeTier::Compact);
        assert_eq!(size_tier(100, 40), SizeTier::Full);
        // Height floor: a wide-but-short terminal is still Floor.
        assert_eq!(size_tier(120, 15), SizeTier::Floor);
        assert_eq!(size_tier(120, 16), SizeTier::Full);
        assert_eq!(size_tier(60, 16), SizeTier::Minimal);
    }

    // -- Table collapse-vs-scroll decision (pure) ---------------------------

    #[test]
    fn plan_table_collapses_when_accessible_or_too_narrow() {
        // Accessible always collapses, no matter how wide.
        assert_eq!(plan_table(&[10, 10], 200, true, 0, 1), TablePlan::Collapse);
        // Can't fit even one column at MIN_COL_WIDTH (8): grid_width([8],1)=12.
        assert_eq!(plan_table(&[20], 11, false, 0, 1), TablePlan::Collapse);
        assert!(matches!(
            plan_table(&[20], 12, false, 0, 1),
            TablePlan::Grid { .. }
        ));
    }

    #[test]
    fn plan_table_fits_all_columns_when_they_reasonably_fit() {
        // Two columns, natural [4, 18], plenty of room -> grid, all columns,
        // width within budget.
        match plan_table(&[4, 18], 40, false, 0, 1) {
            TablePlan::Grid { first_col, widths } => {
                assert_eq!(first_col, 0);
                assert_eq!(widths.len(), 2);
                assert!(grid_width(&widths, 1) <= 40);
            }
            other => panic!("expected a full grid, got {other:?}"),
        }
    }

    #[test]
    fn plan_table_scrolls_a_window_from_the_offset_when_too_wide() {
        let natural = vec![18usize; 6]; // 6 wide columns, can't all fit at 60
        let at0 = plan_table(&natural, 60, false, 0, 1);
        let at2 = plan_table(&natural, 60, false, 2, 1);
        let (first0, len0) = match at0 {
            TablePlan::Grid { first_col, widths } => (first_col, widths.len()),
            other => panic!("expected a scrolling grid, got {other:?}"),
        };
        let first2 = match at2 {
            TablePlan::Grid { first_col, .. } => first_col,
            other => panic!("expected a scrolling grid, got {other:?}"),
        };
        assert_eq!(first0, 0);
        assert!(len0 < natural.len(), "not every column fits, so it scrolls");
        assert_eq!(first2, 2, "the offset shifts the visible column window");
    }

    // -- Table grid rendering (box-drawing) ---------------------------------

    fn table_at(html: &str, width: u16, opts: LayoutOptions) -> Vec<String> {
        let doc = parse_article_html("T", html);
        layout_document(&doc, width, opts)
            .lines
            .iter()
            .map(line_text)
            .collect()
    }

    #[test]
    fn one_by_one_table_has_only_plain_corners() {
        let lines = table_at(
            r##"<html><body><table class="wikitable"><tr><td>x</td></tr></table></body></html>"##,
            60,
            LayoutOptions::default(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains('┌') && joined.contains('┐'));
        assert!(joined.contains('└') && joined.contains('┘'));
        assert!(joined.contains('│'));
        // A single column has no T-junctions.
        assert!(!joined.contains('┬') && !joined.contains('┴') && !joined.contains('┼'));
    }

    #[test]
    fn header_table_draws_every_box_drawing_glyph() {
        let lines = table_at(
            r##"<html><body><table class="wikitable"><tbody>
            <tr><th>A</th><th>B</th><th>C</th></tr>
            <tr><td>1</td><td>2</td><td>3</td></tr>
            </tbody></table></body></html>"##,
            60,
            LayoutOptions::default(),
        );
        let joined = lines.join("\n");
        for glyph in ['┌', '┬', '┐', '├', '┼', '┤', '└', '┴', '┘', '─', '│'] {
            assert!(joined.contains(glyph), "missing {glyph:?} in:\n{joined}");
        }
    }

    #[test]
    fn a_cell_wider_than_its_column_wraps_and_grows_the_row() {
        // A narrow terminal forces the long cell to wrap onto extra rows, and
        // no line may exceed the width.
        let doc = parse_article_html(
            "T",
            r##"<html><body><table class="wikitable"><tbody>
            <tr><th>K</th><th>V</th></tr>
            <tr><td>x</td><td>one two three four five six seven eight nine ten eleven twelve</td></tr>
            </tbody></table></body></html>"##,
        );
        let layout = layout_document(&doc, 40, LayoutOptions::default());
        for line in &layout.lines {
            assert!(
                line.width(false) <= 40,
                "grid overflow: {:?}",
                line_text(line)
            );
        }
        // The long value wrapped: the body occupies more than one content row
        // between the header rule and the bottom border.
        let body_rows = layout
            .lines
            .iter()
            .filter(|l| {
                let t = line_text(l);
                t.starts_with('│') && t.contains("one")
                    || (t.starts_with('│') && (t.contains("eleven") || t.contains("twelve")))
            })
            .count();
        assert!(
            body_rows >= 2,
            "the overflowing cell must wrap onto extra rows"
        );
    }

    #[test]
    fn scroll_offset_reveals_a_right_column_that_was_hidden() {
        let mut html = String::from(r##"<html><body><table class="wikitable"><tbody><tr>"##);
        for c in 0..10 {
            html.push_str(&format!("<th>Col{c:02}</th>"));
        }
        html.push_str("</tr><tr>");
        for c in 0..10 {
            html.push_str(&format!("<td>v{c:02}</td>"));
        }
        html.push_str("</tr></tbody></table></body></html>");

        let at0 = table_at(&html, 60, LayoutOptions::default()).join("\n");
        let at5 = table_at(
            &html,
            60,
            LayoutOptions {
                table_col_offset: 5,
                ..LayoutOptions::default()
            },
        )
        .join("\n");
        assert!(at0.contains("Col00"), "offset 0 shows the first column");
        assert!(
            !at0.contains("Col09"),
            "the far-right column is off-screen at offset 0"
        );
        assert!(
            at5.contains("Col09"),
            "scrolling right brings the far column into view"
        );
    }

    #[test]
    fn accessible_collapses_a_grid_to_a_labeled_list() {
        let html = r##"<html><body><table class="wikitable"><tbody>
        <tr><th>Year</th><th>Event</th></tr>
        <tr><td>1950</td><td>Turing test</td></tr>
        </tbody></table></body></html>"##;
        let lines = table_at(
            html,
            120,
            LayoutOptions {
                accessible: true,
                ..LayoutOptions::default()
            },
        );
        let joined = lines.join("\n");
        assert!(
            !joined.contains('┼') && !joined.contains('┬'),
            "accessible mode must not draw a grid, even wide"
        );
        assert!(joined.contains("Year: 1950"));
        assert!(joined.contains("Event: Turing test"));
    }

    #[test]
    fn cjk_cell_is_sized_by_display_width_not_char_count() {
        // "計算機" is 3 chars but 6 display cells; the column must be wide
        // enough that the cell isn't clipped, and nothing overflows.
        let doc = parse_article_html(
            "T",
            r##"<html><body><table class="wikitable"><tbody>
            <tr><th>用語</th><th>意味</th></tr>
            <tr><td>計算機</td><td>コンピュータ</td></tr>
            </tbody></table></body></html>"##,
        );
        let layout = layout_document(&doc, 60, LayoutOptions::default());
        let joined: String = layout.lines.iter().map(line_text).collect();
        assert!(
            joined.contains("計算機"),
            "CJK cell content preserved whole"
        );
        for line in &layout.lines {
            assert!(line.width(false) <= 60);
        }
    }

    // -- Infobox card vs top-block by width (PRD FR-RD-5) -------------------

    /// An infobox fixture with a lead paragraph long enough to sit beside a
    /// floated card, then a heading (which ends the lead region).
    const INFOBOX_FIXTURE: &str = r##"<html><head><title>Person</title></head><body>
    <table class="infobox"><tbody>
    <tr><th colspan="2">Alan Turing</th></tr>
    <tr><th>Born</th><td>23 June 1912</td></tr>
    <tr><th>Died</th><td>7 June 1954</td></tr>
    <tr><th>Fields</th><td>Mathematics, cryptanalysis, computer science</td></tr>
    </tbody></table>
    <p>Alan Mathison Turing was an English mathematician and computer scientist,
    highly influential in the development of theoretical computer science and
    widely considered the father of artificial intelligence.</p>
    <h2>Career</h2><p>He worked at Bletchley Park during the war.</p>
    </body></html>"##;

    /// The display-cell column at which the first `Infobox` span on a line
    /// begins, or `None` if the line has none.
    fn infobox_start_col(line: &LaidLine) -> Option<usize> {
        let mut col = 0;
        for s in &line.spans {
            if s.kind == SpanKind::Infobox {
                return Some(col);
            }
            col += display_width(&s.text, false);
        }
        None
    }

    /// Whether a line has real (non-blank) body text to the left of its
    /// infobox card — the signature of a right-float.
    fn has_lead_text_beside_infobox(line: &LaidLine) -> bool {
        let has_infobox = line.spans.iter().any(|s| s.kind == SpanKind::Infobox);
        let has_body = line
            .spans
            .iter()
            .any(|s| s.kind != SpanKind::Infobox && !s.text.trim().is_empty());
        has_infobox && has_body
    }

    #[test]
    fn infobox_floats_right_with_lead_text_to_its_left_on_wide_terminals() {
        let doc = parse_article_html("Person", INFOBOX_FIXTURE);
        let layout = layout_document(&doc, 120, LayoutOptions::default());
        let float_row = layout
            .lines
            .iter()
            .find(|l| has_lead_text_beside_infobox(l))
            .expect("a floated row: infobox on the right, lead text on the left");
        // The card sits in the right portion of the screen.
        let start = infobox_start_col(float_row).unwrap();
        assert!(
            start >= 120 / 2,
            "infobox border should be in the right half (started at cell {start})"
        );
        // No line overflows the terminal.
        for line in &layout.lines {
            assert!(line.width(false) <= 120);
        }
    }

    #[test]
    fn infobox_is_a_top_block_not_a_float_on_compact_terminals() {
        let doc = parse_article_html("Person", INFOBOX_FIXTURE);
        let layout = layout_document(&doc, 90, LayoutOptions::default());
        assert!(
            layout
                .lines
                .iter()
                .all(|l| !has_lead_text_beside_infobox(l)),
            "no row should have both lead text and the infobox card at 90 cols"
        );
        // The card is still drawn (top block).
        let joined: String = layout.lines.iter().map(line_text).collect();
        assert!(joined.contains("Alan Turing"));
        assert!(joined.contains("Born: 23 June 1912"));
        for line in &layout.lines {
            assert!(line.width(false) <= 90);
        }
    }

    #[test]
    fn floated_infobox_keeps_block_lines_aligned_for_section_jump() {
        // The float pushes a block_lines anchor for the infobox and for each
        // lead block, so section jumps still land on the right heading line.
        let doc = parse_article_html("Person", INFOBOX_FIXTURE);
        let sections = section_outline(&doc);
        let layout = layout_document(&doc, 120, LayoutOptions::default());
        assert_eq!(
            layout.block_lines.len(),
            doc.blocks.len(),
            "one scroll anchor per block, even across the float"
        );
        for section in &sections {
            let line = layout.block_lines[section.block];
            assert_eq!(
                line_text(&layout.lines[line]).trim(),
                section.title,
                "the heading's anchor line must render its own text"
            );
        }
    }

    /// The cross-module link-ordering invariant must survive the infobox
    /// float: a link inside the floated lead paragraph is still numbered and
    /// located consistently with `doc::collect_links` (which the paint step
    /// relies on to map focus/hint state to the right span).
    #[test]
    fn links_in_a_floated_lead_stay_consistent_with_collect_links() {
        let html = r##"<html><head><title>P</title></head><body>
        <table class="infobox"><tbody>
        <tr><th colspan="2">Name</th></tr>
        <tr><th>Born</th><td>1900</td></tr>
        </tbody></table>
        <p>The lead mentions <a href="./Computer_science">computer science</a> and
        also <a href="./Enigma_machine">the Enigma machine</a> before the first heading.</p>
        <h2>More</h2><p>See <a href="./Alan_Turing">Turing</a> too.</p>
        </body></html>"##;
        let doc = parse_article_html("P", html);
        let links = collect_links(&doc);
        assert_eq!(links.len(), 3, "two lead links plus one after the float");
        let layout = layout_document(&doc, 120, LayoutOptions::default());
        assert_eq!(layout.link_lines.len(), links.len());
        assert_eq!(layout.link_cols.len(), links.len());
        for (occ, link) in links.iter().enumerate() {
            let line = &layout.lines[layout.link_lines[occ]];
            let text: String = line.spans.iter().map(|s| s.text.as_str()).collect();
            let graphemes: Vec<&str> = text.graphemes(true).collect();
            let span = layout.link_cols[occ];
            let sliced: String = graphemes[span.start..span.end].concat();
            assert_eq!(
                sliced, link.text,
                "link {occ} column range must bound its own text"
            );
        }
    }

    #[test]
    fn table_fixtures_never_overflow_across_widths_and_tiers() {
        let mut wide = String::from(r##"<html><body><table class="wikitable"><tbody><tr>"##);
        for c in 0..12 {
            wide.push_str(&format!("<th>Column {c}</th>"));
        }
        wide.push_str("</tr><tr>");
        for c in 0..12 {
            wide.push_str(&format!("<td>data cell number {c}</td>"));
        }
        wide.push_str("</tr></tbody></table></body></html>");
        let docs = [
            parse_article_html("Person", INFOBOX_FIXTURE),
            parse_article_html("Wide", &wide),
        ];
        for doc in &docs {
            for width in [60u16, 70, 80, 90, 100, 120, 200] {
                for offset in [0u16, 3, 8] {
                    for accessible in [false, true] {
                        let opts = LayoutOptions {
                            table_col_offset: offset,
                            accessible,
                            ..LayoutOptions::default()
                        };
                        let layout = layout_document(doc, width, opts);
                        for line in &layout.lines {
                            assert!(
                                line.width(false) <= width as usize,
                                "overflow at width {width}, offset {offset}, accessible \
                                 {accessible}: {:?}",
                                line_text(line)
                            );
                        }
                        assert_eq!(layout.continuation.len(), layout.lines.len() - 1);
                    }
                }
            }
        }
    }
}
