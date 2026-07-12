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
                            em.push_line(
                                finalize(
                                    pad_width,
                                    &[LaidSpan {
                                        text: gutter.to_string(),
                                        kind: SpanKind::Code,
                                    }],
                                    &[],
                                ),
                                false,
                            );
                        }
                        // A source line hard-split across several chunks (it
                        // overflowed `avail`) is one continuous run with no
                        // separator dropped at the cut — every chunk after
                        // the first continues the previous one. Different
                        // source lines never do: that boundary is a real
                        // `\n`, not a wrap.
                        for (i, chunk) in chunks.into_iter().enumerate() {
                            em.push_line(
                                finalize(
                                    pad_width,
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
                    em.push_line(
                        finalize(
                            pad_width,
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
                    em.push_line(
                        finalize(
                            pad_width,
                            &[LaidSpan {
                                text: format!("{top}{}", "─".repeat(top_fill)),
                                kind: SpanKind::Infobox,
                            }],
                            &[],
                        ),
                        false,
                    );
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
                    em.push_line(
                        finalize(
                            pad_width,
                            &[LaidSpan {
                                text: format!("└{}", "─".repeat(bottom_fill)),
                                kind: SpanKind::Infobox,
                            }],
                            &[],
                        ),
                        false,
                    );
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
}
