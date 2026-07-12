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
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            measure: 88,
            ambiguous_wide: false,
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
    pad_width: usize,
    content_width: usize,
    ambiguous_wide: bool,
}

impl Emitter<'_> {
    fn blank(&mut self) {
        self.lines.push(finalize(self.pad_width, &[], &[]));
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
            self.lines
                .push(finalize(self.pad_width, &first_prefix, &[]));
            return;
        }
        for (i, line) in wrapped.iter().enumerate() {
            let prefix = if i == 0 { &first_prefix } else { &cont_prefix };
            self.lines.push(finalize(self.pad_width, prefix, line));
        }
    }

    fn emit_plain_wrapped(&mut self, content: Vec<Cluster>) {
        self.emit_wrapped(content, Vec::new(), Vec::new());
    }
}

/// Lay `doc` out for a terminal `width` cells wide. See the module docs for
/// the guarantees this upholds.
pub fn layout_document(doc: &Document, width: u16, options: LayoutOptions) -> Layout {
    let available = (width.max(1)) as usize;
    let content_width = available.min((options.measure.max(1)) as usize);
    let pad_width = available.saturating_sub(content_width) / 2;
    let aw = options.ambiguous_wide;

    let mut lines: Vec<LaidLine> = Vec::new();
    let mut block_lines: Vec<usize> = Vec::with_capacity(doc.blocks.len());
    let mut link_counter = 0usize;

    {
        let mut em = Emitter {
            lines: &mut lines,
            pad_width,
            content_width,
            ambiguous_wide: aw,
        };
        // Title, then a blank line — mirrors the previous renderer's header.
        em.emit_plain_wrapped(clusters_from_str(&doc.title, SpanKind::Title, aw));
        em.blank();

        for block in &doc.blocks {
            match block {
                Block::Heading { level, spans } => {
                    em.blank();
                    let anchor = em.lines.len();
                    let text = flatten_plain(spans);
                    em.emit_plain_wrapped(clusters_from_str(&text, SpanKind::Heading(*level), aw));
                    block_lines.push(anchor);
                }
                Block::Paragraph(spans) => {
                    let anchor = em.lines.len();
                    let content = flatten_spans(spans, SpanKind::Plain, &mut link_counter, aw);
                    em.emit_plain_wrapped(content);
                    em.blank();
                    block_lines.push(anchor);
                }
                Block::ListItem {
                    ordered,
                    index,
                    depth,
                    spans,
                } => {
                    let anchor = em.lines.len();
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
                    let content = flatten_spans(spans, SpanKind::Plain, &mut link_counter, aw);
                    em.emit_wrapped(content, first_prefix, cont_prefix);
                    block_lines.push(anchor);
                }
                Block::Blockquote(spans) => {
                    let anchor = em.lines.len();
                    let gutter = vec![LaidSpan {
                        text: "▌ ".to_string(),
                        kind: SpanKind::Dim,
                    }];
                    let content = flatten_spans(spans, SpanKind::Quote, &mut link_counter, aw);
                    em.emit_wrapped(content, gutter.clone(), gutter);
                    em.blank();
                    block_lines.push(anchor);
                }
                Block::Code(text) => {
                    let anchor = em.lines.len();
                    let gutter = "    ";
                    let avail = content_width.saturating_sub(4).max(1);
                    for src in text.lines() {
                        let chunks =
                            chunk_by_width(clusters_from_str(src, SpanKind::Code, aw), avail);
                        if chunks.is_empty() {
                            em.lines.push(finalize(
                                pad_width,
                                &[LaidSpan {
                                    text: gutter.to_string(),
                                    kind: SpanKind::Code,
                                }],
                                &[],
                            ));
                        }
                        for chunk in chunks {
                            em.lines.push(finalize(
                                pad_width,
                                &[LaidSpan {
                                    text: gutter.to_string(),
                                    kind: SpanKind::Code,
                                }],
                                &chunk,
                            ));
                        }
                    }
                    em.blank();
                    block_lines.push(anchor);
                }
                Block::Rule => {
                    let anchor = em.lines.len();
                    // Box-drawing dashes are East-Asian-Ambiguous, so under
                    // ambiguous_wide each occupies 2 cells: fill by cells,
                    // not by character count.
                    let dash_w = display_width("─", aw).max(1);
                    let n = content_width.min(40) / dash_w;
                    em.lines.push(finalize(
                        pad_width,
                        &[LaidSpan {
                            text: "─".repeat(n),
                            kind: SpanKind::Dim,
                        }],
                        &[],
                    ));
                    block_lines.push(anchor);
                }
                Block::Table(rows) => {
                    let anchor = em.lines.len();
                    for row in rows {
                        em.emit_plain_wrapped(clusters_from_str(row, SpanKind::Table, aw));
                    }
                    em.blank();
                    block_lines.push(anchor);
                }
                Block::Infobox(rows) => {
                    let anchor = em.lines.len();
                    // As with Rule: the border glyphs are ambiguous-width, so
                    // fills count cells (dash_w per dash), never characters.
                    let dash_w = display_width("─", aw).max(1);
                    let top = truncate_to_width("┌─ infobox ", content_width, aw);
                    let top_fill = content_width.saturating_sub(display_width(&top, aw)) / dash_w;
                    em.lines.push(finalize(
                        pad_width,
                        &[LaidSpan {
                            text: format!("{top}{}", "─".repeat(top_fill)),
                            kind: SpanKind::Infobox,
                        }],
                        &[],
                    ));
                    let gutter = vec![LaidSpan {
                        text: "│ ".to_string(),
                        kind: SpanKind::Infobox,
                    }];
                    for (label, value) in rows {
                        let text = if label.is_empty() {
                            value.clone()
                        } else {
                            format!("{label}: {value}")
                        };
                        em.emit_wrapped(
                            clusters_from_str(&text, SpanKind::Infobox, aw),
                            gutter.clone(),
                            gutter.clone(),
                        );
                    }
                    let bottom_fill = content_width.saturating_sub(display_width("└", aw)) / dash_w;
                    em.lines.push(finalize(
                        pad_width,
                        &[LaidSpan {
                            text: format!("└{}", "─".repeat(bottom_fill)),
                            kind: SpanKind::Infobox,
                        }],
                        &[],
                    ));
                    em.blank();
                    block_lines.push(anchor);
                }
                Block::Image(alt) => {
                    let anchor = em.lines.len();
                    em.emit_plain_wrapped(clusters_from_str(
                        &format!("[image: {alt}]"),
                        SpanKind::Image,
                        aw,
                    ));
                    em.blank();
                    block_lines.push(anchor);
                }
            }
        }
    }

    // Map each link occurrence to the first line it appears on. Occurrence
    // indices were assigned in document order (== collect_links order) during
    // flattening, so this fills every slot.
    let mut link_lines = vec![0usize; link_counter];
    let mut seen = vec![false; link_counter];
    for (i, line) in lines.iter().enumerate() {
        for span in &line.spans {
            if let SpanKind::Link(occ) = span.kind
                && occ < link_counter
                && !seen[occ]
            {
                seen[occ] = true;
                link_lines[occ] = i;
            }
        }
    }

    Layout {
        width,
        options,
        lines,
        block_lines,
        link_lines,
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
}
