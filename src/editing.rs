//! PRD FR-ACC-8: gated typo-fix editing — the product's *only* write-to-an-
//! article path, and a v2 decision gate. Everything here is pure logic (no
//! I/O): the network round trips live in `api.rs`, the terminal/$EDITOR
//! orchestration in `main.rs`. This module is where the guardrails — which
//! *are* the feature — are made structural and testable.
//!
//! ## The hard prohibitions (PRD §3.3 / FR-ACC-8), enforced by construction
//!
//! FR-ACC-8 forbids reverts, rollbacks, page moves, uploads, talk-page
//! writing, and template editing "ever". Those operations are enforced by
//! *absence*: there is no `action=move`/`action=upload`/`action=rollback`
//! endpoint anywhere in this codebase, and no code path that reaches one. The
//! single write this module supports is a whitespace-scoped text splice into
//! the wikitext of a **main-namespace** article, gated by
//! [`is_main_namespace`] — a `Talk:`/`Template:`/`User:`/… title is refused
//! before any request is made (see [`namespace_prefix`]).
//!
//! ## The double opt-in gate
//!
//! Editing is refused unless BOTH are true: the config opt-in
//! (`editing_enabled`, default `false`) AND the OAuth `editpage` grant is
//! present on the current session (a *separate, explicit* re-auth — the
//! ordinary login deliberately does not request it, so a normal session stays
//! read-only). [`edit_gate`] is the single predicate; there is no other way
//! to reach a save.
//!
//! ## Locating the sentence in the source, and aborting on uncertainty
//!
//! The reader selects a *rendered* sentence (from the document model), but an
//! edit must touch the *wikitext* — a fuzzy mapping ([`locate_sentence`]),
//! because `[[Target|text]]` renders as `text`, `''emphasis''` as `emphasis`,
//! `<ref>…</ref>` as nothing, and wikitext wraps whitespace differently than
//! the rendered column. The matcher builds a *stripped* view of the wikitext
//! (markup reduced to what it renders, whitespace collapsed) with a byte-offset
//! map back to the raw source, then requires the sentence to appear **exactly
//! once**. Zero matches → [`Located::NotFound`]; two or more → [`Located::
//! Ambiguous`]; either way the caller ABORTS rather than risk editing the
//! wrong span. The raw range a unique match maps to always covers whole markup
//! tokens (a `[[…]]` is never split), so the [`splice`] is byte-preserving
//! everywhere outside the located sentence.

use serde::Deserialize;

use crate::doc::{Block, Document, SpanStyle};

// ---------------------------------------------------------------------------
// Selecting the sentence to edit (from the document model)
// ---------------------------------------------------------------------------

/// The plain text of the block that contains the link at `target` (an index
/// into `doc::collect_links`'s ordering) — the paragraph/list-item/blockquote
/// the reader's focused link lives in, from which the focused *sentence* is
/// extracted (`App::focused_sentence`). Walks blocks and counts links in the
/// **exact** order `doc::collect_links` does (paragraphs and blockquotes, then
/// list items; `Link`/`RedLink` spans only) so a focused-link index maps to
/// the same block the reading view highlights — that ordering is a
/// cross-module invariant. `None` when the index is past the last link.
pub fn block_text_containing_link(doc: &Document, target: usize) -> Option<String> {
    let mut idx = 0usize;
    for block in &doc.blocks {
        let spans = match block {
            Block::Paragraph(spans) | Block::Blockquote(spans) => spans,
            Block::ListItem { spans, .. } => spans,
            _ => continue,
        };
        let n_links = spans
            .iter()
            .filter(|s| matches!(s.style, SpanStyle::Link(_) | SpanStyle::RedLink(_)))
            .count();
        if target < idx + n_links {
            return Some(spans.iter().map(|s| s.text.as_str()).collect());
        }
        idx += n_links;
    }
    None
}

// ---------------------------------------------------------------------------
// Namespace gate — main-namespace-only (FR-ACC-8's hard prohibition)
// ---------------------------------------------------------------------------

