//! `:trail` (PRD FR-HS-3): the "wander graph" a reading session traces —
//! nodes are distinct articles, edges are link-follows. `history::Visit`
//! already carries the edge for free: its `referrer_*` fields are the
//! article the reader navigated *from* within the same tab (see
//! `history.rs`'s own module doc), so a visit to article B whose referrer is
//! article A means exactly "the reader followed a link from A to B."
//!
//! ## Tree-ification (v1.x scope)
//!
//! FR-HS-3 ships a **tree** in v1.x; the true DAG a wander graph really is
//! (a node can have more than one referrer — re-visiting an article from a
//! second link, or two different articles both linking to a third) is
//! explicitly deferred to v2. The chosen tree-ification: **a node's tree
//! parent is the referrer recorded on its chronologically first visit**.
//! Every other referrer seen on a later revisit is kept as an "also from"
//! note on that node rather than a second parent edge — the node is placed
//! once, under its first referrer, and the alternate referrers are annotated,
//! not dropped. This is a documented simplification of the *display*, not a
//! data loss: [`build_graph`]'s `edges` field is the full referrer-pair set
//! (including every "also from" pair), and the export path always renders
//! that full graph for DOT/Mermaid (real graph formats, so multi-parent is
//! representable); only the on-screen tree and the Markdown outline collapse
//! to one parent per node.
//!
//! ## Cycle safety
//!
//! A referrer must have been visited (and its own first-visit recorded)
//! *before* the reader could navigate away from it, so under real navigation
//! timestamps the first-referrer parent relation is acyclic by construction.
//! [`tree_from_graph`] still guards against a cycle defensively — a `placed`
//! set stops the walk from re-entering a node it already placed — because
//! nothing stops a hand-built visit list (or a future multi-tab/session
//! merge) from producing one; a node caught in such a cycle is promoted to
//! its own root rather than silently dropped or looped over forever.
//!
//! ## DAG view (v2 scope)
//!
//! [`dag_from_graph`] is the "true DAG layout" FR-HS-3 explicitly deferred
//! past the tree above: a node reached from more than one article shows
//! **every** incoming edge, not one parent plus an "also from" note. It is
//! an alternate, opt-in view (`:trail dag` / `App::trail_layout`) — the tree
//! stays the default, unchanged by this section.
//!
//! The chosen text-DAG rendering is a **topologically ordered flat list**,
//! each row explicitly naming all of its parents, rather than a git-log-
//! graph-style lane/column diagram (`git log --graph`'s own approach:
//! persistent columns, diagonal `╱`/`╲` crossing lines for merges and
//! branches). That fuller rendering was considered and set aside: assigning
//! stable columns to an arbitrary DAG so that crossing lines never
//! misrepresent an edge is a nontrivial graph-drawing problem in its own
//! right (git's own implementation runs a dedicated column-allocation pass
//! over the commit graph), and a wrong-but-plausible-looking crossing line
//! is a worse failure mode for a small "wander graph" feature than a plainer
//! rendering that is always unambiguous. The flat list keeps the one
//! property that actually matters for a DAG *reading order*: every parent
//! row appears above every one of its children's rows (true topological
//! order — [`dag_from_graph`]'s own doc comment covers the cycle-safety
//! fallback for hand-built/adversarial data, mirroring `tree_from_graph`'s).
//! Connector glyphs are still drawn (`dag_connector`) — reusing the tree's
//! `│`/`├─`/`└─` family plus a `┴─` "multiple incoming edges join here"
//! marker for an actual merge — but they decorate each row rather than
//! encoding real column geometry, and every row's parents are named in full
//! alongside them regardless (a single-parent row is unambiguous either
//! way; a multi-parent row is exactly the case a git-log-style diagram would
//! need columns for, so this spells it out in text instead).
//!
//! ## Scope
//!
//! [`build`] takes whatever visit slice the caller already selected — this
//! module has no opinion on "session" vs. "all-history" vs. a date range.
//! `App::open_trail` is the scope policy: bare `:trail` filters to this run's
//! own session (`App::session_started_at` onward — "session-scope is the
//! natural wander graph," per the brief this chunk implements against);
//! `:trail all` widens to the full history; `:trail days N` widens to the
//! last N days. `App::export_trail` always uses the session scope,
//! independent of whatever a currently-open `:trail`/`:trail all` view is
//! showing — see that method's doc comment for why.

use std::collections::{HashMap, HashSet};

use crate::history::Visit;

/// One distinct article — a graph node's identity. `wiki` is the FR-ML-4
/// scope (`""` = default Wikipedia), the same convention `history::Visit`/
/// `tab::Tab::wiki` use, so a cross-wiki trail keeps a same-titled article on
/// two different wikis as two distinct nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArticleKey {
    pub wiki: String,
    pub lang: String,
    pub title: String,
}

/// One graph node: an article plus the aggregate stats over every visit to
/// it within the scope `build` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailNode {
    pub article: ArticleKey,
    /// Summed `dwell_secs` across every visit to this article in scope.
    pub total_dwell_secs: i64,
    /// How many times this article was visited in scope (a revisit counts
    /// again) — distinct from `history::ReadingStats::total_visits`, which
    /// counts across *all* history, not one trail's scope.
    pub visit_count: usize,
    /// `opened_at` of the chronologically first visit in scope.
    pub first_visited_at: i64,
    /// The referrer of that first visit, or `None` if the first visit had
    /// none (a direct open/session entry point) — this node's tree parent.
    pub first_referrer: Option<ArticleKey>,
    /// Distinct referrers seen on *later* revisits, other than
    /// `first_referrer`, in the order first seen — PRD FR-HS-3's
    /// "multi-parent node ... later edges noted as 'also from X'."
    pub also_from: Vec<ArticleKey>,
}

