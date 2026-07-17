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
//!   * Optional (default-off) full justification and Knuth-Liang soft
//!     hyphenation (PRD FR-RD-9's v1.x half) ride the same greedy filler —
//!     see [`LayoutOptions::justify`]/[`LayoutOptions::hyphenate`], [`fill`],
//!     [`try_hyphenate`], and [`justify_line`]. Both are no-ops under their
//!     defaults, so the default build lays every page out byte-for-byte as
//!     before they existed.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::doc::{Block, Document, GalleryItem, SpanStyle};

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
    /// PRD FR-RD-1 syntax-highlight token classes, emitted only inside a
    /// `Block::Code` whose language the rule-based highlighter (`syntax::Lang`)
    /// recognizes. Each maps to an existing theme slot in `ui::kind_style`
    /// (keyword→heading, string→quote, comment→dim, number→warning), so the
    /// FR-TH-3 degradation pipeline and `NO_COLOR` already cover them with no
    /// new theme fields; an unhighlighted / unknown-language block keeps using
    /// plain [`SpanKind::Code`] (`theme.code`). A monochrome theme collapses
    /// all four onto its single hue, which is the correct behavior there.
    CodeKeyword,
    CodeString,
    CodeComment,
    CodeNumber,
    Table,
    Infobox,
    Image,
    /// PRD FR-RD-7: TeX passthrough, inline or (via `Block::Math`) a
    /// standalone display line — either way the *text* carried in the
    /// `LaidSpan` is already the ⟨delimited⟩, `normalize_trivial_math`-ed
    /// display form (produced by `flatten_spans`/the `Block::Math` emitter,
    /// not by `paint`), so this variant only needs to say "style it as math."
    Math,
    /// A caption line under an image or gallery (PRD FR-RD-8): dim italics.
    Caption,
    /// One reserved row of an inline image's half-block box (PRD FR-RD-8).
    /// The layout reserves `rows` of these stacked vertically, each the box's
    /// full width (its `LaidSpan.text` is that many spaces, so width math and
    /// find highlighting treat the row as blank). At paint time `ui.rs` looks
    /// the decoded image up by `src` in the [`crate::image::ImageStore`] and
    /// replaces this one span with a run of `▀` half-block cells whose fg/bg
    /// come from the image's pixels — so the layout stays theme-independent
    /// (only box geometry, never a color, lives here). Emitted only when the
    /// image is decoded *and* images are enabled; otherwise the layout emits a
    /// plain `[image: alt]` placeholder instead.
    ImageRow {
        src: String,
        row: u16,
        rows: u16,
    },
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
    /// PRD FR-DL-4: a `{{citation needed}}`-family marker
    /// (`doc::SpanStyle::CitationNeeded`). Its own kind (not folded into
    /// `Dim`) because whether it *paints* dim depends on the `:set show-cn`
    /// runtime toggle (`ui::kind_style`), and `layout::citation_needed_lines`
    /// needs to find it regardless of that toggle's state.
    CitationNeeded,
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

/// PRD FR-PC-1's honest "spacing options": a cell grid can't letter-space,
/// so the one alignment choice on offer is where the leftover width (beyond
/// `measure`) goes. `Center` is FR-RD-9's original behavior — pad both
/// sides so the column floats in the middle of a wide terminal; `Left`
/// drops that pad so the column instead hugs the left margin, for readers
/// who find a floating center column disorienting. Either way the column
/// itself is never wider than `measure` — this only redistributes the
/// leftover space, never the content width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Center,
    Left,
}

impl TextAlign {
    /// Parses the `:set`/`:set-tab text_align=` and config `text_align =`
    /// spelling. Mirrors `HyperlinkMode::parse`'s shape: a small closed set,
    /// `None` for anything else, so the caller can produce its own error
    /// message naming the valid choices.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "center" => Some(Self::Center),
            "left" => Some(Self::Left),
            _ => None,
        }
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
    /// Not a typography knob but a layout **input**: bumped by `App` whenever
    /// inline-image state changes (a thumbnail finishes decoding, `:set
    /// images` flips, a theme change flips `images`). Because it lives in the
    /// L1 cache key (`LayoutCacheKey`) and the `ensure_layout` staleness check
    /// (`l.options != opts`), a newly-decoded image forces one relayout and no
    /// pre-decode cached layout is ever replayed with a stale image box (PRD
    /// FR-RD-8, FR-OFF-1). The reserved box sizes themselves come from an
    /// [`ImageResolver`] passed alongside, not from this field.
    pub image_epoch: u64,
    /// PRD FR-RD-11's WPM divisor for the "N min read" header line
    /// (`config::resolve_reading_wpm` / `:set reading_wpm=N`, default 230).
    /// Part of the L1 cache key like every other field here: changing it
    /// changes the laid-out header line, so a stale cached layout must not
    /// be reused across a change.
    pub reading_wpm: u32,
    /// The *resolved* images-enabled bool for the tab this layout is built
    /// for (PRD FR-TH-7, FR-RD-8) — never consulted inside
    /// `layout_document_with_images` itself (the actual box-vs-placeholder
    /// decision is entirely a property of the `ImageResolver`/box map the
    /// caller passes in, exactly like `image_epoch`), only carried here so
    /// it participates in the L1 cache key. That's load-bearing once FR-PC-4
    /// lets two *tabs* disagree about images on the same article/width/
    /// options: `image_epoch` alone is a nonce, not the answer, so it can't
    /// tell one tab's "on" apart from another tab's "off" at the same nonce
    /// value — this field can.
    pub images_on: bool,
    /// PRD FR-PC-1: `center` (default, FR-RD-9's original behavior) or
    /// `left` — see [`TextAlign`].
    pub text_align: TextAlign,
    /// PRD FR-PC-1: extra left margin in cells, carved out of the *same*
    /// available-width budget `measure`/centering already share (never added
    /// on top of it) — see `layout_document_with_images`'s pad-width math.
    /// Default 0 (no margin beyond whatever centering already applies).
    pub margin: u16,
    /// PRD FR-PC-1: blank rows between one block-level element and the next
    /// (a paragraph, heading, list, quote, table, image, …) — every
    /// `Emitter::blank` call site shares this one knob, so raising or
    /// lowering it is a single configuration change rather than a per-block-
    /// kind rule. Default 1 reproduces the pre-FR-PC-1 layout byte for byte;
    /// 0 is "tight", 2 (and up) is "airy".
    pub paragraph_spacing: u8,
    /// PRD FR-PC-1's honest "interline spacing": a terminal cell grid has no
    /// fractional line height, so this is literally how many blank rows
    /// follow *every* visual (wrapped) line of prose — paragraphs, headings,
    /// list items, blockquotes, captions, image placeholders, and math (see
    /// `Emitter::emit_wrapped`, the one choke point that applies it).
    /// Structured/grid content — table grids and collapse-lists, infobox
    /// cards, image half-block boxes, code blocks, horizontal rules, and
    /// gallery strips — is deliberately exempt: a blank row spliced into a
    /// box-drawing grid or an image's pixel rows would break it, not space
    /// it out. `0` is today's single-spacing default. There is no literal
    /// "1.5" setting — a true half-row isn't renderable on a cell grid, and
    /// this option does not pretend otherwise; `1` (one blank row after
    /// every line) is offered as the closest *honest* approximation of
    /// "1.5-line spacing" a reader coming from a word processor would
    /// recognize, `2` approximates a still-airier double-plus. Also: at
    /// `line_spacing > 0`, in-page find's wrap-straddling glue (`Layout::
    /// continuation`, "a query split across a wrap point is still found")
    /// stops gluing across that particular wrap — the inserted blank row
    /// means the two visual lines are no longer adjacent in `lines`, so a
    /// query is still found within either line alone, just not one that
    /// happens to straddle the old wrap point. A narrow, documented
    /// trade-off, not a bug: exactly the kind of edge case `find_matches`'s
    /// own doc comment already calls "a narrow, documented miss."
    pub line_spacing: u8,
    /// PRD FR-PC-1's honest "inter-word spacing": a cell grid can't
    /// letter-space, but it can widen the single collapsed space between
    /// words by this many extra cells (`0` default, `1` typical) — applied
    /// in `fill`'s greedy line-filler so the extra width always counts
    /// toward line-fill math (a wider gap still wraps correctly, never
    /// overflows). Scoped to prose wrapping (`wrap_content`/`emit_wrapped`'s
    /// call sites); table/infobox cell text (`wrap_cell_text`) is exempt —
    /// widening the gaps inside a fixed-width grid column would misalign it
    /// against its own border, not make it more readable.
    pub word_spacing: u8,
    /// PRD FR-RD-9's optional full justification (`justify = true|false`,
    /// `:set justify=on|off`, default **false** — ragged-right stays the
    /// default). When on, `fill` distributes extra spaces between the words of
    /// every *width-wrapped* prose line so the line reaches exactly the
    /// content width, EXCEPT: the last line of a paragraph/run (stays
    /// ragged), lines with no inter-word gaps (a purely-CJK line can't
    /// justify by spacing — left alone), and lines a "river cap" rejects (a
    /// line so sparse that filling it would open an ugly river of whitespace,
    /// see [`RIVER_CAP_MAX_EXTRA_PER_GAP`] — left ragged). For a justified
    /// line the no-overflow invariant tightens to an *equality*: its content
    /// width == the content column exactly. Only meaningful with the default
    /// `text_align = Left`/center column of prose — it composes with
    /// `word_spacing` (justification stretches on top of the widened base
    /// gap) and is a no-op for the box/grid content `emit_wrapped` never
    /// touches. Part of the L1 cache key like every field here.
    pub justify: bool,
    /// PRD FR-RD-9's optional soft hyphenation (`hyphenate = true|false`,
    /// `:set hyphenate=on|off`, default false). When on, a word that would
    /// overflow the end of a *non-empty* prose line may be broken at a
    /// Knuth-Liang-valid point (embedded en-US patterns) with a trailing
    /// `-`, when a prefix+`-` fits the remaining width — see
    /// [`try_hyphenate`]. The break is always at a grapheme-cluster boundary,
    /// the `-` counts toward the line width (the no-overflow invariant holds
    /// with it), and it composes with `justify` (hyphenation fills the line
    /// tighter first, then justification stretches the result). Scoped to
    /// ASCII-alphabetic words over [`MIN_HYPHEN_WORD_LEN`]; CJK never
    /// hyphenates (per-character breaking already handles it). Part of the L1
    /// cache key like every field here.
    pub hyphenate: bool,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            measure: 88,
            ambiguous_wide: false,
            accessible: false,
            table_col_offset: 0,
            image_epoch: 0,
            reading_wpm: 230,
            images_on: false,
            text_align: TextAlign::Center,
            margin: 0,
            paragraph_spacing: 1,
            line_spacing: 0,
            word_spacing: 0,
            justify: false,
            hyphenate: false,
        }
    }
}

/// Resolves an image source URL to the cell box (`cols`, `rows`) the layout
/// should reserve for it, or `None` to fall back to the `[image: alt]`
/// placeholder (image not decoded yet, images disabled, or below the size
/// tier). Keeps `layout_document` decoupled from the `App`'s image store: the
/// caller precomputes the boxes and hands them in.
pub trait ImageResolver {
    fn image_box(&self, src: &str) -> Option<(u16, u16)>;
}

/// The no-image resolver: every image renders as its alt-text placeholder.
/// Test-only — the reading view always builds a real (possibly empty) box map
/// and calls [`layout_document_with_images`]; `NoImages` backs the
/// image-unaware [`layout_document`] convenience the tests use.
#[cfg(test)]
pub struct NoImages;

#[cfg(test)]
impl ImageResolver for NoImages {
    fn image_box(&self, _src: &str) -> Option<(u16, u16)> {
        None
    }
}

impl ImageResolver for std::collections::HashMap<String, (u16, u16)> {
    fn image_box(&self, src: &str) -> Option<(u16, u16)> {
        self.get(src).copied()
    }
}

/// PRD FR-RD-8 / §6.3 box caps: a single image never occupies more than this
/// many columns or rows, so a huge thumbnail can't eat the screen. Width is
/// additionally clamped to the content column at layout time.
pub const IMAGE_MAX_COLS: u16 = 60;
pub const IMAGE_MAX_ROWS: u16 = 24;
/// PRD §6.3 degradation: below this content width, inline images are not
/// rendered as half-blocks at all (the box would be too small to read) — the
/// alt-text placeholder shows instead. A documented, acceptable tier floor.
pub const IMAGE_MIN_COLS: u16 = 16;
/// Below this content width a gallery renders as a vertical list rather than
/// a horizontal captioned strip (PRD FR-RD-8 "captioned strips or lists").
/// Chosen above the `measure` floor (40) so a narrow reading column
/// (`:set`/config `measure = 40`, or a sub-60 column) reaches the list form,
/// while the default 88-cell column gets the strip.
const GALLERY_STRIP_MIN_WIDTH: usize = 60;

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
    /// into view. A link inside a folded section (PRD FR-NV-3) is not emitted
    /// as a visible span, so its entry here stays `0` and its `link_visible`
    /// flag is `false` — callers must consult `link_visible` before trusting
    /// this line number for a folded-away link.
    pub link_lines: Vec<usize>,
    /// Whether each link occurrence (same index space as `link_lines`/
    /// `doc::collect_links`) actually appears on a visible line. `false` for a
    /// link swallowed by a folded section (PRD FR-NV-3): folded links keep
    /// their global occurrence index — so numbering stays aligned with
    /// `collect_links` — but are not focusable, so Tab-cycling and the
    /// scroll-into-view helper skip them.
    pub link_visible: Vec<bool>,
    /// The sorted heading-block indices folded shut in this layout (PRD
    /// FR-NV-3). Part of the identity of the laid-out lines — two otherwise
    /// identical layouts with different folds are different layouts — so
    /// `App::ensure_layout` compares it (alongside `width`/`options`) to decide
    /// staleness, and it participates in the L1 cache key ([`LayoutCacheKey`]).
    pub folds: Vec<usize>,
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