/// The non-main namespace prefixes (and their `… talk:` counterparts) an edit
/// is refused on. Not exhaustive of every MediaWiki namespace — it is the
/// deny-listed set the PRD names plus the standard talk/meta namespaces — but
/// the check is deliberately *conservative*: [`is_main_namespace`] treats
/// *any* `Prefix:` it recognizes here as non-main, and the flow only ever
/// permits a title with no recognized prefix. A title carrying an
/// unrecognized `Foo:` prefix is still treated as main-namespace (it usually
/// is — "Ada Lovelace: A Life" is a real article), so the risk this guard
/// exists to stop (writing to Talk/Template/User/… space) is fully covered
/// while ordinary colon-bearing article titles keep working.
const NON_MAIN_PREFIXES: &[&str] = &[
    "talk",
    "user",
    "user talk",
    "wikipedia",
    "wikipedia talk",
    "file",
    "file talk",
    "mediawiki",
    "mediawiki talk",
    "template",
    "template talk",
    "help",
    "help talk",
    "category",
    "category talk",
    "portal",
    "portal talk",
    "draft",
    "draft talk",
    "module",
    "module talk",
    "special",
    "media",
    "timedtext",
    "timedtext talk",
    "book",
    "book talk",
];

/// The recognized non-main namespace prefix of `title` (lowercased, without
/// the trailing colon), or `None` when the title is in (or looks like) the
/// main namespace. Underscores in the prefix are normalized to spaces the
/// same way MediaWiki treats them, so `Template_talk:X` is recognized too.
pub fn namespace_prefix(title: &str) -> Option<&'static str> {
    let (prefix, _rest) = title.split_once(':')?;
    let normalized = prefix.trim().replace('_', " ").to_ascii_lowercase();
    NON_MAIN_PREFIXES.iter().copied().find(|p| *p == normalized)
}

/// PRD FR-ACC-8: whether `title` is a main-namespace article — the only
/// namespace an edit is ever permitted on. A recognized `Talk:`/`Template:`/…
/// prefix ([`namespace_prefix`]) makes this `false`.
pub fn is_main_namespace(title: &str) -> bool {
    namespace_prefix(title).is_none()
}

// ---------------------------------------------------------------------------
// The double opt-in gate
// ---------------------------------------------------------------------------

/// The result of the editing gate (PRD FR-ACC-8's double opt-in). Only
/// [`EditGate::Ready`] permits a save; every other variant is a refusal with
/// its own honest message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditGate {
    /// Config opt-in on, logged in, `editpage` grant present.
    Ready,
    /// The `editing_enabled` config opt-in is off (`:enable-editing` to turn
    /// it on) — checked first, so a reader who has not opted into editing at
    /// all is never told to log in for a feature they have not enabled.
    EditingDisabled,
    /// Opted into editing, but logged out — editing is a write, and writes
    /// need a session.
    NotLoggedIn,
    /// Opted in and logged in, but this session lacks the `editpage` grant
    /// (the ordinary read-only login never requests it) — a re-auth via
    /// `:enable-editing` is required.
    GrantMissing,
}

impl EditGate {
    pub fn is_ready(self) -> bool {
        self == EditGate::Ready
    }

    /// The refusal message shown to the reader (empty for [`EditGate::Ready`],
    /// which does not refuse).
    pub fn message(self) -> &'static str {
        match self {
            EditGate::Ready => "",
            EditGate::EditingDisabled => "editing is disabled — :enable-editing to opt in",
            EditGate::NotLoggedIn => "editing is disabled — log in and :enable-editing to opt in",
            EditGate::GrantMissing => {
                "editing needs the editpage grant — :enable-editing to re-authorize"
            }
        }
    }
}

/// PRD FR-ACC-8's double opt-in gate: editing is permitted only when the
/// config `editing_enabled` opt-in is on AND the reader is logged in with the
/// `editpage` grant. The order of the checks fixes which refusal a reader
/// sees: the config opt-in is the outermost gate (a reader who never enabled
/// editing is told exactly that, not to go log in).
pub fn edit_gate(editing_enabled: bool, logged_in: bool, has_editpage: bool) -> EditGate {
    if !editing_enabled {
        EditGate::EditingDisabled
    } else if !logged_in {
        EditGate::NotLoggedIn
    } else if !has_editpage {
        EditGate::GrantMissing
    } else {
        EditGate::Ready
    }
}

// ---------------------------------------------------------------------------
// Sentence extraction (from rendered document text)
// ---------------------------------------------------------------------------

/// Splits rendered paragraph text into sentences on `.`/`!`/`?` followed by
/// whitespace (or end of text), keeping the terminator with its sentence and
/// trimming surrounding space. Deliberately simple (an abbreviation like
/// "Dr. Smith" over-splits) — the safety net is downstream: a fragment that
/// can't be located *uniquely* in the wikitext aborts the edit rather than
/// touching the wrong span, so over-splitting is at worst a refused edit,
/// never a wrong one.
pub fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?') {
            let ends_here = chars.get(i + 1).is_none_or(|n| n.is_whitespace());
            if ends_here {
                let s = cur.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                cur.clear();
            }
        }
    }
    let s = cur.trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