/// One graph edge: `from` (the referrer) to `to` (the article reached) — a
/// single link-follow. Deduped in [`TrailGraph::edges`]: following the same
/// link twice doesn't produce two edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailEdge {
    pub from: ArticleKey,
    pub to: ArticleKey,
}

/// The wander graph: every distinct article visited in scope (in
/// first-visited order) and every distinct referrer pair (in first-seen
/// order). This is the *whole* graph — including edges the tree collapses
/// into "also from" notes — which is exactly what the DOT/Mermaid exports
/// render (see the module doc's "Tree-ification" section).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrailGraph {
    pub nodes: Vec<TrailNode>,
    pub edges: Vec<TrailEdge>,
}

/// Builds the graph from a visit slice. Callers pass whatever scope they
/// want (session/all/date-range — see the module doc); `visits` is expected
/// oldest-first, `history::History::all_visits`'s own order, since a node's
/// "first visit" is simply the first row seen for its `(wiki, lang, title)`.
pub fn build_graph(visits: &[Visit]) -> TrailGraph {
    struct Building {
        total_dwell_secs: i64,
        visit_count: usize,
        first_visited_at: i64,
        first_referrer: Option<ArticleKey>,
        seen_referrers: HashSet<ArticleKey>,
        also_from: Vec<ArticleKey>,
    }

    let mut order: Vec<ArticleKey> = Vec::new();
    let mut building: HashMap<ArticleKey, Building> = HashMap::new();
    let mut edge_seen: HashSet<(ArticleKey, ArticleKey)> = HashSet::new();
    let mut edges: Vec<TrailEdge> = Vec::new();

    for v in visits {
        let key = ArticleKey {
            wiki: v.wiki.clone(),
            lang: v.lang.clone(),
            title: v.title.clone(),
        };
        let referrer_key = match (&v.referrer_lang, &v.referrer_title) {
            (Some(l), Some(t)) => Some(ArticleKey {
                wiki: v.referrer_wiki.clone().unwrap_or_default(),
                lang: l.clone(),
                title: t.clone(),
            }),
            _ => None,
        };

        let is_new = !building.contains_key(&key);
        if is_new {
            order.push(key.clone());
        }
        let entry = building.entry(key.clone()).or_insert_with(|| Building {
            total_dwell_secs: 0,
            visit_count: 0,
            first_visited_at: v.opened_at,
            first_referrer: None,
            seen_referrers: HashSet::new(),
            also_from: Vec::new(),
        });
        entry.total_dwell_secs += v.dwell_secs.max(0);
        entry.visit_count += 1;

        if is_new {
            entry.first_referrer = referrer_key.clone();
            if let Some(r) = &referrer_key {
                entry.seen_referrers.insert(r.clone());
            }
        } else if let Some(r) = referrer_key.clone()
            && entry.first_referrer.as_ref() != Some(&r)
            && entry.seen_referrers.insert(r.clone())
        {
            entry.also_from.push(r);
        }

        if let Some(r) = referrer_key {
            let pair = (r.clone(), key.clone());
            if edge_seen.insert(pair) {
                edges.push(TrailEdge { from: r, to: key });
            }
        }
    }

    let nodes = order
        .into_iter()
        .map(|key| {
            let b = building.remove(&key).expect("just inserted above");
            TrailNode {
                article: key,
                total_dwell_secs: b.total_dwell_secs,
                visit_count: b.visit_count,
                first_visited_at: b.first_visited_at,
                first_referrer: b.first_referrer,
                also_from: b.also_from,
            }
        })
        .collect();

    TrailGraph { nodes, edges }
}

/// One tree node — a node's own stats plus its children (PRD FR-HS-3's
/// navigable tree). Distinct from [`TrailNode`] mainly by owning
/// `children` instead of a graph-wide `first_referrer` pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailTreeNode {
    pub article: ArticleKey,
    pub total_dwell_secs: i64,
    pub visit_count: usize,
    pub also_from: Vec<ArticleKey>,
    pub children: Vec<TrailTreeNode>,
}

/// The tree-ified display form of a [`TrailGraph`] — a forest, since a
/// session commonly has more than one entry point (a tab's first page, a
/// search, a CLI-opened title all start their own root).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrailTree {
    pub roots: Vec<TrailTreeNode>,
}

