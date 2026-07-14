//! `:save export md|txt|html [path]` (PRD FR-OFF-7): export a saved page (or
//! the current article) to Markdown, plain text, or a standalone print-styled
//! HTML document — every one ending in the §10 attribution footer, shared with
//! the bookmark export via `crate::attribution`.
//!
//! ## Non-free image policy (§10)
//!
//! §10 is explicit that non-free/fair-use images are "displayed transiently
//! only and never written into saved pages/exports." wikitui does not fetch
//! per-image `extmetadata`, so it cannot *prove* a given thumbnail is freely
//! licensed. The safe default for a redistributable export artifact is
//! therefore to **omit image data entirely** and render images as their alt
//! text (`[image: …]`). Only when the user opts in with `include_nonfree`
//! does the HTML export embed the pinned thumbnails (as `data:` URIs from the
//! saved store). Markdown and plain text never embed binary image data
//! regardless — they too fall back to alt text. This is the chosen policy the
//! brief asks be documented: free-only is the default; non-free exclusion is
//! enforced by not embedding images unless `include_nonfree`.

use std::path::PathBuf;

use crate::doc::{Block, Document, Span, SpanStyle};
use crate::saved::SavedThumb;

pub const FORMATS: [&str; 3] = ["md", "txt", "html"];

pub fn extension(format: &str) -> Option<&'static str> {
    match format {
        "md" => Some("md"),
        "txt" => Some("txt"),
        "html" => Some("html"),
        _ => None,
    }
}

/// A thumbnail available for embedding in an HTML export: the source URL (to
/// match a `Block::Image`) and its raw bytes (already fetched/pinned).
pub struct EmbeddableThumb {
    pub src: String,
    pub bytes: Vec<u8>,
}

/// Render `doc` in `format`, or `None` for an unrecognized one. `lang` powers
/// canonical link URLs; `include_nonfree` gates HTML image embedding (see the
/// module doc); `thumbs` are the pinned thumbnail bytes available to embed
/// (empty when exporting an unsaved current article, or for md/txt).
pub fn render(
    doc: &Document,
    lang: &str,
    format: &str,
    include_nonfree: bool,
    thumbs: &[EmbeddableThumb],
) -> Option<String> {
    match format {
        "txt" => Some(render_txt(doc)),
        "md" => Some(render_md(doc, lang)),
        "html" => Some(render_html(doc, lang, include_nonfree, thumbs)),
        _ => None,
    }
}

/// `$XDG_DATA_HOME/wikitui/exports/` — same directory the bookmark export uses.
pub fn default_export_dir() -> PathBuf {
    crate::bookmark_export::default_export_dir()
}

/// The default timestamped path for a saved-page export of `title` in
/// `format` — `<title>-<YYYYMMDD-HHMMSS>.<ext>` under [`default_export_dir`].
pub fn default_export_path(title: &str, format: &str) -> Option<PathBuf> {
    let ext = extension(format)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let stem = crate::cache::safe_name(title);
    Some(default_export_dir().join(format!("{stem}-{ts}.{ext}")))
}

fn footer() -> String {
    // Saved pages carry no reader annotations, so the annotation clause is
    // omitted (see `attribution::export_footer`).
    crate::attribution::export_footer(&crate::research::today(), false)
}

/// Resolve a Parsoid href to a canonical article URL where it is an internal
/// link, else pass it through — so a Markdown/HTML export's links point at
/// real pages, not `./Title` relative refs.
fn resolve_href(href: &str, lang: &str) -> String {
    if let Some(rest) = href
        .strip_prefix("./")
        .or_else(|| href.strip_prefix("/wiki/"))
    {
        let title = rest.split('#').next().unwrap_or(rest);
        crate::research::article_url(&title.replace('_', " "), lang)
    } else {
        href.to_string()
    }
}

// ---- Plain text ----------------------------------------------------------

fn render_txt(doc: &Document) -> String {
    let mut out = crate::doc::render_plain(doc);
    out.push_str("\n----\n\n");
    out.push_str(&footer());
    out.push('\n');
    out
}

// ---- Markdown ------------------------------------------------------------