/// The first sentence in `sentences` that contains `needle` (typically the
/// anchor text of the focused link) — how the reader's cursor picks *which*
/// sentence to edit. `None` when no sentence contains it (e.g. the anchor
/// straddled a naive sentence split), which the caller reports rather than
/// guessing.
pub fn sentence_containing<'a>(sentences: &'a [String], needle: &str) -> Option<&'a String> {
    let needle = needle.trim();
    if needle.is_empty() {
        return None;
    }
    sentences.iter().find(|s| s.contains(needle))
}

// ---------------------------------------------------------------------------
// Locating a rendered sentence in the raw wikitext (fuzzy, abort-on-doubt)
// ---------------------------------------------------------------------------

/// The outcome of [`locate_sentence`]. Only [`Located::Found`] permits an
/// edit; the other two both mean "ABORT — don't risk the wrong span".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Located {
    /// The sentence maps to exactly one raw byte range `[start, end)` in the
    /// wikitext. The range always covers whole markup tokens.
    Found { start: usize, end: usize },
    /// The sentence could not be found in the (stripped) wikitext at all.
    NotFound,
    /// The sentence appears more than once — editing either would be a guess.
    Ambiguous,
}

/// A minimum matchable length (in stripped characters). A sentence shorter
/// than this is too likely to occur incidentally more than once — refusing it
/// (`NotFound`) is safer than risking a mislocation.
const MIN_MATCH_CHARS: usize = 12;

/// One stripped character plus the raw byte range of the *source token* it
/// came from. For a literal character that is the character's own bytes; for a
/// character that is part of a markup token (`[[…]]`, a collapsed whitespace
/// run) it is the token's whole span, so a match that touches any character of
/// a token pulls in the entire token — [`splice`] then never splits markup.
struct Mapped {
    ch: char,
    raw_start: usize,
    raw_end: usize,
}

/// Builds the stripped, offset-mapped view of `wt` (see the module doc). Wiki
/// markup is reduced to what it renders: `[[Target|text]]`→`text`,
/// `[[Target]]`→`Target` (with `_`→space), `''`/`'''` emphasis markers
/// dropped, `<ref …>…</ref>`/`<ref …/>` dropped entirely, and every run of
/// ASCII whitespace collapsed to a single space.
fn stripped_view(wt: &str) -> Vec<Mapped> {
    let bytes = wt.as_bytes();
    let n = bytes.len();
    let mut out: Vec<Mapped> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let b = bytes[i];
        // Collapse a run of ASCII whitespace to one mapped space.
        if b.is_ascii_whitespace() {
            let start = i;
            while i < n && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            out.push(Mapped {
                ch: ' ',
                raw_start: start,
                raw_end: i,
            });
            continue;
        }
        // `<ref …>…</ref>` or `<ref …/>` — drop entirely.
        if wt[i..].starts_with("<ref")
            && let Some(end) = ref_token_end(wt, i)
        {
            i = end;
            continue;
        }
        // `[[Target|text]]` / `[[Target]]` internal link — emit the display text.
        if wt[i..].starts_with("[[")
            && let Some(close) = wt[i..].find("]]")
        {
            let token_start = i;
            let token_end = i + close + 2;
            let inner = &wt[i + 2..i + close];
            let display = link_display(inner);
            for ch in display.chars() {
                out.push(Mapped {
                    ch,
                    raw_start: token_start,
                    raw_end: token_end,
                });
            }
            i = token_end;
            continue;
        }
        // `''` / `'''` emphasis markers — drop the quotes (a lone apostrophe,
        // as in "it's", is kept as a literal).
        if b == b'\'' {
            let start = i;
            let mut count = 0;
            while i < n && bytes[i] == b'\'' {
                count += 1;
                i += 1;
            }
            if count >= 2 {
                continue; // markup: emit nothing
            }
            // A single apostrophe is real text.
            out.push(Mapped {
                ch: '\'',
                raw_start: start,
                raw_end: start + 1,
            });
            continue;
        }
        // Ordinary character.
        let ch = wt[i..].chars().next().unwrap();
        let len = ch.len_utf8();
        out.push(Mapped {
            ch,
            raw_start: i,
            raw_end: i + len,
        });
        i += len;
    }
    out
}

/// The rendered display text of an internal-link body (`inner` is what sits
/// between `[[` and `]]`): the part after the last `|` for a piped link, else
/// the target itself with `_`→space (how a bare `[[computer_science]]` shows).
fn link_display(inner: &str) -> String {
    match inner.rsplit_once('|') {
        Some((_, text)) => text.to_string(),
        None => inner.replace('_', " "),
    }
}