/// Tree-ifies `graph` per the module doc's "Tree-ification" section: each
/// node is placed under its `first_referrer` (a root if `None`, or if that
/// referrer isn't itself a node in this graph — e.g. a session-scoped trail
/// whose first article was reached from something visited *before* the
/// session cutoff, which is exactly a session entry point too).
pub fn tree_from_graph(graph: &TrailGraph) -> TrailTree {
    let node_keys: HashSet<&ArticleKey> = graph.nodes.iter().map(|n| &n.article).collect();
    let by_key: HashMap<&ArticleKey, &TrailNode> =
        graph.nodes.iter().map(|n| (&n.article, n)).collect();

    let mut children_of: HashMap<ArticleKey, Vec<ArticleKey>> = HashMap::new();
    let mut roots: Vec<ArticleKey> = Vec::new();
    for n in &graph.nodes {
        match &n.first_referrer {
            Some(parent) if parent != &n.article && node_keys.contains(parent) => {
                children_of
                    .entry(parent.clone())
                    .or_default()
                    .push(n.article.clone());
            }
            _ => roots.push(n.article.clone()),
        }
    }

    let mut placed: HashSet<ArticleKey> = HashSet::new();
    let mut tree_roots: Vec<TrailTreeNode> = Vec::new();
    for r in &roots {
        tree_roots.push(walk_tree(r, &by_key, &children_of, &mut placed));
    }
    // Defensive sweep (see the module doc's "Cycle safety"): a node whose
    // first-referrer chain forms a cycle never appears in `roots` and is
    // never reached by the walk above — promote it to its own root instead
    // of losing it.
    for n in &graph.nodes {
        if !placed.contains(&n.article) {
            tree_roots.push(walk_tree(&n.article, &by_key, &children_of, &mut placed));
        }
    }
    TrailTree { roots: tree_roots }
}

fn walk_tree(
    key: &ArticleKey,
    by_key: &HashMap<&ArticleKey, &TrailNode>,
    children_of: &HashMap<ArticleKey, Vec<ArticleKey>>,
    placed: &mut HashSet<ArticleKey>,
) -> TrailTreeNode {
    let node = by_key[key];
    placed.insert(key.clone());
    let mut out = TrailTreeNode {
        article: key.clone(),
        total_dwell_secs: node.total_dwell_secs,
        visit_count: node.visit_count,
        also_from: node.also_from.clone(),
        children: Vec::new(),
    };
    if let Some(kids) = children_of.get(key) {
        for kid in kids {
            if placed.contains(kid) {
                continue; // a referrer cycle — stop here, don't loop forever.
            }
            out.children
                .push(walk_tree(kid, by_key, children_of, placed));
        }
    }
    out
}

/// FR-HS-3's two trail layouts (PRD: "v1.x ships tree layout; true DAG
/// layout is v2"). `App::trail_layout` picks which one `:trail` shows;
/// [`TrailGraph`] itself has no opinion and can render either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TrailLayout {
    /// The v1.x default (module doc's "Tree-ification"): one parent per
    /// node, extra referrers noted as "also from."
    #[default]
    Tree,
    /// The v2 true DAG (module doc's "DAG view"): every parent shown.
    Dag,
}

impl TrailLayout {
    /// `:trail`/`:trail dag`'s toggle label, for the view's own title bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tree => "Trail",
            Self::Dag => "Trail (DAG)",
        }
    }
}

/// One row of the DAG view (module doc's "DAG view"): a node plus **every**
/// referrer it has been reached from, in first-seen order — unlike
/// [`TrailNode`], which keeps only `first_referrer` and files the rest under
/// `also_from`. `parents` is empty exactly when the node is a DAG root (no
/// in-scope referrer at all), the same condition [`tree_from_graph`] uses to
/// place a node as a tree root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagNode {
    pub article: ArticleKey,
    pub total_dwell_secs: i64,
    pub visit_count: usize,
    pub parents: Vec<ArticleKey>,
}

/// The DAG view's whole renderable list, already in topological order (see
/// [`dag_from_graph`]) — every parent row appears before every one of its
/// children's rows, so a renderer/selector can treat this as one flat,
/// directly-indexable list exactly like [`flatten`]'s tree-line list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dag {
    pub nodes: Vec<DagNode>,
}

/// Builds the v2 true-DAG view from `graph` (module doc's "DAG view"): every
/// node, in topological order, carrying every in-scope referrer as a parent
/// — nothing collapsed to "also from" the way [`tree_from_graph`] does.
///
/// Topological order is produced by repeatedly placing the earliest
/// (first-visited-order) not-yet-placed node whose parents are all already
/// placed — under real navigation timestamps `graph.nodes`' own order
/// already satisfies this on the first pass (module doc's "Cycle safety":
/// a referrer is necessarily visited, and placed, before its child). A hand-
/// built/adversarial cycle (nothing stops one, same caveat
/// `tree_from_graph` documents) is broken the same defensive way: once no
/// remaining node has all its parents placed, the earliest remaining node is
/// force-placed anyway rather than looping forever — its still-unplaced
/// parent(s) simply render *below* it once *they* are placed, the one
/// honest artifact of breaking a cycle in a linear order.
pub fn dag_from_graph(graph: &TrailGraph) -> Dag {
    let by_key: HashMap<&ArticleKey, &TrailNode> =
        graph.nodes.iter().map(|n| (&n.article, n)).collect();

    // Every in-scope, non-self referrer per node, first-seen order — the
    // full edge set already carries every referrer pair (module doc's
    // "Tree-ification"); this simply keeps all of them instead of only the
    // first. An edge whose `from` isn't itself a node in this graph (a
    // referrer outside the scope, e.g. a session cutoff — see
    // `tree_from_graph`'s "root" handling for the identical case) can't be
    // placed as a row of its own, so it can't be a DAG parent either; the
    // child simply has one fewer parent, same as it becoming a tree root.
    let mut parents_of: HashMap<&ArticleKey, Vec<&ArticleKey>> = HashMap::new();
    let mut parent_seen: HashMap<&ArticleKey, HashSet<&ArticleKey>> = HashMap::new();
    for e in &graph.edges {
        if e.from == e.to || !by_key.contains_key(&e.from) {
            continue;
        }
        if parent_seen.entry(&e.to).or_default().insert(&e.from) {
            parents_of.entry(&e.to).or_default().push(&e.from);
        }
    }

    let mut placed: HashSet<&ArticleKey> = HashSet::new();
    let mut order: Vec<ArticleKey> = Vec::with_capacity(graph.nodes.len());
    while order.len() < graph.nodes.len() {
        let ready = graph.nodes.iter().map(|n| &n.article).find(|k| {
            !placed.contains(*k)
                && parents_of
                    .get(*k)
                    .is_none_or(|ps| ps.iter().all(|p| placed.contains(*p)))
        });
        let next = ready.or_else(|| {
            graph
                .nodes
                .iter()
                .map(|n| &n.article)
                .find(|k| !placed.contains(*k))
        });
        let Some(key) = next else { break };
        placed.insert(key);
        order.push(key.clone());
    }

    let nodes = order
        .into_iter()
        .map(|key| {
            let node = by_key[&key];
            let parents = parents_of
                .get(&key)
                .map(|ps| ps.iter().map(|p| (*p).clone()).collect())
                .unwrap_or_default();
            DagNode {
                article: key,
                total_dwell_secs: node.total_dwell_secs,
                visit_count: node.visit_count,
                parents,
            }
        })
        .collect();

    Dag { nodes }
}

