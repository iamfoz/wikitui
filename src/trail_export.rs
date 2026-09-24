//! `:trail export md|dot|mermaid [path]` (PRD FR-HS-3): three export formats
//! over a built [`crate::trail::Trail`], mirroring `bookmark_export.rs`/
//! `saved_export.rs`'s shape (a `FORMATS` const, `extension`/`render`
//! dispatch, a timestamped default path under the shared exports dir, the
//! §10 attribution footer via `crate::attribution`).
//!
//! **Graph vs. tree, and why the formats differ on purpose** (PRD FR-HS-3):
//! Markdown renders the *tree* — a nested bullet outline, one root heading
//! per entry point, matching what the `:trail` view itself shows on screen.
//! DOT and Mermaid render the *graph* — every node and every distinct
//! referrer edge, including the "also from" pairs the tree collapses into a
//! note rather than a second parent (see `trail.rs`'s module doc). This is
//! deliberate, not an inconsistency: DOT/Mermaid are real graph formats that
//! can represent a node with more than one incoming edge, so there is no
//! reason to throw that information away just because the *v1.x on-screen*
//! view has to; Markdown, being a nested-list outline, has no representation
//! for a second parent, so it stays tied to the tree and notes the
//! alternate referrer inline instead (the same "(also from: X)" wording the
//! tree view itself would show).

use std::path::PathBuf;

use crate::trail::{ArticleKey, Trail, TrailTreeNode};

pub const FORMATS: [&str; 3] = ["md", "dot", "mermaid"];

pub fn extension(format: &str) -> Option<&'static str> {
    match format {
        "md" => Some("md"),
        "dot" => Some("dot"),
        "mermaid" => Some("mmd"),
        _ => None,
    }
}

/// `$XDG_DATA_HOME/wikitui/exports/` — the same directory every other
/// export (bookmarks, saved pages) uses.
pub fn default_export_dir() -> PathBuf {
    crate::bookmark_export::default_export_dir()
}

/// The default timestamped path for a trail export in `format` —
/// `trail-<YYYYMMDD-HHMMSS>.<ext>` under [`default_export_dir`].
pub fn default_export_path(format: &str) -> Option<PathBuf> {
    let ext = extension(format)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    Some(default_export_dir().join(format!("trail-{ts}.{ext}")))
}

/// Renders `trail` in the named format, or `None` for an unrecognized one
/// (`:trail export` already validates this at parse time — see
/// `command.rs` — so this is a defensive second check, matching
/// `bookmark_export::render`'s own posture).
pub fn render(trail: &Trail, format: &str, retrieved_on: &str) -> Option<String> {
    match format {
        "md" => Some(render_markdown(trail, retrieved_on)),
        "dot" => Some(render_dot(trail, retrieved_on)),
        "mermaid" => Some(render_mermaid(trail, retrieved_on)),
        _ => None,
    }
}

fn dwell_label(secs: i64) -> String {
    crate::cache::age_human(secs.max(0) as u64)
}

fn wiki_prefix(article: &ArticleKey) -> String {
    if article.wiki.is_empty() {
        String::new()
    } else {
        format!("[{}] ", article.wiki)
    }
}

// ---- Markdown (tree outline) ----------------------------------------------

fn render_markdown(trail: &Trail, retrieved_on: &str) -> String {
    let count = trail.graph.nodes.len();
    let total: i64 = trail.graph.nodes.iter().map(|n| n.total_dwell_secs).sum();
    let mut out = format!(
        "# Trail ({count} article{}, total {})\n",
        if count == 1 { "" } else { "s" },
        dwell_label(total)
    );
    if trail.tree.roots.is_empty() {
        out.push_str("\n(no trail yet — open an article and follow a few links)\n");
    } else {
        out.push('\n');
        for root in &trail.tree.roots {
            write_outline(&mut out, root, 0);
        }
    }
    out.push_str("\n---\n\n*");
    out.push_str(&crate::attribution::export_footer(retrieved_on, false));
    out.push_str("*\n");
    out
}