impl Layout {
    /// PRD FR-NV-9's click-to-follow: which link occurrence (if any) sits
    /// under `(line_index, col)`, both already in the laid-out document's own
    /// coordinate space — `line_index` an index into `lines`, `col` a
    /// **display-cell** offset from that line's own start (`main::
    /// link_at_click` is responsible for turning an absolute terminal
    /// `(row, col)` mouse position into these units first, by subtracting the
    /// content area's origin and adding the current scroll offset).
    ///
    /// Deliberately does *not* reuse `link_cols`/`link_lines` (grapheme-
    /// cluster units, per their own doc comments): a mouse click's column is
    /// a display-cell offset, and the two units only coincide for narrow
    /// text. Scanning the line's spans by cumulative display width instead
    /// keeps this correct for exactly the wide/CJK content FR-RD-10 cares
    /// about, at the cost of an O(spans-per-line) scan instead of an O(1)
    /// index — cheap either way for one click.
    pub fn link_at(&self, line_index: usize, col: usize, ambiguous_wide: bool) -> Option<usize> {
        let line = self.lines.get(line_index)?;
        let mut acc = 0usize;
        for span in &line.spans {
            let w = display_width(&span.text, ambiguous_wide);
            if col >= acc && col < acc + w {
                return match span.kind {
                    SpanKind::Link(idx) => Some(idx),
                    _ => None,
                };
            }
            acc += w;
        }
        None
    }
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

/// A single collapsed inter-word gap `width` cells wide (PRD FR-PC-1's
/// `word_spacing`: `1` normally, `1 + word_spacing` when widened). Built
/// directly rather than through `make_cluster`, whose `is_space` branch
/// hardcodes width 1 regardless of the text's actual length — here the
/// extra cell(s) must count toward line-fill width math so a widened gap
/// still wraps correctly and the no-overflow invariant holds.
fn space_cluster(kind: SpanKind, width: usize) -> Cluster {
    let width = width.max(1);
    Cluster {
        text: " ".repeat(width),
        width,
        kind,
        is_space: true,
        is_newline: false,
        cjk: false,
        first_char: ' ',
        last_char: ' ',
    }
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

/// PRD FR-RD-1: the `syntax::TokenKind`-to-`SpanKind` mapping. Kept here (not
/// in `syntax`) so the tokenizer stays decoupled from the layout's span
/// vocabulary — see [`SpanKind`]'s `CodeKeyword`/… doc comment for why these
/// reuse existing theme slots rather than adding new ones.
fn token_span_kind(k: crate::syntax::TokenKind) -> SpanKind {
    match k {
        crate::syntax::TokenKind::Plain => SpanKind::Code,
        crate::syntax::TokenKind::Keyword => SpanKind::CodeKeyword,
        crate::syntax::TokenKind::Str => SpanKind::CodeString,
        crate::syntax::TokenKind::Comment => SpanKind::CodeComment,
        crate::syntax::TokenKind::Number => SpanKind::CodeNumber,
    }
}

/// PRD FR-RD-1: tokenize one code line and turn its `(text, kind)` runs into
/// width-measured clusters carrying per-token `SpanKind`s — so highlighting
/// survives width-chunking (`chunk_by_width`) and span coalescing
/// (`finalize`) unchanged.
fn highlight_clusters(
    highlighter: &mut crate::syntax::Highlighter,
    line: &str,
    ambiguous_wide: bool,
) -> Vec<Cluster> {
    let mut out = Vec::new();
    for (text, tok) in highlighter.highlight_line(line) {
        let kind = token_span_kind(tok);
        for g in text.graphemes(true) {
            out.push(make_cluster(g, kind.clone(), ambiguous_wide));
        }
    }
    out
}

/// The semantic kind of an inline span in the given block context. Plain text
/// takes the block's body kind (e.g. `Quote` inside a blockquote); links are
/// numbered in document order so paint can resolve focus/visited state. Link
/// numbering must match `doc::collect_links` exactly.
fn span_kind(style: &SpanStyle, plain_kind: &SpanKind, link_counter: &mut usize) -> SpanKind {
    match style {
        // A pure same-page fragment anchor (`#cite_note-N`, `#cite_ref-N`, or
        // any other `#...` href) is a reference/footnote marker, not a
        // followable link — `doc::collect_links` excludes these from the
        // link set entirely (they already have footnote-peek via `K`'s
        // citations data, PRD FR-NV-4), so this must not hand one a
        // `SpanKind::Link` occurrence number either, or `link_counter` would
        // run ahead of `collect_links`'s (shorter) list and every later
        // occurrence index would point at the wrong link. `Dim` matches how
        // a bare `<sup>` (no nested link) already renders — the visual a
        // reference marker had before this fix stripped its followability.
        SpanStyle::Link(href) | SpanStyle::RedLink(href) if href.starts_with('#') => SpanKind::Dim,
        // PRD FR-DL-5: a redlink is still a link for numbering/cycling/hint
        // purposes — it counts here exactly like `Link` (this numbering must
        // match `doc::collect_links`'s order, which also counts both
        // variants as links) — its dim/struck styling is a paint-time
        // decision (`ui::kind_style`) keyed off `LinkRef::redlink`/
        // `App::confirmed_redlinks`, not a distinct `SpanKind`.
        SpanStyle::Link(_) | SpanStyle::RedLink(_) => {
            let occ = *link_counter;
            *link_counter += 1;
            SpanKind::Link(occ)
        }
        SpanStyle::Bold => SpanKind::Bold,
        SpanStyle::Italic => SpanKind::Italic,
        SpanStyle::Superscript => SpanKind::Dim,
        SpanStyle::Plain => plain_kind.clone(),
        // PRD FR-RD-7: styled distinctly at paint time (`ui::kind_style`);
        // the delimiter-wrapped, Unicode-normalized text itself is produced
        // by `flatten_spans` below, not here (this function only maps style
        // to *kind*, never rewrites `text`).
        SpanStyle::Math(_) => SpanKind::Math,
        // PRD FR-DL-4: dimmed only when `:set show-cn` is on (`ui::kind_style`
        // consults `App::show_cn` — a runtime toggle, so this stays a
        // distinct `SpanKind` rather than folding into `Dim` the way
        // `Superscript` does above); always its own kind so
        // `citation_needed_lines` can find it regardless of the toggle.
        SpanStyle::CitationNeeded => SpanKind::CitationNeeded,
    }
}

/// PRD FR-RD-7's inline rendering of a math node: the raw TeX passed through
/// `normalize_trivial_math` (simple sup/sub + Greek-letter cases become
/// Unicode; anything non-trivial stays raw) and wrapped in `⟨…⟩` so it reads
/// as a distinct unit from surrounding prose even before styling is applied.
/// Shared by `flatten_spans` (inline math) and `Emitter::emit_block`'s
/// `Block::Math` case (a standalone display equation) so both spellings
/// agree; `--dump`/`render_plain` deliberately does *not* call this — it
/// shows the raw `doc::Span`/`Block::Math` text untouched (PRD: "TeX as
/// plain text, no escapes").
fn render_math_display(tex: &str) -> String {
    // PRD FR-RD-7: the `math-layout` feature (off by default) upgrades the
    // inner rendering from B14's trivial normalization to the fuller — still
    // deliberately simple, still pure-Rust — Unicode math renderer; the `⟨…⟩`
    // delimiters and the passthrough-when-complex contract are unchanged, so
    // the default build stays byte-identical to B14.
    #[cfg(feature = "math-layout")]
    let inner = crate::doc::render_math_layout(tex);
    #[cfg(not(feature = "math-layout"))]
    let inner = crate::doc::normalize_trivial_math(tex);
    format!("⟨{inner}⟩")
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
        let rendered;
        let text: &str = if let SpanStyle::Math(tex) = &span.style {
            rendered = render_math_display(tex);
            &rendered
        } else {
            &span.text
        };
        out.extend(clusters_from_str(text, kind, ambiguous_wide));
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

/// The three optional wrap-shaping knobs `fill` needs, bundled so the many
/// non-prose call sites (table/infobox cells, gallery strips) can keep
/// passing "plain wrapping" as one value while prose call sites opt into
/// justification/hyphenation. `word_spacing` is PRD FR-PC-1's base inter-word
/// gap widening; `justify`/`hyphenate` are PRD FR-RD-9's v1.x additions.
#[derive(Clone, Copy)]
struct WrapOpts {
    word_spacing: usize,
    justify: bool,
    hyphenate: bool,
}

impl WrapOpts {
    /// Plain wrapping with no justify/hyphenate — the byte-identical
    /// pre-FR-RD-9 behavior, carrying only `word_spacing` through. Every
    /// non-prose call site (grid cells, gallery captions) uses this.
    fn plain(word_spacing: usize) -> Self {
        Self {
            word_spacing,
            justify: false,
            hyphenate: false,
        }
    }
}

/// PRD FR-RD-9's river cap: the most extra cells justification will ever add
/// to a single inter-word gap. A width-wrapped line that would need more than
/// this per gap to reach the content column is left ragged instead — the
/// documented heuristic that keeps a sparse line (few words, or a long
/// just-wrapped word leaving a big hole) from opening an ugly "river" of
/// whitespace down the page. Three cells is generous enough that ordinary
/// greedily-packed lines (whose deficit is at most the width of the word that
/// didn't fit, spread over several gaps) justify cleanly, while a two-word
/// line with a gaping hole stays ragged.
const RIVER_CAP_MAX_EXTRA_PER_GAP: usize = 3;

/// PRD FR-RD-9 soft hyphenation: the shortest word (in grapheme clusters)
/// `try_hyphenate` will attempt to break. Below this, breaking buys too
/// little to be worth a mid-word hyphen. Chosen above en-US's own
/// left(2)+right(3) minima so the shortest hyphenation still leaves a
/// readable piece on each side.
const MIN_HYPHEN_WORD_LEN: usize = 6;

/// PRD FR-RD-9's embedded en-US Knuth-Liang patterns (the one language
/// wikitui enables; the crate bundles them, `embed_en-us` in `Cargo.toml`).
/// Loaded once, lazily, and shared — `None` only if the embedded data somehow
/// fails to decode, in which case hyphenation silently no-ops (ragged wrap,
/// never a panic). A word in any other script/language simply isn't
/// hyphenated: `try_hyphenate` restricts itself to ASCII-alphabetic words, so
/// accented-Latin and non-Latin text is left unbroken rather than mangled by
/// English patterns.
fn en_us_dict() -> Option<&'static hyphenation::Standard> {
    use std::sync::OnceLock;
    static EN_US: OnceLock<Option<hyphenation::Standard>> = OnceLock::new();
    EN_US
        .get_or_init(|| {
            use hyphenation::{Language, Load};
            hyphenation::Standard::from_embedded(Language::EnglishUS).ok()
        })
        .as_ref()
}

/// PRD FR-RD-9: try to break `clusters` (one word) so a prefix plus a
/// trailing `-` fits in `remaining` cells, at a Knuth-Liang-valid point.
/// Returns `(head, tail)` where `head` is the prefix clusters *plus* the
/// hyphen cluster (to end the current line) and `tail` is the remainder (to
/// continue on the next line), or `None` when no valid break fits.
///
/// Scope, all enforced here so the invariants hold: only ASCII-alphabetic
/// words (so a byte offset from the hyphenator lands exactly on a grapheme
/// boundary — no cluster is ever split — and non-English/CJK text is left
/// alone), only words of at least [`MIN_HYPHEN_WORD_LEN`] clusters, only
/// breaks the embedded en-US dictionary marks valid, and only the *largest*
/// such prefix that still leaves room for the `-` (best fill). The `-`
/// counts toward the width the caller checked, so the emitted line honors the
/// no-overflow invariant with the hyphen included.
fn try_hyphenate(clusters: &[Cluster], remaining: usize) -> Option<(Vec<Cluster>, Vec<Cluster>)> {
    use hyphenation::Hyphenator;
    if clusters.len() < MIN_HYPHEN_WORD_LEN || clusters.iter().any(|c| c.cjk) {
        return None;
    }
    let word: String = clusters.iter().map(|c| c.text.as_str()).collect();
    // ASCII-alphabetic only: keeps offset↔cluster mapping trivially 1:1 and
    // scopes hyphenation to the language whose patterns we actually loaded.
    if !word.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let dict = en_us_dict()?;
    let hyphen_w = 1; // a plain ASCII '-' is one cell.
    let mut best: Option<usize> = None; // cluster count in the prefix
    for &off in &dict.hyphenate(&word).breaks {
        // ASCII: byte offset == cluster index. Reject the degenerate ends.
        if off == 0 || off >= clusters.len() {
            continue;
        }
        let prefix_w: usize = clusters[..off].iter().map(|c| c.width).sum();
        if prefix_w + hyphen_w <= remaining {
            best = Some(best.map_or(off, |b| b.max(off)));
        }
    }
    let split = best?;
    let mut head: Vec<Cluster> = clusters[..split].to_vec();
    let hyphen_kind = head
        .last()
        .map(|c| c.kind.clone())
        .unwrap_or(SpanKind::Plain);
    head.push(make_cluster("-", hyphen_kind, false));
    Some((head, clusters[split..].to_vec()))
}

/// PRD FR-RD-9: how many extra cells each of `gaps` inter-word gaps receives
/// to close a `deficit`-cell shortfall, distributed as evenly as possible
/// with the remainder round-robined onto the *leading* gaps (a deterministic,
/// testable rule). `sum(spread_extra(d, g)) == d` exactly, which is what makes
/// a justified line reach the content column precisely.
fn spread_extra(deficit: usize, gaps: usize) -> Vec<usize> {
    if gaps == 0 {
        return Vec::new();
    }
    let base = deficit / gaps;
    let rem = deficit % gaps;
    (0..gaps).map(|k| base + usize::from(k < rem)).collect()
}

/// PRD FR-RD-9: stretch one width-wrapped prose line's inter-word gaps so it
/// exactly fills `avail`, or leave it untouched when it shouldn't justify —
/// a purely-CJK / single-word line (no gaps to stretch) or a line the river
/// cap ([`RIVER_CAP_MAX_EXTRA_PER_GAP`]) judges too sparse. Only the
/// [`Cluster::is_space`] gaps (which `fill` only ever places *between* words,
/// never leading/trailing) are widened, so no word or grapheme cluster is
/// ever touched and the link/find column mappings — recomputed from the final
/// lines — stay correct.
fn justify_line(line: &mut [Cluster], avail: usize) {
    let cur_w: usize = line.iter().map(|c| c.width).sum();
    if cur_w >= avail {
        return;
    }
    let deficit = avail - cur_w;
    let gap_idx: Vec<usize> = line
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_space)
        .map(|(i, _)| i)
        .collect();
    if gap_idx.is_empty() {
        return;
    }
    // River cap: the widest single gap this would open. `div_ceil` because
    // the round-robin remainder lands the extra cell on some gap.
    if deficit.div_ceil(gap_idx.len()) > RIVER_CAP_MAX_EXTRA_PER_GAP {
        return;
    }
    for (idx, extra) in gap_idx.iter().zip(spread_extra(deficit, gap_idx.len())) {
        if extra == 0 {
            continue;
        }
        let c = &mut line[*idx];
        c.width += extra;
        c.text = " ".repeat(c.width);
    }
}

/// Greedily pack pieces into lines no wider than `avail`. A piece wider than
/// `avail` is hard-split at cluster boundaries so nothing ever overflows.
/// `WrapOpts::word_spacing` (PRD FR-PC-1) widens every collapsed inter-word
/// gap from 1 cell to `1 + word_spacing`; the extra width is folded into the
/// same wrap-or-not check a plain 1-cell gap already used, so a widened gap
/// still wraps at the right point instead of silently overflowing.
///
/// PRD FR-RD-9 (both no-ops under their defaults, so the `WrapOpts::plain`
/// path is byte-identical to the pre-FR-RD-9 filler): when
/// `WrapOpts::hyphenate` is set, a word that would overflow a non-empty line
/// is offered to [`try_hyphenate`], which may place a hyphenated prefix on the
/// current line and continue the remainder on the next; when
/// `WrapOpts::justify` is set, every *width-wrapped* line (all but the last
/// line this call emits, which is the paragraph/run's ragged final line) is
/// stretched by [`justify_line`] to exactly fill `avail`.
fn fill(pieces: Vec<Piece>, avail: usize, opts: WrapOpts) -> Vec<Vec<Cluster>> {
    let avail = avail.max(1);
    let space_w = 1 + opts.word_spacing;
    let mut lines: Vec<Vec<Cluster>> = Vec::new();
    let mut cur: Vec<Cluster> = Vec::new();
    let mut cur_w = 0usize;
    let mut pending_space: Option<SpanKind> = None;

    let mut queue: std::collections::VecDeque<Piece> = pieces.into();
    while let Some(piece) = queue.pop_front() {
        let pw = piece.width();
        let sep_w = if pending_space.is_some() && !cur.is_empty() {
            space_w
        } else {
            0
        };
        if !cur.is_empty() && cur_w + sep_w + pw > avail {
            // PRD FR-RD-9: before giving up the rest of this line, try to
            // hyphenate the word that didn't fit onto its tail end.
            if opts.hyphenate
                && let Some((head, tail)) =
                    try_hyphenate(&piece.clusters, avail.saturating_sub(cur_w + sep_w))
            {
                // The width was already reserved in the fit check above; the
                // line is pushed and `cur_w` reset immediately, so we don't
                // bother re-accounting it here.
                if let Some(kind) = pending_space.take() {
                    cur.push(space_cluster(kind, space_w));
                }
                cur.extend(head);
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
                pending_space = None;
                queue.push_front(Piece {
                    clusters: tail,
                    sep: piece.sep,
                });
                continue;
            }
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            pending_space = None;
        }
        if let Some(kind) = pending_space.take()
            && !cur.is_empty()
        {
            cur.push(space_cluster(kind, space_w));
            cur_w += space_w;
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
    // PRD FR-RD-9 full justification: stretch every width-wrapped line — all
    // but the last, which is the ragged final line of this run — to fill the
    // column exactly. Done here, after packing, so the last-line-stays-ragged
    // rule is just "skip the final element."
    if opts.justify && lines.len() > 1 {
        let last = lines.len() - 1;
        for line in &mut lines[..last] {
            justify_line(line, avail);
        }
    }
    lines
}

/// Wrap a run of clusters (with hard `\n` breaks honored) into visual lines.
/// `opts` is threaded straight through to `fill` — each hard-`\n` sub-run is
/// filled independently, so the line before a hard break is that sub-run's
/// ragged last line (never justified), exactly like a paragraph's final line.
fn wrap_content(clusters: Vec<Cluster>, avail: usize, opts: WrapOpts) -> Vec<Vec<Cluster>> {
    let mut out = Vec::new();
    let mut sub: Vec<Cluster> = Vec::new();
    for c in clusters {
        if c.is_newline {
            out.extend(fill(build_pieces(&sub), avail, opts));
            sub = Vec::new();
        } else {
            sub.push(c);
        }
    }
    out.extend(fill(build_pieces(&sub), avail, opts));
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
    /// Reserves inline-image boxes (PRD FR-RD-8); `&NoImages` when images are
    /// off, so every image emits its alt-text placeholder instead.
    images: &'a dyn ImageResolver,
    /// PRD FR-PC-1 `paragraph_spacing`: how many blank rows `blank()` inserts
    /// at every block-separator call site (default 1, today's behavior).
    paragraph_spacing: usize,
    /// PRD FR-PC-1 `line_spacing`: how many blank rows `emit_wrapped` inserts
    /// after every visual (wrapped) line of prose — see its own field doc
    /// comment on `LayoutOptions` for the full honest framing and the
    /// find-glue trade-off.
    line_spacing: usize,
    /// PRD FR-PC-1 `word_spacing`: extra cells added to every collapsed
    /// inter-word gap in prose wrapping (`emit_wrapped`/`wrap_content`'s
    /// Emitter call sites) — see `fill`'s doc comment for the width-safety
    /// argument.
    word_spacing: usize,
    /// PRD FR-RD-9 `justify`/`hyphenate`: the two default-off wrap-shaping
    /// knobs. Applied only to *prose* blocks (paragraphs, list items,
    /// blockquotes), which pass them to `emit_wrapped` via
    /// [`Emitter::prose_wrap_opts`]; headings, math, captions, image
    /// placeholders, fold summaries, and every grid/box builder deliberately
    /// wrap plain (`emit_plain_wrapped`), so a heading or a display equation
    /// is never stretched flush or mid-word hyphenated.
    justify: bool,
    hyphenate: bool,
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

    /// The standard inter-block blank-line gap (PRD FR-PC-1
    /// `paragraph_spacing`): every block-separator call site in this emitter
    /// shares this one knob, so raising or lowering the gap is a single
    /// configuration change, never a per-block-kind rule. `paragraph_spacing
    /// = 0` means no gap at all — a still-valid, "tight" layout.
    fn blank(&mut self) {
        for _ in 0..self.paragraph_spacing {
            self.push_line(finalize(self.pad_width, &[], &[]), false);
        }
    }

    /// `line_spacing` blank filler rows, inserted immediately after one
    /// visual line of prose (PRD FR-PC-1) — factored out of `emit_wrapped`
    /// since it is pushed after *every* wrapped line, including the
    /// zero-content-line early return.
    fn line_spacing_filler(&mut self) {
        for _ in 0..self.line_spacing {
            self.push_line(finalize(self.pad_width, &[], &[]), false);
        }
    }

    /// The `WrapOpts` for a *prose* block: the session/tab `word_spacing` plus
    /// this emitter's resolved `justify`/`hyphenate` (PRD FR-RD-9). The one
    /// place those two knobs enter the filler, so a non-prose builder that
    /// wants plain wrapping simply doesn't call it (it uses
    /// [`WrapOpts::plain`] / [`Self::emit_plain_wrapped`] instead).
    fn prose_wrap_opts(&self) -> WrapOpts {
        WrapOpts {
            word_spacing: self.word_spacing,
            justify: self.justify,
            hyphenate: self.hyphenate,
        }
    }

    /// Emit a block whose content wraps under an optional hanging prefix
    /// (list bullet, blockquote gutter). Returns nothing; the caller records
    /// the anchor line before calling. `opts` selects plain vs.
    /// justified/hyphenated wrapping (PRD FR-RD-9) — prose blocks pass
    /// [`Self::prose_wrap_opts`], everything else passes
    /// [`WrapOpts::plain`].
    fn emit_wrapped(
        &mut self,
        content: Vec<Cluster>,
        first_prefix: Vec<LaidSpan>,
        cont_prefix: Vec<LaidSpan>,
        opts: WrapOpts,
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
        let wrapped = wrap_content(content, avail, opts);
        if wrapped.is_empty() {
            self.push_line(finalize(self.pad_width, &first_prefix, &[]), false);
            self.line_spacing_filler();
            return;
        }
        for (i, line) in wrapped.iter().enumerate() {
            let prefix = if i == 0 { &first_prefix } else { &cont_prefix };
            self.push_line(finalize(self.pad_width, prefix, line), i > 0);
            self.line_spacing_filler();
        }
    }

    fn emit_plain_wrapped(&mut self, content: Vec<Cluster>) {
        self.emit_wrapped(
            content,
            Vec::new(),
            Vec::new(),
            WrapOpts::plain(self.word_spacing),
        );
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
                // PRD FR-RD-9: body paragraphs are the prototypical prose that
                // justifies/hyphenates (when enabled) — every other block wraps
                // plain.
                self.emit_wrapped(content, Vec::new(), Vec::new(), self.prose_wrap_opts());
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
                self.emit_wrapped(content, first_prefix, cont_prefix, self.prose_wrap_opts());
                block_lines.push(anchor);
            }
            Block::Blockquote(spans) => {
                let anchor = self.lines.len();
                let gutter = vec![LaidSpan {
                    text: "▌ ".to_string(),
                    kind: SpanKind::Dim,
                }];
                let content = flatten_spans(spans, SpanKind::Quote, link_counter, aw);
                self.emit_wrapped(content, gutter.clone(), gutter, self.prose_wrap_opts());
                self.blank();
                block_lines.push(anchor);
            }
            Block::Code { text, lang } => {
                let anchor = self.lines.len();
                let gutter = "    ";
                let avail = self.content_width.saturating_sub(4).max(1);
                // PRD FR-RD-1: highlight only when the language hint resolves
                // to one the rule-based tokenizer covers; otherwise every line
                // is a single `SpanKind::Code` run (the prior uniform paint).
                // The highlighter carries block-comment state across lines, so
                // it's built once per block, not per line.
                let mut highlighter = lang
                    .as_deref()
                    .and_then(crate::syntax::Lang::detect)
                    .map(crate::syntax::Highlighter::new);
                for src in text.lines() {
                    let line_clusters = match highlighter.as_mut() {
                        Some(h) => highlight_clusters(h, src, aw),
                        None => clusters_from_str(src, SpanKind::Code, aw),
                    };
                    let chunks = chunk_by_width(line_clusters, avail);
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
            Block::Image { src, alt, caption } => {
                let anchor = self.lines.len();
                self.emit_image(src.as_deref(), alt);
                if let Some(caption) = caption {
                    self.emit_caption(caption);
                }
                self.blank();
                block_lines.push(anchor);
            }
            Block::Gallery(items) => {
                let anchor = self.lines.len();
                self.emit_gallery(items);
                self.blank();
                block_lines.push(anchor);
            }
            Block::Math { tex, display } => {
                let anchor = self.lines.len();
                self.emit_math(tex, *display);
                block_lines.push(anchor);
            }
        }
    }

    /// PRD FR-NV-3: emit one folded section's `▸ Title (N ¶, M subsections)`
    /// summary line in place of its whole body. `body` is the folded range's
    /// blocks (`doc.blocks[heading + 1 .. end]`); every block in it — the
    /// heading plus each body block — gets a `block_lines` anchor pointing at
    /// the summary line (keeping `block_lines` index-aligned with `doc.blocks`),
    /// and each body block's links advance `link_counter` without being laid
    /// out (so a visible link past the fold keeps its `collect_links` index but
    /// is left non-focusable). The `▸ Title` part is styled as the heading it
    /// stands in for; the dim count suffix reads as secondary.
    fn emit_fold_summary(
        &mut self,
        block_lines: &mut Vec<usize>,
        link_counter: &mut usize,
        heading_spans: &[crate::doc::Span],
        level: u8,
        body: &[Block],
    ) {
        let aw = self.ambiguous_wide;
        self.blank();
        let anchor = self.lines.len();
        let title = flatten_plain(heading_spans);
        let (paragraphs, subsections) = fold_body_counts(body);
        let mut content = clusters_from_str(&format!("▸ {title}"), SpanKind::Heading(level), aw);
        let suffix = {
            let sub = if subsections == 1 {
                "1 subsection".to_string()
            } else {
                format!("{subsections} subsections")
            };
            format!(" ({paragraphs} ¶, {sub})")
        };
        content.extend(clusters_from_str(&suffix, SpanKind::Dim, aw));
        self.emit_plain_wrapped(content);
        self.blank();
        // The heading's own anchor, then one per swallowed body block, all
        // pointing at the summary line — `block_lines` must stay exactly
        // `doc.blocks.len()` long and index-aligned for section jumps.
        block_lines.push(anchor);
        for block in body {
            *link_counter += block_link_count(block);
            block_lines.push(anchor);
        }
    }

    /// Emit a standalone math node (PRD FR-RD-7): a display equation centers
    /// on its own line (approximately — a leading pad wide enough to center
    /// the *first* wrapped line, reused as every continuation line's prefix
    /// too, rather than re-centering each one individually; exact per-line
    /// centering of a wrapped equation is not worth the complexity for v1.0
    /// passthrough). Reuses `emit_wrapped`'s existing centered-content-column
    /// math (via a blank-space "prefix") rather than hand-rolling padding, so
    /// a formula too wide for the column still wraps safely instead of
    /// overflowing. `display: false` (reached only if a math node with no
    /// `display="block"` signal was found directly at block-scanning level —
    /// see `doc::walk_blocks`'s `"span"` arm) just left-aligns like any other
    /// block.
    fn emit_math(&mut self, tex: &str, display: bool) {
        let aw = self.ambiguous_wide;
        let text = render_math_display(tex);
        self.blank();
        if display {
            let text_width = display_width(&text, aw);
            let extra_pad = self.content_width.saturating_sub(text_width) / 2;
            let pad_prefix = vec![LaidSpan {
                text: " ".repeat(extra_pad),
                kind: SpanKind::Plain,
            }];
            // Plain wrap: a display equation is centered, never stretched
            // flush or hyphenated (PRD FR-RD-9 justify/hyphenate are prose-only).
            self.emit_wrapped(
                clusters_from_str(&text, SpanKind::Math, aw),
                pad_prefix.clone(),
                pad_prefix,
                WrapOpts::plain(self.word_spacing),
            );
        } else {
            self.emit_plain_wrapped(clusters_from_str(&text, SpanKind::Math, aw));
        }
        self.blank();
    }

    /// Emit one inline image (PRD FR-RD-8): the reserved half-block box when
    /// the resolver hands back a decoded box (and the content column clears
    /// the [`IMAGE_MIN_COLS`] tier floor), else the `[image: alt]` placeholder
    /// on a single line. The box is `rows` [`SpanKind::ImageRow`] lines, each
    /// `cols` cells wide (its text is that many spaces); `ui.rs` fills the
    /// cells with the image's pixels at paint time.
    fn emit_image(&mut self, src: Option<&str>, alt: &str) {
        let aw = self.ambiguous_wide;
        let reserved = src.and_then(|s| self.images.image_box(s).map(|b| (s, b)));
        if let Some((src, (cols, rows))) = reserved {
            let cols = (cols as usize).min(self.content_width);
            if cols >= IMAGE_MIN_COLS as usize && rows >= 1 {
                for r in 0..rows {
                    let span = LaidSpan {
                        text: " ".repeat(cols),
                        kind: SpanKind::ImageRow {
                            src: src.to_string(),
                            row: r,
                            rows,
                        },
                    };
                    self.push_line(finalize(self.pad_width, &[span], &[]), r > 0);
                }
                return;
            }
        }
        self.emit_plain_wrapped(clusters_from_str(
            &format!("[image: {alt}]"),
            SpanKind::Image,
            aw,
        ));
    }

    /// Emit a caption line (PRD FR-RD-8): dim italics, wrapped to the content
    /// column.
    fn emit_caption(&mut self, caption: &str) {
        let aw = self.ambiguous_wide;
        self.emit_plain_wrapped(clusters_from_str(caption, SpanKind::Caption, aw));
    }

    /// Emit a gallery (PRD FR-RD-8 "captioned strips or lists"): a horizontal
    /// captioned strip when the content column is wide enough (captions laid
    /// across the row, wrapping), else a vertical list of `[image: caption]`.
    /// Gallery thumbnails are not decoded to half-blocks in this build (a
    /// documented scope choice — the strip/list is caption-only); the alt/
    /// caption text is always present.
    fn emit_gallery(&mut self, items: &[GalleryItem]) {
        let aw = self.ambiguous_wide;
        let strip = self.content_width >= GALLERY_STRIP_MIN_WIDTH && items.len() > 1;
        if strip {
            let joined = items
                .iter()
                .map(|it| format!("[{}]", it.caption))
                .collect::<Vec<_>>()
                .join("   ");
            let clusters = clusters_from_str(&joined, SpanKind::Caption, aw);
            for (i, wl) in wrap_content(
                clusters,
                self.content_width.max(1),
                WrapOpts::plain(self.word_spacing),
            )
            .into_iter()
            .enumerate()
            {
                self.push_line(finalize(self.pad_width, &[], &wl), i > 0);
            }
        } else {
            for item in items {
                self.emit_plain_wrapped(clusters_from_str(
                    &format!("[image: {}]", item.caption),
                    SpanKind::Image,
                    aw,
                ));
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
                    for wl in wrap_content(
                        clusters,
                        self.content_width.max(1),
                        WrapOpts::plain(self.word_spacing),
                    ) {
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
                images: self.images,
                paragraph_spacing: self.paragraph_spacing,
                line_spacing: self.line_spacing,
                word_spacing: self.word_spacing,
                justify: self.justify,
                hyphenate: self.hyphenate,
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
///
/// Deliberately plain-wrapped (PRD FR-PC-1/FR-RD-9): never widened by
/// `word_spacing`, never justified, never hyphenated. This feeds fixed-width
/// table/infobox grid cells, where a widened/stretched inter-word gap would
/// misalign the cell's own padding against its column border rather than make
/// it more readable, and a mid-word hyphen would collide with the border —
/// those knobs are scoped to prose wrapping (`Emitter::emit_wrapped`'s prose
/// call sites via `prose_wrap_opts`).
fn wrap_cell_text(text: &str, w: usize, aw: bool) -> Vec<String> {
    let clusters = clusters_from_str(text, SpanKind::Table, aw);
    let wrapped = wrap_content(clusters, w.max(1), WrapOpts::plain(0));
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
/// Image-unaware entry point (PRD FR-RD-8): every image renders as its
/// alt-text placeholder. Test-only convenience — the reading view uses
/// [`layout_document_with_images`] with a real box map.
#[cfg(test)]
pub fn layout_document(doc: &Document, width: u16, options: LayoutOptions) -> Layout {
    layout_document_with_images(doc, width, options, &NoImages, &[])
}

/// PRD FR-NV-3 section folding. `folds` is the sorted set of heading-block
/// indices (`doc.blocks` indices, as `doc::SectionRef::block` reports) that are
/// folded shut: each collapses the run from its heading to the next same-or-
/// higher-level heading into a single `▸ Title (N ¶, M subsections)` summary
/// line. Passed as a layout **input** (not a `LayoutOptions` field — that type
/// is `Copy` and shared across every width bucket) so folded content is never
/// laid out at all, keeping the no-overflow and `block_lines`/link-ordering
/// invariants intact for the visible content. Empty `folds` reproduces the
/// pre-folding layout byte for byte.
pub fn layout_document_with_images(
    doc: &Document,
    width: u16,
    options: LayoutOptions,
    images: &dyn ImageResolver,
    folds: &[usize],
) -> Layout {
    let available = (width.max(1)) as usize;
    // PRD FR-PC-1's `margin`: carved out of the *same* available-width
    // budget `measure`/centering already share, never added on top of it —
    // clamped so at least 1 cell of content column always remains, which is
    // what keeps `pad_width + content_width <= available` an unconditional
    // invariant (the no-overflow property tests rely on it) regardless of
    // how large a margin is configured on a narrow terminal.
    let margin = (options.margin as usize).min(available.saturating_sub(1));
    let avail_after_margin = (available - margin).max(1);
    let content_width = avail_after_margin.min((options.measure.max(1)) as usize);
    let center_pad = match options.text_align {
        TextAlign::Center => avail_after_margin.saturating_sub(content_width) / 2,
        TextAlign::Left => 0,
    };
    let pad_width = margin + center_pad;
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
            images,
            paragraph_spacing: options.paragraph_spacing as usize,
            line_spacing: options.line_spacing as usize,
            word_spacing: options.word_spacing as usize,
            justify: options.justify,
            hyphenate: options.hyphenate,
        };
        // Title, then a blank line — mirrors the previous renderer's header.
        em.emit_plain_wrapped(clusters_from_str(&doc.title, SpanKind::Title, aw));
        // PRD FR-RD-11: "N min read" in the article header area, right under
        // the title. A pure function of the document + configured WPM, so —
        // unlike the quality badge (network data, shown in the status bar
        // instead, `ui::draw_status_bar`) — it is safe to bake into the
        // cached layout: nothing about it can arrive *after* this layout
        // pass already ran.
        let words = crate::doc::word_count(doc);
        let minutes = crate::doc::reading_minutes(words, options.reading_wpm);
        if minutes > 0 {
            em.emit_plain_wrapped(clusters_from_str(
                &format!("{minutes} min read"),
                SpanKind::Dim,
                aw,
            ));
        }
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
            // PRD FR-NV-3: a folded heading collapses its whole range to one
            // summary line. The range's blocks are never laid out — but their
            // links still advance the occurrence counter (so visible links
            // downstream keep their `collect_links`-aligned indices) and each
            // gets a `block_lines` anchor pointing at the summary line (so a
            // section jump into folded content lands on the fold, and
            // `block_lines` stays exactly `doc.blocks.len()` long).
            if let Block::Heading { level, spans } = &doc.blocks[i]
                && folds.contains(&i)
            {
                let end = fold_range_end(&doc.blocks, i, *level);
                em.emit_fold_summary(
                    &mut block_lines,
                    &mut link_counter,
                    spans,
                    *level,
                    &doc.blocks[i + 1..end],
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
        // A link occurrence that never appeared as a span (folded away) keeps
        // `seen[occ] == false` — exactly the "not focusable" set PRD FR-NV-3
        // needs, derived from the same pass that fills `link_lines`.
        link_visible: seen,
        link_cols,
        continuation,
        folds: folds.to_vec(),
    }
}

/// PRD FR-NV-3: the exclusive end of a section's block range — the first block
/// after `heading` that is a heading of the same or higher level (lower or
/// equal `level` number), or the end of the document. The range folded shut is
/// `[heading, end)`; the heading itself collapses to the summary line and
/// `heading + 1 .. end` is the body swallowed by the fold.
pub fn fold_range_end(blocks: &[Block], heading: usize, level: u8) -> usize {
    blocks[heading + 1..]
        .iter()
        .position(|b| matches!(b, Block::Heading { level: l, .. } if *l <= level))
        .map(|off| heading + 1 + off)
        .unwrap_or(blocks.len())
}

/// PRD FR-NV-3's fold-summary counts: how many paragraphs and how many
/// (deeper) subsection headings live in a folded range's body `blocks`
/// (`doc.blocks[heading + 1 .. end]`). Every heading in that slice is deeper by
/// construction (the range ends at the next same-or-higher heading), so a plain
/// heading count is the subsection count.
pub fn fold_body_counts(body: &[Block]) -> (usize, usize) {
    let paragraphs = body
        .iter()
        .filter(|b| matches!(b, Block::Paragraph(_)))
        .count();
    let subsections = body
        .iter()
        .filter(|b| matches!(b, Block::Heading { .. }))
        .count();
    (paragraphs, subsections)
}

/// The number of link occurrences in one block, counted exactly as
/// `flatten_spans`/`doc::collect_links` do (both `Link` and `RedLink`, only in
/// the block kinds those two functions descend into). Used to advance the
/// occurrence counter across a folded block without laying it out, so a visible
/// link after the fold keeps the same global index the unfolded layout gave it.
///
/// The `#`-fragment exclusion is load-bearing and must mirror `span_kind`/
/// `doc::collect_links` exactly: a pure same-page fragment anchor
/// (`#cite_note-N`, a reference marker) is *not* handed a `SpanKind::Link`
/// occurrence by `span_kind`, so folding a section whose body holds one must
/// not advance `link_counter` for it either. Counting it here would push every
/// real link after the fold to a phantom index that `link_lines`/`link_visible`
/// never fill, silently hiding an on-screen link from Tab-cycling (the very
/// desync the ordering invariant forbids).
fn block_link_count(block: &Block) -> usize {
    let count = |spans: &[crate::doc::Span]| {
        spans
            .iter()
            .filter(|s| match &s.style {
                SpanStyle::Link(href) | SpanStyle::RedLink(href) => !href.starts_with('#'),
                _ => false,
            })
            .count()
    };
    match block {
        Block::Paragraph(spans) | Block::Blockquote(spans) => count(spans),
        Block::ListItem { spans, .. } => count(spans),
        _ => 0,
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

/// PRD FR-DL-4's `]c`/`[c` jump targets: the line index of every rendered
/// `{{citation needed}}` marker (`SpanKind::CitationNeeded`), one entry per
/// occurrence — mirroring `find_matches`' "one entry per hit, never
/// double-counted across a soft wrap" contract. A marker that wraps across a
/// `continuation`-linked line break is still one stop, keyed to the line the
/// run started on.
///
/// Documented simplification, honest rather than silently wrong: two
/// *separate* markers that happen to land on the exact same rendered line
/// (both fit on one wide-terminal row with no wrap between them) collapse to
/// one stop here — a jump-granularity trade-off, not a miscount. The
/// status-bar count (`doc::count_citation_needed`) is the authoritative
/// template count and is never derived from this list.
pub fn citation_needed_lines(lines: &[LaidLine], continuation: &[bool]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut in_run = false;
    for (i, line) in lines.iter().enumerate() {
        let has_marker = line
            .spans
            .iter()
            .any(|s| s.kind == SpanKind::CitationNeeded);
        let continues_prev_run =
            in_run && i > 0 && continuation.get(i - 1).copied().unwrap_or(false);
        if has_marker && !continues_prev_run {
            out.push(i);
        }
        in_run = has_marker;
    }
    out
}

/// Bump whenever `Layout`/`LaidLine`'s shape, or `layout_document`'s
/// wrapping/breaking semantics, change in a way that would make an old
/// cached `Layout` wrong to keep serving. Participates in
/// [`LayoutCacheKey`] (PRD FR-OFF-1's L1 layer) so a stale schema can never
/// be silently replayed across an upgrade — a version bump makes every
/// existing L1 entry a guaranteed miss instead.
/// v5 (PRD FR-PC-1): line emission gained `paragraph_spacing`/`line_spacing`/
/// `word_spacing`, which insert or widen rows a v4-shaped `LayoutOptions`
/// never accounted for.
/// v6 (PRD FR-RD-9): line emission gained `justify` (stretches a wrapped
/// line's inter-word gaps to fill the column) and `hyphenate` (may break a
/// word with a trailing `-`), both of which change the emitted glyphs a
/// v5-shaped `LayoutOptions` never accounted for.
pub const LAYOUT_SCHEMA_VERSION: u32 = 6;

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
    /// PRD FR-NV-3: the sorted folded-heading-block set (see `Layout::folds`).
    /// Folding changes the laid-out lines, so a layout laid out under one fold
    /// set must never be served for another — it is part of the L1 key exactly
    /// like `width`/`options`.
    pub folds: Vec<usize>,
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

    const IMG_HTML: &str = r#"<html><head><title>Img</title></head><body>
      <p>Lead.</p>
      <figure><img src="https://ex.org/p.png" alt="An owl"/><figcaption>An owl</figcaption></figure>
    </body></html>"#;

    fn image_rows(layout: &Layout) -> usize {
        layout
            .lines
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| matches!(s.kind, SpanKind::ImageRow { .. }))
            })
            .count()
    }

    /// Collect the distinct `SpanKind` discriminants present across a layout,
    /// as a helper for the FR-RD-1 highlighting assertions below.
    fn code_kinds_present(layout: &Layout) -> Vec<SpanKind> {
        let mut kinds = Vec::new();
        for line in &layout.lines {
            for span in &line.spans {
                let is_code_token = matches!(
                    span.kind,
                    SpanKind::Code
                        | SpanKind::CodeKeyword
                        | SpanKind::CodeString
                        | SpanKind::CodeComment
                        | SpanKind::CodeNumber
                );
                if is_code_token && !kinds.contains(&span.kind) {
                    kinds.push(span.kind.clone());
                }
            }
        }
        kinds
    }

    const RUST_CODE_HTML: &str = r#"<html><head><title>Code</title></head><body>
      <p>Lead.</p>
      <pre class="mw-highlight mw-highlight-lang-rust"><code>fn main() {
    let x = "hi"; // greet
}</code></pre>
    </body></html>"#;

    /// PRD FR-RD-1: a fenced code block whose language the highlighter covers
    /// gets multiple distinct token colors — keyword, string, and comment all
    /// render in different `SpanKind`s (hence different theme slots).
    #[test]
    fn highlighted_code_block_has_distinct_token_kinds() {
        let doc = parse_article_html("Code", RUST_CODE_HTML);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let kinds = code_kinds_present(&layout);
        assert!(
            kinds.contains(&SpanKind::CodeKeyword),
            "fn/let are keywords: {kinds:?}"
        );
        assert!(
            kinds.contains(&SpanKind::CodeString),
            "\"hi\" is a string: {kinds:?}"
        );
        assert!(
            kinds.contains(&SpanKind::CodeComment),
            "// greet is a comment: {kinds:?}"
        );
    }

    /// PRD FR-RD-1's plain fallback: a code block with no/unknown language
    /// hint stays uniform `SpanKind::Code` — no token classes emitted.
    #[test]
    fn unhighlighted_code_block_stays_uniform_code() {
        let html = r#"<html><head><title>C</title></head><body>
          <pre>fn main() { let x = "hi"; }</pre>
        </body></html>"#;
        let doc = parse_article_html("C", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let kinds = code_kinds_present(&layout);
        assert_eq!(
            kinds,
            vec![SpanKind::Code],
            "no lang hint -> uniform code: {kinds:?}"
        );
    }

    #[test]
    fn image_without_resolver_box_is_the_alt_placeholder() {
        let doc = parse_article_html("Img", IMG_HTML);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        assert_eq!(image_rows(&layout), 0, "no box reserved without a resolver");
        let has_placeholder = layout.lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| matches!(s.kind, SpanKind::Image) && s.text.contains("[image: An owl]"))
        });
        assert!(has_placeholder, "expected the [image: alt] placeholder");
        // The caption still renders below the placeholder.
        assert!(layout.lines.iter().any(|l| l.spans.iter().any(|s| matches!(
            s.kind,
            SpanKind::Caption
        )
            && s.text.contains("An owl"))));
    }

    #[test]
    fn image_with_resolver_reserves_a_half_block_box_and_caption() {
        let doc = parse_article_html("Img", IMG_HTML);
        let mut boxes = std::collections::HashMap::new();
        boxes.insert("https://ex.org/p.png".to_string(), (20u16, 5u16));
        let layout = layout_document_with_images(&doc, 80, LayoutOptions::default(), &boxes, &[]);
        assert_eq!(image_rows(&layout), 5, "five reserved image rows");
        // Each image row is exactly the reserved width, and rows count is
        // carried in every row's kind.
        for l in &layout.lines {
            for s in &l.spans {
                if let SpanKind::ImageRow { rows, .. } = s.kind {
                    assert_eq!(rows, 5);
                    assert_eq!(s.text.chars().count(), 20);
                }
            }
        }
        assert!(
            layout.lines.iter().any(|l| l
                .spans
                .iter()
                .any(|s| matches!(&s.kind, SpanKind::Caption) && s.text.contains("An owl"))),
            "caption below the box"
        );
    }

    #[test]
    fn huge_image_box_is_clamped_to_the_content_column() {
        let doc = parse_article_html("Img", IMG_HTML);
        let mut boxes = std::collections::HashMap::new();
        // Resolver hands back an over-wide box; layout must clamp cols to the
        // content column (width 30 → content 30).
        boxes.insert("https://ex.org/p.png".to_string(), (200u16, 4u16));
        let layout = layout_document_with_images(&doc, 30, LayoutOptions::default(), &boxes, &[]);
        for l in &layout.lines {
            for s in &l.spans {
                if matches!(s.kind, SpanKind::ImageRow { .. }) {
                    assert!(
                        s.text.chars().count() <= 30,
                        "image row wider than content column"
                    );
                }
            }
        }
    }

    #[test]
    fn gallery_is_a_strip_when_wide_and_a_list_when_narrow() {
        let html = r#"<html><head><title>G</title></head><body>
          <ul class="gallery">
            <li class="gallerybox"><img src="https://ex.org/1.png" alt="a"/><div class="gallerytext">Alpha</div></li>
            <li class="gallerybox"><img src="https://ex.org/2.png" alt="b"/><div class="gallerytext">Beta</div></li>
          </ul>
        </body></html>"#;
        let doc = parse_article_html("G", html);

        // Wide: both captions share one line (a horizontal strip).
        let wide = layout_document(&doc, 100, LayoutOptions::default());
        let strip_line = wide.lines.iter().any(|l| {
            let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
            text.contains("Alpha") && text.contains("Beta")
        });
        assert!(
            strip_line,
            "wide gallery should place captions side by side"
        );

        // Narrow: each caption on its own [image: …] line (a list).
        let narrow = layout_document(&doc, 24, LayoutOptions::default());
        let alpha_line = narrow
            .lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.text.contains("Alpha")));
        let beta_line = narrow
            .lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.text.contains("Beta")));
        assert!(alpha_line.is_some() && beta_line.is_some());
        assert_ne!(alpha_line, beta_line, "narrow gallery is a vertical list");
        assert!(
            narrow
                .lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.text.contains("[image: Alpha]")))
        );
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