/// `dag_from_graph`'s row connector (module doc's "DAG view"): reuses the
/// tree's `│`/`├─`/`└─` glyph family for a root/single-parent row, plus a
/// `┴─` "multiple incoming edges join here" marker for an actual merge — a
/// visual nod to `git log --graph`'s own merge glyph, not a literal column
/// diagram (see the module doc for why this build doesn't attempt one). A
/// root gets no prefix at all, matching `connector`'s own `depth == 0` rule.
pub fn dag_connector(node: &DagNode) -> &'static str {
    match node.parents.len() {
        0 => "",
        1 => "\u{2514}\u{2500} ",
        _ => "\u{2534}\u{2500} ",
    }
}

/// The full trail: the graph (nodes + full edge set) and its tree-ified
/// display form, built together so a caller never has to keep them in sync
/// by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trail {
    pub graph: TrailGraph,
    pub tree: TrailTree,
}

/// Builds a [`Trail`] from a visit slice (see [`build_graph`]'s doc comment
/// for the expected ordering).
pub fn build(visits: &[Visit]) -> Trail {
    let graph = build_graph(visits);
    let tree = tree_from_graph(&graph);
    Trail { graph, tree }
}

/// One flattened, renderable/selectable row of the tree (PRD FR-HS-3's
/// "navigable tree" — `j`/`k` moves an index into this list, Enter reopens
/// `article`). `ancestor_continues`/`is_last` carry everything a renderer
/// needs to draw git-log-graph-style connectors (`connector`) without
/// re-walking the tree itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailLine {
    pub article: ArticleKey,
    /// Root is depth 0.
    pub depth: usize,
    pub total_dwell_secs: i64,
    pub visit_count: usize,
    pub also_from: Vec<ArticleKey>,
    /// One entry per ancestor level (root..immediate parent): `true` means
    /// that ancestor was *not* the last child of its own parent, so a
    /// vertical bar continues in that column; `false` means blank. Length
    /// equals `depth`.
    pub ancestor_continues: Vec<bool>,
    /// Whether this line is the last child among its own siblings (picks
    /// `└─` vs `├─` — irrelevant, and always `true`, at `depth == 0`).
    pub is_last: bool,
}

/// Flattens `tree` into the navigable line list, depth-first, each root
/// rendered as an independent subtree (never sibling-connected to another
/// root — roots are distinct entry points, not children of a shared node).
pub fn flatten(tree: &TrailTree) -> Vec<TrailLine> {
    let mut out = Vec::new();
    for root in &tree.roots {
        push_line(root, 0, &mut Vec::new(), true, &mut out);
    }
    out
}

fn push_line(
    node: &TrailTreeNode,
    depth: usize,
    ancestors: &mut Vec<bool>,
    is_last: bool,
    out: &mut Vec<TrailLine>,
) {
    out.push(TrailLine {
        article: node.article.clone(),
        depth,
        total_dwell_secs: node.total_dwell_secs,
        visit_count: node.visit_count,
        also_from: node.also_from.clone(),
        ancestor_continues: ancestors.clone(),
        is_last,
    });
    ancestors.push(!is_last);
    let last_index = node.children.len().saturating_sub(1);
    for (i, child) in node.children.iter().enumerate() {
        push_line(child, depth + 1, ancestors, i == last_index, out);
    }
    ancestors.pop();
}

/// Renders `line`'s indentation + connector glyphs — git-log-graph
/// aesthetics (`│`, `├─`, `└─`), the same idiom `git log --graph`/the `tree`
/// command use. A root (`depth == 0`) gets no prefix at all.
pub fn connector(line: &TrailLine) -> String {
    if line.depth == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(line.depth * 3 + 3);
    for continues in &line.ancestor_continues {
        out.push_str(if *continues { "\u{2502}  " } else { "   " });
    }
    out.push_str(if line.is_last {
        "\u{2514}\u{2500} "
    } else {
        "\u{251c}\u{2500} "
    });
    out
}

