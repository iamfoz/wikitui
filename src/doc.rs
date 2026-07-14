//! The document model: a small, renderer-agnostic representation of an
//! article, produced by parsing Parsoid HTML (see PRD §6.3). Everything the
//! terminal UI draws, and everything `--dump` prints, comes from here.
//!
//! PRD SEC-3 parser hardening lives in this module too, in three layers:
//! [`MAX_ARTICLE_HTML_BYTES`] caps parser input size (degrading to a
//! truncation banner, never a crash or unbounded work); `cap_html_nesting_depth`
//! is a cheap pre-parse scan that cuts off pathologically deep tag nesting
//! *before* html5ever ever sees it (its tree builder is empirically
//! superlinear in nesting depth — see that function's doc comment for
//! measurements); and `MAX_DOM_DEPTH` separately caps the recursive DOM
//! walkers below, flattening anything deeper to plain text instead of
//! recursing further, as a backstop for whatever the pre-scan's heuristic
//! nature lets through. `MAX_CITATIONS` bounds how many References-section
//! entries are harvested. PRD SEC-1 sanitization is the last step of
//! `parse_article_html` (`sanitize_document`) — see its doc comment for the
//! single choke point this module routes every emitted string through.

use crate::sanitize;
use ego_tree::NodeRef;
use scraper::{Html, Node, Selector};

/// PRD SEC-3: "10x the largest real article" pathological-payload budget.
/// HTML beyond this is never handed to the parser at all — `parse_article_html`
/// truncates to this many bytes (on a UTF-8 char boundary) first and sets
/// `Document::truncated`, so parse time and memory are bounded regardless
/// of how large (or how hostile) the source response was.
pub const MAX_ARTICLE_HTML_BYTES: usize = 10 * 1024 * 1024;

/// PRD SEC-3: recursion depth cap for the DOM walkers below
/// (`walk_blocks`/`collect_inline`/`collect_text`/`descendant_tags`/
/// `find_img_src_alt`). A real article's DOM nests at most a few dozen levels
/// deep; a hostile page can otherwise force unbounded recursion (e.g. 10k
/// nested `<div>`s) and overflow the stack. Past this depth, a walker stops
/// recursing and flattens whatever remains of the subtree to plain text via
/// `flatten_deep_subtree`, which walks with `NodeRef::descendants()` —
/// pointer-following, not call-stack recursion, so it can't itself overflow.
const MAX_DOM_DEPTH: usize = 256;

/// PRD SEC-3: caps how many `ol.references li` entries `extract_citations`
/// harvests. A real article has dozens to a few hundred; 5000 is generous
/// headroom while still bounding a hostile page that pads its references
/// list arbitrarily.
const MAX_CITATIONS: usize = 5000;

/// PRD SEC-3 (in the spirit of the "10x the largest real article" budget),
/// applied to table grids: a real table has at most a few dozen columns and
/// a few thousand rows; a hostile page can otherwise pad `colspan`/`rowspan`
/// or the row/cell count to force a huge grid. `parse_table` caps the
/// expanded grid at these dimensions (and clamps each cell's own
/// colspan/rowspan to them first, so a single `colspan="100000000"` can't
/// blow up before the grid-level cap even applies), flagging `Table::truncated`.
const MAX_TABLE_COLS: usize = 100;
const MAX_TABLE_ROWS: usize = 2000;

#[derive(Debug, Clone, PartialEq)]
pub enum SpanStyle {
    Plain,
    Bold,
    Italic,
    Superscript,
    Link(String),
    /// PRD FR-DL-5: a link Parsoid itself pre-marked as pointing at a
    /// nonexistent article (`class="new"` on the `<a>` — the cheapest
    /// redlink signal, checked first; see `collect_inline`'s `"a"` arm).
    /// Carries the href, same as `Link`. A link *not* pre-marked this way
    /// can still turn out to be a redlink — the batched `generator=links&
    /// prop=info` check (`api::WikiClient::fetch_missing_links`) catches
    /// those after the fact via `App::confirmed_redlinks`, layered on at
    /// paint time exactly like visited-link coloring rather than mutating
    /// the span.
    RedLink(String),
    /// PRD FR-RD-7: inline TeX passthrough, extracted from a Parsoid math
    /// node (see `extract_math`). Carries the raw TeX source — the same
    /// string as this span's own `text` (mirroring how `Link`'s payload and
    /// a link's visible text are two separate things but happen to start
    /// from the same node); kept on the style, not just the text, so paint
    /// code can apply `normalize_trivial_math` without re-deriving "is this
    /// a math span" from content.
    Math(String),
}

#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Debug, Clone)]
pub enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph(Vec<Span>),
    ListItem {
        ordered: bool,
        index: usize,
        depth: u8,
        spans: Vec<Span>,
    },
    Blockquote(Vec<Span>),
    Code(String),
    Rule,
    /// A parsed table (PRD FR-RD-4): a rectangular grid of cells with
    /// rowspan/colspan already resolved by expansion (see [`Table`]). The
    /// renderer (`layout.rs`) decides per width whether to draw it as a
    /// box-drawing grid (with per-column sizing, cell wrapping, and
    /// horizontal scroll for wide tables) or to collapse it to a
    /// "Header: value" list (accessible mode / too narrow / `--dump`).
    Table(Table),
    /// A collapsed infobox: ordered (label, value) pairs. An empty label
    /// marks a full-width row (e.g. a section title inside the infobox).
    /// Rendered as a boxed card — floated right of the lead on wide
    /// terminals, a top block on narrower ones (PRD FR-RD-5, §6.3 tiers).
    Infobox(Vec<(String, String)>),
    /// An inline image (PRD FR-RD-8). `src` is the sanitized thumbnail URL
    /// (http(s) only — see [`sanitize_image_src`]; `None` when the source was
    /// missing or a rejected scheme, in which case only the alt text renders).
    /// `alt` is always present (the placeholder / accessibility text, kept
    /// available regardless of render path). `caption` is the `<figcaption>`
    /// text, rendered in dim italics below the image.
    Image {
        src: Option<String>,
        alt: String,
        caption: Option<String>,
    },
    /// A `<ul class="gallery">` of images (PRD FR-RD-8 "galleries as captioned
    /// strips or lists"). Rendered as a captioned horizontal strip when the
    /// width allows, else a vertical list of `[image: caption]`.
    Gallery(Vec<GalleryItem>),
    /// A standalone display equation (PRD FR-RD-7): a math node that appeared
    /// on its own at block-scanning level (Parsoid's usual `<dl><dd>`
    /// indentation for a leading-colon display equation), rather than inline
    /// within a paragraph's running text — see `walk_blocks`'s `"span"` arm.
    /// `display` is the source `<math>`'s own `display="block"` MathML
    /// attribute (or, lacking a `<math>` element at all, the fallback image's
    /// `-display` vs `-inline` class); `layout.rs` centers the line when
    /// true. `tex` is raw TeX passthrough (v1.0 scope, FR-RD-7); rendering
    /// applies `normalize_trivial_math` at paint time only, so `--dump`
    /// (`render_plain`) and every other consumer of the document model still
    /// see the exact source string.
    Math {
        tex: String,
        display: bool,
    },
}

/// One entry of a [`Block::Gallery`] (PRD FR-RD-8). `caption` is the
/// gallery-box label (falling back to the image's alt text when the box has
/// no explicit caption), always non-empty enough to identify the item in the
/// list/strip; `src` is the sanitized thumbnail URL (http(s) only) or `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryItem {
    pub src: Option<String>,
    pub caption: String,
}

/// One cell of a [`Table`]'s expanded grid (PRD FR-RD-4). Content is
/// flattened to inline text: nested block elements (including tables nested
/// in a cell) collapse to their text, and images collapse to their alt text
/// ("flattening pass for rowspan/colspan and images-in-cells"). `header`
/// marks a `<th>` so the renderer can style/underline the header row.
///
/// A cell produced purely to fill a rowspan/colspan span is *blank* (empty
/// `text`): the standard TUI expansion is "value in the origin cell, blanks
/// in the cells it spans over" — see [`Table`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub header: bool,
}

impl Cell {
    fn blank() -> Self {
        Self {
            text: String::new(),
            header: false,
        }
    }
}

/// A table as a **rectangular** grid (PRD FR-RD-4): `rows[r][c]` after
/// rowspan/colspan expansion, so every row has the same column count and the
/// renderer never has to reason about spans again. A cell with `colspan=3`
/// occupies three grid columns — the value in the first, blank [`Cell`]s in
/// the other two; a cell with `rowspan=2` occupies its column in the next
/// row too, again with a blank there. `truncated` is set when the source
/// exceeded [`MAX_TABLE_COLS`]/[`MAX_TABLE_ROWS`] (PRD SEC-3) and the grid
/// was capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub rows: Vec<Vec<Cell>>,
    pub truncated: bool,
}

impl Table {
    /// The grid's column count (every row is padded to this width).
    pub fn cols(&self) -> usize {
        self.rows.first().map(Vec::len).unwrap_or(0)
    }

    /// Whether the first row is a header row (any `<th>` in it) — drives the
    /// header underline in both the box grid and the collapse-to-list view.
    pub fn has_header_row(&self) -> bool {
        self.rows
            .first()
            .is_some_and(|r| r.iter().any(|c| c.header))
    }

    /// PRD FR-RD-4's collapse-to-list ("Header: value") form, shared by the
    /// accessible/too-narrow layout path and `--dump` (`render_plain`). When
    /// the first row is a header, each subsequent row renders one
    /// "Header: value" line per column; otherwise rows render as their cells
    /// joined by " | ". Blank cells (rowspan/colspan fill) are skipped.
    pub fn to_list_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.rows.is_empty() {
            return lines;
        }
        if self.has_header_row() {
            let headers: Vec<&str> = self.rows[0].iter().map(|c| c.text.as_str()).collect();
            for row in &self.rows[1..] {
                for (i, cell) in row.iter().enumerate() {
                    if cell.text.is_empty() {
                        continue;
                    }
                    match headers.get(i).copied().filter(|h| !h.is_empty()) {
                        Some(h) => lines.push(format!("{h}: {}", cell.text)),
                        None => lines.push(cell.text.clone()),
                    }
                }
                lines.push(String::new());
            }
            // Drop the trailing separator blank.
            if lines.last().is_some_and(String::is_empty) {
                lines.pop();
            }
        } else {
            for row in &self.rows {
                let joined = row
                    .iter()
                    .filter(|c| !c.text.is_empty())
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ");
                if !joined.is_empty() {
                    lines.push(joined);
                }
            }
        }
        if self.truncated {
            lines.push("… (table truncated)".to_string());
        }
        lines
    }
}

#[derive(Debug, Clone)]
pub struct Document {
    pub title: String,
    pub blocks: Vec<Block>,
    /// Sources this article itself cites, extracted from its References
    /// section — the raw material for Research mode's "sources cited by
    /// this entry" half (the other half being the article's own citation,
    /// generated on demand — see `research::self_citation`).
    pub citations: Vec<Citation>,
    /// PRD SEC-3: `true` when the source HTML exceeded
    /// [`MAX_ARTICLE_HTML_BYTES`] and only the first slice of it was
    /// parsed. `parse_article_html` also prepends a visible banner block
    /// when this is set (§7's "degraded rendering" contract); the flag
    /// itself is kept for callers/tests that want to check the condition
    /// without string-matching the banner text.
    pub truncated: bool,
}

/// One entry from an article's References/bibliography list: the raw
/// citation text (author, title, publisher, date — whatever the article's
/// citation template rendered) and, where present, the first external URL
/// in it. Extraction is a heuristic over common MediaWiki Cite-extension
/// markup, not a bibliographic parser — see `extract_citations`.
#[derive(Debug, Clone)]
pub struct Citation {
    pub id: String,
    pub text: String,
    pub url: Option<String>,
}

/// A link encountered while reading, in document order. `internal_title` is
/// `Some` for links to another article on the same wiki (resolved from
/// Parsoid's `./Title` / `/wiki/Title` conventions) and `None` for anything
/// else (external URLs, interwiki links) — those aren't followable yet.
#[derive(Debug, Clone)]
pub struct LinkRef {
    pub href: String,
    pub text: String,
    pub internal_title: Option<String>,
    /// PRD FR-DL-5: Parsoid pre-marked this link as a redlink (`class="new"`)
    /// — the cheapest of the two detection paths, always known at parse
    /// time. `false` does not mean "definitely exists": a link not pre-marked
    /// can still turn out missing via the batched info check (see
    /// `App::confirmed_redlinks`), which paint code consults as a second,
    /// independent signal rather than something this field tries to capture.
    pub redlink: bool,
}

fn internal_title_from_href(href: &str) -> Option<String> {
    let path = href
        .strip_prefix("./")
        .or_else(|| href.strip_prefix("/wiki/"))?;
    if path.starts_with("http://") || path.starts_with("https://") || path.contains("://") {
        return None;
    }
    let path = path.split('#').next().unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    let decoded = urlencoding::decode(path).ok()?.into_owned();
    // PRD SEC-1: percent-decoding happens here, downstream of
    // `sanitize_document`'s own href sanitization — a percent-escape like
    // `%1B` is inert ASCII text before decoding and only becomes a live
    // control byte after it, so the decoded title (which becomes the next
    // page's fetch title if this link is followed) is sanitized again here.
    Some(sanitize::sanitize_single_line(&decoded.replace('_', " ")).into_owned())
}

/// Collects every link in the blocks that can render one interactively
/// (paragraphs, list items, blockquotes), in the exact order the reading
/// view renders them — `ui::draw_reading` relies on this ordering to map a
/// cycled-to link index back to the span it highlights. Headings are
/// deliberately excluded: they're rendered as flattened plain text with no
/// per-span styling, so a link inside one couldn't be highlighted anyway.
pub fn collect_links(doc: &Document) -> Vec<LinkRef> {
    let mut links = Vec::new();
    let mut visit = |spans: &[Span]| {
        for s in spans {
            let (href, redlink) = match &s.style {
                SpanStyle::Link(href) => (href, false),
                SpanStyle::RedLink(href) => (href, true),
                _ => continue,
            };
            links.push(LinkRef {
                href: href.clone(),
                text: s.text.clone(),
                internal_title: internal_title_from_href(href),
                redlink,
            });
        }
    };
    for block in &doc.blocks {
        match block {
            Block::Paragraph(spans) | Block::Blockquote(spans) => visit(spans),
            Block::ListItem { spans, .. } => visit(spans),
            _ => {}
        }
    }
    links
}

/// A heading in the reading view, identified by the index of its block in
/// `doc.blocks` — the target of the table-of-contents jump (PRD FR-NV-2). The
/// laid-out line it maps to is resolved by `layout::Layout::block_lines`, so
/// there is a single source of line truth (the layout), not two competing
/// ones.
#[derive(Debug, Clone)]
pub struct SectionRef {
    pub level: u8,
    pub title: String,
    /// Index into `doc.blocks`.
    pub block: usize,
}