fn write_outline(out: &mut String, node: &TrailTreeNode, depth: usize) {
    out.push_str(&"  ".repeat(depth));
    out.push_str("- ");
    out.push_str(&wiki_prefix(&node.article));
    out.push_str(&node.article.title);
    out.push_str(&format!(" ({})", dwell_label(node.total_dwell_secs)));
    if !node.also_from.is_empty() {
        let names: Vec<String> = node
            .also_from
            .iter()
            .map(|a| format!("{}{}", wiki_prefix(a), a.title))
            .collect();
        out.push_str(&format!(" (also from: {})", names.join(", ")));
    }
    out.push('\n');
    for child in &node.children {
        write_outline(out, child, depth + 1);
    }
}

// ---- DOT (Graphviz digraph, full graph) -----------------------------------

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn node_label(article: &ArticleKey, dwell_secs: i64) -> String {
    format!(
        "{}{} ({})",
        wiki_prefix(article),
        article.title,
        dwell_label(dwell_secs)
    )
}

fn render_dot(trail: &Trail, retrieved_on: &str) -> String {
    let ids: std::collections::HashMap<ArticleKey, String> = trail
        .graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.article.clone(), format!("n{i}")))
        .collect();

    let mut out = String::from("digraph trail {\n");
    for node in &trail.graph.nodes {
        let id = &ids[&node.article];
        let label = dot_escape(&node_label(&node.article, node.total_dwell_secs));
        out.push_str(&format!("  \"{id}\" [label=\"{label}\"];\n"));
    }
    for edge in &trail.graph.edges {
        if let (Some(from), Some(to)) = (ids.get(&edge.from), ids.get(&edge.to)) {
            out.push_str(&format!("  \"{from}\" -> \"{to}\";\n"));
        }
    }
    out.push_str("}\n");
    // PRD §10: "every export" carries the attribution footer, DOT/Mermaid
    // included — Graphviz's lexer strips `//`-to-end-of-line comments
    // wherever they appear, including after the closing brace, so this
    // trails the graph body rather than needing to live inside it.
    out.push_str("// ");
    out.push_str(&crate::attribution::export_footer(retrieved_on, false));
    out.push('\n');
    out
}

// ---- Mermaid (flowchart, full graph) --------------------------------------

fn mermaid_escape(s: &str) -> String {
    s.replace('"', "&quot;")
}

