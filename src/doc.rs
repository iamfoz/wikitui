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
/// `find_img_alt`). A real article's DOM nests at most a few dozen levels
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

#[derive(Debug, Clone, PartialEq)]
pub enum SpanStyle {
    Plain,
    Bold,
    Italic,
    Superscript,
    Link(String),
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
    /// A collapsed table: pre-formatted "Label: value" (or cell-joined) lines.
    Table(Vec<String>),
    /// A collapsed infobox: ordered (label, value) pairs. An empty label
    /// marks a full-width row (e.g. a section title inside the infobox).
    Infobox(Vec<(String, String)>),
    Image(String),
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
            if let SpanStyle::Link(href) = &s.style {
                links.push(LinkRef {
                    href: href.clone(),
                    text: s.text.clone(),
                    internal_title: internal_title_from_href(href),
                });
            }
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
                // Once inside a link, keep treating the whole run as a link
                // (a bold word inside a link stays a link for our purposes).
                let child_style = if matches!(style, SpanStyle::Link(_)) {
                    style.clone()
                } else {
                    match tag {
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

fn collapse_table(node: NodeRef<Node>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    descendant_tags(node, "tr", &mut rows, 0);

    let mut headers: Vec<String> = Vec::new();
    for tr in rows {
        let mut header_cells = Vec::new();
        let mut data_cells = Vec::new();
        for cell in tr.children() {
            if let Node::Element(el) = cell.value() {
                let text = normalize_ws(&text_content(cell));
                if text.is_empty() {
                    continue;
                }
                match el.name() {
                    "th" => header_cells.push(text),
                    "td" => data_cells.push(text),
                    _ => {}
                }
            }
        }
        if !header_cells.is_empty() && data_cells.is_empty() {
            headers = header_cells;
            if !headers.is_empty() {
                lines.push(headers.join(" | "));
                lines
                    .push("-".repeat(lines.last().map(|l: &String| l.len().min(60)).unwrap_or(20)));
            }
        } else if !data_cells.is_empty() {
            if !headers.is_empty() && headers.len() == data_cells.len() {
                for (h, v) in headers.iter().zip(data_cells.iter()) {
                    lines.push(format!("{h}: {v}"));
                }
                lines.push(String::new());
            } else {
                lines.push(data_cells.join(" | "));
            }
        }
    }
    lines
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

/// PRD SEC-3: past `MAX_DOM_DEPTH` this gives up and reports no image
/// rather than recursing further — a missed caption on a pathologically
/// nested `<figure>` is an acceptable degradation; a stack overflow is not.
fn find_img_alt(node: NodeRef<Node>, depth: usize) -> Option<String> {
    if depth > MAX_DOM_DEPTH {
        return None;
    }
    for child in node.children() {
        if let Node::Element(el) = child.value() {
            if el.name() == "img" {
                let alt = el.attr("alt").unwrap_or("").trim();
                return Some(if alt.is_empty() {
                    "image".to_string()
                } else {
                    alt.to_string()
                });
            }
            if let Some(found) = find_img_alt(child, depth + 1) {
                return Some(found);
            }
        }
    }
    None
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
                    let lines = collapse_table(child);
                    if !lines.is_empty() {
                        blocks.push(Block::Table(lines));
                    }
                }
            }
            "figure" => {
                if let Some(alt) = find_img_alt(child, 0) {
                    blocks.push(Block::Image(alt));
                }
            }
            "img" => {
                let alt = el.attr("alt").unwrap_or("").trim();
                blocks.push(Block::Image(if alt.is_empty() {
                    "image".to_string()
                } else {
                    alt.to_string()
                }));
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
            Block::Table(lines) => {
                for line in lines {
                    *line = sanitize::sanitize_and_cap_single_line(line, sanitize::MAX_SPAN_CHARS);
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
            Block::Image(alt) => {
                *alt = sanitize::sanitize_and_cap_single_line(alt, sanitize::MAX_SPAN_CHARS);
            }
            Block::Rule => {}
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
        if let SpanStyle::Link(href) = &mut span.style {
            *href = sanitize::sanitize_and_cap_single_line(href, sanitize::MAX_SPAN_CHARS);
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

/// Render a document as plain text (FR-RD-12, the `--dump` linear mode and
/// the honest screen-reader path). No color, no cursor addressing.
pub fn render_plain(doc: &Document) -> String {
    let mut out = String::new();
    out.push_str(&doc.title);
    out.push('\n');
    out.push_str(&"=".repeat(doc.title.chars().count()));
    out.push_str("\n\n");

    let flatten = |spans: &[Span]| -> String {
        spans
            .iter()
            .map(|s| s.text.as_str())
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
            Block::Table(lines) => {
                for line in lines {
                    out.push_str(line);
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
            Block::Image(alt) => {
                out.push_str(&format!("[image: {alt}]\n\n"));
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
                Block::Table(lines) => Some(lines),
                _ => None,
            })
            .collect();
        // The navbox table must be dropped by the noise filter; only the
        // "wikitable" should survive.
        assert_eq!(data_tables.len(), 1, "navbox table should be filtered out");
        assert!(data_tables[0].iter().any(|l| l.contains("1950")));

        let has_image = doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Image(alt) if alt == "A test picture"));
        assert!(
            has_image,
            "expected the figure's image to produce a placeholder block"
        );
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
        let plain = render_plain(&doc);
        assert!(plain.contains("Test Article"));
        assert!(plain.contains("History"));
        assert!(plain.contains("First item"));
        assert!(plain.contains("[infobox]"));
        assert!(plain.contains("[image: A test picture]"));
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
                Block::Image(alt) => Some(alt),
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
            render_plain(&family_doc).contains(family_emoji),
            "ZWJ emoji family must survive sanitization intact"
        );

        let ipa_doc = parse_article_html(
            "Fuzz",
            &format!("<html><body><p>IPA: {ipa}</p></body></html>"),
        );
        assert!(
            render_plain(&ipa_doc).contains(ipa),
            "IPA combining marks must survive sanitization intact"
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
                        if let SpanStyle::Link(href) = &span.style {
                            assert_terminal_safe(href, &format!("{context} (href)"));
                        }
                    }
                }
                Block::Code(text) => assert_terminal_safe(text, &format!("{context} (code)")),
                Block::Table(lines) => {
                    for line in lines {
                        assert_terminal_safe(line, &format!("{context} (table)"));
                    }
                }
                Block::Infobox(rows) => {
                    for (label, value) in rows {
                        assert_terminal_safe(label, &format!("{context} (infobox label)"));
                        assert_terminal_safe(value, &format!("{context} (infobox value)"));
                    }
                }
                Block::Image(alt) => assert_terminal_safe(alt, &format!("{context} (alt)")),
                Block::Rule => {}
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