/// Every heading in the article, in reading order, tagged with its block
/// index. The width-aware layout maps that block index to a laid-out line.
pub fn section_outline(doc: &Document) -> Vec<SectionRef> {
    doc.blocks
        .iter()
        .enumerate()
        .filter_map(|(i, block)| match block {
            Block::Heading { level, spans } => Some(SectionRef {
                level: *level,
                title: spans.iter().map(|s| s.text.as_str()).collect(),
                block: i,
            }),
            _ => None,
        })
        .collect()
}

/// Tags whose content is handled by a dedicated `Block`, and which
/// `inline_spans` must therefore never descend into (otherwise their text
/// would be captured twice: once as a block, once as part of an ancestor's
/// inline run).
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "h1" | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "p"
            | "ul"
            | "ol"
            | "dl"
            | "table"
            | "blockquote"
            | "pre"
            | "figure"
            | "hr"
    )
}

/// Tags that carry no reader-visible content at all.
fn is_skipped_tag(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "noscript" | "link" | "meta" | "head"
    )
}

fn has_class(el: &scraper::node::Element, needle: &str) -> bool {
    el.attr("class")
        .map(|c| {
            c.split_whitespace()
                .any(|c| c == needle || c.contains(needle))
        })
        .unwrap_or(false)
}

/// Parsoid's RDFa `typeof` attribute is space-separated and can carry more
/// than one type (e.g. a transclusion that is also a math extension node),
/// so this checks membership the same way `has_class` does, over the
/// separate `typeof` attribute.
fn has_typeof(el: &scraper::node::Element, needle: &str) -> bool {
    el.attr("typeof")
        .map(|t| t.split_whitespace().any(|t| t == needle))
        .unwrap_or(false)
}

/// Whether `el` is a Parsoid math extension node — either the `<span
/// typeof="mw:Extension/math">` wrapper Parsoid emits, or a bare `<math>`
/// element some legacy-parser/third-party output uses directly with no
/// wrapping span (PRD §6.2 rule 3, Appendix A "Math (optional)").
fn is_math_node(el: &scraper::node::Element) -> bool {
    el.name() == "math" || (el.name() == "span" && has_typeof(el, "mw:Extension/math"))
}

/// One math node's extracted content (PRD FR-RD-7): the TeX source and
/// whether it is a display (own-line) equation vs. inline with surrounding
/// text.
struct MathNode {
    tex: String,
    display: bool,
}

/// Extracts a math node's TeX and display-vs-inline flag from a Parsoid math
/// span (or bare `<math>` — see `is_math_node`). Walks with `.descendants()`
/// (pointer-following, like `flatten_deep_subtree` — see its doc comment for
/// why that can't stack-overflow) rather than recursion; math subtrees are a
/// handful of nodes in practice, so no separate SEC-3 depth cap is needed
/// here the way the block/inline walkers need `MAX_DOM_DEPTH`.
///
/// TeX precedence (PRD §6.2 rule 3, "math nodes carry TeX in alttext"): the
/// `<math>` element's own `alttext` attribute wins when present — the PRD
/// names it as the primary source, and it is exactly the TeX MediaWiki's math
/// extension stored, with no MathML-rendering round-trip. The `<annotation
/// encoding="application/x-tex">` child (MathML's own "here is the source
/// markup that generated this" convention) is the fallback for a renderer
/// that omitted `alttext`. If there is no `<math>` element at all (a
/// stripped-down or accessible-only rendering), or it has neither `alttext`
/// nor a matching `annotation`, the last resort is the math extension's own
/// accessible fallback `<img alt="{\displaystyle ...}">` — present precisely
/// so a non-MathML-aware reader still has *some* text — with the
/// `\displaystyle`/`\textstyle` rendering-mode wrapper stripped. A node with
/// none of the three yields `None`: nothing is invented (the "missing-tex
/// graceful" case — the caller simply emits no span/block for it).
fn extract_math(node: NodeRef<Node>) -> Option<MathNode> {
    let math_el = node.descendants().find_map(|n| match n.value() {
        Node::Element(el) if el.name() == "math" => Some((n, el)),
        _ => None,
    });

    let Some((math_node, el)) = math_el else {
        // No `<math>` element at all: the fallback image's own class is the
        // only display-vs-inline signal left.
        let display = node.descendants().any(|n| {
            matches!(n.value(), Node::Element(el) if el.name() == "img" && has_class(el, "mwe-math-fallback-image-display"))
        });
        return fallback_image_tex(node).map(|tex| MathNode { tex, display });
    };

    let display = el.attr("display") == Some("block");
    if let Some(alttext) = el.attr("alttext").map(str::trim).filter(|s| !s.is_empty()) {
        return Some(MathNode {
            tex: alttext.to_string(),
            display,
        });
    }
    let annotation_tex = math_node.descendants().find_map(|n| match n.value() {
        Node::Element(el)
            if el.name() == "annotation" && el.attr("encoding") == Some("application/x-tex") =>
        {
            let text: String = n
                .descendants()
                .filter_map(|d| match d.value() {
                    Node::Text(t) => Some(t.text.as_ref()),
                    _ => None,
                })
                .collect();
            Some(text)
        }
        _ => None,
    });
    if let Some(tex) = annotation_tex
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(MathNode {
            tex: tex.to_string(),
            display,
        });
    }
    // Neither `alttext` nor a usable annotation: fall back to the image, but
    // keep this `<math>`'s own `display` attribute (more reliable than the
    // fallback image's class, which some renderers omit).
    fallback_image_tex(node).map(|tex| MathNode { tex, display })
}

/// The math extension's accessible fallback `<img alt="{\displaystyle ...}">`
/// text, with MediaWiki's `\displaystyle`/`\textstyle` rendering-mode wrapper
/// stripped — that wrapper marks how the fallback image was rasterized, not
/// part of the TeX a reader would recognize as the formula.
fn fallback_image_tex(node: NodeRef<Node>) -> Option<String> {
    let alt = node.descendants().find_map(|n| match n.value() {
        Node::Element(el) if el.name() == "img" => el.attr("alt"),
        _ => None,
    })?;
    let trimmed = alt.trim();
    let unwrapped = trimmed
        .strip_prefix("{\\displaystyle")
        .or_else(|| trimmed.strip_prefix("{\\textstyle"))
        .and_then(|s| s.strip_suffix('}'))
        .map(str::trim)
        .unwrap_or(trimmed);
    (!unwrapped.is_empty()).then(|| unwrapped.to_string())
}

/// FR-RD-7's "Unicode superscript/subscript normalization for simple cases":
/// a small, deliberately incomplete table — superscript/subscript digits
/// (plus the punctuation marks Unicode also defines alongside them) and the
/// Greek letters LaTeX's macros commonly spell out. A `^`/`_` argument that
/// isn't entirely covered by that table (a multi-token exponent, an
/// unrecognized macro, a mix of letters this table doesn't have a subscript
/// glyph for) is left exactly as raw TeX — FR-RD-7 v1.0 scope is passthrough
/// plus *only* the trivial cases; general math typesetting is the v1.x
/// Unicode-layout upgrade this deliberately doesn't attempt.
pub fn normalize_trivial_math(tex: &str) -> String {
    let chars: Vec<char> = tex.chars().collect();
    let mut out = String::with_capacity(tex.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            let name: String = chars[start..j].iter().collect();
            if let Some(g) = greek_letter(&name) {
                out.push(g);
                i = j;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if (c == '^' || c == '_')
            && let Some((converted, next)) = trivial_script(&chars, i + 1, c == '^')
        {
            out.push_str(&converted);
            i = next;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Reads one `^`/`_` argument starting at `start` in `chars` (a `{...}`
/// group, or — matching real TeX's own "only the next single token" rule —
/// one bare character when there's no brace) and converts it to Unicode
/// super/subscript form, or returns `None` if any character in the argument
/// has no glyph in the (deliberately small) table — the caller then leaves
/// the original `^`/`_` and its argument untouched.
fn trivial_script(chars: &[char], start: usize, sup: bool) -> Option<(String, usize)> {
    let (arg, next): (&[char], usize) = if chars.get(start) == Some(&'{') {
        let close = chars[start + 1..].iter().position(|&c| c == '}')? + start + 1;
        (&chars[start + 1..close], close + 1)
    } else if start < chars.len() {
        (&chars[start..start + 1], start + 1)
    } else {
        return None;
    };
    if arg.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(arg.len());
    for &c in arg {
        out.push(if sup {
            superscript_char(c)?
        } else {
            subscript_char(c)?
        });
    }
    Some((out, next))
}

fn superscript_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        _ => return None,
    })
}

fn subscript_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        's' => 'ₛ',
        't' => 'ₜ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// LaTeX's standard one-word Greek-letter macros (the 24 lowercase letters
/// plus the 11 uppercase ones that actually have a command — the rest of the
/// uppercase alphabet is identical to Latin, so LaTeX defines no macro for
/// it). Matched on the *maximal* run of ASCII letters after a `\`
/// (`normalize_trivial_math`'s caller), so a longer word like `\alphabet`
/// never partially matches `\alpha` — it simply isn't a key in this table.
fn greek_letter(name: &str) -> Option<char> {
    Some(match name {
        "alpha" => 'α',
        "beta" => 'β',
        "gamma" => 'γ',
        "delta" => 'δ',
        "epsilon" => 'ε',
        "zeta" => 'ζ',
        "eta" => 'η',
        "theta" => 'θ',
        "iota" => 'ι',
        "kappa" => 'κ',
        "lambda" => 'λ',
        "mu" => 'μ',
        "nu" => 'ν',
        "xi" => 'ξ',
        "omicron" => 'ο',
        "pi" => 'π',
        "rho" => 'ρ',
        "sigma" => 'σ',
        "tau" => 'τ',
        "upsilon" => 'υ',
        "phi" => 'φ',
        "chi" => 'χ',
        "psi" => 'ψ',
        "omega" => 'ω',
        "Gamma" => 'Γ',
        "Delta" => 'Δ',
        "Theta" => 'Θ',
        "Lambda" => 'Λ',
        "Xi" => 'Ξ',
        "Pi" => 'Π',
        "Sigma" => 'Σ',
        "Upsilon" => 'Υ',
        "Phi" => 'Φ',
        "Psi" => 'Ψ',
        "Omega" => 'Ω',
        _ => return None,
    })
}

/// True for scripts wrapped per-character rather than counted as
/// whitespace-split "words" (PRD FR-RD-11's word-count heuristic) — the same
/// Han/kana/Hangul/fullwidth ranges `layout::is_cjk` uses for line-wrapping,
/// duplicated rather than imported: `doc` is the lower-level document model
/// that `layout` builds on, and word-counting has no other reason to depend
/// upward on the layout engine.
fn is_cjk_word_char(c: char) -> bool {
    let u = c as u32;
    (0x3000..=0x303F).contains(&u)
        || (0x3040..=0x309F).contains(&u)
        || (0x30A0..=0x30FF).contains(&u)
        || (0x31F0..=0x31FF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0x1100..=0x11FF).contains(&u)
        || (0xFF00..=0xFFEF).contains(&u)
}

/// PRD FR-RD-11: how many Latin-script "words" one CJK character counts as
/// for reading-time purposes. CJK text has no space separators, so counting
/// whitespace-split tokens (as the Latin path does) would treat whole
/// sentences as "one word" and wildly undercount; this instead weights each
/// CJK codepoint as a fraction of a word, calibrated so a CJK article reads
/// at a plausible pace rather than "0 min" or an inflated one. A documented
/// heuristic, not a segmenter — real reading speed varies by character
/// density and register, and this is deliberately a single constant, not a
/// per-language model.
pub const CJK_CHARS_PER_WORD: f64 = 2.5;

/// The word count contributed by one run of spans (PRD FR-RD-11): ordinary
/// Latin-script text counts whitespace-split tokens; a token containing any
/// CJK character is instead counted by character, weighted by
/// `CJK_CHARS_PER_WORD` (mixed-script tokens are rare enough in practice that
/// treating a token as "CJK" the moment it contains any CJK character, rather
/// than splitting it further, is an acceptable simplification). Math spans
/// are skipped entirely: raw TeX source isn't prose a reader reads at word
/// pace, and counting it would skew the estimate on a math-heavy article.
fn count_words_in_spans(spans: &[Span]) -> f64 {
    let mut total = 0.0;
    for span in spans {
        if matches!(span.style, SpanStyle::Math(_)) {
            continue;
        }
        for word in span.text.split_whitespace() {
            if word.chars().any(is_cjk_word_char) {
                total += word.chars().filter(|c| is_cjk_word_char(*c)).count() as f64
                    / CJK_CHARS_PER_WORD;
            } else {
                total += 1.0;
            }
        }
    }
    total
}

/// PRD FR-RD-11's word-count pass over the document model — the same model
/// that feeds `--dump`/FTS, so this walks exactly what a reader actually
/// reads: headings, paragraphs, list items, and blockquotes. Deliberately
/// skips table/infobox cells (label/data text reads at a different pace than
/// prose, and would skew "N min read" toward nonsense on a table-heavy
/// article), code blocks (source text, not prose), and image/gallery
/// captions (usually a few words that would round the estimate up
/// disproportionately on an image-heavy page) — the same "what counts as
/// reading" boundary `render_plain`'s structure suggests, just narrower.
pub fn word_count(doc: &Document) -> u32 {
    let mut total = 0.0f64;
    for block in &doc.blocks {
        match block {
            Block::Heading { spans, .. }
            | Block::Paragraph(spans)
            | Block::ListItem { spans, .. }
            | Block::Blockquote(spans) => total += count_words_in_spans(spans),
            Block::Table(_)
            | Block::Infobox(_)
            | Block::Code(_)
            | Block::Image { .. }
            | Block::Gallery(_)
            | Block::Rule
            | Block::Math { .. } => {}
        }
    }
    total.round() as u32
}

/// PRD FR-RD-11: word count ÷ configurable WPM (`reading_wpm`, default 230),
/// rounded *up* so a short article reads "1 min" rather than "0 min" — a
/// reader seeing "0 min read" would reasonably wonder if the estimate is
/// broken, not "instant." An empty/wordless document is the one genuine "0
/// min" case (there is nothing to round up from).
pub fn reading_minutes(words: u32, wpm: u32) -> u32 {
    if words == 0 {
        return 0;
    }
    words.div_ceil(wpm.max(1))
}

/// True for elements whose entire subtree should be dropped: edit-section
/// links, and common navigation/maintenance boxes that are noise for a
/// reader (navboxes, maintenance banners). Infoboxes are deliberately not
/// filtered here — they get their own `Block::Infobox`.
fn is_noise(el: &scraper::node::Element) -> bool {
    has_class(el, "mw-editsection")
        || has_class(el, "navbox")
        || has_class(el, "ambox")
        || has_class(el, "metadata")
        || has_class(el, "noprint")
}

fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true; // trims leading whitespace
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Collapse whitespace runs to single spaces WITHOUT trimming the ends —
/// the boundary space between a text node and an adjacent inline element
/// ("See <a>X</a> and…") is real content; end-trimming each text node (as
/// `normalize_ws` does) jams words together across span boundaries, which
/// both reads wrong and defeats space-based line breaking in the layout
/// engine. Block-level end-trimming happens once, in `trim_inline_ends`.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out
}

/// Trim leading/trailing whitespace across a whole inline run (dropping
/// spans that become empty) — the block-level counterpart of `collapse_ws`,
/// keeping paragraphs from starting or ending with stray boundary spaces.
fn trim_inline_ends(spans: &mut Vec<Span>) {
    while let Some(first) = spans.first_mut() {
        let trimmed = first.text.trim_start();
        if trimmed.is_empty() {
            spans.remove(0);
        } else {
            if trimmed.len() != first.text.len() {
                first.text = trimmed.to_string();
            }
            break;
        }
    }
    while let Some(last) = spans.last_mut() {
        let trimmed = last.text.trim_end();
        if trimmed.is_empty() {
            spans.pop();
        } else {
            if trimmed.len() != last.text.len() {
                last.text = trimmed.to_string();
            }
            break;
        }
    }
}

/// PRD SEC-3's non-recursive fallback: once a walker hits `MAX_DOM_DEPTH` it
/// stops recursing and instead collects the subtree's text via
/// `NodeRef::descendants()`, which walks by following sibling/parent
/// pointers rather than the call stack — so, unlike every other function in
/// this file, it cannot itself overflow no matter how deep the subtree
/// nests below this point.
fn flatten_deep_subtree(node: NodeRef<Node>) -> String {
    let mut out = String::new();
    for n in node.descendants() {
        if let Node::Text(t) = n.value() {
            out.push_str(&t.text);
        }
    }
    out
}

fn text_content(node: NodeRef<Node>) -> String {
    let mut out = String::new();
    collect_text(node, &mut out, 0);
    out
}

fn collect_text(node: NodeRef<Node>, out: &mut String, depth: usize) {
    if depth > MAX_DOM_DEPTH {
        out.push_str(&flatten_deep_subtree(node));
        return;
    }
    for child in node.children() {
        match child.value() {
            Node::Text(t) => out.push_str(&t.text),
            Node::Element(el) => {
                if is_skipped_tag(el.name()) || is_noise(el) {
                    continue;
                }
                collect_text(child, out, depth + 1);
            }
            _ => {}
        }
    }
}

fn collect_inline(node: NodeRef<Node>, style: &SpanStyle, spans: &mut Vec<Span>, depth: usize) {
    if depth > MAX_DOM_DEPTH {
        // Flatten-to-text (SEC-3): stop descending and fold whatever
        // remains of this subtree into one plain-styled span rather than
        // recursing further.
        let text = collapse_ws(&flatten_deep_subtree(node));
        if !text.is_empty() {
            spans.push(Span {
                text,
                style: style.clone(),
            });
        }
        return;
    }
    for child in node.children() {
        match child.value() {
            Node::Text(t) => {
                let s = collapse_ws(&t.text);
                if !s.is_empty() {
                    spans.push(Span {
                        text: s,
                        style: style.clone(),
                    });
                }
            }
            Node::Element(el) => {
                let tag = el.name();
                if is_skipped_tag(tag) || is_noise(el) || is_block_tag(tag) {
                    continue;
                }
                if tag == "br" {
                    spans.push(Span {
                        text: "\n".to_string(),
                        style: SpanStyle::Plain,
                    });
                    continue;
                }
                // PRD FR-RD-7: a math node found inline (the common case —
                // Parsoid math almost always sits inside running prose)
                // becomes one Math span and is never descended into: its
                // children are MathML presentation markup and a fallback
                // `<img>`, neither of which `collect_inline`'s ordinary tag
                // handling below should ever see as "text"/"an image block".
                if is_math_node(el) {
                    if let Some(math) = extract_math(child) {
                        spans.push(Span {
                            text: math.tex.clone(),
                            style: SpanStyle::Math(math.tex),
                        });
                    }
                    continue;
                }
                // Once inside a link, keep treating the whole run as a link
                // (a bold word inside a link stays a link for our purposes).
                let child_style = if matches!(style, SpanStyle::Link(_) | SpanStyle::RedLink(_)) {
                    style.clone()
                } else {
                    match tag {
                        // PRD FR-DL-5: Parsoid's own redlink pre-marking
                        // (`class="new"`) is the cheapest detection path,
                        // checked here at parse time — see `SpanStyle::RedLink`.
                        "a" if has_class(el, "new") => {
                            SpanStyle::RedLink(el.attr("href").unwrap_or("").to_string())
                        }
                        "a" => SpanStyle::Link(el.attr("href").unwrap_or("").to_string()),
                        "b" | "strong" => SpanStyle::Bold,
                        "i" | "em" => SpanStyle::Italic,
                        "sup" => SpanStyle::Superscript,
                        _ => style.clone(),
                    }
                };
                collect_inline(child, &child_style, spans, depth + 1);
            }
            _ => {}
        }
    }
}

fn inline_spans(node: NodeRef<Node>) -> Vec<Span> {
    let mut spans = Vec::new();
    collect_inline(node, &SpanStyle::Plain, &mut spans, 0);
    trim_inline_ends(&mut spans);
    spans
}

/// Blockquotes commonly wrap their content in one or more `<p>` elements
/// (Parsoid's standard output for a quote), which `inline_spans` alone
/// would miss: `<p>` is a block tag and `collect_inline` correctly refuses
/// to cross into it (that rule is what stops list items from swallowing
/// nested lists twice). So a blockquote's paragraphs are pulled explicitly
/// here and joined; a blockquote with bare inline content falls back to
/// treating its own children as inline.
fn blockquote_spans(node: NodeRef<Node>) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut saw_paragraph = false;
    for child in node.children() {
        if let Node::Element(el) = child.value()
            && el.name() == "p"
        {
            if saw_paragraph && !spans.is_empty() {
                spans.push(Span {
                    text: " ".to_string(),
                    style: SpanStyle::Plain,
                });
            }
            saw_paragraph = true;
            spans.extend(inline_spans(child));
        }
    }
    if saw_paragraph {
        spans
    } else {
        inline_spans(node)
    }
}

/// Collects every descendant element matching `tag`, stopping at (but
/// including) nested tables so a top-level table's row-collapse doesn't
/// also vacuum up rows that belong to a table nested inside one of its
/// cells (a common infobox pattern).
///
/// PRD SEC-3: beyond `MAX_DOM_DEPTH` this simply stops descending —
/// pathologically nested table markup yields a table missing its
/// deepest rows rather than a stack overflow.
fn descendant_tags<'a>(
    node: NodeRef<'a, Node>,
    tag: &str,
    out: &mut Vec<NodeRef<'a, Node>>,
    depth: usize,
) {
    if depth > MAX_DOM_DEPTH {
        return;
    }
    for child in node.children() {
        if let Node::Element(el) = child.value() {
            if el.name() == tag {
                out.push(child);
                continue;
            }
            if el.name() == "table" && tag != "table" {
                // Don't dive into a nested table's own rows/cells here;
                // it will be walked as its own Block::Table separately.
                continue;
            }
        }
        descendant_tags(child, tag, out, depth + 1);
    }
}