fn spans_to_md(spans: &[Span], lang: &str) -> String {
    let mut out = String::new();
    for span in spans {
        let text = crate::cite::escape_markdown(&span.text);
        match &span.style {
            SpanStyle::Bold => out.push_str(&format!("**{text}**")),
            SpanStyle::Italic => out.push_str(&format!("*{text}*")),
            SpanStyle::Link(href) => {
                out.push_str(&format!("[{text}]({})", resolve_href(href, lang)));
            }
            // PRD FR-DL-5: still a link to a real title once it's created —
            // a saved/exported copy has no ongoing "does it exist" state to
            // preserve, so this renders exactly like an ordinary link.
            SpanStyle::RedLink(href) => {
                out.push_str(&format!("[{text}]({})", resolve_href(href, lang)));
            }
            // Superscript/plain render as their text (Markdown has no portable
            // superscript; references read fine inline).
            SpanStyle::Plain | SpanStyle::Superscript => out.push_str(&text),
            // PRD FR-RD-7: raw TeX passthrough in an inline code span — the
            // *un*-escaped source (Markdown's backslash-escaping would mangle
            // TeX's own backslash commands), backticks so it renders
            // literally rather than being reinterpreted as Markdown itself.
            SpanStyle::Math(tex) => out.push_str(&format!("`{tex}`")),
        }
    }
    out
}

fn render_md(doc: &Document, lang: &str) -> String {
    let mut out = format!("# {}\n\n", doc.title);
    for block in &doc.blocks {
        match block {
            Block::Heading { level, spans } => {
                let hashes = "#".repeat((*level as usize).clamp(1, 6));
                out.push_str(&format!("{hashes} {}\n\n", spans_to_md(spans, lang)));
            }
            Block::Paragraph(spans) => {
                out.push_str(&spans_to_md(spans, lang));
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
                out.push_str(&format!("{indent}{bullet} {}\n", spans_to_md(spans, lang)));
            }
            Block::Blockquote(spans) => {
                out.push_str(&format!("> {}\n\n", spans_to_md(spans, lang)));
            }
            Block::Code(text) => {
                out.push_str("```\n");
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            }
            Block::Rule => out.push_str("---\n\n"),
            Block::Table(table) => {
                for line in table.to_list_lines() {
                    out.push_str(&line);
                    out.push('\n');
                }
                out.push('\n');
            }
            Block::Infobox(rows) => {
                for (label, value) in rows {
                    if label.is_empty() {
                        out.push_str(&format!("**{value}**\n"));
                    } else {
                        out.push_str(&format!("- **{label}:** {value}\n"));
                    }
                }
                out.push('\n');
            }
            // §10 policy: alt text only, never embedded binary, in Markdown.
            Block::Image { alt, caption, .. } => {
                out.push_str(&format!("*[image: {alt}]*\n"));
                if let Some(caption) = caption {
                    out.push_str(&format!("{caption}\n"));
                }
                out.push('\n');
            }
            Block::Gallery(items) => {
                for item in items {
                    out.push_str(&format!("*[image: {}]*\n", item.caption));
                }
                out.push('\n');
            }
            // PRD FR-RD-7: raw TeX in a fenced code block — passthrough, no
            // escaping, same choice as `spans_to_md`'s inline case.
            Block::Math { tex, .. } => {
                out.push_str(&format!("```\n{tex}\n```\n\n"));
            }
        }
    }
    out.push_str("\n---\n\n*");
    out.push_str(&footer());
    out.push_str("*\n");
    out
}

// ---- HTML (standalone, print-styled) -------------------------------------

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn spans_to_html(spans: &[Span], lang: &str) -> String {
    let mut out = String::new();
    for span in spans {
        let text = escape_html(&span.text);
        match &span.style {
            SpanStyle::Bold => out.push_str(&format!("<strong>{text}</strong>")),
            SpanStyle::Italic => out.push_str(&format!("<em>{text}</em>")),
            SpanStyle::Superscript => out.push_str(&format!("<sup>{text}</sup>")),
            SpanStyle::Link(href) | SpanStyle::RedLink(href) => {
                out.push_str(&format!(
                    "<a href=\"{}\">{text}</a>",
                    escape_html(&resolve_href(href, lang))
                ));
            }
            SpanStyle::Plain => out.push_str(&text),
            // PRD FR-RD-7: raw TeX passthrough, HTML-entity-escaped (`text`
            // already is, above) but otherwise untouched, in `<code>` so it
            // renders literally.
            SpanStyle::Math(_) => out.push_str(&format!("<code>{text}</code>")),
        }
    }
    out
}