    // ---- FR-PC-1: spacing/typography options -------------------------

    /// A tiny, hand-verifiable fixture for the spacing tests below: two short
    /// paragraphs, nothing else, so the exact line sequence a given set of
    /// spacing options produces can be reasoned about directly instead of
    /// through a big fixture's incidental structure.
    const TWO_PARAGRAPHS: &str = r#"<html><head><title>Spacing Test</title></head><body>
      <p>First paragraph here.</p>
      <p>Second paragraph here.</p>
    </body></html>"#;

    /// PRD FR-PC-1: `LayoutOptions::default()` must reproduce the exact
    /// pre-FR-PC-1 layout — a golden-ish regression guard so a future change
    /// to a default value (rather than to an explicit `:set`) doesn't slip
    /// by unnoticed. Pinned against the same `FIXTURE` several other tests
    /// already exercise, at a width wide enough to also exercise the default
    /// centered `text_align`.
    #[test]
    fn default_options_reproduce_the_pre_fr_pc_1_layout() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let layout = layout_document(&doc, 90, LayoutOptions::default());
        // measure=88 inside a 90-wide terminal centers with a 1-cell pad —
        // the default `text_align = Center`, `margin = 0`.
        assert_eq!(
            layout.lines[0].spans[0].text, " ",
            "default centering pads (90-88)/2 = 1 cell"
        );
        // No `line_spacing` filler anywhere under the default (0): the "N min
        // read" line (FR-RD-11) immediately follows the title with no blank
        // row of its own, then exactly one unconditional block-separator
        // blank (paragraph_spacing=1), then the first real block — never two.
        let min_read = layout
            .lines
            .iter()
            .position(|l| line_text(l).contains("min read"))
            .expect("a nonempty article always reports a nonzero reading time");
        assert_eq!(
            min_read, 1,
            "immediately follows the title, no filler row between"
        );
        assert_eq!(line_text(&layout.lines[min_read + 1]).trim(), "");
        assert_ne!(
            line_text(&layout.lines[min_read + 2]).trim(),
            "",
            "default paragraph_spacing=1 means exactly one blank row, not two"
        );
        // Words stay separated by exactly one space (default `word_spacing =
        // 0`): a known two-word run from the fixture renders with a single
        // collapsed space, never a widened gap.
        let joined: String = layout
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("bold and"),
            "default word_spacing=0 keeps a single collapsed space: {joined:?}"
        );
    }

    /// `paragraph_spacing = 2` doubles every block-separator gap — a second
    /// paragraph's lead line now sits two blank rows below the first's, not
    /// one.
    #[test]
    fn paragraph_spacing_two_adds_a_second_blank_line_between_paragraphs() {
        let doc = parse_article_html("Spacing Test", TWO_PARAGRAPHS);
        let opts = LayoutOptions {
            paragraph_spacing: 2,
            ..LayoutOptions::default()
        };
        let layout = layout_document(&doc, 40, opts);
        let first = layout
            .lines
            .iter()
            .position(|l| line_text(l).contains("First paragraph"))
            .expect("first paragraph line");
        let second = layout
            .lines
            .iter()
            .position(|l| line_text(l).contains("Second paragraph"))
            .expect("second paragraph line");
        let gap = second - first - 1;
        assert_eq!(gap, 2, "two blank rows between the paragraphs' own lines");
        for i in first + 1..second {
            assert_eq!(
                line_text(&layout.lines[i]).trim(),
                "",
                "gap row {i} must be blank"
            );
        }

        // The default (paragraph_spacing=1) is exactly one blank row, so this
        // really is "a second blank line," not just "the fixture changed."
        let default_layout = layout_document(&doc, 40, LayoutOptions::default());
        let d_first = default_layout
            .lines
            .iter()
            .position(|l| line_text(l).contains("First paragraph"))
            .unwrap();
        let d_second = default_layout
            .lines
            .iter()
            .position(|l| line_text(l).contains("Second paragraph"))
            .unwrap();
        assert_eq!(d_second - d_first - 1, 1);
    }

    /// `line_spacing = 1` inserts one blank row after *every* wrapped line of
    /// prose — including a single-line paragraph, so the gap between two
    /// consecutive one-line paragraphs grows by exactly 1 on top of whatever
    /// `paragraph_spacing` already contributes.
    #[test]
    fn line_spacing_one_inserts_a_blank_after_each_visual_line() {
        let doc = parse_article_html("Spacing Test", TWO_PARAGRAPHS);
        let opts = LayoutOptions {
            line_spacing: 1,
            ..LayoutOptions::default()
        };
        let layout = layout_document(&doc, 40, opts);
        let first = layout
            .lines
            .iter()
            .position(|l| line_text(l).contains("First paragraph"))
            .expect("first paragraph line");
        let second = layout
            .lines
            .iter()
            .position(|l| line_text(l).contains("Second paragraph"))
            .expect("second paragraph line");
        // line_spacing's own filler (1) plus the default paragraph_spacing's
        // separator (1) = 2 blank rows between the two paragraph lines.
        assert_eq!(
            second - first - 1,
            2,
            "line_spacing=1 adds its own filler row on top of paragraph_spacing's"
        );

        // A multi-line paragraph gets a filler after *each* of its own
        // wrapped lines, not just at the end of the block.
        let long = "word ".repeat(30);
        let html = format!("<html><body><p>{long}</p></body></html>");
        let doc2 = parse_article_html("Long", &html);
        let opts2 = LayoutOptions {
            line_spacing: 1,
            ..LayoutOptions::default()
        };
        let laid = layout_document(&doc2, 20, opts2);
        // Only the paragraph's own wrapped lines (never the title line above
        // it, which sits behind an extra unconditional block-separator blank
        // that would otherwise throw off the "every gap is 2" pattern below).
        let content_lines: Vec<usize> = laid
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| line_text(l).contains("word"))
            .map(|(i, _)| i)
            .collect();
        assert!(
            content_lines.len() >= 3,
            "the long paragraph wraps to several lines"
        );
        for w in content_lines.windows(2) {
            assert_eq!(
                w[1] - w[0],
                2,
                "each content line is followed by exactly one filler blank"
            );
        }
    }

    /// `word_spacing = 1` widens every collapsed inter-word gap by one cell
    /// and the extra width still counts toward wrapping — a line built from
    /// widened gaps never overflows, and packs (very slightly) fewer words
    /// per line than the default.
    #[test]
    fn word_spacing_one_widens_gaps_and_still_wraps_within_width() {
        let doc = parse_article_html("T", "<html><body><p>Alpha Beta Gamma</p></body></html>");
        let widened = layout_document(
            &doc,
            40,
            LayoutOptions {
                word_spacing: 1,
                ..LayoutOptions::default()
            },
        );
        let plain = layout_document(&doc, 40, LayoutOptions::default());
        let plain_line = plain
            .lines
            .iter()
            .find(|l| line_text(l).contains("Alpha"))
            .expect("the paragraph's own line");
        assert_eq!(line_text(plain_line).trim(), "Alpha Beta Gamma");
        let widened_line = widened
            .lines
            .iter()
            .find(|l| line_text(l).contains("Alpha"))
            .expect("the paragraph's own line");
        assert_eq!(
            line_text(widened_line).trim(),
            "Alpha  Beta  Gamma",
            "word_spacing=1 doubles every inter-word gap to 2 cells"
        );

        // No-overflow across the CJK and long-ASCII-token fixtures, exactly
        // like the existing width-invariant sweep, but with word_spacing on.
        let docs = [
            parse_article_html("Test Article", FIXTURE),
            parse_article_html("アラン・チューリング", JA_FIXTURE),
            hard_cases_doc(),
        ];
        for doc in &docs {
            for width in [20u16, 40, 60, 80, 100, 200] {
                assert_no_overflow(
                    doc,
                    width,
                    LayoutOptions {
                        word_spacing: 1,
                        ..LayoutOptions::default()
                    },
                );
            }
        }
    }

    /// `text_align = left` drops the centering pad entirely — the column
    /// still never exceeds `measure`, but it now hugs the left edge instead
    /// of floating in the middle of a wide terminal. Uses `TWO_PARAGRAPHS`
    /// (no infobox/table) rather than `FIXTURE`: a floated infobox's own
    /// lead/card merge legitimately emits its *own* all-space filler spans
    /// (padding a lead row with no text out to the card's column) that have
    /// nothing to do with `text_align` and would make a blanket "no line
    /// starts with an all-space span" assertion a false positive.
    #[test]
    fn text_align_left_removes_the_centering_pad() {
        let doc = parse_article_html("Spacing Test", TWO_PARAGRAPHS);
        let centered = layout_document(&doc, 200, LayoutOptions::default());
        let left = layout_document(
            &doc,
            200,
            LayoutOptions {
                text_align: TextAlign::Left,
                ..LayoutOptions::default()
            },
        );
        // Centered: measure=88 inside 200 cells pads (200-88)/2 = 56 cells.
        assert_eq!(display_width(&centered.lines[0].spans[0].text, false), 56);
        assert_eq!(centered.lines[0].spans[0].kind, SpanKind::Plain);
        // Left: the title's own first span is the title content directly —
        // no pad span at all, since `finalize` only ever emits one when
        // `pad_width > 0`.
        assert_eq!(left.lines[0].spans[0].kind, SpanKind::Title);
        for line in &left.lines {
            assert!(line.width(false) <= 88, "content still capped at measure");
        }
    }

    /// `margin` shifts the whole column right by that many cells (carved out
    /// of the same width budget as `measure`, never added on top of it), and
    /// combines with `text_align = left` as "hug the margin, not the center."
    #[test]
    fn margin_adds_a_left_offset_without_overflowing() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let opts = LayoutOptions {
            margin: 5,
            text_align: TextAlign::Left,
            ..LayoutOptions::default()
        };
        let layout = layout_document(&doc, 60, opts);
        assert_eq!(
            layout.lines[0].spans[0].text, "     ",
            "left-aligned with a 5-cell margin and no centering pad"
        );
        for line in &layout.lines {
            assert!(
                line.width(false) <= 60,
                "margin must never push a line past the terminal width"
            );
        }
    }

    /// PRD FR-PC-1 + FR-NV-3/6/8: with `paragraph_spacing = 2` and
    /// `line_spacing = 1` both active, section-jump anchors
    /// (`block_lines`), link occurrence mapping (`link_lines`/`link_cols`),
    /// and in-page find (`find_matches`) must all still land on the correct
    /// laid-out line — the extra blank rows must never desynchronize these
    /// mappings from the content they describe.
    #[test]
    fn block_and_link_and_find_mappings_stay_correct_under_spacing() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let opts = LayoutOptions {
            paragraph_spacing: 2,
            line_spacing: 1,
            ..LayoutOptions::default()
        };
        let layout = layout_document(&doc, 80, opts);

        // Section jump: block_lines must still point at the heading's own
        // rendered text (same property `section_block_lines_land_on_the_
        // rendered_heading` checks under defaults).
        let sections = section_outline(&doc);
        assert_eq!(sections.len(), 1);
        for section in &sections {
            let line = layout.block_lines[section.block];
            assert_eq!(
                line_text(&layout.lines[line]).trim(),
                section.title,
                "block_lines must still land on the heading under spacing"
            );
        }

        // Link occurrence mapping: every link's own text is exactly what
        // `link_cols` slices out of its `link_lines` line.
        let links = collect_links(&doc);
        assert_eq!(layout.link_cols.len(), links.len());
        for (occ, link) in links.iter().enumerate() {
            if !layout.link_visible[occ] {
                continue;
            }
            let line = &layout.lines[layout.link_lines[occ]];
            let text = line_text(line);
            let graphemes: Vec<&str> = text.graphemes(true).collect();
            let span = layout.link_cols[occ];
            let sliced: String = graphemes[span.start..span.end].concat();
            assert_eq!(
                sliced, link.text,
                "link_cols[{occ}] must still bound the link's own text under spacing"
            );
        }

        // In-page find: a word unique to the fixture's body is found, and it
        // lands on a line that actually contains it (the extra blank rows
        // are never mistaken for a match, and the mapping is self-consistent
        // even though absolute line numbers shifted from the unspaced case).
        let occurrences = find_matches(&layout.lines, &layout.continuation, "history");
        assert!(
            !occurrences.is_empty(),
            "find must still find matches under spacing"
        );
        for occ in &occurrences {
            for (line_idx, span) in &occ.pieces {
                let text = line_text(&layout.lines[*line_idx]);
                let graphemes: Vec<&str> = text.graphemes(true).collect();
                let matched: String = graphemes[span.start..span.end].concat();
                assert_eq!(matched.to_lowercase(), "history");
            }
        }
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

    // ---- PRD FR-NV-3 section folding --------------------------------------

    /// Lead + two h2 sections; the first (`History`) contains two paragraphs
    /// and an h3 subsection, and holds a link that folding must hide. Every
    /// section has a distinct link so link visibility is checkable.
    const FOLD_FIXTURE: &str = r##"
    <html><head><title>Fold Test</title></head><body>
      <p>Lead paragraph with a <a href="./Lead_Link">lead link</a> here.</p>
      <h2>History</h2>
      <p>First history paragraph mentioning a <a href="./History_Link">history link</a> inline.</p>
      <h3>Early</h3>
      <p>Early sub paragraph body text.</p>
      <p>Second history-area paragraph body text.</p>
      <h2>Legacy</h2>
      <p>Legacy paragraph with a <a href="./Legacy_Link">legacy link</a>.</p>
    </body></html>
    "##;

    fn heading_block(doc: &Document, title: &str) -> usize {
        doc.blocks
            .iter()
            .position(
                |b| matches!(b, Block::Heading { spans, .. } if flatten_plain(spans) == title),
            )
            .unwrap_or_else(|| panic!("no heading {title:?}"))
    }

    #[test]
    fn fold_range_end_stops_at_the_next_same_or_higher_heading() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let history = heading_block(&doc, "History");
        let legacy = heading_block(&doc, "Legacy");
        // History (h2) folds through its h3 subsection, ending at the next h2.
        assert_eq!(fold_range_end(&doc.blocks, history, 2), legacy);
        // The last section folds to the end of the document.
        assert_eq!(fold_range_end(&doc.blocks, legacy, 2), doc.blocks.len());
    }

    #[test]
    fn fold_body_counts_count_paragraphs_and_subsections() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let history = heading_block(&doc, "History");
        let end = fold_range_end(&doc.blocks, history, 2);
        // Two paragraphs + the early-sub paragraph = 3 paragraphs; one h3.
        assert_eq!(fold_body_counts(&doc.blocks[history + 1..end]), (3, 1));
        let legacy = heading_block(&doc, "Legacy");
        let lend = fold_range_end(&doc.blocks, legacy, 2);
        assert_eq!(fold_body_counts(&doc.blocks[legacy + 1..lend]), (1, 0));
    }

    #[test]
    fn folding_a_section_collapses_its_body_to_one_summary_line() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let history = heading_block(&doc, "History");
        let folded =
            layout_document_with_images(&doc, 80, LayoutOptions::default(), &NoImages, &[history]);
        let joined = folded
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("▸ History (3 ¶, 1 subsection)"),
            "the fold summary line must appear: {joined:?}"
        );
        assert!(
            !joined.contains("First history paragraph"),
            "the folded body text must be gone"
        );
        assert!(
            !joined.contains("Early sub paragraph"),
            "the folded subsection's body must be gone too"
        );
        assert!(
            joined.contains("Legacy paragraph"),
            "content outside the fold is untouched"
        );
    }

    #[test]
    fn folding_keeps_block_lines_aligned_and_points_folded_blocks_at_the_summary() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let history = heading_block(&doc, "History");
        let legacy = heading_block(&doc, "Legacy");
        let folded =
            layout_document_with_images(&doc, 80, LayoutOptions::default(), &NoImages, &[history]);
        assert_eq!(
            folded.block_lines.len(),
            doc.blocks.len(),
            "block_lines must stay index-aligned with doc.blocks"
        );
        // Every swallowed block (heading .. next h2) maps to the summary line.
        let summary_line = folded.block_lines[history];
        for b in history..legacy {
            assert_eq!(
                folded.block_lines[b], summary_line,
                "folded block {b} must anchor at the fold summary"
            );
        }
        // The summary line really is the `▸ History` line.
        assert!(line_text(&folded.lines[summary_line]).contains("▸ History"));
    }

    #[test]
    fn folded_links_keep_their_index_but_are_not_visible() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let links = collect_links(&doc);
        // Sanity: three links, the middle one inside History.
        assert_eq!(links.len(), 3);
        let history = heading_block(&doc, "History");
        let folded =
            layout_document_with_images(&doc, 80, LayoutOptions::default(), &NoImages, &[history]);
        assert_eq!(folded.link_visible.len(), links.len());
        assert!(folded.link_visible[0], "the lead link stays visible");
        assert!(
            !folded.link_visible[1],
            "the folded History link is not focusable"
        );
        assert!(folded.link_visible[2], "the Legacy link stays visible");
        // The visible link spans are exactly occurrences 0 and 2 — the folded
        // occurrence keeps its index (never renumbered) but emits no span.
        let mut emitted: Vec<usize> = folded
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| match s.kind {
                SpanKind::Link(occ) => Some(occ),
                _ => None,
            })
            .collect();
        emitted.dedup();
        assert_eq!(emitted, vec![0, 2]);
    }

    /// Lead link, an `h2 History` whose body carries a Cite-extension
    /// reference marker (`<sup class="reference"><a href="#cite_note-1">[1]
    /// </a></sup>`), then an `h2 Legacy` with a second link. The marker is a
    /// same-page fragment anchor — excluded from `collect_links` and given no
    /// `SpanKind::Link` occurrence — so folding History must NOT advance the
    /// link counter for it.
    const FOLD_CITE_FIXTURE: &str = r##"
    <html><head><title>Cite Fold</title></head><body>
      <p>Lead with a <a href="./Lead_Link">lead link</a> here.</p>
      <h2>History</h2>
      <p>A claim<sup class="reference"><a href="#cite_note-1">[1]</a></sup> in history.</p>
      <h2>Legacy</h2>
      <p>Legacy paragraph with a <a href="./Legacy_Link">legacy link</a>.</p>
    </body></html>
    "##;

    #[test]
    fn folding_a_section_with_a_citation_marker_keeps_link_numbering_aligned() {
        let doc = parse_article_html("Cite Fold", FOLD_CITE_FIXTURE);
        let links = collect_links(&doc);
        // Two followable links; the `#cite_note-1` marker is not one of them.
        assert_eq!(links.len(), 2);
        let history = heading_block(&doc, "History");
        let folded =
            layout_document_with_images(&doc, 80, LayoutOptions::default(), &NoImages, &[history]);
        // The folded citation marker must not inflate the per-occurrence
        // vectors past `collect_links`'s length — before the `block_link_count`
        // fix these were length 3 (a phantom trailing entry) and desynced from
        // `collect_links`.
        assert_eq!(folded.link_lines.len(), links.len());
        assert_eq!(folded.link_visible.len(), links.len());
        assert_eq!(folded.link_cols.len(), links.len());
        // Both real links stay visible — the Legacy link after the fold is not
        // pushed onto a phantom index and silently hidden.
        assert!(folded.link_visible[0], "the lead link stays visible");
        assert!(
            folded.link_visible[1],
            "the Legacy link after the fold stays visible"
        );
        // The Legacy link still emits its span at occurrence 1, not 2.
        let mut emitted: Vec<usize> = folded
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| match s.kind {
                SpanKind::Link(occ) => Some(occ),
                _ => None,
            })
            .collect();
        emitted.dedup();
        assert_eq!(emitted, vec![0, 1]);
    }

    #[test]
    fn folding_never_overflows_the_width_even_when_narrow() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let history = heading_block(&doc, "History");
        let legacy = heading_block(&doc, "Legacy");
        for width in [20u16, 40, 80] {
            let folded = layout_document_with_images(
                &doc,
                width,
                LayoutOptions::default(),
                &NoImages,
                &[history, legacy],
            );
            for (i, line) in folded.lines.iter().enumerate() {
                let w = line.width(false);
                assert!(
                    w <= width as usize,
                    "folded line {i} width {w} exceeds {width}: {:?}",
                    line_text(line)
                );
            }
        }
    }

    #[test]
    fn folding_the_last_section_summarizes_to_eof() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let legacy = heading_block(&doc, "Legacy");
        let folded =
            layout_document_with_images(&doc, 80, LayoutOptions::default(), &NoImages, &[legacy]);
        let joined = folded
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("▸ Legacy (1 ¶, 0 subsections)"));
        assert!(!joined.contains("Legacy paragraph"));
    }

    #[test]
    fn folding_the_cjk_fixture_never_overflows_the_width() {
        // Folding + per-character CJK wrapping is a real invariant risk (the
        // summary line and the kinsoku breaker both touch width math).
        let doc = parse_article_html("アラン・チューリング", JA_FIXTURE);
        let folds: Vec<usize> = section_outline(&doc).iter().map(|s| s.block).collect();
        for width in [20u16, 40, 80] {
            let layout = layout_document_with_images(
                &doc,
                width,
                LayoutOptions::default(),
                &NoImages,
                &folds,
            );
            for (i, line) in layout.lines.iter().enumerate() {
                let w = line.width(false);
                assert!(
                    w <= width as usize,
                    "folded CJK line {i} width {w} exceeds {width}: {:?}",
                    line_text(line)
                );
            }
        }
    }

    #[test]
    fn empty_folds_lays_out_identically_to_the_unfolded_layout() {
        let doc = parse_article_html("Fold Test", FOLD_FIXTURE);
        let plain = layout_document(&doc, 80, LayoutOptions::default());
        let folded =
            layout_document_with_images(&doc, 80, LayoutOptions::default(), &NoImages, &[]);
        assert_eq!(plain.lines, folded.lines);
        assert_eq!(plain.block_lines, folded.block_lines);
        assert_eq!(plain.link_lines, folded.link_lines);
        assert!(folded.link_visible.iter().all(|&v| v));
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

    /// PRD FR-NV-9: `Layout::link_at` must find the right link occurrence by
    /// display-cell column, including on a line whose link sits after wide
    /// (CJK) text where grapheme count and display width diverge — the exact
    /// case `link_cols` (grapheme units) would get wrong if reused directly
    /// for a mouse click's column.
    #[test]
    fn link_at_finds_the_right_occurrence_past_wide_characters() {
        let doc = parse_article_html("アラン・チューリング", JA_FIXTURE);
        let links = collect_links(&doc);
        let layout = layout_document(&doc, 40, LayoutOptions::default());
        for (occ, _) in links.iter().enumerate() {
            let line_index = layout.link_lines[occ];
            let line = &layout.lines[line_index];
            // Find the display-cell start of this occurrence's own span by
            // walking the line the same way `link_at` does, independently of
            // `link_cols`'s grapheme units.
            let mut acc = 0usize;
            let mut start = None;
            for span in &line.spans {
                if let SpanKind::Link(o) = span.kind
                    && o == occ
                {
                    start = Some(acc);
                    break;
                }
                acc += display_width(&span.text, true);
            }
            let start = start.expect("occurrence must appear on its own link_lines entry");
            assert_eq!(
                layout.link_at(line_index, start, true),
                Some(occ),
                "clicking the first cell of occurrence {occ}'s own span must resolve to it"
            );
        }
    }

    /// A click that lands on plain text (not any link's span) resolves to
    /// `None` — mouse click-to-follow must never guess.
    #[test]
    fn link_at_returns_none_off_any_link() {
        let doc = parse_article_html("T", "<p>Plain text with no links at all.</p>");
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        assert_eq!(layout.link_at(0, 0, false), None);
        assert_eq!(layout.link_at(0, 5, false), None);
    }

    /// A click past the end of the line, or on a line index beyond the
    /// document, is a no-op rather than a panic.
    #[test]
    fn link_at_is_bounds_safe() {
        let doc = parse_article_html("T", "<p>Short.</p>");
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        assert_eq!(layout.link_at(0, 10_000, false), None);
        assert_eq!(layout.link_at(10_000, 0, false), None);
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

    // ---- FR-DL-4: citation_needed_lines -------------------------------------

    fn cn_span(text: &str) -> LaidSpan {
        LaidSpan {
            text: text.to_string(),
            kind: SpanKind::CitationNeeded,
        }
    }

    #[test]
    fn citation_needed_lines_is_empty_with_no_markers() {
        let lines = vec![plain_line("nothing to see here")];
        assert!(citation_needed_lines(&lines, &[]).is_empty());
    }

    #[test]
    fn citation_needed_lines_finds_one_marker_per_line() {
        let lines = vec![
            plain_line("a claim"),
            LaidLine {
                spans: vec![cn_span("[citation needed]")],
            },
            plain_line("another claim"),
            LaidLine {
                spans: vec![cn_span("[citation needed]")],
            },
        ];
        assert_eq!(
            citation_needed_lines(&lines, &[false, false, false]),
            vec![1, 3]
        );
    }

    /// A marker that wraps across a soft line break (`continuation`) is one
    /// stop, keyed to the run's first line — mirrors `find_matches`' own
    /// "never double-count a wrap" contract.
    #[test]
    fn a_marker_split_across_a_continuation_run_is_one_stop_not_two() {
        let lines = vec![
            LaidLine {
                spans: vec![cn_span("[citation")],
            },
            LaidLine {
                spans: vec![cn_span("needed]")],
            },
        ];
        // `continuation[0] == true`: line 1 is a soft-wrap continuation of
        // line 0 (the same convention `Layout::continuation` uses).
        assert_eq!(citation_needed_lines(&lines, &[true]), vec![0]);
    }

    /// Two markers immediately adjacent (no continuation between them) are
    /// two distinct stops, even though both carry the same `SpanKind`.
    #[test]
    fn two_consecutive_non_continuation_marker_lines_are_two_stops() {
        let lines = vec![
            LaidLine {
                spans: vec![cn_span("[citation needed]")],
            },
            LaidLine {
                spans: vec![cn_span("[citation needed]")],
            },
        ];
        assert_eq!(citation_needed_lines(&lines, &[false]), vec![0, 1]);
    }

    /// Wired to real `layout_document` output: the citation-needed detector
    /// in `doc.rs` plus this function together find the marker's rendered
    /// line.
    #[test]
    fn citation_needed_lines_wired_to_a_real_layout() {
        let html = "<html><body><p>A claim<sup typeof=\"mw:Transclusion\" \
             data-mw='{&quot;parts&quot;:[{&quot;template&quot;:{&quot;target&quot;:\
             {&quot;wt&quot;:&quot;Citation needed&quot;}}}]}'>x</sup>.</p></body></html>";
        let doc = parse_article_html("Test", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let hits = citation_needed_lines(&layout.lines, &layout.continuation);
        assert_eq!(hits.len(), 1, "exactly one marker in this fixture");
        let line_text: String = layout.lines[hits[0]]
            .spans
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            line_text.contains("[citation needed]"),
            "the hit line must contain the marker text: {line_text:?}"
        );
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
            folds: Vec::new(),
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

    // ---- FR-RD-11: "N min read" header line -------------------------------

    /// The header emits "N min read" right under the title (before the
    /// blank line the previous renderer's header already had), styled Dim —
    /// distinct from the bold, colorless `Title` line above it.
    #[test]
    fn reading_time_header_line_appears_under_the_title() {
        let words: String = (0..500).map(|_| "word ").collect();
        let doc = parse_article_html("Test", &format!("<html><body><p>{words}</p></body></html>"));
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        assert_eq!(
            line_text(&layout.lines[0]),
            "Test",
            "title is the first line"
        );
        let minutes = crate::doc::reading_minutes(crate::doc::word_count(&doc), 230);
        assert!(minutes > 0, "fixture must have enough words to read >0 min");
        assert_eq!(line_text(&layout.lines[1]), format!("{minutes} min read"));
        assert!(
            layout.lines[1]
                .spans
                .iter()
                .all(|s| s.kind == SpanKind::Dim),
            "the reading-time line must be styled Dim"
        );
    }

    /// An empty document (0 words) has nothing to estimate — no reading-time
    /// line at all, not a nonsensical "0 min read".
    #[test]
    fn reading_time_header_line_is_absent_for_an_empty_document() {
        let doc = parse_article_html("Empty", "<html><body></body></html>");
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        assert_eq!(line_text(&layout.lines[0]), "Empty");
        assert_eq!(
            line_text(&layout.lines[1]),
            "",
            "no reading-time line: the blank line follows the title directly"
        );
    }

    /// Changing `reading_wpm` changes the estimate — proving `LayoutOptions`
    /// actually threads it through, and that it participates in the L1 cache
    /// key (it derives `PartialEq`, so two options differing only here are
    /// unequal — the same guarantee `layout_cache_hits_on_an_identical_key_
    /// and_misses_on_any_field_change` locks for the other fields).
    #[test]
    fn reading_wpm_option_changes_the_estimate() {
        let words: String = (0..300).map(|_| "word ").collect();
        let doc = parse_article_html("Test", &format!("<html><body><p>{words}</p></body></html>"));
        let slow = layout_document(
            &doc,
            80,
            LayoutOptions {
                reading_wpm: 100,
                ..LayoutOptions::default()
            },
        );
        let fast = layout_document(
            &doc,
            80,
            LayoutOptions {
                reading_wpm: 1000,
                ..LayoutOptions::default()
            },
        );
        assert_ne!(line_text(&slow.lines[1]), line_text(&fast.lines[1]));
    }

    // ---- FR-RD-7: math passthrough rendering ------------------------------

    /// Inline math renders as ⟨normalized TeX⟩, styled `SpanKind::Math`, and
    /// a trivial superscript (`^2`) converts to its Unicode digit.
    #[test]
    fn inline_math_renders_delimited_and_normalized() {
        let html = r#"<html><body><p>Energy: <span typeof="mw:Extension/math">
            <math alttext="E=mc^2"></math></span> is famous.</p></body></html>"#;
        let doc = parse_article_html("Test", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let full: String = layout
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            full.contains("⟨E=mc²⟩"),
            "expected the delimited, superscript-normalized form in: {full:?}"
        );
        let math_span_found = layout
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.kind == SpanKind::Math && s.text.contains('²'));
        assert!(math_span_found, "the math text must carry SpanKind::Math");
    }

    /// A display equation (`Block::Math { display: true, .. }`) lays out on
    /// its own line with a nonzero centering pad, distinct from an ordinary
    /// left-aligned paragraph.
    #[test]
    fn display_math_block_centers_on_its_own_line() {
        let html = r#"<html><body><p>Intro.</p>
            <dl><dd><span typeof="mw:Extension/math">
                <math display="block" alttext="F = m a"></math>
            </span></dd></dl>
            <p>Outro.</p></body></html>"#;
        let doc = parse_article_html("Test", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let math_line = layout
            .lines
            .iter()
            .find(|l| line_text(l).contains("⟨F = m a⟩"))
            .expect("display equation must appear on its own line");
        let leading_spaces = math_line
            .spans
            .first()
            .map(|s| s.text.chars().take_while(|c| *c == ' ').count())
            .unwrap_or(0);
        assert!(
            leading_spaces > 0,
            "a short display equation on an 80-wide line should have a centering pad, got {leading_spaces}"
        );
    }

    // ---- FR-RD-9: justification (river-capped) ----------------------------

    /// The prose (lowercase-letters-and-spaces) content lines of a laid-out
    /// document — the paragraph lines the spacing/justify tests reason about,
    /// filtered away from the title, "N min read", headings, and blanks.
    fn prose_lines(layout: &Layout) -> Vec<String> {
        layout
            .lines
            .iter()
            .map(line_text)
            .filter(|t| {
                !t.trim().is_empty() && t.chars().all(|c| c.is_ascii_lowercase() || c == ' ')
            })
            .collect()
    }

    fn paragraph_doc(title: &str, body: &str) -> Document {
        parse_article_html(
            title,
            &format!("<html><head><title>{title}</title></head><body><p>{body}</p></body></html>"),
        )
    }

    #[test]
    fn spread_extra_round_robins_the_remainder_onto_leading_gaps() {
        assert_eq!(spread_extra(7, 3), vec![3, 2, 2]);
        assert_eq!(spread_extra(4, 4), vec![1, 1, 1, 1]);
        assert_eq!(spread_extra(2, 3), vec![1, 1, 0]);
        assert_eq!(spread_extra(0, 3), vec![0, 0, 0]);
        assert!(spread_extra(5, 0).is_empty());
        // The exact-fill guarantee: the extras always sum back to the deficit.
        for (d, g) in [(7usize, 3usize), (10, 4), (1, 5), (13, 6), (23, 7)] {
            assert_eq!(spread_extra(d, g).iter().sum::<usize>(), d, "sum({d},{g})");
        }
    }

    #[test]
    fn justify_fills_full_lines_exactly_and_leaves_the_last_line_ragged() {
        let doc = paragraph_doc(
            "JT",
            "the cat sat on a mat and the dog ran to the sun for fun in the big red car by a bee hive",
        );
        // width 30 (< measure 88) → content column 30, no centering pad, so a
        // line's display width IS its content width.
        let default = prose_lines(&layout_document(&doc, 30, LayoutOptions::default()));
        let justified = prose_lines(&layout_document(
            &doc,
            30,
            LayoutOptions {
                justify: true,
                ..LayoutOptions::default()
            },
        ));
        assert!(default.len() >= 2, "the paragraph wraps to several lines");
        assert_eq!(
            default.len(),
            justified.len(),
            "justify redistributes spaces without changing where lines break"
        );
        let n = justified.len();
        let mut saw_stretched = false;
        for i in 0..n - 1 {
            // The tightened equality: every justified full line reaches the
            // content column EXACTLY, not merely `<=`.
            assert_eq!(
                display_width(&justified[i], false),
                30,
                "justified full line must fill the content column exactly: {:?}",
                justified[i]
            );
            // Same words, only the spacing changed.
            assert_eq!(
                justified[i].split_whitespace().collect::<Vec<_>>(),
                default[i].split_whitespace().collect::<Vec<_>>(),
            );
            if justified[i] != default[i] {
                saw_stretched = true;
            }
        }
        assert!(
            saw_stretched,
            "at least one line actually received extra spaces"
        );
        // The paragraph's last line is never justified — byte-identical to
        // the ragged-right default.
        assert_eq!(
            justified[n - 1],
            default[n - 1],
            "the last line of the paragraph stays ragged"
        );
    }

    #[test]
    fn justify_river_cap_leaves_a_sparse_line_ragged() {
        // "aa bb" then a 26-cell word that can't share the line: filling that
        // one gap would need 25 extra cells (>> the river cap) → left ragged.
        let long = "c".repeat(26);
        let doc = paragraph_doc("jr", &format!("aa bb {long} dd ee ff gg hh"));
        let layout = layout_document(
            &doc,
            30,
            LayoutOptions {
                justify: true,
                ..LayoutOptions::default()
            },
        );
        let sparse = layout
            .lines
            .iter()
            .map(line_text)
            .find(|t| t.trim() == "aa bb")
            .expect("the sparse line stays 'aa bb', not stretched into a river");
        assert!(
            display_width(&sparse, false) < 30,
            "the river cap kept the sparse line ragged: {sparse:?}"
        );
        for l in &layout.lines {
            assert!(l.width(false) <= 30, "no overflow: {:?}", line_text(l));
        }
    }

    #[test]
    fn justify_composes_with_word_spacing_and_still_fills_exactly() {
        let doc = paragraph_doc(
            "JW",
            "the cat sat on a mat and the dog ran to the sun for fun in the red car today",
        );
        let layout = layout_document(
            &doc,
            30,
            LayoutOptions {
                justify: true,
                word_spacing: 1,
                ..LayoutOptions::default()
            },
        );
        let prose = prose_lines(&layout);
        assert!(prose.len() >= 2);
        for l in &prose[..prose.len() - 1] {
            assert_eq!(
                display_width(l, false),
                30,
                "justify fills exactly even atop the word_spacing base gap: {l:?}"
            );
            // Every inter-word gap is at least the widened base (2 cells).
            for gap in l.split(|c: char| c != ' ').filter(|s| !s.is_empty()) {
                assert!(
                    gap.len() >= 2,
                    "word_spacing=1 keeps every gap at least 2 cells wide: {l:?}"
                );
            }
        }
        for l in &layout.lines {
            assert!(l.width(false) <= 30);
        }
    }

    #[test]
    fn justify_does_not_corrupt_cjk_lines() {
        let doc = parse_article_html("アラン・チューリング", JA_FIXTURE);
        for width in [20u16, 40, 60, 80] {
            let just = layout_document(
                &doc,
                width,
                LayoutOptions {
                    justify: true,
                    ..LayoutOptions::default()
                },
            );
            let def = layout_document(&doc, width, LayoutOptions::default());
            assert_eq!(just.lines.len(), def.lines.len());
            for l in &just.lines {
                assert!(
                    l.width(false) <= width as usize,
                    "justify must not overflow a CJK line at width {width}"
                );
            }
            // A line with no inter-word ASCII gap (purely CJK) can't justify by
            // spacing, so it must be byte-identical to the default layout.
            for (j, d) in just.lines.iter().zip(def.lines.iter()) {
                if !line_text(d).contains(' ') {
                    assert_eq!(
                        line_text(j),
                        line_text(d),
                        "a gapless CJK line must be untouched by justify"
                    );
                }
            }
        }
        // The CJK content and a CJK link both survive justification intact.
        let just = layout_document(
            &doc,
            40,
            LayoutOptions {
                justify: true,
                ..LayoutOptions::default()
            },
        );
        let joined: String = just.lines.iter().map(line_text).collect();
        assert!(
            joined.contains("計算機科学"),
            "CJK link text preserved whole"
        );
    }

    // ---- FR-RD-9: soft (Knuth-Liang) hyphenation -------------------------

    fn word_clusters(w: &str) -> Vec<Cluster> {
        clusters_from_str(w, SpanKind::Plain, false)
    }

    #[test]
    fn try_hyphenate_breaks_only_at_knuth_liang_points() {
        // en-US hyphenates "hyphenation" as hy-phen-a-tion (breaks after 2, 6,
        // 7 chars). Ample room → the largest valid prefix that still fits '-'.
        let cl = word_clusters("hyphenation");
        let (head, tail) = try_hyphenate(&cl, 20).expect("a valid break exists");
        let head_text: String = head.iter().map(|c| c.text.as_str()).collect();
        let tail_text: String = tail.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            head_text, "hyphena-",
            "largest fitting KL prefix, with hyphen"
        );
        assert_eq!(tail_text, "tion");
        // The hyphen carries the word's own kind (so a hyphenated link keeps
        // its span), and one cluster's worth of width.
        assert_eq!(head.last().unwrap().text, "-");
        assert_eq!(head.last().unwrap().width, 1);
        // Tight room forces the earliest valid break — never an invalid
        // mid-syllable cut.
        let (head, tail) = try_hyphenate(&cl, 4).expect("earliest break fits in 4");
        let ht: String = head.iter().map(|c| c.text.as_str()).collect();
        let tt: String = tail.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(ht, "hy-");
        assert_eq!(tt, "phenation");
        // No valid break fits in 2 cells (even "hy-" needs 3).
        assert!(try_hyphenate(&cl, 2).is_none());
    }

    #[test]
    fn try_hyphenate_respects_scope_min_length_and_cjk() {
        // Below MIN_HYPHEN_WORD_LEN: never hyphenated.
        assert!(try_hyphenate(&word_clusters("cats"), 40).is_none());
        // Non-ASCII (accented) words are out of scope — patterns are en-US
        // only, so they're left unbroken rather than mangled.
        assert!(try_hyphenate(&word_clusters("naïveté"), 40).is_none());
        // CJK never hyphenates (per-character breaking already handles it).
        assert!(try_hyphenate(&word_clusters("計算機科学"), 40).is_none());
        // A word with digits is not a spaced-script word for this purpose.
        assert!(try_hyphenate(&word_clusters("abc123def"), 40).is_none());
    }

    #[test]
    fn hyphenate_breaks_a_long_word_with_a_trailing_hyphen_and_reconstructs() {
        let doc = paragraph_doc("hy", "aaaa bbbb cccc hyphenation dddd eeee ffff");
        let on = LayoutOptions {
            hyphenate: true,
            ..LayoutOptions::default()
        };
        let hyph = layout_document(&doc, 20, on);
        let plain = layout_document(&doc, 20, LayoutOptions::default());
        // The hyphen counts toward the width — nothing overflows.
        for l in &hyph.lines {
            assert!(
                l.width(false) <= 20,
                "hyphenated line overflowed: {:?}",
                line_text(l)
            );
        }
        let ends_with_hyphen = |ll: &Layout| {
            ll.lines
                .iter()
                .any(|l| line_text(l).trim_end().ends_with('-'))
        };
        assert!(
            ends_with_hyphen(&hyph),
            "a long word should have been hyphenated"
        );
        assert!(
            !ends_with_hyphen(&plain),
            "without the flag no word is ever hyphenated"
        );
        // Reconstruct: gluing each soft-hyphen line straight onto the next
        // (dropping the '-') puts the word back together whole — proof the
        // break fell on a grapheme boundary that reforms a real word.
        let mut recon = String::new();
        for l in &hyph.lines {
            let t = line_text(l);
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            match t.strip_suffix('-') {
                Some(head) => recon.push_str(head),
                None => {
                    recon.push_str(t);
                    recon.push(' ');
                }
            }
        }
        assert!(
            recon.contains("hyphenation"),
            "the hyphenated word reconstructs intact: {recon:?}"
        );
    }

    #[test]
    fn hyphenate_and_justify_together_never_overflow_and_fill_full_lines() {
        let doc = paragraph_doc(
            "HJ",
            "aaaa bbbb cccc hyphenation dddd eeee ffff gggg hyphenation iiii jjjj",
        );
        let layout = layout_document(
            &doc,
            24,
            LayoutOptions {
                hyphenate: true,
                justify: true,
                ..LayoutOptions::default()
            },
        );
        for l in &layout.lines {
            assert!(
                l.width(false) <= 24,
                "hyphenate+justify must never overflow: {:?}",
                line_text(l)
            );
        }
        // The combination still produces flush full lines: at least one
        // non-empty content line reaches the content column exactly (24), the
        // tightened justify equality holding even with hyphenation reflowing
        // the words. (Checked on raw lines, since a justified line may now end
        // in a soft hyphen.)
        assert!(
            layout
                .lines
                .iter()
                .any(|l| l.width(false) == 24 && !line_text(l).trim().is_empty()),
            "a justified line under hyphenation still fills the column exactly"
        );
    }

    // ---- FR-RD-9: mappings survive justify + hyphenate --------------------

    const MAPPING_FIXTURE: &str = r##"<html><head><title>Mapping</title></head><body>
      <p>The systematic study of <a href="./Computer_science">computer science</a>
      frequently involves extraordinarily complicated hyphenation scenarios that
      are worth testing here today with the <a href="./Enigma_machine">Enigma</a>.</p>
      <h2>History</h2>
      <p>Some history paragraph text mentioning <a href="./Alan_Turing">Turing</a> once.</p>
    </body></html>"##;

    #[test]
    fn block_link_and_find_mappings_survive_justify_and_hyphenate() {
        let doc = parse_article_html("Mapping", MAPPING_FIXTURE);
        let opts = LayoutOptions {
            justify: true,
            hyphenate: true,
            ..LayoutOptions::default()
        };
        let layout = layout_document(&doc, 40, opts);

        // No overflow with both knobs on.
        for l in &layout.lines {
            assert!(l.width(false) <= 40, "overflow: {:?}", line_text(l));
        }

        // Section jump: block_lines still lands on the heading's own text.
        let sections = section_outline(&doc);
        assert_eq!(sections.len(), 1);
        for section in &sections {
            let line = layout.block_lines[section.block];
            assert_eq!(
                line_text(&layout.lines[line]).trim(),
                section.title,
                "block_lines must land on the heading under justify+hyphenate"
            );
        }

        // Link order: occurrences stay in document order.
        for w in layout.link_lines.windows(2) {
            assert!(w[0] <= w[1], "link occurrences stay in document order");
        }

        // Link mapping: link_cols bounds EXACTLY this occurrence's own rendered
        // span(s) on its line — the property that lets a hint/focus overlay
        // paint the right cells even after justification widened the gaps that
        // precede the link (shifting its grapheme columns) and hyphenation
        // reflowed the surrounding words.
        let links = collect_links(&doc);
        assert_eq!(layout.link_cols.len(), links.len());
        for (occ, _link) in links.iter().enumerate() {
            if !layout.link_visible[occ] {
                continue;
            }
            let line = &layout.lines[layout.link_lines[occ]];
            let text = line_text(line);
            let graphemes: Vec<&str> = text.graphemes(true).collect();
            let span = layout.link_cols[occ];
            let sliced: String = graphemes[span.start..span.end].concat();
            let own: String = line
                .spans
                .iter()
                .filter(|s| s.kind == SpanKind::Link(occ))
                .map(|s| s.text.as_str())
                .collect();
            assert_eq!(
                sliced, own,
                "link_cols[{occ}] must bound exactly the link's own rendered span"
            );
            assert!(
                !sliced.is_empty(),
                "a visible link maps to a nonempty range"
            );
        }

        // In-page find: a single-word query still resolves onto a line that
        // really contains it (widened gaps never masquerade as matches).
        let occurrences = find_matches(&layout.lines, &layout.continuation, "history");
        assert!(
            !occurrences.is_empty(),
            "find still works under justify+hyphenate"
        );
        for occ in &occurrences {
            for (line_idx, span) in &occ.pieces {
                let text = line_text(&layout.lines[*line_idx]);
                let graphemes: Vec<&str> = text.graphemes(true).collect();
                let matched: String = graphemes[span.start..span.end].concat();
                assert_eq!(matched.to_lowercase(), "history");
            }
        }
    }

    // ---- FR-RD-9: defaults reproduce the pre-FR-RD-9 layout ---------------

    #[test]
    fn defaults_are_byte_identical_with_justify_and_hyphenate_off() {
        // The wrap path with both knobs at their (false) defaults renders a
        // paragraph with single collapsed spaces and no hyphens — and is
        // byte-identical to explicitly turning them off.
        let doc = paragraph_doc("t", "one two three");
        let layout = layout_document(&doc, 40, LayoutOptions::default());
        assert!(
            layout.lines.iter().any(|l| line_text(l) == "one two three"),
            "default (justify/hyphenate off) keeps single spaces and no hyphen"
        );
        let explicit_off = layout_document(
            &doc,
            40,
            LayoutOptions {
                justify: false,
                hyphenate: false,
                ..LayoutOptions::default()
            },
        );
        assert_eq!(layout.lines, explicit_off.lines);

        // Across the broader fixtures too, the default layout is unchanged by
        // spelling the new flags out as false — the whole-suite byte-identity
        // guard, localized here.
        for html in [FIXTURE, JA_FIXTURE] {
            let doc = parse_article_html("F", html);
            for width in [24u16, 40, 80, 120] {
                let a = layout_document(&doc, width, LayoutOptions::default());
                let b = layout_document(
                    &doc,
                    width,
                    LayoutOptions {
                        justify: false,
                        hyphenate: false,
                        ..LayoutOptions::default()
                    },
                );
                assert_eq!(a.lines, b.lines, "default path unchanged at width {width}");
            }
        }
    }

    #[test]
    fn justify_and_hyphenate_never_overflow_across_fixtures_and_widths() {
        let docs = [
            parse_article_html("Test Article", FIXTURE),
            parse_article_html("アラン・チューリング", JA_FIXTURE),
            hard_cases_doc(),
        ];
        for doc in &docs {
            for width in [20u16, 40, 60, 80, 100, 200] {
                for (justify, hyphenate) in [(true, false), (false, true), (true, true)] {
                    assert_no_overflow(
                        doc,
                        width,
                        LayoutOptions {
                            justify,
                            hyphenate,
                            ..LayoutOptions::default()
                        },
                    );
                }
            }
        }
    }

    // ---- FR-RD-7: math-layout feature (off by default) --------------------

    /// Under the DEFAULT build (no `math-layout` feature), inline math is
    /// B14's trivial-normalized passthrough — this proves the feature is
    /// genuinely off by default and the default rendering is unchanged.
    #[cfg(not(feature = "math-layout"))]
    #[test]
    fn default_build_uses_trivial_math_passthrough() {
        let html = r#"<html><body><p>x <span typeof="mw:Extension/math"><math alttext="\sum_{i=1}^{n}"></math></span></p></body></html>"#;
        let doc = parse_article_html("T", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let joined: String = layout
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(" ");
        // `\sum` is NOT a trivial-normalization case, so it stays raw TeX
        // inside the ⟨…⟩ delimiters (subscript still normalizes).
        assert!(
            joined.contains("⟨\\sum") || joined.contains("\\sum"),
            "default build leaves \\sum as raw TeX passthrough: {joined:?}"
        );
        assert!(
            !joined.contains('∑'),
            "the Unicode ∑ only appears under the math-layout feature: {joined:?}"
        );
    }

    /// Under `--features math-layout`, the same inline math renders the
    /// richer Unicode form.
    #[cfg(feature = "math-layout")]
    #[test]
    fn math_layout_feature_renders_richer_inline_math() {
        let html = r#"<html><body><p>x <span typeof="mw:Extension/math"><math alttext="\sum_{k=0}^{n} k"></math></span> y</p></body></html>"#;
        let doc = parse_article_html("T", html);
        let layout = layout_document(&doc, 80, LayoutOptions::default());
        let joined: String = layout
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("⟨∑ₖ₌₀ⁿ k⟩"),
            "the feature renders the sum via Unicode: {joined:?}"
        );
    }
}