/// The flattened inline text of one table cell (PRD FR-RD-4's flattening
/// pass): text nodes plus `<img>` alt text, with nested block elements
/// (paragraphs, and even tables nested inside a cell) contributing only
/// their text. Distinct from `text_content` in exactly one respect — it
/// substitutes an image's alt text where `text_content` would emit nothing
/// — so a cell that is just an icon still reads as its label.
fn cell_text(node: NodeRef<Node>) -> String {
    let mut out = String::new();
    collect_cell_text(node, &mut out, 0);
    normalize_ws(&out)
}

fn collect_cell_text(node: NodeRef<Node>, out: &mut String, depth: usize) {
    if depth > MAX_DOM_DEPTH {
        out.push_str(&flatten_deep_subtree(node));
        return;
    }
    for child in node.children() {
        match child.value() {
            Node::Text(t) => out.push_str(&t.text),
            Node::Element(el) => {
                if is_skipped_tag(el.name()) || is_noise(el) {
                    continue;
                }
                if el.name() == "img" {
                    let alt = el.attr("alt").unwrap_or("").trim();
                    if !alt.is_empty() {
                        out.push(' ');
                        out.push_str(alt);
                        out.push(' ');
                    }
                    continue;
                }
                collect_cell_text(child, out, depth + 1);
            }
            _ => {}
        }
    }
}

/// Reads a `colspan`/`rowspan` attribute, defaulting to 1 and clamping to
/// `max` (PRD SEC-3): a hostile `colspan="99999999"` must not be able to
/// allocate a giant row before the grid-level cap in `parse_table` even runs.
fn span_attr(el: &scraper::node::Element, name: &str, max: usize) -> usize {
    el.attr(name)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, max)
}

/// Parse a `<table>` into a rectangular [`Table`] grid (PRD FR-RD-4),
/// resolving rowspan/colspan by **expansion**: a cell occupies
/// `rowspan × colspan` grid positions — the value goes in the origin cell,
/// and every other position it covers becomes a blank [`Cell`] ("fill first,
/// blanks after" for colspan; "value on top, blanks below" for rowspan).
/// This is the standard TUI grid model: after this pass no cell carries a
/// span, so the renderer only ever sees a plain 2-D array.
///
/// PRD SEC-3: the grid is capped at [`MAX_TABLE_COLS`] × [`MAX_TABLE_ROWS`]
/// (with per-cell spans clamped first, see `span_attr`); anything beyond is
/// dropped and `Table::truncated` is set so the renderer can note it.
fn parse_table(node: NodeRef<Node>) -> Table {
    let mut trs = Vec::new();
    descendant_tags(node, "tr", &mut trs, 0);

    let mut rows: Vec<Vec<Cell>> = Vec::new();
    // `carried[col]` = how many more rows a rowspan started above keeps this
    // column occupied (each such position becomes a blank cell).
    let mut carried: Vec<usize> = Vec::new();
    let mut truncated = false;

    for tr in trs {
        if rows.len() >= MAX_TABLE_ROWS {
            truncated = true;
            break;
        }
        let cells: Vec<NodeRef<Node>> = tr
            .children()
            .filter(|c| {
                matches!(c.value(), Node::Element(el) if el.name() == "th" || el.name() == "td")
            })
            .collect();

        let mut row: Vec<Cell> = Vec::new();
        let mut col = 0usize;
        let mut cell_iter = cells.into_iter();

        loop {
            // Emit blanks for any column a rowspan from above still occupies.
            while col < carried.len() && carried[col] > 0 {
                if col >= MAX_TABLE_COLS {
                    break;
                }
                row.push(Cell::blank());
                carried[col] -= 1;
                col += 1;
            }
            if col >= MAX_TABLE_COLS {
                truncated = true;
                break;
            }
            let Some(cell_node) = cell_iter.next() else {
                break;
            };
            let Node::Element(el) = cell_node.value() else {
                continue;
            };
            let header = el.name() == "th";
            let colspan = span_attr(el, "colspan", MAX_TABLE_COLS);
            let rowspan = span_attr(el, "rowspan", MAX_TABLE_ROWS);
            let text = cell_text(cell_node);

            for k in 0..colspan {
                if col >= MAX_TABLE_COLS {
                    truncated = true;
                    break;
                }
                while carried.len() <= col {
                    carried.push(0);
                }
                let cell = if k == 0 {
                    Cell {
                        text: text.clone(),
                        header,
                    }
                } else {
                    Cell::blank()
                };
                row.push(cell);
                // Reserve this column for the remaining rowspan rows (blanks).
                if rowspan > 1 {
                    carried[col] = rowspan - 1;
                }
                col += 1;
            }
        }

        // Trailing columns still held by a rowspan from above.
        while col < carried.len() && col < MAX_TABLE_COLS {
            if carried[col] > 0 {
                row.push(Cell::blank());
                carried[col] -= 1;
            }
            col += 1;
        }

        if !row.is_empty() {
            rows.push(row);
        }
    }

    // Rectangularize: pad every row to the widest, capped at MAX_TABLE_COLS.
    let width = rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .min(MAX_TABLE_COLS);
    for row in &mut rows {
        if row.len() > width {
            row.truncate(width);
        }
        while row.len() < width {
            row.push(Cell::blank());
        }
    }
    // Drop rows that ended up entirely blank (a rowspan-only tail row).
    rows.retain(|r| r.iter().any(|c| !c.text.is_empty()));

    Table { rows, truncated }
}

fn collapse_infobox(node: NodeRef<Node>) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let mut trs = Vec::new();
    descendant_tags(node, "tr", &mut trs, 0);
    for tr in trs {
        let mut cells = Vec::new();
        for cell in tr.children() {
            if let Node::Element(el) = cell.value()
                && (el.name() == "th" || el.name() == "td")
            {
                let text = normalize_ws(&text_content(cell));
                if !text.is_empty() {
                    cells.push(text);
                }
            }
        }
        match cells.len() {
            0 => {}
            1 => rows.push((String::new(), cells.into_iter().next().unwrap())),
            _ => {
                let mut it = cells.into_iter();
                let label = it.next().unwrap();
                let value = it.collect::<Vec<_>>().join(", ");
                rows.push((label, value));
            }
        }
    }
    rows
}

/// PRD SEC-2 spirit: only `http`/`https` image URLs are kept; every other
/// scheme (`data:`, `javascript:`, relative wiki paths that need a base we
/// don't resolve here, …) is dropped so only the alt text renders (PRD
/// FR-RD-8: "alt text always available"). Parsoid's common protocol-relative
/// thumbnail form (`//upload.wikimedia.org/...`) is upgraded to `https`.
fn sanitize_image_src(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let url = match s.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => s.to_string(),
    };
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Some(sanitize::sanitize_and_cap_single_line(
            &url,
            sanitize::MAX_SPAN_CHARS,
        ))
    } else {
        None
    }
}