fn render_mermaid(trail: &Trail, retrieved_on: &str) -> String {
    let ids: std::collections::HashMap<ArticleKey, String> = trail
        .graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.article.clone(), format!("n{i}")))
        .collect();

    let mut out = String::from("graph TD\n");
    for node in &trail.graph.nodes {
        let id = &ids[&node.article];
        let label = mermaid_escape(&node_label(&node.article, node.total_dwell_secs));
        out.push_str(&format!("    {id}[\"{label}\"]\n"));
    }
    for edge in &trail.graph.edges {
        if let (Some(from), Some(to)) = (ids.get(&edge.from), ids.get(&edge.to)) {
            out.push_str(&format!("    {from} --> {to}\n"));
        }
    }
    // PRD §10: "every export" carries the attribution footer — `%%` is
    // Mermaid's own comment syntax, so this trails the diagram without
    // becoming a phantom node/edge.
    out.push_str("%% ");
    out.push_str(&crate::attribution::export_footer(retrieved_on, false));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Visit;
    use crate::trail::build;

    fn visit(title: &str, opened_at: i64, referrer: Option<&str>) -> Visit {
        Visit {
            id: opened_at,
            wiki: String::new(),
            lang: "en".to_string(),
            title: title.to_string(),
            opened_at,
            dwell_secs: 90,
            referrer_wiki: referrer.map(|_| String::new()),
            referrer_lang: referrer.map(|_| "en".to_string()),
            referrer_title: referrer.map(str::to_string),
        }
    }

    fn sample_trail() -> Trail {
        build(&[
            visit("Alan Turing", 100, None),
            visit("Enigma machine", 200, Some("Alan Turing")),
            visit("Bletchley Park", 300, Some("Enigma machine")),
        ])
    }

    // ---- extension / default path ------------------------------------------

    #[test]
    fn extension_and_default_path_agree_on_known_formats() {
        for format in FORMATS {
            let ext = extension(format).unwrap();
            let path = default_export_path(format).unwrap();
            assert!(path.to_string_lossy().ends_with(&format!(".{ext}")));
        }
        assert_eq!(extension("json"), None);
        assert_eq!(default_export_path("json"), None);
        assert_eq!(render(&Trail::default(), "json", "2026-07-15"), None);
    }

    // ---- Markdown -------------------------------------------------------------

    #[test]
    fn markdown_outline_is_nested_and_has_the_attribution_footer() {
        let trail = sample_trail();
        let md = render_markdown(&trail, "2026-07-15");
        assert!(md.starts_with("# Trail (3 articles, total 4m)"));
        let turing_line = md.lines().find(|l| l.contains("Alan Turing")).unwrap();
        assert!(turing_line.starts_with("- "), "root has no leading indent");
        let enigma_line = md.lines().find(|l| l.contains("Enigma machine")).unwrap();
        assert!(
            enigma_line.starts_with("  - "),
            "a depth-1 child is indented two spaces: {enigma_line:?}"
        );
        let bletchley_line = md.lines().find(|l| l.contains("Bletchley Park")).unwrap();
        assert!(
            bletchley_line.starts_with("    - "),
            "a depth-2 grandchild is indented four spaces: {bletchley_line:?}"
        );
        assert!(md.contains("CC BY-SA 4.0"), "attribution footer present");
        assert!(md.contains("Exported 2026-07-15"));
    }

    #[test]
    fn markdown_notes_an_also_from_referrer_inline() {
        let trail = build(&[
            visit("A", 100, None),
            visit("B", 150, None),
            visit("C", 200, Some("A")),
            visit("C", 250, Some("B")),
        ]);
        let md = render_markdown(&trail, "2026-07-15");
        let c_line = md.lines().find(|l| l.contains("- C")).unwrap();
        assert!(
            c_line.contains("also from: B"),
            "the alternate referrer is noted inline: {c_line:?}"
        );
    }

    #[test]
    fn empty_trail_markdown_says_no_trail_yet_but_still_has_the_footer() {
        let md = render_markdown(&Trail::default(), "2026-07-15");
        assert!(md.contains("no trail yet"));
        assert!(md.contains("CC BY-SA 4.0"));
    }

    // ---- DOT --------------------------------------------------------------

    #[test]
    fn dot_export_is_a_valid_digraph_with_expected_nodes_and_edges() {
        let trail = sample_trail();
        let dot = render_dot(&trail, "2026-07-15");
        assert!(dot.starts_with("digraph trail {\n"));
        // The graph body's closing brace is its own line — the §10
        // attribution footer trails it as a `//` comment, not more graph
        // syntax, so the file no longer ends on `}` (see the dedicated
        // attribution test below).
        assert!(dot.lines().any(|l| l == "}"));
        assert!(dot.contains("label=\"Alan Turing (1m)\""));
        assert!(dot.contains("label=\"Enigma machine (1m)\""));
        assert!(dot.contains("label=\"Bletchley Park (1m)\""));
        // Every `->` line has the `"id" -> "id";` shape.
        let edge_lines: Vec<&str> = dot.lines().filter(|l| l.contains("->")).collect();
        assert_eq!(edge_lines.len(), 2, "A->B and B->C");
        for line in &edge_lines {
            assert!(line.trim_start().starts_with('"'));
            assert!(line.trim_end().ends_with(';'));
        }
    }

    /// PRD §10: "every export" embeds the attribution footer — DOT and
    /// Mermaid included, as a format-appropriate comment (`//`/`%%`) so it
    /// never breaks the graph a viewer parses.
    #[test]
    fn dot_and_mermaid_both_carry_the_attribution_footer_as_a_comment() {
        let trail = sample_trail();
        let dot = render_dot(&trail, "2026-07-15");
        let dot_comment_line = dot
            .lines()
            .find(|l| l.starts_with("// "))
            .expect("a `//` comment line carrying the footer");
        assert!(dot_comment_line.contains("CC BY-SA 4.0"));
        assert!(dot_comment_line.contains("Exported 2026-07-15"));

        let mmd = render_mermaid(&trail, "2026-07-15");
        let mmd_comment_line = mmd
            .lines()
            .find(|l| l.starts_with("%% "))
            .expect("a `%%` comment line carrying the footer");
        assert!(mmd_comment_line.contains("CC BY-SA 4.0"));
        assert!(mmd_comment_line.contains("Exported 2026-07-15"));
    }

    #[test]
    fn dot_escapes_quotes_and_backslashes_in_titles() {
        let trail = build(&[visit("A \"quote\" \\ test", 100, None)]);
        let dot = render_dot(&trail, "2026-07-15");
        assert!(dot.contains("A \\\"quote\\\" \\\\ test"));
    }

    #[test]
    fn dot_carries_a_multi_parent_edge_the_tree_view_collapses() {
        let trail = build(&[
            visit("A", 100, None),
            visit("B", 150, None),
            visit("C", 200, Some("A")),
            visit("C", 250, Some("B")), // "also from" in the tree, a real edge in DOT
        ]);
        let dot = render_dot(&trail, "2026-07-15");
        let edge_lines: Vec<&str> = dot.lines().filter(|l| l.contains("->")).collect();
        assert_eq!(
            edge_lines.len(),
            2,
            "DOT keeps BOTH referrer edges into C, unlike the one-parent tree"
        );
    }

    #[test]
    fn empty_trail_dot_is_still_a_syntactically_valid_empty_digraph() {
        let dot = render_dot(&Trail::default(), "2026-07-15");
        assert!(dot.starts_with("digraph trail {\n}\n"));
        // PRD §10: "every export" — an empty trail is no exception.
        assert!(dot.contains("// "));
        assert!(dot.contains("CC BY-SA 4.0"));
    }

    // ---- Mermaid ----------------------------------------------------------

    #[test]
    fn mermaid_export_is_a_valid_graph_td_with_expected_nodes_and_edges() {
        let trail = sample_trail();
        let mmd = render_mermaid(&trail, "2026-07-15");
        assert!(mmd.starts_with("graph TD\n"));
        assert!(mmd.contains("[\"Alan Turing (1m)\"]"));
        assert!(mmd.contains("--> "));
        let arrow_lines: Vec<&str> = mmd.lines().filter(|l| l.contains("-->")).collect();
        assert_eq!(arrow_lines.len(), 2);
    }

    #[test]
    fn mermaid_escapes_quotes_in_titles() {
        let trail = build(&[visit("A \"quote\" test", 100, None)]);
        let mmd = render_mermaid(&trail, "2026-07-15");
        assert!(mmd.contains("A &quot;quote&quot; test"));
        assert!(!mmd.contains("A \"quote\" test"));
    }

    #[test]
    fn empty_trail_mermaid_is_still_a_syntactically_valid_empty_graph() {
        let mmd = render_mermaid(&Trail::default(), "2026-07-15");
        assert!(mmd.starts_with("graph TD\n"));
        // PRD §10: "every export" — an empty trail is no exception.
        assert!(mmd.contains("%% "));
        assert!(mmd.contains("CC BY-SA 4.0"));
    }

    // ---- wiki-awareness ------------------------------------------------------

    #[test]
    fn a_non_default_wiki_node_carries_its_wiki_tag_in_every_format() {
        let trail = build(&[Visit {
            id: 1,
            wiki: "wiktionary".to_string(),
            lang: "en".to_string(),
            title: "Mercury".to_string(),
            opened_at: 100,
            dwell_secs: 60,
            referrer_wiki: None,
            referrer_lang: None,
            referrer_title: None,
        }]);
        let md = render_markdown(&trail, "2026-07-15");
        assert!(md.contains("[wiktionary] Mercury"));
        let dot = render_dot(&trail, "2026-07-15");
        assert!(dot.contains("[wiktionary] Mercury"));
        let mmd = render_mermaid(&trail, "2026-07-15");
        assert!(mmd.contains("[wiktionary] Mercury"));
    }
}