/// The byte offset just past a `<ref …>…</ref>` or self-closing `<ref …/>`
/// starting at `start`, or `None` if it is unterminated (in which case the
/// scanner falls back to treating `<` as a literal, never hanging).
fn ref_token_end(wt: &str, start: usize) -> Option<usize> {
    let rest = &wt[start..];
    // Self-closing `<ref .../>` before any `>`.
    if let Some(gt) = rest.find('>') {
        if rest[..gt].ends_with('/') {
            return Some(start + gt + 1);
        }
        // Paired: find the matching close tag.
        if let Some(close) = rest.find("</ref>") {
            return Some(start + close + "</ref>".len());
        }
    }
    None
}

/// Normalizes a rendered sentence for matching against the stripped wikitext
/// view: strips `[n]` reference-marker superscripts (the document keeps them
/// as text, the wikitext's `<ref>` produced them and was dropped), collapses
/// whitespace to single spaces, and trims.
fn normalize_sentence(sentence: &str) -> Vec<char> {
    let mut out = String::with_capacity(sentence.len());
    let chars: Vec<char> = sentence.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Drop a `[123]`-style reference marker.
        if c == '[' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && chars.get(j) == Some(&']') {
                i = j + 1;
                continue;
            }
        }
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.trim().chars().collect()
}

/// PRD FR-ACC-8: locates the raw wikitext byte range of a rendered
/// `sentence`, aborting on any uncertainty (see [`Located`] / the module doc).
/// A match must be unique; the returned range covers whole markup tokens so
/// the subsequent [`splice`] is byte-preserving outside the sentence.
pub fn locate_sentence(wikitext: &str, sentence: &str) -> Located {
    let needle = normalize_sentence(sentence);
    if needle.len() < MIN_MATCH_CHARS {
        return Located::NotFound;
    }
    let hay = stripped_view(wikitext);
    let hay_chars: Vec<char> = hay.iter().map(|m| m.ch).collect();

    let mut found: Option<(usize, usize)> = None;
    let mut count = 0usize;
    // Slide the needle over the stripped haystack.
    if hay_chars.len() >= needle.len() {
        for start in 0..=hay_chars.len() - needle.len() {
            if hay_chars[start..start + needle.len()] == needle[..] {
                count += 1;
                if count == 1 {
                    let raw_start = hay[start].raw_start;
                    let raw_end = hay[start + needle.len() - 1].raw_end;
                    found = Some((raw_start, raw_end));
                }
                if count > 1 {
                    return Located::Ambiguous;
                }
            }
        }
    }
    match found {
        Some((start, end)) => Located::Found { start, end },
        None => Located::NotFound,
    }
}

// ---------------------------------------------------------------------------
// The byte-preserving splice
// ---------------------------------------------------------------------------

/// Replaces the raw byte range `[start, end)` of `wikitext` with
/// `replacement`, byte-preserving everything outside it. `start`/`end` come
/// from [`Located::Found`] (always on char boundaries and within bounds); an
/// out-of-range or non-boundary range returns the input unchanged rather than
/// panicking — the caller treats an unchanged result as "nothing spliced".
pub fn splice(wikitext: &str, start: usize, end: usize, replacement: &str) -> String {
    if start > end || end > wikitext.len() {
        return wikitext.to_string();
    }
    if !wikitext.is_char_boundary(start) || !wikitext.is_char_boundary(end) {
        return wikitext.to_string();
    }
    let mut out = String::with_capacity(wikitext.len() - (end - start) + replacement.len());
    out.push_str(&wikitext[..start]);
    out.push_str(replacement);
    out.push_str(&wikitext[end..]);
    out
}

// ---------------------------------------------------------------------------
// Diff preview + change detection
// ---------------------------------------------------------------------------

/// A before/after pair for the diff-preview overlay (PRD FR-ACC-8's "diff
/// preview … before/after"). Kept intentionally small — the overlay renders
/// the two strings; there is no word-level diff algorithm to get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentenceDiff {
    pub before: String,
    pub after: String,
}

impl SentenceDiff {
    pub fn new(before: &str, after: &str) -> Self {
        Self {
            before: before.to_string(),
            after: after.to_string(),
        }
    }

    /// Whether the edit actually changed the text (ignoring pure trailing-
    /// whitespace churn every editor adds on save). A no-op edit must never
    /// reach the save path.
    pub fn is_change(&self) -> bool {
        self.before.trim_end() != self.after.trim_end()
    }
}