/// The sanitized `src` and always-present `alt` of the first `<img>` under
/// `node` (PRD FR-RD-8). `src` from the `src` attribute, falling back to
/// Parsoid's `resource`; `alt` from `alt`, defaulting to "image".
///
/// PRD SEC-3: past `MAX_DOM_DEPTH` this gives up rather than recursing — a
/// missed image on a pathologically nested `<figure>` is acceptable; a stack
/// overflow is not.
fn find_img_src_alt(node: NodeRef<Node>, depth: usize) -> Option<(Option<String>, String)> {
    if depth > MAX_DOM_DEPTH {
        return None;
    }
    for child in node.children() {
        if let Node::Element(el) = child.value() {
            if el.name() == "img" {
                let alt = el.attr("alt").unwrap_or("").trim();
                let alt = if alt.is_empty() {
                    "image".to_string()
                } else {
                    alt.to_string()
                };
                let src = el
                    .attr("src")
                    .or_else(|| el.attr("resource"))
                    .and_then(sanitize_image_src);
                return Some((src, alt));
            }
            if let Some(found) = find_img_src_alt(child, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// The first `<figcaption>` text under `node` (a `<figure>`'s caption).
fn find_figcaption(node: NodeRef<Node>, depth: usize) -> Option<String> {
    if depth > MAX_DOM_DEPTH {
        return None;
    }
    for child in node.children() {
        if let Node::Element(el) = child.value() {
            if el.name() == "figcaption" {
                let text = normalize_ws(&text_content(child));
                return (!text.is_empty()).then_some(text);
            }
            if let Some(found) = find_figcaption(child, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// The caption of one gallery box: its `.gallerytext` (MediaWiki's standard
/// gallery-item caption wrapper) or a `<figcaption>` fallback.
fn find_gallery_caption(node: NodeRef<Node>, depth: usize) -> Option<String> {
    if depth > MAX_DOM_DEPTH {
        return None;
    }
    for child in node.children() {
        if let Node::Element(el) = child.value() {
            let is_caption = el.name() == "figcaption"
                || el.attr("class").is_some_and(|c| c.contains("gallerytext"));
            if is_caption {
                let text = normalize_ws(&text_content(child));
                if !text.is_empty() {
                    return Some(text);
                }
            }
            if let Some(found) = find_gallery_caption(child, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Parse a `<ul class="gallery">` into its items (PRD FR-RD-8). Each `<li>`
/// (`.gallerybox`) yields a [`GalleryItem`] with its thumbnail src and a
/// caption (its `.gallerytext`, else the image alt). Boxes with neither an
/// image nor a caption are skipped.
fn parse_gallery(node: NodeRef<Node>) -> Vec<GalleryItem> {
    let mut items = Vec::new();
    for li in node.children() {
        let Node::Element(el) = li.value() else {
            continue;
        };
        if el.name() != "li" {
            continue;
        }
        let img = find_img_src_alt(li, 0);
        let caption_text = find_gallery_caption(li, 0);
        if img.is_none() && caption_text.is_none() {
            continue;
        }
        let (src, alt) = img.unwrap_or((None, "image".to_string()));
        items.push(GalleryItem {
            src,
            caption: caption_text.unwrap_or(alt),
        });
    }
    items
}

/// PRD SEC-3: `depth` guards recursion (distinct from `list_depth`, which is
/// a rendering concept — how many `<ul>/<ol>` levels deep a `<li>` sits, used
/// only for indentation). Past `MAX_DOM_DEPTH` a walker stops descending and
/// flattens whatever remains of the subtree into one paragraph instead — a
/// pathologically deep DOM (e.g. thousands of nested `<div>`s) degrades to
/// plain text rather than overflowing the stack.
fn walk_blocks(node: NodeRef<Node>, blocks: &mut Vec<Block>, list_depth: u8, depth: usize) {
    if depth > MAX_DOM_DEPTH {
        let text = normalize_ws(&flatten_deep_subtree(node));
        if !text.is_empty() {
            blocks.push(Block::Paragraph(vec![Span {
                text,
                style: SpanStyle::Plain,
            }]));
        }
        return;
    }
    for child in node.children() {
        let el = match child.value() {
            Node::Element(el) => el,
            _ => continue,
        };
        let tag = el.name();
        if is_skipped_tag(tag) || is_noise(el) {
            continue;
        }
        match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level: u8 = tag[1..].parse().unwrap_or(2);
                let spans = inline_spans(child);
                if !spans.is_empty() {
                    blocks.push(Block::Heading { level, spans });
                }
            }
            "p" => {
                let spans = inline_spans(child);
                if !spans.is_empty() {
                    blocks.push(Block::Paragraph(spans));
                }
            }
            "ul" | "ol" => {
                // A `<ul class="gallery">` is an image gallery (PRD FR-RD-8),
                // not an ordinary list — intercept it before list handling.
                if tag == "ul" && el.attr("class").is_some_and(|c| c.contains("gallery")) {
                    let items = parse_gallery(child);
                    if !items.is_empty() {
                        blocks.push(Block::Gallery(items));
                    }
                    continue;
                }
                let ordered = tag == "ol";
                let mut index = 0usize;
                for li in child.children() {
                    if let Node::Element(le) = li.value()
                        && le.name() == "li"
                    {
                        index += 1;
                        let spans = inline_spans(li);
                        if !spans.is_empty() {
                            blocks.push(Block::ListItem {
                                ordered,
                                index,
                                depth: list_depth,
                                spans,
                            });
                        }
                        // Recurse for nested lists/paragraphs inside this <li>.
                        walk_blocks(li, blocks, list_depth + 1, depth + 1);
                    }
                }
            }
            "blockquote" => {
                let spans = blockquote_spans(child);
                if !spans.is_empty() {
                    blocks.push(Block::Blockquote(spans));
                }
            }
            "pre" => {
                let text = text_content(child);
                if !text.trim().is_empty() {
                    blocks.push(Block::Code(text));
                }
            }
            "hr" => blocks.push(Block::Rule),
            "table" => {
                let is_infobox = el
                    .attr("class")
                    .map(|c| c.contains("infobox"))
                    .unwrap_or(false);
                if is_infobox {
                    let rows = collapse_infobox(child);
                    if !rows.is_empty() {
                        blocks.push(Block::Infobox(rows));
                    }
                } else {
                    let table = parse_table(child);
                    if !table.rows.is_empty() {
                        blocks.push(Block::Table(table));
                    }
                }
            }
            "figure" => {
                let (src, alt) = find_img_src_alt(child, 0).unwrap_or((None, "image".to_string()));
                blocks.push(Block::Image {
                    src,
                    alt,
                    caption: find_figcaption(child, 0),
                });
            }
            "img" => {
                let alt = el.attr("alt").unwrap_or("").trim();
                let alt = if alt.is_empty() {
                    "image".to_string()
                } else {
                    alt.to_string()
                };
                let src = el
                    .attr("src")
                    .or_else(|| el.attr("resource"))
                    .and_then(sanitize_image_src);
                blocks.push(Block::Image {
                    src,
                    alt,
                    caption: None,
                });
            }
            // PRD FR-RD-7: a math node encountered directly at block-scanning
            // level rather than inside a `<p>`'s inline run — Parsoid's usual
            // shape for a leading-colon display equation (`<dl><dd><span
            // typeof="mw:Extension/math">...`), reached here via the `_`
            // wildcard's plain recursion through the intervening `<dl>`/`<dd>`
            // (neither is block-tag-handled on its own, so this arm is what
            // actually stops the descent once it reaches the math span
            // itself — without it, the wildcard would recurse straight into
            // the math node's MathML/fallback-image internals instead).
            "span" if is_math_node(el) => {
                if let Some(math) = extract_math(child) {
                    blocks.push(Block::Math {
                        tex: math.tex,
                        display: math.display,
                    });
                }
            }
            _ => walk_blocks(child, blocks, list_depth, depth + 1),
        }
    }
}

/// Parse a Parsoid (or legacy-parser) HTML document into a `Document`.
/// The page's real display title, from Parsoid HTML's `<head><title>`
/// (MediaWiki always renders this with spaces, not underscores). Preferred
/// over the caller-supplied title, which may be whatever underscored or
/// differently-cased form a link href or CLI argument happened to use —
/// this matters beyond cosmetics: it's what ends up in a saved citation's
/// text (`research::self_citation`).
fn page_display_title(parsed: &Html) -> Option<String> {
    let sel = Selector::parse("head > title").unwrap();
    let text: String = parsed.select(&sel).next()?.text().collect();
    let normalized = normalize_ws(&text);
    (!normalized.is_empty()).then_some(normalized)
}

/// PRD SEC-3: truncates `html` to at most `MAX_ARTICLE_HTML_BYTES` bytes (on
/// a UTF-8 char boundary, never splitting a multi-byte character) before it
/// ever reaches the parser. Returns `(slice, was_truncated)`. This is the
/// authoritative truncation decision for the whole app: `api.rs`'s own
/// network-level read cap only bounds memory during the fetch and does not
/// itself decide the truncation flag — whatever HTML string parse_article_html
/// is handed (fresh fetch, on-disk cache, or a test fixture), this is where
/// "too big" is decided, consistently.
fn cap_html_size(html: &str) -> (&str, bool) {
    if html.len() <= MAX_ARTICLE_HTML_BYTES {
        return (html, false);
    }
    let mut cut = MAX_ARTICLE_HTML_BYTES;
    while cut > 0 && !html.is_char_boundary(cut) {
        cut -= 1;
    }
    (&html[..cut], true)
}

/// HTML5 void elements: never have a closing tag and never nest content, so
/// they must not count as a lasting depth increment even when a hostile (or
/// just old-style) document writes them without a self-closing `/>`.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Raw-text elements: everything up to the matching close tag is opaque
/// content, never tag nesting (a `<script>` body routinely contains `<`/`>`
/// that isn't markup at all).
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style"];

/// PRD SEC-3: html5ever's tree-construction algorithm is empirically
/// superlinear in DOM nesting depth — measured directly against this
/// crate's `scraper`/`html5ever` versions, a document that's just `<div>`
/// nested a few thousand deep (a few hundred KB, nowhere near
/// `MAX_ARTICLE_HTML_BYTES`) takes seconds to parse, and doubling depth
/// roughly quadruples the time. The byte-size cap alone does not bound this
/// class of pathological input, so this is a cheap **linear** pre-scan run
/// *before* `Html::parse_document` ever sees the content, cutting the HTML
/// off the moment counted nesting depth exceeds `max_depth` — comfortably
/// inside html5ever's fast zone, since a real article's DOM nests at most a
/// few dozen levels.
///
/// This is deliberately not a full HTML5 tokenizer — that would mean
/// reimplementing html5ever — but it accounts for the cases that would
/// otherwise make it wildly wrong on ordinary articles: comments,
/// doctype/processing-instruction markers, quoted attribute values (so a
/// `>` inside `alt="a > b"` doesn't end a tag early), the void-element list,
/// and raw-text element bodies. It can still mis-count on sufficiently
/// unusual or malformed markup (mismatched quote types, for instance); that
/// is an accepted approximation for a pre-parse guard, not the source of
/// truth — the DOM-depth guard on the walkers below (`MAX_DOM_DEPTH`) is the
/// backstop for whatever this pre-scan lets through.
fn cap_html_nesting_depth(html: &str, max_depth: usize) -> (&str, bool) {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut depth: usize = 0;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if html[i..].starts_with("<!--") {
            match html[i + 4..].find("-->") {
                Some(end) => i += 4 + end + 3,
                None => break,
            }
            continue;
        }
        if matches!(bytes.get(i + 1), Some(b'!') | Some(b'?')) {
            match html[i..].find('>') {
                Some(rel) => i += rel + 1,
                None => break,
            }
            continue;
        }
        if bytes.get(i + 1) == Some(&b'/') {
            depth = depth.saturating_sub(1);
            match html[i..].find('>') {
                Some(rel) => i += rel + 1,
                None => break,
            }
            continue;
        }

        // Opening tag: extract the name, then scan to its end respecting
        // quoted attribute values.
        let tag_start = i + 1;
        let name_end = html[tag_start..]
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .map(|o| tag_start + o)
            .unwrap_or(len);
        if name_end == tag_start {
            // A bare '<' not starting a tag name (stray '<' in text) — not
            // markup; move past just this character.
            i += 1;
            continue;
        }
        let name = html[tag_start..name_end].to_ascii_lowercase();

        let mut j = name_end;
        let mut in_quote: Option<u8> = None;
        let mut self_closing = false;
        while j < len {
            let b = bytes[j];
            match in_quote {
                Some(q) if b == q => in_quote = None,
                Some(_) => {}
                None => match b {
                    b'"' | b'\'' => in_quote = Some(b),
                    b'>' => break,
                    b'/' if bytes.get(j + 1) == Some(&b'>') => self_closing = true,
                    _ => {}
                },
            }
            j += 1;
        }
        i = (j + 1).min(len);

        if RAW_TEXT_ELEMENTS.contains(&name.as_str()) {
            let closing = format!("</{name}");
            match html[i..].to_ascii_lowercase().find(&closing) {
                Some(rel) => i += rel,
                None => i = len,
            }
            continue;
        }

        if !self_closing && !VOID_ELEMENTS.contains(&name.as_str()) {
            depth += 1;
            if depth > max_depth {
                return (&html[..tag_start - 1], true);
            }
        }
    }
    (html, false)
}

/// PRD §7's "degraded rendering" banner text for a truncated article,
/// prepended as the first block so it shows up both in the interactive
/// reading view and in `--dump` output — no separate UI plumbing needed.
const TRUNCATED_BANNER: &str = "⚠ Degraded rendering: this article exceeded a PRD SEC-3 parser limit (size or DOM nesting depth) and was truncated. Content beyond that point was not parsed.";

pub fn parse_article_html(title: &str, html: &str) -> Document {
    let (html, size_truncated) = cap_html_size(html);
    // Depth guard runs second (SEC-3): a genuinely oversized article is
    // already cut to at most MAX_ARTICLE_HTML_BYTES above, so this scan
    // — and the parse after it — is bounded by that same ceiling either
    // way, size-pathological or depth-pathological.
    let (html, depth_truncated) = cap_html_nesting_depth(html, MAX_DOM_DEPTH);
    let truncated = size_truncated || depth_truncated;
    let parsed = Html::parse_document(html);
    let body_sel = Selector::parse("body").unwrap();
    let start = parsed
        .select(&body_sel)
        .next()
        .map(|er| *er)
        .unwrap_or_else(|| parsed.tree.root());

    let mut blocks = Vec::new();
    walk_blocks(start, &mut blocks, 0, 0);
    let citations = extract_citations(&parsed);
    let display_title = page_display_title(&parsed).unwrap_or_else(|| title.to_string());
    let mut document = Document {
        title: display_title,
        blocks,
        citations,
        truncated,
    };

    // PRD SEC-1: the single choke point every string this function put into
    // `document` passes through before it's handed back to callers (the UI,
    // `--dump`, the clipboard yank, `cite.rs` exports) — see its doc comment.
    sanitize_document(&mut document);

    if document.truncated {
        document.blocks.insert(
            0,
            Block::Paragraph(vec![Span {
                text: TRUNCATED_BANNER.to_string(),
                style: SpanStyle::Bold,
            }]),
        );
    }

    document
}

/// PRD SEC-1's single sanitization choke point for this module. Called
/// exactly once, as the last step of `parse_article_html`, over the fully
/// assembled `Document` — not scattered across `collect_inline`/
/// `walk_blocks`/etc, so there is one place to audit and one place future
/// fields must be wired into.
///
/// Every consumer downstream of this function only ever sees
/// already-sanitized data, "by construction": `--dump` calls `render_plain`
/// on this same `Document`; the OSC 52 clipboard yank (`main::yank_to_clipboard`)
/// only ever encodes `research::article_url`/`yank_markdown`, both built
/// from `doc.title`; `cite.rs`'s bibliography export only ever formats
/// `Citation`s that were either produced here or copied from here into
/// `research::SavedCitation`. None of those call sites need — or get — a
/// second sanitization pass.
fn sanitize_document(doc: &mut Document) {
    doc.title = sanitize::sanitize_and_cap_single_line(&doc.title, sanitize::MAX_SPAN_CHARS);

    for block in &mut doc.blocks {
        match block {
            Block::Heading { spans, .. }
            | Block::Paragraph(spans)
            | Block::ListItem { spans, .. }
            | Block::Blockquote(spans) => sanitize_spans(spans),
            Block::Code(text) => {
                *text = sanitize::sanitize_and_cap_multiline(text, sanitize::MAX_SPAN_CHARS);
            }
            Block::Table(table) => {
                for row in &mut table.rows {
                    for cell in row {
                        cell.text = sanitize::sanitize_and_cap_single_line(
                            &cell.text,
                            sanitize::MAX_SPAN_CHARS,
                        );
                    }
                }
            }
            Block::Infobox(rows) => {
                for (label, value) in rows {
                    *label =
                        sanitize::sanitize_and_cap_single_line(label, sanitize::MAX_SPAN_CHARS);
                    *value =
                        sanitize::sanitize_and_cap_single_line(value, sanitize::MAX_SPAN_CHARS);
                }
            }
            Block::Image { alt, caption, .. } => {
                *alt = sanitize::sanitize_and_cap_single_line(alt, sanitize::MAX_SPAN_CHARS);
                if let Some(caption) = caption {
                    *caption =
                        sanitize::sanitize_and_cap_single_line(caption, sanitize::MAX_SPAN_CHARS);
                }
            }
            Block::Gallery(items) => {
                for item in items {
                    item.caption = sanitize::sanitize_and_cap_single_line(
                        &item.caption,
                        sanitize::MAX_SPAN_CHARS,
                    );
                }
            }
            Block::Rule => {}
            Block::Math { tex, .. } => {
                *tex = sanitize::sanitize_and_cap_multiline(tex, sanitize::MAX_SPAN_CHARS);
            }
        }
    }

    for citation in &mut doc.citations {
        citation.id =
            sanitize::sanitize_and_cap_single_line(&citation.id, sanitize::MAX_SPAN_CHARS);
        citation.text =
            sanitize::sanitize_and_cap_single_line(&citation.text, sanitize::MAX_SPAN_CHARS);
        if let Some(url) = &mut citation.url {
            *url = sanitize::sanitize_and_cap_single_line(url, sanitize::MAX_SPAN_CHARS);
        }
    }
}

/// Sanitizes both a span's visible text and, for a link span, the `href` it
/// carries — the latter matters because `main.rs` displays an external
/// link's raw href verbatim ("External link: {href}") and because
/// `internal_title_from_href` decodes it into the title used to open the
/// next page; sanitizing here means neither path can see a raw control
/// byte, even though `internal_title_from_href` also sanitizes its own
/// decoded output (a hostile `%1B`-style percent-escape only becomes a live
/// control byte *after* decoding, downstream of this pass).
fn sanitize_spans(spans: &mut [Span]) {
    for span in spans {
        span.text = sanitize::sanitize_and_cap_multiline(&span.text, sanitize::MAX_SPAN_CHARS);
        match &mut span.style {
            SpanStyle::Link(href) | SpanStyle::RedLink(href) => {
                *href = sanitize::sanitize_and_cap_single_line(href, sanitize::MAX_SPAN_CHARS);
            }
            // The span's own `text` (sanitized just above) and this payload
            // started from the same raw TeX; sanitize the copy on the style
            // too so the two can never diverge into "text sanitized, style
            // payload not" — see `SpanStyle::Math`'s doc comment.
            SpanStyle::Math(tex) => {
                *tex = sanitize::sanitize_and_cap_multiline(tex, sanitize::MAX_SPAN_CHARS);
            }
            SpanStyle::Plain | SpanStyle::Bold | SpanStyle::Italic | SpanStyle::Superscript => {}
        }
    }
}

/// Wikipedia's leading reference-list backlink glyph (a caret or, for
/// multiply-cited references, lettered backlinks like "a b c") gets swept
/// up by `ElementRef::text()` along with the actual citation; strip a
/// short leading run of those before the real content starts.
fn strip_backlink_markers(s: &str) -> String {
    s.trim_start()
        .trim_start_matches(['^', '↑'])
        .trim_start()
        .to_string()
}

/// Extracts each entry of an article's References section (PRD-adjacent:
/// Research mode's "sources this article cites" half). Heuristic over the
/// common MediaWiki Cite-extension markup (`<ol class="references">`, a
/// `.reference-text` span holding the rendered citation) — not a full
/// bibliographic parser, and third-party wikis without that convention
/// simply yield no citations.
///
/// PRD SEC-3: capped at `MAX_CITATIONS` — a hostile page padding its
/// references list arbitrarily large shouldn't make Research mode (or a
/// bibliography export) scale with it.
fn extract_citations(parsed: &Html) -> Vec<Citation> {
    let li_sel = Selector::parse("ol.references li").unwrap();
    let reftext_sel = Selector::parse(".reference-text").unwrap();
    let link_sel = Selector::parse("a[href^='http']").unwrap();

    parsed
        .select(&li_sel)
        .take(MAX_CITATIONS)
        .map(|li| {
            let id = li.value().attr("id").unwrap_or_default().to_string();
            let text_source = li.select(&reftext_sel).next().unwrap_or(li);
            let raw_text: String = text_source.text().collect();
            let text = normalize_ws(&strip_backlink_markers(&raw_text));
            let url = text_source
                .select(&link_sel)
                .next()
                .or_else(|| li.select(&link_sel).next())
                .and_then(|a| a.value().attr("href"))
                .map(str::to_string);
            Citation { id, text, url }
        })
        .collect()
}

/// Resolves a Parsoid-style href to the absolute URL it targets: the
/// canonical article URL for an internal link (`./Title`/`/wiki/Title`,
/// via [`internal_title_from_href`] + `research::article_url` — always
/// `https://`), or the href itself for anything else (external URL,
/// interwiki, `mailto:`, ...). Shared by `render_plain`'s `[link: target]`
/// fallback (PRD FR-ACS-1) and OSC 8 emission's own resolution step
/// (`main::emit_hyperlinks`) so the two paths can never disagree about what a
/// link "goes to" — SEC-2's scheme gate for OSC 8 is applied by that caller,
/// not here: printing an arbitrary scheme as plain informative text (this
/// function's other use) is harmless in a way emitting it as a terminal
/// escape is not.
pub fn resolve_link_url(href: &str, lang: &str) -> String {
    match internal_title_from_href(href) {
        Some(title) => crate::research::article_url(&title, lang),
        None => href.to_string(),
    }
}

/// Render a document as plain text (FR-RD-12, the `--dump` linear mode and
/// the honest screen-reader path). No color, no cursor addressing. `lang`
/// resolves internal links to their canonical URL (PRD FR-ACS-1's `[link:
/// target]` — the only way a link is "followable" from a plain stdout dump,
/// which has no clickable escape and no interactive focus/Enter to follow
/// one).
pub fn render_plain(doc: &Document, lang: &str) -> String {
    let mut out = String::new();
    out.push_str(&doc.title);
    out.push('\n');
    out.push_str(&"=".repeat(doc.title.chars().count()));
    out.push_str("\n\n");

    let flatten = |spans: &[Span]| -> String {
        spans
            .iter()
            .map(|s| match &s.style {
                SpanStyle::Link(href) | SpanStyle::RedLink(href) => {
                    format!("{}[link: {}]", s.text, resolve_link_url(href, lang))
                }
                _ => s.text.clone(),
            })
            .collect::<Vec<_>>()
            .join("")
    };

    for block in &doc.blocks {
        match block {
            Block::Heading { level, spans } => {
                let text = flatten(spans);
                let marker = "#".repeat(*level as usize);
                out.push_str(&format!("\n{marker} {text}\n\n"));
            }
            Block::Paragraph(spans) => {
                out.push_str(&flatten(spans));
                out.push_str("\n\n");
            }
            Block::ListItem {
                ordered,
                index,
                depth,
                spans,
            } => {
                let indent = "  ".repeat(*depth as usize);
                let bullet = if *ordered {
                    format!("{index}.")
                } else {
                    "-".to_string()
                };
                out.push_str(&format!("{indent}{bullet} {}\n", flatten(spans)));
            }
            Block::Blockquote(spans) => {
                out.push_str(&format!("> {}\n\n", flatten(spans)));
            }
            Block::Code(text) => {
                for line in text.lines() {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            Block::Rule => out.push_str("----\n\n"),
            Block::Table(table) => {
                // PRD FR-ACS-1: `--dump` is the screen-reader path, where
                // tables are collapsed to lists ("Header: value"), not drawn
                // as a grid — consistent, no cursor addressing, no ANSI.
                for line in table.to_list_lines() {
                    out.push_str(&line);
                    out.push('\n');
                }
                out.push('\n');
            }
            Block::Infobox(rows) => {
                out.push_str("[infobox]\n");
                for (label, value) in rows {
                    if label.is_empty() {
                        out.push_str(&format!("  {value}\n"));
                    } else {
                        out.push_str(&format!("  {label}: {value}\n"));
                    }
                }
                out.push('\n');
            }
            Block::Image { alt, caption, .. } => {
                out.push_str(&format!("[image: {alt}]\n"));
                if let Some(caption) = caption {
                    out.push_str(&format!("{caption}\n"));
                }
                out.push('\n');
            }
            Block::Gallery(items) => {
                for item in items {
                    out.push_str(&format!("[image: {}]\n", item.caption));
                }
                out.push('\n');
            }
            // PRD FR-RD-7: `--dump` shows the raw TeX plainly — no ⟨⟩
            // delimiters, no Unicode sup/sub normalization, no escaping.
            // Those are interactive-rendering choices (`layout.rs`); the
            // document model's own plain-text form is the source string
            // untouched, exactly like every other block above.
            Block::Math { tex, .. } => {
                out.push_str(tex);
                out.push_str("\n\n");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-written approximation of Parsoid HTML for a small article,
    /// covering the structures the parser needs to handle: headings,
    /// paragraphs with links/bold/italic/references, a two-level list, a
    /// blockquote, an infobox table, a data table, an image, and noise
    /// (edit-section spans, navboxes) that must be dropped.
    const FIXTURE: &str = r##"
    <html><head><title>Test Article</title></head><body>
    <section data-mw-section-id="0">
      <table class="infobox">
        <tbody>
          <tr><th colspan="2">Test Subject</th></tr>
          <tr><th>Born</th><td>1912</td></tr>
          <tr><th>Field</th><td>Computer science</td></tr>
        </tbody>
      </table>
      <p>This is <b>bold</b> and <i>italic</i> text with a <a href="./Other_Article">link</a>
      and a reference<sup class="reference"><a href="#cite_note-1">[1]</a></sup>.</p>
    </section>
    <section data-mw-section-id="1">
      <h2 id="History"><span class="mw-headline">History</span><span class="mw-editsection">[edit]</span></h2>
      <p>Some history text.</p>
      <ul>
        <li>First item</li>
        <li>Second item
          <ul><li>Nested item</li></ul>
        </li>
      </ul>
      <blockquote><p>A quoted remark.</p></blockquote>
      <table class="wikitable">
        <tbody>
          <tr><th>Year</th><th>Event</th></tr>
          <tr><td>1950</td><td>Something happened</td></tr>
        </tbody>
      </table>
      <figure><img src="pic.jpg" alt="A test picture"/></figure>
      <table class="navbox"><tbody><tr><td>Nav junk that should be dropped</td></tr></tbody></table>
    </section>
    </body></html>
    "##;

    #[test]
    fn parses_expected_block_shape() {
        let doc = parse_article_html("Test Article", FIXTURE);

        let infobox_count = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Infobox(_)))
            .count();
        assert_eq!(infobox_count, 1, "expected exactly one infobox block");

        let heading_count = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Heading { .. }))
            .count();
        assert_eq!(heading_count, 1, "expected exactly one heading (History)");

        let list_items: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::ListItem { depth, spans, .. } => Some((
                    *depth,
                    spans.iter().map(|s| s.text.clone()).collect::<String>(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            list_items.len(),
            3,
            "expected 3 list items (2 top-level + 1 nested)"
        );
        assert_eq!(list_items[0], (0, "First item".to_string()));
        assert_eq!(list_items[1], (0, "Second item".to_string()));
        assert_eq!(list_items[2].0, 1, "nested item should be at depth 1");

        let has_blockquote = doc.blocks.iter().any(|b| matches!(b, Block::Blockquote(_)));
        assert!(has_blockquote, "expected a blockquote block");

        let data_tables: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Table(table) => Some(table),
                _ => None,
            })
            .collect();
        // The navbox table must be dropped by the noise filter; only the
        // "wikitable" should survive.
        assert_eq!(data_tables.len(), 1, "navbox table should be filtered out");
        assert!(
            data_tables[0]
                .rows
                .iter()
                .flatten()
                .any(|c| c.text.contains("1950"))
        );
        // The header row must be recognized as such (Year/Event are <th>).
        assert!(data_tables[0].has_header_row());

        let has_image = doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Image { alt, src, .. } if alt == "A test picture" && src.is_none()));
        assert!(
            has_image,
            "expected the figure's image to produce an image block (relative src dropped to alt-only)"
        );
    }

    #[test]
    fn figure_parses_http_src_alt_and_caption() {
        let html = r#"<html><head><title>T</title></head><body>
          <figure typeof="mw:File">
            <img src="https://upload.example.org/thumb/Pic.png" alt="A cat"/>
            <figcaption>A tabby cat, 1904</figcaption>
          </figure>
        </body></html>"#;
        let doc = parse_article_html("T", html);
        let img = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Image { src, alt, caption } => {
                    Some((src.clone(), alt.clone(), caption.clone()))
                }
                _ => None,
            })
            .expect("figure produces an image block");
        assert_eq!(
            img.0.as_deref(),
            Some("https://upload.example.org/thumb/Pic.png")
        );
        assert_eq!(img.1, "A cat");
        assert_eq!(img.2.as_deref(), Some("A tabby cat, 1904"));
    }

    #[test]
    fn protocol_relative_src_is_upgraded_and_bad_schemes_dropped() {
        // Parsoid's `//host/...` thumbnails upgrade to https; data:/relative
        // URLs drop to alt-only (SEC-2 spirit), alt always preserved.
        let cases = [
            (
                "//upload.example.org/a.png",
                Some("https://upload.example.org/a.png"),
            ),
            ("data:image/png;base64,AAAA", None),
            ("/w/local/thumb.png", None),
            ("javascript:alert(1)", None),
        ];
        for (raw, expect) in cases {
            let html = format!(
                "<html><body><figure><img src=\"{raw}\" alt=\"x\"/></figure></body></html>"
            );
            let doc = parse_article_html("T", &html);
            let src = doc.blocks.iter().find_map(|b| match b {
                Block::Image { src, .. } => Some(src.clone()),
                _ => None,
            });
            assert_eq!(
                src.expect("image block").as_deref(),
                expect,
                "src {raw:?} sanitized wrong"
            );
        }
    }

    #[test]
    fn gallery_parses_items_with_captions() {
        let html = r#"<html><head><title>T</title></head><body>
          <ul class="gallery mw-gallery-traditional">
            <li class="gallerybox">
              <div class="thumb"><img src="https://ex.org/1.png" alt="one"/></div>
              <div class="gallerytext">First caption</div>
            </li>
            <li class="gallerybox">
              <div class="thumb"><img src="https://ex.org/2.png" alt="two"/></div>
              <div class="gallerytext">Second caption</div>
            </li>
          </ul>
        </body></html>"#;
        let doc = parse_article_html("T", html);
        let items = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Gallery(items) => Some(items.clone()),
                _ => None,
            })
            .expect("gallery block present");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].caption, "First caption");
        assert_eq!(items[0].src.as_deref(), Some("https://ex.org/1.png"));
        assert_eq!(items[1].caption, "Second caption");
    }

    #[test]
    fn render_plain_shows_alt_caption_and_gallery_as_text() {
        let html = r#"<html><head><title>T</title></head><body>
          <figure><img src="https://ex.org/p.png" alt="An owl"/><figcaption>An owl at night</figcaption></figure>
          <ul class="gallery"><li class="gallerybox"><img src="https://ex.org/g.png" alt="g"/><div class="gallerytext">Gallery one</div></li></ul>
        </body></html>"#;
        let doc = parse_article_html("T", html);
        let dump = render_plain(&doc, "en");
        assert!(dump.contains("[image: An owl]"), "alt in dump: {dump:?}");
        assert!(dump.contains("An owl at night"), "caption in dump");
        assert!(
            dump.contains("[image: Gallery one]"),
            "gallery caption in dump"
        );
        // A plain dump carries no escape/half-block bytes.
        assert!(!dump.contains('\u{1b}'));
        assert!(!dump.contains('▀'));
    }

    #[test]
    fn strips_editsection_noise_from_heading() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let heading = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Heading { spans, .. } => {
                    Some(spans.iter().map(|s| s.text.clone()).collect::<String>())
                }
                _ => None,
            })
            .expect("heading present");
        assert_eq!(
            heading, "History",
            "the [edit] mw-editsection span must not leak into heading text"
        );
    }

    #[test]
    fn preserves_link_and_emphasis_styles() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let paragraph = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Paragraph(spans) if spans.iter().any(|s| s.text.contains("bold")) => {
                    Some(spans)
                }
                _ => None,
            })
            .expect("intro paragraph present");

        assert!(
            paragraph
                .iter()
                .any(|s| s.style == SpanStyle::Bold && s.text == "bold")
        );
        assert!(
            paragraph
                .iter()
                .any(|s| s.style == SpanStyle::Italic && s.text == "italic")
        );
        assert!(paragraph.iter().any(
            |s| matches!(&s.style, SpanStyle::Link(href) if href == "./Other_Article")
                && s.text == "link"
        ));
        // The reference marker's own link text (rendered by Parsoid as
        // "[1]") should survive as plain/link text, giving the reader a
        // visible marker without extra bookkeeping.
        let flattened: String = paragraph.iter().map(|s| s.text.as_str()).collect();
        assert!(flattened.contains('.'));
    }

    /// The space between a text node and an adjacent inline element is real
    /// content: "See <a>X</a> and" must flatten to "See X and", never
    /// "SeeXand". Per-text-node end-trimming used to eat these boundary
    /// spaces, which jammed words together and defeated space-based line
    /// breaking in the layout engine.
    #[test]
    fn preserves_boundary_spaces_around_inline_elements() {
        let html = r##"<html><body><p>See <a href="./A">Alpha Beta</a> and <b>bold</b> text.</p></body></html>"##;
        let doc = parse_article_html("T", html);
        let flattened: String = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Paragraph(spans) => {
                    Some(spans.iter().map(|s| s.text.as_str()).collect::<String>())
                }
                _ => None,
            })
            .expect("paragraph present");
        assert_eq!(flattened, "See Alpha Beta and bold text.");
    }

    /// Paragraph-level leading/trailing whitespace (indentation in the HTML
    /// source) must still be trimmed even though boundary spaces are kept.
    #[test]
    fn paragraphs_do_not_start_or_end_with_stray_spaces() {
        let html = "<html><body><p>\n      Indented source.\n    </p></body></html>";
        let doc = parse_article_html("T", html);
        let Block::Paragraph(spans) = &doc.blocks[0] else {
            panic!("paragraph expected")
        };
        assert_eq!(spans[0].text, "Indented source.");
    }

    #[test]
    fn infobox_collapses_to_label_value_pairs() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let rows = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Infobox(rows) => Some(rows),
                _ => None,
            })
            .expect("infobox present");

        assert_eq!(
            rows[0],
            (String::new(), "Test Subject".to_string()),
            "single-cell row is a full-width title"
        );
        assert!(rows.iter().any(|(l, v)| l == "Born" && v == "1912"));
        assert!(
            rows.iter()
                .any(|(l, v)| l == "Field" && v == "Computer science")
        );
    }

    #[test]
    fn plain_render_contains_all_sections() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let plain = render_plain(&doc, "en");
        assert!(plain.contains("Test Article"));
        assert!(plain.contains("History"));
        assert!(plain.contains("First item"));
        assert!(plain.contains("[infobox]"));
        assert!(plain.contains("[image: A test picture]"));
    }

    /// PRD FR-ACS-1: `--dump`/the screen-reader path has no clickable escape
    /// and no interactive focus/Enter to follow a link, so the plain-text
    /// render must print the link's resolved target right after its text —
    /// the canonical article URL for an internal link.
    #[test]
    fn render_plain_prints_link_target_urls() {
        let html =
            r#"<html><body><p>See <a href="./Other_Article">this</a> for more.</p></body></html>"#;
        let doc = parse_article_html("T", html);
        let plain = render_plain(&doc, "en");
        assert!(
            plain.contains("this[link: https://en.wikipedia.org/wiki/Other_Article]"),
            "got: {plain:?}"
        );
    }

    #[test]
    fn render_plain_prints_external_link_targets_as_is() {
        let html =
            r#"<html><body><p>See <a href="https://example.com/x">this</a>.</p></body></html>"#;
        let doc = parse_article_html("T", html);
        let plain = render_plain(&doc, "en");
        assert!(
            plain.contains("this[link: https://example.com/x]"),
            "got: {plain:?}"
        );
    }

    #[test]
    fn resolve_link_url_builds_the_canonical_article_url_for_internal_links() {
        assert_eq!(
            resolve_link_url("./Alan_Turing", "en"),
            "https://en.wikipedia.org/wiki/Alan_Turing"
        );
        assert_eq!(
            resolve_link_url("/wiki/Bletchley_Park", "de"),
            "https://de.wikipedia.org/wiki/Bletchley_Park"
        );
        // A non-internal href passes through unchanged.
        assert_eq!(
            resolve_link_url("https://example.com/x", "en"),
            "https://example.com/x"
        );
        assert_eq!(
            resolve_link_url("mailto:x@example.com", "en"),
            "mailto:x@example.com"
        );
    }

    /// Extracts the single `Block::Table` from a one-table fixture.
    fn only_table(html: &str) -> Table {
        let doc = parse_article_html("T", html);
        doc.blocks
            .into_iter()
            .find_map(|b| match b {
                Block::Table(t) => Some(t),
                _ => None,
            })
            .expect("a table block")
    }

    /// PRD FR-RD-4: a `colspan=2` header over a 2×2 body must expand so the
    /// header row is `["Pair", <blank>]` (value first, blank in the spanned
    /// column) and stays rectangular with the body rows.
    #[test]
    fn colspan_header_expands_value_then_blank() {
        let table = only_table(
            r##"<html><body><table class="wikitable"><tbody>
            <tr><th colspan="2">Pair</th></tr>
            <tr><td>a</td><td>b</td></tr>
            <tr><td>c</td><td>d</td></tr>
            </tbody></table></body></html>"##,
        );
        assert_eq!(table.cols(), 2, "grid is rectangular at 2 columns");
        assert_eq!(table.rows[0][0].text, "Pair");
        assert!(table.rows[0][0].header);
        assert_eq!(
            table.rows[0][1].text, "",
            "the spanned-over column is a blank cell (value-first, blanks-after)"
        );
        assert_eq!(table.rows[1][0].text, "a");
        assert_eq!(table.rows[1][1].text, "b");
        assert_eq!(table.rows[2][1].text, "d");
    }

    /// PRD FR-RD-4: a `rowspan=2` left cell must fill *downward* — the origin
    /// cell holds the value on the first row, and the second row gets a blank
    /// cell in that column so the grid stays rectangular and column-aligned.
    #[test]
    fn rowspan_left_cell_fills_downward_with_a_blank() {
        let table = only_table(
            r##"<html><body><table class="wikitable"><tbody>
            <tr><td rowspan="2">L</td><td>r1</td></tr>
            <tr><td>r2</td></tr>
            </tbody></table></body></html>"##,
        );
        assert_eq!(table.cols(), 2);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0][0].text, "L");
        assert_eq!(table.rows[0][1].text, "r1");
        assert_eq!(
            table.rows[1][0].text, "",
            "the rowspan-covered position on the second row is a blank cell"
        );
        assert_eq!(
            table.rows[1][1].text, "r2",
            "the second row's real cell lands in the correct column, not shifted left"
        );
    }

    /// A pathological nested case: a table inside a cell must flatten to that
    /// cell's text (FR-RD-4 "tables-in-cells flatten"), never spawn a second
    /// `Block::Table`, and rowspan+colspan on the same cell must expand
    /// correctly (a 2×2 span from one origin cell).
    #[test]
    fn nested_table_flattens_and_combined_span_expands() {
        let table = only_table(
            r##"<html><body><table class="wikitable"><tbody>
            <tr>
              <td rowspan="2" colspan="2">Big<table class="wikitable"><tr><td>inner</td></tr></table>Cell</td>
              <td>x</td>
            </tr>
            <tr><td>y</td></tr>
            </tbody></table></body></html>"##,
        );
        // 3 columns wide (2 spanned + 1), 2 rows tall.
        assert_eq!(table.cols(), 3);
        assert_eq!(table.rows.len(), 2);
        // The origin holds the flattened text (inner table folded in), the
        // three positions it does not cover are blank.
        assert!(table.rows[0][0].text.contains("Big"));
        assert!(
            table.rows[0][0].text.contains("inner"),
            "the nested table's text flattens into the cell, got {:?}",
            table.rows[0][0].text
        );
        assert!(table.rows[0][0].text.contains("Cell"));
        assert_eq!(table.rows[0][1].text, "", "colspan fill");
        assert_eq!(table.rows[0][2].text, "x");
        assert_eq!(table.rows[1][0].text, "", "rowspan fill");
        assert_eq!(table.rows[1][1].text, "", "rowspan+colspan fill");
        assert_eq!(
            table.rows[1][2].text, "y",
            "the second row's own cell lands past the 2×2 span"
        );
    }

    /// FR-RD-4: an image inside a cell flattens to its alt text, not nothing.
    #[test]
    fn image_in_cell_flattens_to_alt_text() {
        let table = only_table(
            r##"<html><body><table class="wikitable"><tbody>
            <tr><td><img src="flag.png" alt="Flag of Nowhere"/></td><td>caption</td></tr>
            </tbody></table></body></html>"##,
        );
        assert_eq!(table.rows[0][0].text, "Flag of Nowhere");
        assert_eq!(table.rows[0][1].text, "caption");
    }

    /// PRD SEC-3: a hostile `colspan` far beyond the column cap must not
    /// allocate a giant row — the grid caps at `MAX_TABLE_COLS` and flags
    /// truncation, promptly.
    #[test]
    fn pathological_colspan_is_capped_and_flagged() {
        let html = r##"<html><body><table class="wikitable"><tbody>
            <tr><td colspan="100000000">wide</td></tr>
            </tbody></table></body></html>"##;
        let start = std::time::Instant::now();
        let table = only_table(html);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "a hostile colspan must not stall parsing"
        );
        assert!(table.cols() <= MAX_TABLE_COLS);
        assert!(table.truncated, "capping the grid must set truncated");
    }

    /// A CJK cell keeps its full text intact through parsing (width handling
    /// is the layout engine's job — this locks that the model doesn't mangle
    /// multi-byte content).
    #[test]
    fn cjk_cell_text_is_preserved() {
        let table = only_table(
            r##"<html><body><table class="wikitable"><tbody>
            <tr><th>用語</th><th>意味</th></tr>
            <tr><td>計算機科学</td><td>コンピュータの研究</td></tr>
            </tbody></table></body></html>"##,
        );
        assert_eq!(table.rows[1][0].text, "計算機科学");
        assert_eq!(table.rows[1][1].text, "コンピュータの研究");
    }

    /// PRD FR-ACS-1: `--dump` collapses a table to "Header: value" lines,
    /// carrying its content but no ANSI/box-drawing grid.
    #[test]
    fn dump_collapses_a_table_to_a_labeled_list() {
        let doc = parse_article_html(
            "T",
            r##"<html><body><table class="wikitable"><tbody>
            <tr><th>Year</th><th>Event</th></tr>
            <tr><td>1950</td><td>Turing test proposed</td></tr>
            </tbody></table></body></html>"##,
        );
        let plain = render_plain(&doc, "en");
        assert!(plain.contains("Year: 1950"), "got: {plain:?}");
        assert!(plain.contains("Event: Turing test proposed"));
        assert!(
            !plain.contains('\u{1b}') && !plain.contains('│') && !plain.contains('┌'),
            "the dump must be plain text — no ANSI, no box-drawing grid"
        );
    }

    #[test]
    fn resolves_internal_wiki_links() {
        assert_eq!(
            internal_title_from_href("./Alan_Turing"),
            Some("Alan Turing".to_string())
        );
        assert_eq!(
            internal_title_from_href("/wiki/Alan_Turing"),
            Some("Alan Turing".to_string())
        );
        assert_eq!(
            internal_title_from_href("./Bletchley_Park#History"),
            Some("Bletchley Park".to_string())
        );
        assert_eq!(internal_title_from_href("https://example.com/"), None);
        assert_eq!(internal_title_from_href("#cite_note-1"), None);
        assert_eq!(internal_title_from_href(""), None);
    }

    #[test]
    fn collect_links_finds_body_link_but_skips_heading_and_reference_anchor() {
        let doc = parse_article_html("Test Article", FIXTURE);
        let links = collect_links(&doc);

        let body_link = links
            .iter()
            .find(|l| l.internal_title.as_deref() == Some("Other Article"))
            .expect("the intro paragraph's internal link should be collected");
        assert_eq!(body_link.text, "link");

        // The reference marker's anchor (#cite_note-1) is a same-page
        // fragment, not an internal article link, and must not resolve.
        assert!(
            links
                .iter()
                .all(|l| l.href != "#cite_note-1" || l.internal_title.is_none())
        );
    }

    /// Approximates real MediaWiki Cite-extension output: a backlink caret,
    /// a `.reference-text` span holding the actual citation (with an
    /// external URL), and — separately — a reference with no URL at all.
    const CITATIONS_FIXTURE: &str = r##"
    <html><body>
    <p>A claim needing support<sup id="cite_ref-1" class="reference"><a href="#cite_note-1">[1]</a></sup>
    and another<sup id="cite_ref-2" class="reference"><a href="#cite_note-2">[2]</a></sup>.</p>
    <div class="mw-references-wrap">
      <ol class="references">
        <li id="cite_note-1">
          <span class="mw-cite-backlink"><a href="#cite_ref-1">^</a></span>
          <span class="reference-text">Smith, John. <cite>A Book About Things</cite>. Example Press, 2020.
            <a href="https://example.com/book">https://example.com/book</a></span>
        </li>
        <li id="cite_note-2">
          <span class="mw-cite-backlink"><a href="#cite_ref-2">^</a></span>
          <span class="reference-text">Doe, Jane. "An Article With No Link." Journal of Examples, 2019.</span>
        </li>
      </ol>
    </div>
    <table class="navbox"><tbody><tr><td>Not a reference list, must be ignored</td></tr></tbody></table>
    </body></html>
    "##;

    #[test]
    fn extracts_citations_with_and_without_urls() {
        let doc = parse_article_html("Test Article", CITATIONS_FIXTURE);
        assert_eq!(doc.citations.len(), 2, "exactly two <li> in ol.references");

        let with_url = &doc.citations[0];
        assert_eq!(with_url.id, "cite_note-1");
        assert!(with_url.text.contains("Smith, John"));
        assert!(with_url.text.contains("A Book About Things"));
        assert_eq!(with_url.url.as_deref(), Some("https://example.com/book"));

        let without_url = &doc.citations[1];
        assert_eq!(without_url.id, "cite_note-2");
        assert!(without_url.text.contains("Doe, Jane"));
        assert_eq!(
            without_url.url, None,
            "a reference with no link must not fabricate one"
        );
    }

    #[test]
    fn citation_text_strips_the_backlink_caret() {
        let doc = parse_article_html("Test Article", CITATIONS_FIXTURE);
        let text = &doc.citations[0].text;
        assert!(
            !text.starts_with('^'),
            "backlink caret must be stripped, got {text:?}"
        );
        assert!(
            text.starts_with("Smith"),
            "should start at the real citation content, got {text:?}"
        );
    }

    #[test]
    fn non_reference_tables_never_become_citations() {
        let doc = parse_article_html("Test Article", CITATIONS_FIXTURE);
        assert!(
            doc.citations
                .iter()
                .all(|c| !c.text.contains("Not a reference list")),
            "only ol.references entries should be extracted, not arbitrary tables"
        );
    }

    #[test]
    fn article_with_no_references_section_has_no_citations() {
        let doc = parse_article_html("Test Article", FIXTURE);
        assert!(doc.citations.is_empty());
    }

    #[test]
    fn prefers_the_page_display_title_over_an_underscored_caller_title() {
        // FIXTURE's <title> is "Test Article" (spaces); passing the
        // underscored form a link href or CLI arg might use should not
        // leak into the Document — a saved citation would otherwise read
        // "Test_Article" instead of "Test Article".
        let doc = parse_article_html("Test_Article", FIXTURE);
        assert_eq!(doc.title, "Test Article");
    }

    #[test]
    fn falls_back_to_the_caller_title_when_html_has_no_title_element() {
        let html = "<html><body><p>No head/title here.</p></body></html>";
        let doc = parse_article_html("Fallback Title", html);
        assert_eq!(doc.title, "Fallback Title");
    }

    // ---- PRD SEC-1/SEC-3: sanitizer, size cap, and DOM-depth guard ----

    #[test]
    fn hostile_span_text_and_link_href_are_sanitized() {
        let html = "<html><body><p>before\x1b[31mhostile<a href=\"./Evil\x1bTitle\">link</a>\
                    after\x07</p></body></html>";
        let doc = parse_article_html("Test", html);
        let paragraph = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Paragraph(spans) => Some(spans),
                _ => None,
            })
            .expect("paragraph present");
        for span in paragraph {
            assert!(!span.text.contains('\x1b'), "{:?}", span.text);
            assert!(!span.text.contains('\x07'), "{:?}", span.text);
            if let SpanStyle::Link(href) = &span.style {
                assert!(!href.contains('\x1b'), "{href:?}");
            }
        }
    }

    #[test]
    fn hostile_title_code_alt_and_citation_text_are_sanitized() {
        let html = concat!(
            "<html><head><title>Evil\u{202E}Title\u{202C}</title></head><body>",
            "<pre>code\u{9B}31mline</pre>",
            "<figure><img src=\"x.jpg\" alt=\"alt\u{200B}text\u{1B}hostile\"/></figure>",
            "<div class=\"mw-references-wrap\"><ol class=\"references\"><li id=\"cite_note-1\">",
            "<span class=\"reference-text\">Ref\u{1B}[31mtext</span></li></ol></div>",
            "</body></html>"
        );
        let doc = parse_article_html("Test", html);
        assert!(!doc.title.contains('\u{202E}'));
        assert!(!doc.title.contains('\u{202C}'));

        let code = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Code(text) => Some(text),
                _ => None,
            })
            .expect("code block present");
        assert!(!code.contains('\u{9B}'));

        let alt = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Image { alt, .. } => Some(alt),
                _ => None,
            })
            .expect("image present");
        assert!(!alt.contains('\u{200B}'));
        assert!(!alt.contains('\u{1B}'));

        assert_eq!(doc.citations.len(), 1);
        assert!(!doc.citations[0].text.contains('\u{1B}'));
    }

    #[test]
    fn citation_harvest_is_capped_at_max_citations() {
        let mut html =
            String::from("<html><body><div class=\"mw-references-wrap\"><ol class=\"references\">");
        for i in 0..(MAX_CITATIONS + 50) {
            html.push_str(&format!(
                "<li id=\"cite_note-{i}\"><span class=\"reference-text\">Source {i}</span></li>"
            ));
        }
        html.push_str("</ol></div></body></html>");
        let doc = parse_article_html("Test", &html);
        assert_eq!(doc.citations.len(), MAX_CITATIONS);
    }

    #[test]
    fn oversized_html_is_truncated_and_flagged_with_a_banner() {
        let mut html = String::from("<html><body><p>");
        html.push_str(&"A".repeat(MAX_ARTICLE_HTML_BYTES + 1_000_000));
        html.push_str("</p></body></html>");
        let start = std::time::Instant::now();
        let doc = parse_article_html("Test", &html);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "parsing an oversized article must stay within a generous time bound, took {:?}",
            start.elapsed()
        );
        assert!(
            doc.truncated,
            "an 11 MB article must set the truncated flag"
        );
        let has_banner = doc.blocks.first().is_some_and(|b| matches!(
            b,
            Block::Paragraph(spans) if spans.iter().any(|s| s.text.contains("Degraded rendering"))
        ));
        assert!(
            has_banner,
            "the first block must be the degraded-rendering banner"
        );
    }

    #[test]
    fn html_at_or_under_the_cap_is_not_truncated() {
        let doc = parse_article_html("Test", FIXTURE);
        assert!(!doc.truncated);
    }

    /// 10k-deep nested `<div>`s trigger `cap_html_nesting_depth`'s pre-parse
    /// cut (SEC-3): html5ever's tree builder is empirically superlinear in
    /// nesting depth (measured directly — see that function's doc comment),
    /// so without this cut a document like this one takes single-digit
    /// *seconds* to parse despite being only a few hundred KB. The content
    /// past the cut point (including the paragraph, which sits at depth
    /// 10,000) is never handed to the parser at all — that's the point —
    /// so this locks in "completes fast and is flagged truncated", not "the
    /// deep content survives".
    #[test]
    fn deeply_nested_divs_are_cut_before_the_parser_sees_them() {
        let depth = 10_000;
        let mut html = String::from("<html><body>");
        html.push_str(&"<div>".repeat(depth));
        html.push_str("<p>deeply nested text</p>");
        html.push_str(&"</div>".repeat(depth));
        html.push_str("</body></html>");

        let start = std::time::Instant::now();
        let doc = parse_article_html("Test", &html);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "10k-deep nested divs must parse within a generous time bound \
             (pre-parse nesting-depth cap), took {:?}",
            start.elapsed()
        );
        assert!(
            doc.truncated,
            "nesting depth past MAX_DOM_DEPTH must set the truncated flag"
        );
    }

    /// A DOM that nests well within `MAX_DOM_DEPTH` (a handful of levels, as
    /// any real article does) must parse untouched — `cap_html_nesting_depth`
    /// must not false-positive on ordinary structure, including the void
    /// elements (`<br>`, `<img>`, `<hr>`) and self-closing tags a real
    /// article is full of at the *same* depth, not nested inside each other.
    #[test]
    fn shallow_nesting_with_void_elements_is_left_untouched() {
        let html = "<html><body><div><p>Text with a break<br>and an image \
                     <img src=\"x.jpg\" alt=\"a > b\"/> and a rule<hr>done.</p></div></body></html>";
        let (kept, truncated) = cap_html_nesting_depth(html, MAX_DOM_DEPTH);
        assert!(!truncated);
        assert_eq!(kept, html);

        let doc = parse_article_html("Test", html);
        assert!(!doc.truncated);
    }

    /// `<script>`/`<style>` bodies are opaque raw text, not tag nesting —
    /// their content (which can itself contain `<`/`>`) must not be
    /// misread as deeply nested markup.
    #[test]
    fn script_and_style_bodies_do_not_count_toward_nesting_depth() {
        let html = "<html><body><script>if (1 < 2) { console.log('<div><div>'); }</script>\
                     <style>.a { content: '<<<'; }</style><p>ok</p></body></html>";
        let (kept, truncated) = cap_html_nesting_depth(html, 10);
        assert!(!truncated, "raw-text content must not inflate depth");
        assert_eq!(kept, html);
    }

    #[test]
    fn nesting_depth_cap_truncates_exactly_at_the_offending_tag() {
        let html = "<div><div><div>too deep</div></div></div>";
        let (kept, truncated) = cap_html_nesting_depth(html, 2);
        assert!(truncated);
        assert_eq!(kept, "<div><div>");
    }

    /// PRD §9's fuzz corpus: a battery of hostile inputs across every text
    /// surface `parse_article_html` emits, each asserting the resulting
    /// `Document` (and, for the ones that matter to rendering, its laid-out
    /// lines) never carries a raw control/DEL/C1/bidi-override character —
    /// while zero-width-joiner emoji and IPA combining marks, which must
    /// NOT be stripped, survive intact.
    #[test]
    fn fuzz_corpus_never_leaks_control_bidi_or_c1_bytes_and_terminates_promptly() {
        let family_emoji = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let ipa = "\u{0283}\u{0361}\u{0288} n\u{0303}";

        let corpus: Vec<(&str, String)> = vec![
            (
                "esc_csi_in_paragraph",
                "<html><body><p>before\x1b[31;1mhostile\x1b[0m after</p></body></html>".to_string(),
            ),
            (
                "osc_title_set_in_heading",
                "<html><body><h2>Title\x1b]0;pwned\x07End</h2></body></html>".to_string(),
            ),
            (
                "c1_bytes_in_code_block",
                "<html><body><pre>code\u{9B}31mline\u{9D}0;pwned\u{9C}</pre></body></html>"
                    .to_string(),
            ),
            (
                "bidi_override_in_alt_text",
                "<html><body><figure><img src=\"x.jpg\" alt=\"safe\u{202E}evil\u{202C}text\"/></figure></body></html>"
                    .to_string(),
            ),
            (
                "bidi_isolate_in_paragraph",
                "<html><body><p>a\u{2066}bidi\u{2069}b</p></body></html>".to_string(),
            ),
            (
                "hostile_citation_text",
                concat!(
                    "<html><body><div class=\"mw-references-wrap\"><ol class=\"references\">",
                    "<li id=\"cite_note-1\"><span class=\"reference-text\">Ref\x1b[31m\u{202E}text</span></li>",
                    "</ol></div></body></html>"
                )
                .to_string(),
            ),
            (
                "nul_bytes_in_paragraph",
                "<html><body><p>before\0after</p></body></html>".to_string(),
            ),
            (
                "zwj_emoji_family_must_survive",
                format!("<html><body><p>Family: {family_emoji} together</p></body></html>"),
            ),
            (
                "ipa_combining_marks_must_survive",
                format!("<html><body><p>IPA: {ipa}</p></body></html>"),
            ),
        ];

        let options = crate::layout::LayoutOptions::default();
        for (name, html) in &corpus {
            let start = std::time::Instant::now();
            let doc = parse_article_html("Fuzz", html);
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "corpus entry {name} must parse within a generous time bound, took {:?}",
                start.elapsed()
            );
            assert_document_is_terminal_safe(&doc, name);

            let laid_out = crate::layout::layout_document(&doc, 80, options);
            for line in &laid_out.lines {
                for span in &line.spans {
                    assert_terminal_safe(&span.text, &format!("{name} (laid-out line)"));
                }
            }
        }

        // The two "must survive" entries: confirm the content wasn't merely
        // left non-hostile but was actually preserved, not deleted.
        let family_doc = parse_article_html(
            "Fuzz",
            &format!("<html><body><p>Family: {family_emoji} together</p></body></html>"),
        );
        assert!(
            render_plain(&family_doc, "en").contains(family_emoji),
            "ZWJ emoji family must survive sanitization intact"
        );

        let ipa_doc = parse_article_html(
            "Fuzz",
            &format!("<html><body><p>IPA: {ipa}</p></body></html>"),
        );
        assert!(
            render_plain(&ipa_doc, "en").contains(ipa),
            "IPA combining marks must survive sanitization intact"
        );
    }

    // ---- FR-RD-7: math passthrough ---------------------------------------

    /// The `alttext` attribute wins over an `<annotation>` child when both
    /// are present — PRD §6.2 rule 3's "math nodes carry TeX in alttext" is
    /// this parser's stated precedence (see `extract_math`'s doc comment).
    #[test]
    fn math_alttext_wins_over_annotation_when_both_present() {
        let html = r#"<html><body><p>Energy: <span typeof="mw:Extension/math">
            <math alttext="E=mc^2"><semantics><mrow></mrow>
            <annotation encoding="application/x-tex">E = m c^2 (from annotation)</annotation>
            </semantics></math></span>.</p></body></html>"#;
        let doc = parse_article_html("Test", html);
        let math_tex: Vec<&str> = doc
            .blocks
            .iter()
            .flat_map(|b| match b {
                Block::Paragraph(spans) => spans.as_slice(),
                _ => &[],
            })
            .filter_map(|s| match &s.style {
                SpanStyle::Math(tex) => Some(tex.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(math_tex, vec!["E=mc^2"]);
    }

    /// With no `alttext`, the `<annotation encoding="application/x-tex">`
    /// child is the fallback source.
    #[test]
    fn math_falls_back_to_annotation_when_alttext_is_absent() {
        let html = r#"<html><body><p><span typeof="mw:Extension/math">
            <math><semantics><mrow></mrow>
            <annotation encoding="application/x-tex">\alpha + \beta</annotation>
            </semantics></math></span></p></body></html>"#;
        let doc = parse_article_html("Test", html);
        let tex = first_math_tex(&doc);
        assert_eq!(tex.as_deref(), Some("\\alpha + \\beta"));
    }

    /// With no `<math>` element at all, the accessible fallback image's own
    /// `alt` (with its `\displaystyle`/`\textstyle` wrapper stripped) is the
    /// last resort.
    #[test]
    fn math_falls_back_to_the_fallback_image_alt_when_no_math_element_exists() {
        let html = r#"<html><body><p><span typeof="mw:Extension/math">
            <img class="mwe-math-fallback-image-inline" alt="{\displaystyle x^2+y^2=z^2}"/>
            </span></p></body></html>"#;
        let doc = parse_article_html("Test", html);
        let tex = first_math_tex(&doc);
        assert_eq!(tex.as_deref(), Some("x^2+y^2=z^2"));
    }

    /// A math node with no `alttext`, no usable annotation, and no fallback
    /// image at all yields nothing — the "missing-tex graceful" case: no
    /// span is produced, and parsing doesn't panic or invent a placeholder.
    #[test]
    fn math_node_with_no_extractable_tex_produces_no_span_and_does_not_panic() {
        let html = r#"<html><body><p>before <span typeof="mw:Extension/math">
            <span class="mwe-math-mathml-inline"></span>
            </span> after</p></body></html>"#;
        let doc = parse_article_html("Test", html);
        assert_eq!(
            first_math_tex(&doc),
            None,
            "no math span should exist when nothing extractable was found"
        );
        // The surrounding plain text must still have made it through.
        let flattened: String = doc
            .blocks
            .iter()
            .flat_map(|b| match b {
                Block::Paragraph(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
                _ => vec![],
            })
            .collect();
        assert!(flattened.contains("before"));
        assert!(flattened.contains("after"));
    }

    /// A display equation (Parsoid's `<dl><dd>`-wrapped shape for a
    /// leading-colon indented equation, `<math display="block">`) becomes a
    /// standalone `Block::Math { display: true, .. }`, distinct from inline
    /// math within running prose.
    #[test]
    fn display_math_in_dl_dd_becomes_a_standalone_display_block() {
        let html = r#"<html><body><p>Intro text.</p>
            <dl><dd><span typeof="mw:Extension/math">
                <math display="block" alttext="F = m a"><semantics><mrow></mrow>
                <annotation encoding="application/x-tex">F = m a</annotation></semantics></math>
            </span></dd></dl>
            <p>Trailing text.</p></body></html>"#;
        let doc = parse_article_html("Test", html);
        let math_blocks: Vec<(&str, bool)> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Math { tex, display } => Some((tex.as_str(), *display)),
                _ => None,
            })
            .collect();
        assert_eq!(math_blocks, vec![("F = m a", true)]);
    }

    /// Helper: the TeX of the first `SpanStyle::Math` span found anywhere in
    /// the document's paragraph blocks.
    fn first_math_tex(doc: &Document) -> Option<String> {
        doc.blocks.iter().find_map(|b| match b {
            Block::Paragraph(spans) => spans.iter().find_map(|s| match &s.style {
                SpanStyle::Math(tex) => Some(tex.clone()),
                _ => None,
            }),
            _ => None,
        })
    }

    /// FR-RD-7's trivial-case table: superscript/subscript digits and the
    /// documented punctuation, both bare and brace-grouped.
    #[test]
    fn normalize_trivial_math_converts_digit_sup_and_sub() {
        assert_eq!(normalize_trivial_math("E=mc^2"), "E=mc²");
        assert_eq!(normalize_trivial_math("x_n"), "xₙ");
        assert_eq!(normalize_trivial_math("a^{23}"), "a²³");
        assert_eq!(normalize_trivial_math("a^{-1}"), "a⁻¹");
        assert_eq!(normalize_trivial_math("H_2O"), "H₂O");
    }

    /// The 24 lowercase Greek letters plus the uppercase ones LaTeX actually
    /// defines a macro for; a longer word starting with a valid macro name
    /// (`\alphabet`) must not partially match `\alpha`.
    #[test]
    fn normalize_trivial_math_converts_greek_letters_and_never_partial_matches() {
        assert_eq!(normalize_trivial_math("\\alpha + \\beta"), "α + β");
        assert_eq!(normalize_trivial_math("\\Gamma\\Omega"), "ΓΩ");
        assert_eq!(
            normalize_trivial_math("\\alphabet"),
            "\\alphabet",
            "a longer word must not partially match a shorter macro name"
        );
    }

    /// Anything the trivial sup/sub table doesn't cover — a multi-character
    /// exponent mixing a letter with no superscript glyph — leaves the `^`/
    /// `_` and its braces exactly as raw TeX passthrough (FR-RD-7 v1.0
    /// scope). Greek-letter substitution is a separate, independent rule
    /// (any `\alpha`-style macro anywhere in the string, brace-nested or
    /// not, is unambiguous on its own), so it still applies *inside* an
    /// otherwise-non-trivial exponent — `x^{i\pi}`'s braces stay literal
    /// (no superscript glyph for `i`+`π` together) but `\pi` itself still
    /// becomes `π`.
    #[test]
    fn normalize_trivial_math_leaves_non_trivial_cases_raw() {
        assert_eq!(normalize_trivial_math("x^{i\\pi}"), "x^{iπ}");
        assert_eq!(normalize_trivial_math("\\frac{1}{2}"), "\\frac{1}{2}");
        assert_eq!(normalize_trivial_math("y^{2x}"), "y^{2x}");
        assert_eq!(
            normalize_trivial_math("plain text, no math at all"),
            "plain text, no math at all"
        );
    }

    // ---- FR-RD-11: reading time -------------------------------------------

    fn doc_with_paragraph(text: &str) -> Document {
        parse_article_html("Test", &format!("<html><body><p>{text}</p></body></html>"))
    }

    #[test]
    fn word_count_counts_whitespace_split_latin_words() {
        let doc = doc_with_paragraph("The quick brown fox jumps over the lazy dog");
        assert_eq!(word_count(&doc), 9);
    }

    #[test]
    fn word_count_is_zero_for_an_empty_document() {
        let doc = parse_article_html("Test", "<html><body></body></html>");
        assert_eq!(word_count(&doc), 0);
        assert_eq!(reading_minutes(0, 230), 0);
    }

    /// CJK text has no space separators; each CJK character is weighted as
    /// `1 / CJK_CHARS_PER_WORD` of a word rather than the whole run counting
    /// as a single whitespace-split "word".
    #[test]
    fn word_count_weights_cjk_characters_by_the_documented_heuristic() {
        let ten_chars = "計算機科学は情報の学問"; // 11 CJK characters
        let doc = doc_with_paragraph(ten_chars);
        let chars = ten_chars.chars().count() as f64;
        let expected = (chars / CJK_CHARS_PER_WORD).round() as u32;
        assert_eq!(word_count(&doc), expected);
        assert!(
            word_count(&doc) > 0,
            "a dense CJK paragraph must not read as 0 words"
        );
    }

    /// Table/infobox/code content is excluded from the word count — a
    /// table-heavy stub with almost no prose shouldn't inflate "N min read"
    /// with cell text a reader skims rather than reads linearly.
    #[test]
    fn word_count_skips_table_infobox_and_code_blocks() {
        let html = r#"<html><body>
            <p>one two three</p>
            <table class="infobox"><tbody><tr><th>Label</th><td>four five six seven</td></tr></tbody></table>
            <table class="wikitable"><tbody><tr><td>eight nine ten eleven</td></tr></tbody></table>
            <pre>twelve thirteen fourteen</pre>
        </body></html>"#;
        let doc = parse_article_html("Test", html);
        assert!(
            doc.blocks
                .iter()
                .any(|b| matches!(b, Block::Infobox(_) | Block::Table(_) | Block::Code(_))),
            "fixture must actually produce the excluded block kinds"
        );
        assert_eq!(
            word_count(&doc),
            3,
            "only the paragraph's 3 words should count"
        );
    }

    /// FR-RD-11's WPM division, rounded up so a short article never reads
    /// "0 min" (only a truly empty document does).
    #[test]
    fn reading_minutes_rounds_up_and_respects_wpm() {
        assert_eq!(reading_minutes(230, 230), 1);
        assert_eq!(reading_minutes(231, 230), 2, "one word over rounds up");
        assert_eq!(reading_minutes(460, 230), 2);
        assert_eq!(reading_minutes(100, 100), 1);
        assert_eq!(
            reading_minutes(50, 100),
            1,
            "any nonzero count rounds up to at least 1"
        );
        assert_eq!(reading_minutes(1000, 500), 2);
    }

    // ---- FR-DL-5: redlink detection (parse-time signal) -------------------

    /// Parsoid's own `class="new"` pre-marking is the cheapest redlink
    /// signal, recognized at parse time with no network involved.
    #[test]
    fn class_new_link_parses_as_a_redlink_and_ordinary_link_does_not() {
        let html = concat!(
            "<html><body><p>See <a href=\"./Nonexistent_Concept_X\" class=\"new\">",
            "the concept</a> and <a href=\"./Computer_science\">computer science</a>.</p>",
            "</body></html>"
        );
        let doc = parse_article_html("Test", html);
        let links = collect_links(&doc);
        assert_eq!(links.len(), 2);
        assert!(links[0].redlink, "class=\"new\" link must be a redlink");
        assert_eq!(
            links[0].internal_title.as_deref(),
            Some("Nonexistent Concept X")
        );
        assert!(
            !links[1].redlink,
            "an ordinary link must not be marked a redlink"
        );
    }

    /// PRD §9's assertion, checked against every string a `Document`
    /// exposes: no C0 control other than `\n`/`\t`, no DEL, no C1, no bidi
    /// override/isolate character.
    fn assert_terminal_safe(s: &str, context: &str) {
        for c in s.chars() {
            let cp = c as u32;
            assert!(
                !((cp < 0x20 && c != '\n' && c != '\t') || c == '\u{7F}'),
                "found a raw control/DEL byte {c:?} (U+{cp:04X}) in {context}: {s:?}"
            );
            assert!(
                !(0x80..=0x9F).contains(&cp),
                "found a raw C1 control byte {c:?} (U+{cp:04X}) in {context}: {s:?}"
            );
            assert!(
                !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'),
                "found a bidi override/isolate character {c:?} (U+{cp:04X}) in {context}: {s:?}"
            );
        }
    }

    fn assert_document_is_terminal_safe(doc: &Document, context: &str) {
        assert_terminal_safe(&doc.title, &format!("{context} (title)"));
        for block in &doc.blocks {
            match block {
                Block::Heading { spans, .. }
                | Block::Paragraph(spans)
                | Block::ListItem { spans, .. }
                | Block::Blockquote(spans) => {
                    for span in spans {
                        assert_terminal_safe(&span.text, &format!("{context} (span)"));
                        match &span.style {
                            SpanStyle::Link(href) | SpanStyle::RedLink(href) => {
                                assert_terminal_safe(href, &format!("{context} (href)"));
                            }
                            SpanStyle::Math(tex) => {
                                assert_terminal_safe(tex, &format!("{context} (math)"));
                            }
                            SpanStyle::Plain
                            | SpanStyle::Bold
                            | SpanStyle::Italic
                            | SpanStyle::Superscript => {}
                        }
                    }
                }
                Block::Code(text) => assert_terminal_safe(text, &format!("{context} (code)")),
                Block::Table(table) => {
                    for cell in table.rows.iter().flatten() {
                        assert_terminal_safe(&cell.text, &format!("{context} (table cell)"));
                    }
                }
                Block::Infobox(rows) => {
                    for (label, value) in rows {
                        assert_terminal_safe(label, &format!("{context} (infobox label)"));
                        assert_terminal_safe(value, &format!("{context} (infobox value)"));
                    }
                }
                Block::Image { alt, caption, .. } => {
                    assert_terminal_safe(alt, &format!("{context} (alt)"));
                    if let Some(caption) = caption {
                        assert_terminal_safe(caption, &format!("{context} (caption)"));
                    }
                }
                Block::Gallery(items) => {
                    for item in items {
                        assert_terminal_safe(&item.caption, &format!("{context} (gallery)"));
                    }
                }
                Block::Rule => {}
                Block::Math { tex, .. } => assert_terminal_safe(tex, &format!("{context} (math)")),
            }
        }
        for citation in &doc.citations {
            assert_terminal_safe(&citation.id, &format!("{context} (citation id)"));
            assert_terminal_safe(&citation.text, &format!("{context} (citation text)"));
            if let Some(url) = &citation.url {
                assert_terminal_safe(url, &format!("{context} (citation url)"));
            }
        }
    }
}