/// A minimal print stylesheet (§FR-OFF-7 "HTML (print stylesheet)"): readable
/// measure, sober type, no color dependence.
const PRINT_CSS: &str = "body{max-width:42rem;margin:2rem auto;padding:0 1rem;\
font-family:Georgia,'Times New Roman',serif;line-height:1.5;color:#111}\
h1,h2,h3{font-family:Helvetica,Arial,sans-serif;line-height:1.2}\
figure{margin:1rem 0}figcaption{font-style:italic;color:#555;font-size:.9em}\
img{max-width:100%}blockquote{border-left:3px solid #ccc;margin-left:0;padding-left:1rem;color:#333}\
footer{margin-top:2rem;padding-top:1rem;border-top:1px solid #ccc;font-size:.85em;color:#555}";

fn render_html(
    doc: &Document,
    lang: &str,
    include_nonfree: bool,
    thumbs: &[EmbeddableThumb],
) -> String {
    use base64::Engine as _;
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"");
    out.push_str(&escape_html(lang));
    out.push_str("\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str(&format!("<title>{}</title>\n", escape_html(&doc.title)));
    out.push_str(&format!("<style>{PRINT_CSS}</style>\n</head>\n<body>\n"));
    out.push_str(&format!("<h1>{}</h1>\n", escape_html(&doc.title)));

    for block in &doc.blocks {
        match block {
            Block::Heading { level, spans } => {
                let l = (*level as usize).clamp(2, 6);
                out.push_str(&format!("<h{l}>{}</h{l}>\n", spans_to_html(spans, lang)));
            }
            Block::Paragraph(spans) => {
                out.push_str(&format!("<p>{}</p>\n", spans_to_html(spans, lang)));
            }
            Block::ListItem { spans, .. } => {
                // Each item as its own single-item list keeps this renderer
                // simple; browsers render consecutive <ul>s acceptably and the
                // export is a reading artifact, not a structural round-trip.
                out.push_str(&format!(
                    "<ul><li>{}</li></ul>\n",
                    spans_to_html(spans, lang)
                ));
            }
            Block::Blockquote(spans) => {
                out.push_str(&format!(
                    "<blockquote>{}</blockquote>\n",
                    spans_to_html(spans, lang)
                ));
            }
            Block::Code(text) => {
                out.push_str(&format!("<pre><code>{}</code></pre>\n", escape_html(text)));
            }
            Block::Rule => out.push_str("<hr>\n"),
            Block::Table(table) => {
                out.push_str("<ul>\n");
                for line in table.to_list_lines() {
                    out.push_str(&format!("<li>{}</li>\n", escape_html(&line)));
                }
                out.push_str("</ul>\n");
            }
            Block::Infobox(rows) => {
                out.push_str("<table>\n");
                for (label, value) in rows {
                    if label.is_empty() {
                        out.push_str(&format!(
                            "<tr><th colspan=\"2\">{}</th></tr>\n",
                            escape_html(value)
                        ));
                    } else {
                        out.push_str(&format!(
                            "<tr><th>{}</th><td>{}</td></tr>\n",
                            escape_html(label),
                            escape_html(value)
                        ));
                    }
                }
                out.push_str("</table>\n");
            }
            Block::Image { src, alt, caption } => {
                out.push_str("<figure>\n");
                // §10: only embed the pinned image bytes when the reader opted
                // into non-free content; otherwise alt text stands in.
                let embedded = include_nonfree
                    .then_some(src.as_ref())
                    .flatten()
                    .and_then(|s| thumbs.iter().find(|t| &t.src == s))
                    .map(|t| {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&t.bytes);
                        format!(
                            "<img src=\"data:image/png;base64,{b64}\" alt=\"{}\">\n",
                            escape_html(alt)
                        )
                    });
                match embedded {
                    Some(img) => out.push_str(&img),
                    None => out.push_str(&format!("<p>[image: {}]</p>\n", escape_html(alt))),
                }
                if let Some(caption) = caption {
                    out.push_str(&format!(
                        "<figcaption>{}</figcaption>\n",
                        escape_html(caption)
                    ));
                }
                out.push_str("</figure>\n");
            }
            Block::Gallery(items) => {
                out.push_str("<ul>\n");
                for item in items {
                    out.push_str(&format!(
                        "<li>[image: {}]</li>\n",
                        escape_html(&item.caption)
                    ));
                }
                out.push_str("</ul>\n");
            }
            // PRD FR-RD-7: raw TeX passthrough, same choice as `render_md`'s
            // fenced block.
            Block::Math { tex, .. } => {
                out.push_str(&format!("<pre><code>{}</code></pre>\n", escape_html(tex)));
            }
        }
    }

    out.push_str(&format!(
        "<footer>{}</footer>\n</body>\n</html>\n",
        escape_html(&footer())
    ));
    out
}