// ---------------------------------------------------------------------------
// Edit summary
// ---------------------------------------------------------------------------

/// PRD FR-ACC-8: the automatic edit-summary prefix, always present on every
/// save so an edit is honestly attributed to this client in page history.
pub const SUMMARY_PREFIX: &str = "Typo fix via wikitui";

/// Builds the edit summary: the mandatory [`SUMMARY_PREFIX`] plus an optional
/// user note appended after a colon.
pub fn build_summary(user_note: Option<&str>) -> String {
    match user_note.map(str::trim).filter(|s| !s.is_empty()) {
        Some(note) => format!("{SUMMARY_PREFIX}: {note}"),
        None => SUMMARY_PREFIX.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Wikitext fetch response (`prop=revisions&rvprop=content|ids|timestamp`)
// ---------------------------------------------------------------------------

/// The current wikitext of an article plus the revision identity an edit needs
/// for conflict detection (PRD FR-ACC-8): `baserevid` and `basetimestamp` are
/// sent back on save so the API rejects the edit if the page changed underneath
/// us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedWikitext {
    pub text: String,
    pub revid: u64,
    pub timestamp: String,
}

#[derive(Debug, Deserialize)]
struct RevisionsResponse {
    #[serde(default)]
    query: Option<RevisionsQuery>,
}

#[derive(Debug, Deserialize)]
struct RevisionsQuery {
    #[serde(default)]
    pages: Vec<RevisionsPage>,
}

#[derive(Debug, Deserialize)]
struct RevisionsPage {
    #[serde(default)]
    missing: bool,
    #[serde(default)]
    revisions: Vec<Revision>,
}

#[derive(Debug, Deserialize)]
struct Revision {
    #[serde(default)]
    revid: u64,
    #[serde(default)]
    timestamp: String,
    /// `rvslots=main` nests the content under `slots.main.content` (the modern
    /// shape); a flat `content` field is the legacy fallback.
    #[serde(default)]
    slots: Option<RevisionSlots>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RevisionSlots {
    #[serde(default)]
    main: Option<RevisionSlot>,
}

#[derive(Debug, Deserialize)]
struct RevisionSlot {
    #[serde(default)]
    content: String,
}

/// Parses `action=query&prop=revisions&rvprop=content|ids|timestamp` into the
/// current wikitext + revid + timestamp. `None` for a missing page or any
/// shape without a usable revision — the caller treats that as "couldn't fetch
/// the source", never as an empty article to overwrite.
pub fn parse_wikitext_response(body: &[u8]) -> Option<FetchedWikitext> {
    let parsed: RevisionsResponse = serde_json::from_slice(body).ok()?;
    let page = parsed.query?.pages.into_iter().next()?;
    if page.missing {
        return None;
    }
    let rev = page.revisions.into_iter().next()?;
    let text = rev
        .slots
        .and_then(|s| s.main)
        .map(|m| m.content)
        .or(rev.content)?;
    Some(FetchedWikitext {
        text,
        revid: rev.revid,
        timestamp: rev.timestamp,
    })
}

// ---------------------------------------------------------------------------
// Edit response (`action=edit`) — success vs. edit-conflict vs. failure
// ---------------------------------------------------------------------------

/// The outcome of an `action=edit` save, read back off its own response (never
/// inferred from what was sent). [`EditOutcome::Conflict`] is the base-revid
/// conflict-detection signal (PRD FR-ACC-8): the page changed since the
/// wikitext was fetched, so the edit is *not* forced — the reader re-fetches
/// and retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    Success { newrevid: u64 },
    Conflict,
    Failure(String),
}

#[derive(Debug, Deserialize)]
struct EditResponse {
    #[serde(default)]
    edit: Option<EditResult>,
    #[serde(default)]
    error: Option<EditError>,
}

#[derive(Debug, Deserialize)]
struct EditResult {
    #[serde(default)]
    result: String,
    #[serde(default)]
    newrevid: u64,
}

#[derive(Debug, Deserialize)]
struct EditError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    info: String,
}

/// The MediaWiki error code an `action=edit` returns when the base revision is
/// stale (PRD FR-ACC-8's conflict detection).
pub const EDIT_CONFLICT_CODE: &str = "editconflict";