/// PRD FR-HS-3's dwell "sizing" honestly framed for a text UI: not a literal
/// resized node, but a bar of filled block characters proportional to
/// `dwell_secs` against `peak_secs` (the largest dwell in the trail being
/// shown) — the same "scale to peak, `width`-char bar" idiom `ui::
/// draw_interests` already uses for topic-affinity scores. Empty when
/// `peak_secs` is `0` (nothing dwelt on yet).
pub fn dwell_bar(dwell_secs: i64, peak_secs: i64, width: usize) -> String {
    if peak_secs <= 0 || dwell_secs <= 0 {
        return String::new();
    }
    let filled = ((dwell_secs as f64 / peak_secs as f64) * width as f64).round() as usize;
    "\u{2588}".repeat(filled.min(width))
}

/// PRD FR-DL-8 seam: pure stats over a trail, consumed by
/// `achievements::newly_crossed` (via `App::check_achievements`) for the
/// achievement-toast easter egg ("Rabbit Hole: 15 articles in one session").
/// This module exposes only the numbers — the toast wording/thresholds and
/// the `pro = true` opt-out live in `achievements.rs`/`app.rs`; nothing here
/// renders or decides when to show anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrailStats {
    /// Distinct articles in the trail's scope.
    pub article_count: usize,
    /// The deepest tree level reached (root = depth 0), `0` for an empty
    /// trail or one with no chains at all.
    pub max_depth: usize,
    /// Node count along the single longest root-to-leaf path — `max_depth +
    /// 1` when the trail has any nodes, else `0`.
    pub longest_chain: usize,
}