/// Convert stored `SavedThumb`s + their bytes into embeddable thumbs. A
/// convenience for the app layer, which reads bytes from the store by src.
pub fn embeddable_from(
    thumbs: &[SavedThumb],
    mut load: impl FnMut(&str) -> Option<Vec<u8>>,
) -> Vec<EmbeddableThumb> {
    thumbs
        .iter()
        .filter_map(|t| {
            load(&t.src).map(|bytes| EmbeddableThumb {
                src: t.src.clone(),
                bytes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_doc() -> Document {
        crate::doc::parse_article_html(
            "Alan Turing",
            r#"<html><body>
            <p>He founded <a href="./Computer_science">computer science</a>.</p>
            <h2>Legacy</h2>
            <figure typeof="mw:File">
              <img src="http://example/pic.png" alt="A portrait"/>
              <figcaption>A caption.</figcaption>
            </figure>
            </body></html>"#,
        )
    }

    #[test]
    fn every_format_ends_with_the_attribution_footer() {
        let doc = fixture_doc();
        for format in FORMATS {
            let out = render(&doc, "en", format, false, &[]).unwrap();
            assert!(
                out.contains("CC BY-SA 4.0"),
                "{format} export must carry the §10 footer"
            );
            assert!(out.contains("not endorsed by the Wikimedia Foundation"));
        }
        assert_eq!(render(&doc, "en", "pdf", false, &[]), None);
    }

    #[test]
    fn markdown_resolves_internal_links_to_canonical_urls() {
        let md = render_md(&fixture_doc(), "en");
        assert!(md.starts_with("# Alan Turing"));
        assert!(md.contains("[computer science](https://en.wikipedia.org/wiki/Computer_science)"));
        assert!(md.contains("## Legacy"));
    }

    #[test]
    fn html_export_is_standalone_and_print_styled() {
        let html = render_html(&fixture_doc(), "en", false, &[]);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<style>"));
        assert!(html.contains("<h1>Alan Turing</h1>"));
        assert!(html.contains("<footer>"));
    }

    /// §10: with `include_nonfree = false` the HTML export must render images
    /// as alt text and embed NO image bytes; with it true and the pinned bytes
    /// available, it embeds a data URI.
    #[test]
    fn nonfree_policy_gates_image_embedding_in_html() {
        let doc = fixture_doc();
        let thumbs = vec![EmbeddableThumb {
            src: "http://example/pic.png".to_string(),
            bytes: vec![1, 2, 3, 4],
        }];

        let default_export = render_html(&doc, "en", false, &thumbs);
        assert!(
            !default_export.contains("data:image"),
            "free-only default must not embed image data"
        );
        assert!(default_export.contains("[image: A portrait]"));

        let optin = render_html(&doc, "en", true, &thumbs);
        assert!(
            optin.contains("data:image/png;base64,"),
            "opting into non-free embeds the pinned bytes"
        );
    }

    #[test]
    fn txt_export_uses_the_plain_renderer_plus_footer() {
        let txt = render_txt(&fixture_doc());
        assert!(txt.contains("Alan Turing"));
        assert!(txt.contains("[image: A portrait]"));
        assert!(txt.trim_end().ends_with("article text.") || txt.contains("CC BY-SA 4.0"));
    }

    #[test]
    fn default_export_path_uses_the_title_stem_and_extension() {
        for format in FORMATS {
            let path = default_export_path("Alan Turing", format).unwrap();
            let ext = extension(format).unwrap();
            assert!(path.to_string_lossy().ends_with(&format!(".{ext}")));
        }
        assert_eq!(default_export_path("X", "pdf"), None);
    }
}