/// Parses `action=edit`'s response into [`EditOutcome`]. A `badtoken` error is
/// left for `account::is_badtoken_response` (the shared retry predicate) to
/// classify — this reports it as a [`EditOutcome::Failure`] so a caller that
/// skips the badtoken check still surfaces *something* rather than a false
/// success.
pub fn parse_edit_response(body: &[u8]) -> EditOutcome {
    let parsed: EditResponse = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => return EditOutcome::Failure(format!("unparseable edit response: {e}")),
    };
    if let Some(err) = parsed.error {
        if err.code == EDIT_CONFLICT_CODE {
            return EditOutcome::Conflict;
        }
        let detail = if err.info.is_empty() {
            err.code
        } else {
            format!("{}: {}", err.code, err.info)
        };
        return EditOutcome::Failure(detail);
    }
    match parsed.edit {
        Some(e) if e.result.eq_ignore_ascii_case("success") => EditOutcome::Success {
            newrevid: e.newrevid,
        },
        Some(e) => EditOutcome::Failure(format!("edit result: {}", e.result)),
        None => EditOutcome::Failure("edit response had neither 'edit' nor 'error'".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Block, Document, Span, SpanStyle};

    fn span(text: &str, style: SpanStyle) -> Span {
        Span {
            text: text.to_string(),
            style,
        }
    }

    // ---- block/sentence selection ----------------------------------------

    #[test]
    fn block_text_containing_link_maps_index_to_the_right_paragraph() {
        let doc = Document {
            title: "T".into(),
            blocks: vec![
                Block::Paragraph(vec![
                    span("Lead with a ", SpanStyle::Plain),
                    span("first link", SpanStyle::Link("./A".into())),
                    span(" here.", SpanStyle::Plain),
                ]),
                Block::Paragraph(vec![
                    span("Body mentions ", SpanStyle::Plain),
                    span("second link", SpanStyle::Link("./B".into())),
                    span(" too.", SpanStyle::Plain),
                ]),
            ],
            citations: vec![],
            truncated: false,
        };
        // Link index 0 → first paragraph; index 1 → second paragraph.
        assert_eq!(
            block_text_containing_link(&doc, 0).unwrap(),
            "Lead with a first link here."
        );
        assert_eq!(
            block_text_containing_link(&doc, 1).unwrap(),
            "Body mentions second link too."
        );
        assert!(block_text_containing_link(&doc, 2).is_none());
    }

    // ---- namespace gate ---------------------------------------------------

    #[test]
    fn main_namespace_articles_are_editable() {
        assert!(is_main_namespace("Alan Turing"));
        assert!(is_main_namespace("Computer science"));
        // An ordinary colon-bearing article title is still main-namespace.
        assert!(is_main_namespace("Ada Lovelace: A Life"));
        assert!(namespace_prefix("Alan Turing").is_none());
    }

    #[test]
    fn talk_template_user_and_friends_are_blocked() {
        for t in [
            "Talk:Alan Turing",
            "Template:Infobox",
            "User:Someone",
            "User talk:Someone",
            "Wikipedia:Sandbox",
            "File:Sample.jpg",
            "Category:Physics",
            "Help:Contents",
            "Module:Citation",
            "Portal:Science",
            "Draft:New Article",
            "MediaWiki:Common.css",
            "Special:Random",
        ] {
            assert!(!is_main_namespace(t), "{t} must be blocked");
            assert!(namespace_prefix(t).is_some(), "{t}");
        }
    }

    #[test]
    fn namespace_prefix_is_case_and_underscore_insensitive() {
        assert!(!is_main_namespace("talk:Alan Turing"));
        assert!(!is_main_namespace("TEMPLATE:Infobox"));
        assert!(!is_main_namespace("Template_talk:Infobox"));
    }

    // ---- double opt-in gate ----------------------------------------------

    #[test]
    fn edit_gate_requires_both_config_optin_and_grant() {
        // Disabled by config → refused first, even fully logged in with grant.
        assert_eq!(edit_gate(false, true, true), EditGate::EditingDisabled);
        // Enabled but logged out → refused.
        assert_eq!(edit_gate(true, false, false), EditGate::NotLoggedIn);
        // Enabled, logged in, but no editpage grant → refused.
        assert_eq!(edit_gate(true, true, false), EditGate::GrantMissing);
        // Both halves satisfied → ready.
        assert_eq!(edit_gate(true, true, true), EditGate::Ready);
        assert!(edit_gate(true, true, true).is_ready());
        assert!(!edit_gate(false, true, true).is_ready());
    }

    #[test]
    fn disabled_gate_message_names_the_optin_command() {
        assert!(
            EditGate::EditingDisabled
                .message()
                .contains(":enable-editing")
        );
        assert!(EditGate::GrantMissing.message().contains("editpage"));
    }

    // ---- sentence extraction ---------------------------------------------

    #[test]
    fn split_sentences_keeps_terminators_and_trims() {
        let s = split_sentences("First sentence. Second one! A third? Trailing");
        assert_eq!(
            s,
            vec!["First sentence.", "Second one!", "A third?", "Trailing"]
        );
    }

    #[test]
    fn sentence_containing_finds_the_anchor_sentence() {
        let sentences = split_sentences(
            "He founded theoretical computer science. He also broke Enigma. The end.",
        );
        let got = sentence_containing(&sentences, "Enigma").unwrap();
        assert_eq!(got, "He also broke Enigma.");
        assert!(sentence_containing(&sentences, "nowhere").is_none());
    }

    // ---- locate + splice --------------------------------------------------

    #[test]
    fn locate_finds_a_unique_plain_sentence() {
        let wt = "Intro line.\n\nAlan Turing was a briliant mathematician. He led Hut 8.";
        let located = locate_sentence(wt, "Alan Turing was a briliant mathematician.");
        let (start, end) = match located {
            Located::Found { start, end } => (start, end),
            other => panic!("expected Found, got {other:?}"),
        };
        assert_eq!(&wt[start..end], "Alan Turing was a briliant mathematician.");
    }

    #[test]
    fn locate_maps_across_a_link_and_preserves_the_markup_on_splice() {
        // The rendered sentence has "computer science" (a piped link's display
        // text); the wikitext has a `[[Target|text]]` link there. The located
        // raw range must include the whole `[[…]]`, and splicing an edited
        // version back must keep the link markup byte-for-byte outside the fix.
        let wt =
            "He founded theoretical [[Computer science|computer science]], a briliant field. Next.";
        let sentence = "He founded theoretical computer science, a briliant field.";
        let located = locate_sentence(wt, sentence);
        let (start, end) = match located {
            Located::Found { start, end } => (start, end),
            other => panic!("expected Found, got {other:?}"),
        };
        let raw = &wt[start..end];
        assert!(
            raw.contains("[[Computer science|computer science]]"),
            "raw span: {raw:?}"
        );
        // Fix the typo in the raw span, keeping the link.
        let fixed = raw.replace("briliant", "brilliant");
        let spliced = splice(wt, start, end, &fixed);
        assert_eq!(
            spliced,
            "He founded theoretical [[Computer science|computer science]], a brilliant field. Next."
        );
        // Everything outside the sentence is byte-identical.
        assert!(
            spliced.starts_with("He founded theoretical [[Computer science|computer science]],")
        );
        assert!(spliced.ends_with(" Next."));
    }

    #[test]
    fn locate_collapses_whitespace_across_newlines() {
        let wt = "Alan Turing was\n   a briliant   mathematician here.";
        let located = locate_sentence(wt, "Alan Turing was a briliant mathematician here.");
        assert!(matches!(located, Located::Found { .. }));
    }

    #[test]
    fn locate_aborts_when_not_found() {
        let wt = "Completely unrelated wikitext about something else entirely.";
        assert_eq!(
            locate_sentence(wt, "Alan Turing was a briliant mathematician."),
            Located::NotFound
        );
    }

    #[test]
    fn locate_aborts_when_ambiguous() {
        // Two byte-identical occurrences (same case) — editing either is a guess.
        let wt = "Repeated clause appears twice here. Repeated clause appears twice here.";
        assert_eq!(
            locate_sentence(wt, "Repeated clause appears twice here."),
            Located::Ambiguous
        );
    }

    #[test]
    fn locate_rejects_a_too_short_fragment() {
        assert_eq!(
            locate_sentence("a b c d e f g h.", "Short."),
            Located::NotFound
        );
    }

    #[test]
    fn locate_drops_reference_markers_on_the_rendered_side() {
        // Wikitext has a `<ref>`; the rendered sentence shows "[1]" instead.
        let wt = "Turing was born in London<ref>Hodges 2012</ref> in the spring here.";
        let sentence = "Turing was born in London[1] in the spring here.";
        let located = locate_sentence(wt, sentence);
        let (start, end) = match located {
            Located::Found { start, end } => (start, end),
            other => panic!("expected Found, got {other:?}"),
        };
        // The located raw span still contains the ref markup (preserved on splice).
        assert!(wt[start..end].contains("<ref>Hodges 2012</ref>"));
    }

    #[test]
    fn splice_is_byte_preserving_and_bounds_checked() {
        let wt = "abc DEF ghi";
        assert_eq!(splice(wt, 4, 7, "XYZ"), "abc XYZ ghi");
        // Out-of-range / crossed range → unchanged.
        assert_eq!(splice(wt, 4, 100, "X"), wt);
        assert_eq!(splice(wt, 7, 4, "X"), wt);
    }

    // ---- diff / change detection -----------------------------------------

    #[test]
    fn diff_detects_a_real_change_and_ignores_trailing_whitespace() {
        assert!(SentenceDiff::new("briliant", "brilliant").is_change());
        assert!(!SentenceDiff::new("same text", "same text\n").is_change());
        assert!(!SentenceDiff::new("same", "same").is_change());
    }

    // ---- summary ----------------------------------------------------------

    #[test]
    fn summary_always_carries_the_prefix() {
        assert_eq!(build_summary(None), "Typo fix via wikitui");
        assert_eq!(
            build_summary(Some("fixed teh->the")),
            "Typo fix via wikitui: fixed teh->the"
        );
        assert_eq!(build_summary(Some("   ")), "Typo fix via wikitui");
    }

    // ---- wikitext fetch parse --------------------------------------------

    #[test]
    fn parse_wikitext_reads_slots_content_revid_and_timestamp() {
        let body = br#"{"query":{"pages":[{"title":"Alan Turing","revisions":[
            {"revid":1001,"timestamp":"2026-07-15T08:00:00Z","slots":{"main":{"content":"Wikitext here."}}}
        ]}]}}"#;
        let got = parse_wikitext_response(body).unwrap();
        assert_eq!(got.text, "Wikitext here.");
        assert_eq!(got.revid, 1001);
        assert_eq!(got.timestamp, "2026-07-15T08:00:00Z");
    }

    #[test]
    fn parse_wikitext_falls_back_to_flat_content() {
        let body = br#"{"query":{"pages":[{"revisions":[{"revid":5,"timestamp":"t","content":"flat"}]}]}}"#;
        let got = parse_wikitext_response(body).unwrap();
        assert_eq!(got.text, "flat");
        assert_eq!(got.revid, 5);
    }

    #[test]
    fn parse_wikitext_of_a_missing_page_is_none() {
        let body = br#"{"query":{"pages":[{"title":"Nope","missing":true}]}}"#;
        assert!(parse_wikitext_response(body).is_none());
    }

    // ---- edit response parse ---------------------------------------------

    #[test]
    fn parse_edit_success_reads_the_new_revid() {
        let body = br#"{"edit":{"result":"Success","pageid":1,"title":"Alan Turing","newrevid":1002,"oldrevid":1001}}"#;
        assert_eq!(
            parse_edit_response(body),
            EditOutcome::Success { newrevid: 1002 }
        );
    }

    #[test]
    fn parse_edit_conflict_is_recognized() {
        let body = br#"{"error":{"code":"editconflict","info":"Edit conflict detected"}}"#;
        assert_eq!(parse_edit_response(body), EditOutcome::Conflict);
    }

    #[test]
    fn parse_edit_other_error_is_a_failure() {
        let body = br#"{"error":{"code":"protectedpage","info":"This page is protected"}}"#;
        match parse_edit_response(body) {
            EditOutcome::Failure(msg) => assert!(msg.contains("protectedpage")),
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    // ---- the hard prohibitions, enforced by absence -----------------------

    /// PRD FR-ACC-8 / §3.3: reverts, rollbacks, page moves, uploads,
    /// talk-page writing, and template editing are forbidden "ever". They are
    /// enforced by ABSENCE — this asserts no such Action-API write endpoint
    /// exists in the write-bearing modules (as a form-tuple value or a URL
    /// query value). Comment lines are skipped, since several modules
    /// deliberately *name* these ops in prose to document their absence; this
    /// checks actual request-building code, not documentation.
    #[test]
    fn no_forbidden_write_operations_exist_in_the_codebase() {
        let root = env!("CARGO_MANIFEST_DIR");
        let forbidden = ["move", "rollback", "upload", "delete", "undo", "undelete"];
        for file in [
            "src/api.rs",
            "src/main.rs",
            "src/editing.rs",
            "src/account.rs",
        ] {
            let src = std::fs::read_to_string(format!("{root}/{file}"))
                .unwrap_or_else(|e| panic!("reading {file}: {e}"));
            for line in src.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue; // documentation prose is allowed to name them
                }
                let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                for op in forbidden {
                    assert!(
                        !compact.contains(&format!("(\"action\",\"{op}\")")),
                        "{file}: forbidden write action {op:?} present as a form tuple"
                    );
                    assert!(
                        !compact.contains(&format!("action={op}&"))
                            && !compact.contains(&format!("?action={op}")),
                        "{file}: forbidden write action {op:?} present in a URL"
                    );
                }
            }
        }
    }
}