/// Computes [`TrailStats`] from an already-built trail — see the module
/// doc's FR-DL-8 seam; `App::check_achievements` calls this after every
/// recorded visit.
pub fn stats(trail: &Trail) -> TrailStats {
    let article_count = trail.graph.nodes.len();
    let lines = flatten(&trail.tree);
    let max_depth = lines.iter().map(|l| l.depth).max().unwrap_or(0);
    let longest_chain = if lines.is_empty() { 0 } else { max_depth + 1 };
    TrailStats {
        article_count,
        max_depth,
        longest_chain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(
        wiki: &str,
        lang: &str,
        title: &str,
        opened_at: i64,
        dwell: i64,
        referrer: Option<(&str, &str, &str)>,
    ) -> Visit {
        let (rw, rl, rt) = match referrer {
            Some((w, l, t)) => (
                Some(w.to_string()),
                Some(l.to_string()),
                Some(t.to_string()),
            ),
            None => (None, None, None),
        };
        Visit {
            id: opened_at, // unique-enough id per test, ordering already explicit via opened_at
            wiki: wiki.to_string(),
            lang: lang.to_string(),
            title: title.to_string(),
            opened_at,
            dwell_secs: dwell,
            referrer_wiki: rw,
            referrer_lang: rl,
            referrer_title: rt,
        }
    }

    fn key(wiki: &str, title: &str) -> ArticleKey {
        ArticleKey {
            wiki: wiki.to_string(),
            lang: "en".to_string(),
            title: title.to_string(),
        }
    }

    // ---- build_graph: node dedup + dwell aggregation -----------------------

    #[test]
    fn build_graph_dedups_nodes_and_sums_dwell_across_revisits() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 30, None),
            visit(
                "",
                "en",
                "Enigma machine",
                200,
                20,
                Some(("", "en", "Alan Turing")),
            ),
            visit("", "en", "Alan Turing", 300, 15, None), // revisit
        ];
        let graph = build_graph(&visits);
        assert_eq!(
            graph.nodes.len(),
            2,
            "two distinct articles, not three rows"
        );
        let turing = graph
            .nodes
            .iter()
            .find(|n| n.article == key("", "Alan Turing"))
            .unwrap();
        assert_eq!(turing.total_dwell_secs, 45, "30 + 15 across both visits");
        assert_eq!(turing.visit_count, 2);
        assert_eq!(turing.first_visited_at, 100);
    }

    #[test]
    fn build_graph_edges_come_from_referrers() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 0, None),
            visit(
                "",
                "en",
                "Enigma machine",
                200,
                0,
                Some(("", "en", "Alan Turing")),
            ),
        ];
        let graph = build_graph(&visits);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, key("", "Alan Turing"));
        assert_eq!(graph.edges[0].to, key("", "Enigma machine"));
    }

    #[test]
    fn build_graph_dedups_a_repeated_edge() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 0, None),
            visit(
                "",
                "en",
                "Enigma machine",
                200,
                0,
                Some(("", "en", "Alan Turing")),
            ),
            visit(
                "",
                "en",
                "Enigma machine",
                300,
                0,
                Some(("", "en", "Alan Turing")),
            ), // same edge again
        ];
        let graph = build_graph(&visits);
        assert_eq!(
            graph.edges.len(),
            1,
            "following the same link twice is one edge, not two"
        );
    }

    #[test]
    fn build_graph_wiki_scoped_nodes_stay_distinct_across_wikis() {
        let visits = vec![
            visit("", "en", "Mercury", 100, 10, None),
            visit("wiktionary", "en", "Mercury", 200, 5, None),
        ];
        let graph = build_graph(&visits);
        assert_eq!(
            graph.nodes.len(),
            2,
            "a same-titled article on two wikis is two nodes"
        );
        assert!(graph.nodes.iter().any(|n| n.article.wiki.is_empty()));
        assert!(graph.nodes.iter().any(|n| n.article.wiki == "wiktionary"));
    }

    #[test]
    fn build_graph_on_empty_visits_is_an_empty_graph() {
        let graph = build_graph(&[]);
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
    }

    // ---- tree-ification: roots, children, multi-parent, cycles ------------

    #[test]
    fn tree_roots_are_articles_with_no_referrer() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 0, None),
            visit("", "en", "Ada Lovelace", 150, 0, None), // a second, independent root
            visit(
                "",
                "en",
                "Enigma machine",
                200,
                0,
                Some(("", "en", "Alan Turing")),
            ),
        ];
        let trail = build(&visits);
        assert_eq!(trail.tree.roots.len(), 2);
        assert_eq!(trail.tree.roots[0].article, key("", "Alan Turing"));
        assert_eq!(trail.tree.roots[0].children.len(), 1);
        assert_eq!(
            trail.tree.roots[0].children[0].article,
            key("", "Enigma machine")
        );
        assert_eq!(trail.tree.roots[1].article, key("", "Ada Lovelace"));
    }

    #[test]
    fn a_multi_parent_node_is_placed_under_its_first_referrer_with_an_also_from_note() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 0, None),
            visit("", "en", "Ada Lovelace", 150, 0, None),
            // Bletchley Park is first reached from Alan Turing...
            visit(
                "",
                "en",
                "Bletchley Park",
                200,
                0,
                Some(("", "en", "Alan Turing")),
            ),
            // ...then revisited later, this time from Ada Lovelace.
            visit(
                "",
                "en",
                "Bletchley Park",
                300,
                0,
                Some(("", "en", "Ada Lovelace")),
            ),
        ];
        let trail = build(&visits);
        let turing_root = trail
            .tree
            .roots
            .iter()
            .find(|r| r.article == key("", "Alan Turing"))
            .unwrap();
        assert_eq!(
            turing_root.children.len(),
            1,
            "Bletchley Park is parented under its FIRST referrer only"
        );
        let bletchley = &turing_root.children[0];
        assert_eq!(bletchley.article, key("", "Bletchley Park"));
        assert_eq!(
            bletchley.also_from,
            vec![key("", "Ada Lovelace")],
            "the later, different referrer is noted, not a second parent"
        );
        let lovelace_root = trail
            .tree
            .roots
            .iter()
            .find(|r| r.article == key("", "Ada Lovelace"))
            .unwrap();
        assert!(
            lovelace_root.children.is_empty(),
            "Bletchley Park must not ALSO appear as Ada Lovelace's child"
        );
        // But the full graph keeps both edges — nothing lost, just collapsed
        // in the tree display.
        assert_eq!(trail.graph.edges.len(), 2);
    }

    #[test]
    fn a_revisit_from_the_same_referrer_does_not_duplicate_the_also_from_note() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 0, None),
            visit(
                "",
                "en",
                "Enigma machine",
                200,
                0,
                Some(("", "en", "Alan Turing")),
            ),
            visit(
                "",
                "en",
                "Enigma machine",
                300,
                0,
                Some(("", "en", "Alan Turing")),
            ), // same referrer again
        ];
        let trail = build(&visits);
        let enigma = &trail.tree.roots[0].children[0];
        assert!(
            enigma.also_from.is_empty(),
            "revisiting from the SAME referrer is not a new also-from note"
        );
    }

    #[test]
    fn a_referrer_outside_the_scope_makes_the_node_a_root() {
        // Only Enigma machine is in scope; its referrer (Alan Turing) was
        // visited before the scope's cutoff and never made it into `visits`
        // at all — exactly a session-scoped trail whose first article was
        // reached from something read in an earlier session.
        let visits = vec![visit(
            "",
            "en",
            "Enigma machine",
            200,
            0,
            Some(("", "en", "Alan Turing")),
        )];
        let trail = build(&visits);
        assert_eq!(trail.tree.roots.len(), 1);
        assert_eq!(trail.tree.roots[0].article, key("", "Enigma machine"));
        assert!(trail.tree.roots[0].children.is_empty());
    }

    #[test]
    fn a_referrer_cycle_does_not_infinite_loop_and_keeps_both_nodes() {
        // Hand-built, adversarial data: A's first (only) visit claims a
        // referrer of B, and B's first (only) visit claims a referrer of A —
        // impossible from real navigation timestamps (see the module doc's
        // "Cycle safety"), but nothing stops a corrupted/hand-edited history
        // file from producing it. The tree-ify must terminate and must not
        // silently drop either node.
        let visits = vec![
            visit("", "en", "A", 100, 0, Some(("", "en", "B"))),
            visit("", "en", "B", 200, 0, Some(("", "en", "A"))),
        ];
        let trail = build(&visits); // must return, not hang
        let mut titles: Vec<&str> = trail
            .tree
            .roots
            .iter()
            .map(|r| r.article.title.as_str())
            .collect();
        titles.sort_unstable();
        assert_eq!(
            trail.graph.nodes.len(),
            2,
            "both nodes survive the cycle, none silently dropped"
        );
        // Exactly one of the two becomes a root (whichever the sweep hits
        // first); the other hangs as its child — either shape is fine, the
        // invariant is "both present, exactly once, no infinite loop."
        let flat = flatten(&trail.tree);
        assert_eq!(flat.len(), 2);
    }

    #[test]
    fn a_self_referencing_visit_does_not_parent_a_node_under_itself() {
        let visits = vec![visit("", "en", "A", 100, 0, Some(("", "en", "A")))];
        let trail = build(&visits);
        assert_eq!(trail.tree.roots.len(), 1);
        assert!(trail.tree.roots[0].children.is_empty());
    }

    #[test]
    fn empty_trail_has_no_roots() {
        let trail = build(&[]);
        assert!(trail.tree.roots.is_empty());
    }

    // ---- dag_from_graph: the v2 true-DAG view ------------------------------

    /// The scenario the module doc's "DAG view" section exists for: A→B,
    /// A→C, B→D, C→D — D has two parents. The tree collapses D under
    /// whichever referrer it saw first; the DAG must show BOTH.
    #[test]
    fn dag_shows_a_merge_nodes_full_parent_set_not_just_the_first() {
        let visits = vec![
            visit("", "en", "A", 100, 0, None),
            visit("", "en", "B", 200, 0, Some(("", "en", "A"))),
            visit("", "en", "C", 300, 0, Some(("", "en", "A"))),
            visit("", "en", "D", 400, 0, Some(("", "en", "B"))),
            visit("", "en", "D", 500, 0, Some(("", "en", "C"))), // second parent
        ];
        let graph = build_graph(&visits);
        let dag = dag_from_graph(&graph);
        let d = dag
            .nodes
            .iter()
            .find(|n| n.article == key("", "D"))
            .unwrap();
        assert_eq!(
            d.parents,
            vec![key("", "B"), key("", "C")],
            "D must carry BOTH incoming edges, first-seen order"
        );
        // Distinct from the tree, which keeps only one parent and files the
        // other under "also from" — this is the whole point of the DAG view.
        let tree = tree_from_graph(&graph);
        let a_root = tree
            .roots
            .iter()
            .find(|r| r.article == key("", "A"))
            .unwrap();
        // A -> B -> D and A -> C, with D appearing exactly once in the tree
        // (under B, its first referrer).
        let b = a_root
            .children
            .iter()
            .find(|c| c.article == key("", "B"))
            .unwrap();
        assert_eq!(b.children.len(), 1, "D is the tree's single child of B");
        assert_eq!(b.children[0].article, key("", "D"));
        let c = a_root
            .children
            .iter()
            .find(|c| c.article == key("", "C"))
            .unwrap();
        assert!(
            c.children.is_empty(),
            "the tree must NOT also place D under C"
        );
    }

    /// Every parent row must appear above every one of its children's rows
    /// — the DAG's whole reading-order guarantee (module doc's "DAG view").
    #[test]
    fn dag_is_topologically_ordered_parents_before_children() {
        let visits = vec![
            visit("", "en", "A", 100, 0, None),
            visit("", "en", "B", 200, 0, Some(("", "en", "A"))),
            visit("", "en", "C", 300, 0, Some(("", "en", "A"))),
            visit("", "en", "D", 400, 0, Some(("", "en", "B"))),
            visit("", "en", "D", 500, 0, Some(("", "en", "C"))),
        ];
        let graph = build_graph(&visits);
        let dag = dag_from_graph(&graph);
        let index_of = |title: &str| {
            dag.nodes
                .iter()
                .position(|n| n.article.title == title)
                .unwrap()
        };
        assert!(index_of("A") < index_of("B"));
        assert!(index_of("A") < index_of("C"));
        assert!(index_of("B") < index_of("D"));
        assert!(index_of("C") < index_of("D"));
    }

    /// A node with no in-scope referrer is a DAG root — empty `parents`,
    /// same condition `tree_from_graph` uses to place a tree root.
    #[test]
    fn dag_roots_have_no_parents() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 0, None),
            visit("", "en", "Ada Lovelace", 150, 0, None),
        ];
        let graph = build_graph(&visits);
        let dag = dag_from_graph(&graph);
        assert!(dag.nodes.iter().all(|n| n.parents.is_empty()));
        assert_eq!(dag.nodes.len(), 2);
    }

    /// A referrer outside the scope (module doc's "Tree-ification": e.g. a
    /// session cutoff) can't be a DAG parent either — same rule
    /// `tree_from_graph` applies when deciding root vs. child.
    #[test]
    fn dag_treats_an_out_of_scope_referrer_as_no_parent() {
        let visits = vec![visit(
            "",
            "en",
            "Enigma machine",
            200,
            0,
            Some(("", "en", "Alan Turing")), // never itself visited in scope
        )];
        let graph = build_graph(&visits);
        let dag = dag_from_graph(&graph);
        assert_eq!(dag.nodes.len(), 1);
        assert!(dag.nodes[0].parents.is_empty());
    }

    /// A self-referencing visit must not parent a node under itself in the
    /// DAG either — mirrors `a_self_referencing_visit_does_not_parent_a_node_
    /// under_itself`'s tree-side assertion.
    #[test]
    fn dag_self_reference_does_not_parent_a_node_under_itself() {
        let visits = vec![visit("", "en", "A", 100, 0, Some(("", "en", "A")))];
        let graph = build_graph(&visits);
        let dag = dag_from_graph(&graph);
        assert_eq!(dag.nodes.len(), 1);
        assert!(dag.nodes[0].parents.is_empty());
    }

    /// Adversarial, hand-built cyclic data (impossible from real navigation
    /// timestamps — module doc's "Cycle safety") must not hang
    /// `dag_from_graph` and must not silently drop a node.
    #[test]
    fn dag_from_graph_does_not_infinite_loop_on_a_referrer_cycle() {
        let visits = vec![
            visit("", "en", "A", 100, 0, Some(("", "en", "B"))),
            visit("", "en", "B", 200, 0, Some(("", "en", "A"))),
        ];
        let graph = build_graph(&visits); // must return, not hang
        let dag = dag_from_graph(&graph); // must return, not hang
        assert_eq!(dag.nodes.len(), 2, "both nodes survive the cycle");
        let mut titles: Vec<&str> = dag.nodes.iter().map(|n| n.article.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, vec!["A", "B"]);
    }

    #[test]
    fn dag_from_graph_on_an_empty_graph_is_empty() {
        let dag = dag_from_graph(&TrailGraph::default());
        assert!(dag.nodes.is_empty());
    }

    #[test]
    fn dag_connector_glyphs_distinguish_root_single_parent_and_merge() {
        let root = DagNode {
            article: key("", "A"),
            total_dwell_secs: 0,
            visit_count: 1,
            parents: Vec::new(),
        };
        let single = DagNode {
            article: key("", "B"),
            total_dwell_secs: 0,
            visit_count: 1,
            parents: vec![key("", "A")],
        };
        let merge = DagNode {
            article: key("", "D"),
            total_dwell_secs: 0,
            visit_count: 1,
            parents: vec![key("", "B"), key("", "C")],
        };
        assert_eq!(dag_connector(&root), "");
        assert!(dag_connector(&single).contains('\u{2514}'));
        assert!(dag_connector(&merge).contains('\u{2534}'));
        assert_ne!(dag_connector(&single), dag_connector(&merge));
    }

    #[test]
    fn trail_layout_defaults_to_tree() {
        assert_eq!(TrailLayout::default(), TrailLayout::Tree);
    }

    // ---- flatten: ordering + connector info --------------------------------

    #[test]
    fn flatten_orders_depth_first_and_reports_correct_depths() {
        let visits = vec![
            visit("", "en", "Alan Turing", 100, 0, None),
            visit(
                "",
                "en",
                "Enigma machine",
                200,
                0,
                Some(("", "en", "Alan Turing")),
            ),
            visit(
                "",
                "en",
                "Bletchley Park",
                300,
                0,
                Some(("", "en", "Enigma machine")),
            ),
            visit("", "en", "Ada Lovelace", 400, 0, None),
        ];
        let trail = build(&visits);
        let lines = flatten(&trail.tree);
        let titles: Vec<&str> = lines.iter().map(|l| l.article.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Alan Turing",
                "Enigma machine",
                "Bletchley Park",
                "Ada Lovelace"
            ]
        );
        assert_eq!(lines[0].depth, 0);
        assert_eq!(lines[1].depth, 1);
        assert_eq!(lines[2].depth, 2);
        assert_eq!(lines[3].depth, 0);
    }

    #[test]
    fn connector_is_empty_at_depth_zero_and_marks_last_child_distinctly() {
        let visits = vec![
            visit("", "en", "Root", 100, 0, None),
            visit("", "en", "First", 200, 0, Some(("", "en", "Root"))),
            visit("", "en", "Last", 300, 0, Some(("", "en", "Root"))),
        ];
        let trail = build(&visits);
        let lines = flatten(&trail.tree);
        assert_eq!(connector(&lines[0]), "");
        let first = connector(&lines[1]);
        let last = connector(&lines[2]);
        assert!(first.contains('\u{251c}'), "mid sibling uses ├─: {first}");
        assert!(last.contains('\u{2514}'), "last sibling uses └─: {last}");
        assert_ne!(first, last);
    }

    // ---- flatten -> selection mapping (what the UI needs) ------------------

    #[test]
    fn flatten_gives_a_stable_index_for_each_line_selection_can_map_to() {
        let visits = vec![
            visit("", "en", "A", 100, 0, None),
            visit("", "en", "B", 200, 0, Some(("", "en", "A"))),
        ];
        let trail = build(&visits);
        let lines = flatten(&trail.tree);
        assert_eq!(lines.len(), 2);
        // Selecting index 1 must resolve to B, exactly what a `j` press then
        // Enter needs.
        assert_eq!(lines[1].article, key("", "B"));
    }

    // ---- dwell_bar ----------------------------------------------------------

    #[test]
    fn dwell_bar_scales_to_the_peak_and_is_empty_for_zero() {
        assert_eq!(dwell_bar(0, 100, 10), "");
        assert_eq!(
            dwell_bar(100, 0, 10),
            "",
            "no peak means nothing to scale to"
        );
        assert_eq!(dwell_bar(50, 100, 10).chars().count(), 5);
        assert_eq!(dwell_bar(100, 100, 10).chars().count(), 10);
    }

    // ---- stats (FR-DL-8 seam) ------------------------------------------------

    #[test]
    fn stats_reports_article_count_depth_and_longest_chain() {
        let visits = vec![
            visit("", "en", "A", 100, 0, None),
            visit("", "en", "B", 200, 0, Some(("", "en", "A"))),
            visit("", "en", "C", 300, 0, Some(("", "en", "B"))),
        ];
        let trail = build(&visits);
        let s = stats(&trail);
        assert_eq!(s.article_count, 3);
        assert_eq!(s.max_depth, 2, "A -> B -> C is two edges deep");
        assert_eq!(s.longest_chain, 3, "three nodes on the deepest path");
    }

    #[test]
    fn stats_on_an_empty_trail_is_all_zero() {
        let trail = build(&[]);
        assert_eq!(stats(&trail), TrailStats::default());
    }
}
