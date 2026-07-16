//! Logged-in account features (PRD FR-ACC-2/3/4/6/7): watchlist, Echo
//! notifications, contributions, thanks, and read-only prefs. All five build
//! directly on the FR-ACC-1 auth substrate (`auth::AuthState`,
//! `api::WikiClient::authed_get`) added earlier: every feature here degrades
//! to a friendly login prompt when logged out, never an error, and never
//! makes an authenticated (or, for the login-gated ones, any) request in
//! that state — see `main.rs`'s `require_login` chokepoint, which every one
//! of these routes through.
//!
//! ## Split: this module parses, `api.rs` fetches
//!
//! Mirroring `prefetch.rs`/`startpage.rs`'s existing convention:
//! `api::WikiClient` methods do the actual HTTP round trip and hand back raw
//! response bytes (or, for the handful of one-line shapes, an already-parsed
//! scalar); every non-trivial response shape here has a `parse_*` function
//! that's a pure `&[u8] -> Result<_, serde_json::Error>`, testable with a
//! JSON fixture and no network at all. `main.rs` is the glue: it calls the
//! `WikiClient` method, feeds the bytes to this module's parser, and writes
//! the result into `App` state.
//!
//! ## CSRF/watch tokens and the badtoken retry (PRD §6.2 rule 8)
//!
//! Every write (`action=watch`, `thank`, `echomarkread`) needs a token from
//! `meta=tokens&type=csrf|watch` first. [`TokenCache`] fetches each kind
//! once and reuses it — a CSRF token stays valid for the rest of the login
//! session on a real wiki, so re-fetching it before every write would be
//! pure waste. The standard MediaWiki failure mode is a stale cached token
//! coming back `{"error":{"code":"badtoken",...}}`; [`is_badtoken_response`]
//! is the pure predicate that recognizes it, and every write call site in
//! `main.rs` follows the same shape: try with the cached token, and if (and
//! only if) the response is a badtoken error, invalidate the cache, fetch a
//! fresh token, and retry the exact same request **exactly once more** — a
//! second badtoken means something other than a stale cache is wrong, so
//! there is no loop.
//!
//! ## Notification poll cadence
//!
//! The unread badge (`meta=notifications&notprop=count`) is fetched exactly
//! twice in a normal session: once right after a session is established
//! (`finish_login`, and again at startup when a stored session is restored),
//! and once more whenever `:notifications` opens the pane (which also
//! fetches the full list). There is no timer-driven poll — PRD §6.5's
//! politeness rules and this app's "no background chatter without a reason"
//! posture both argue against inventing a poll interval nothing asked for.
//! Marking read (`echomarkread`) updates the in-memory badge locally instead
//! of firing a third network call.
//!
//! ## SEC-1: every remote display field is sanitized at this parse boundary
//!
//! wikitui talks to arbitrary third-party MediaWiki wikis (FR-ML-4/5), so the
//! server is fully untrusted, and `ratatui` emits a raw ESC/C1/control byte in
//! a `Span` verbatim to the terminal (PRD §6.6 SEC-1). The account/social
//! panes (`ui.rs`'s `draw_watchlist`/`draw_notifications`/`draw_contribs`/
//! `draw_prefs_overlay`) do no sanitizing of their own, so — exactly like
//! `api.rs` does for search/typeahead/langlink fields with
//! `sanitize_search_result` and friends — this module cleans every
//! remote-derived display field once, right after parsing, in each `parse_*`
//! function ([`clean_field`]/[`clean_text`]). Doing it here means every
//! render site downstream receives already-clean, length-capped strings and
//! no injection can reach the terminal through the account features. The cap
//! also closes a layout/allocation DoS: these fields (an edit summary, a
//! notification body) otherwise had no length bound at all, so a multi-MB
//! value would be carried and laid out in full.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::api::WikiClient;

/// SEC-1/SEC-3 cap for a single-line remote account/social field that is a
/// title, username, timestamp, skin, or language code — sized like `api.rs`'s
/// own title/search-field handling. Anything longer is either abuse or a
/// display bug, never legitimate content in one of these slots.
const MAX_SOCIAL_FIELD_CHARS: usize = 512;

/// SEC-1/SEC-3 cap for the two remote fields that are legitimately longer
/// prose — an edit summary (`comment`) and a notification body (`text`). Well
/// above any real value (MediaWiki edit summaries cap at ~1000 bytes) while
/// still bounding a hostile multi-megabyte payload to a fixed size.
const MAX_SOCIAL_TEXT_CHARS: usize = 2_048;

/// PRD SEC-1: sanitize (single-line) and length-cap one short remote field —
/// the account-feature counterpart of `api.rs`'s per-field sanitizers.
fn clean_field(s: &str) -> String {
    crate::sanitize::sanitize_and_cap_single_line(s, MAX_SOCIAL_FIELD_CHARS)
}

/// PRD SEC-1: [`clean_field`] with the larger [`MAX_SOCIAL_TEXT_CHARS`] cap,
/// for the legitimately-longer `comment`/`text` prose fields.
fn clean_text(s: &str) -> String {
    crate::sanitize::sanitize_and_cap_single_line(s, MAX_SOCIAL_TEXT_CHARS)
}

// ---------------------------------------------------------------------------
// CSRF / watch tokens (PRD §6.2 rule 8)
// ---------------------------------------------------------------------------

/// The cache key for a token: `(wiki_scope, lang)`. MediaWiki tokens are
/// per-wiki — each `[wiki.<name>]` edition and each language (`en`/`de`/…) is
/// a distinct login session with its own csrf/watch token, and a token minted
/// on one is rejected (`badtoken`) on another. `wiki_scope` is
/// `api::wiki_scope` (`""` = default Wikipedia), matching every other
/// wiki-scoped store; `lang` is the edition the token was fetched for.
type TokenKey = (String, String);

/// Caches the two token kinds this build's writes need (`csrf` for thank/
/// echomarkread, `watch` for watch/unwatch), **keyed by `(wiki, lang)`** so a
/// session fetches each at most once per wiki edition — until a badtoken
/// forces a refetch. Keyed rather than a single slot per kind (CORR-M7):
/// watching on `en` then `:sync`ing on `de` (or a `[wiki.<name>]` project)
/// must NOT reuse `en`'s token, which the server rejects as `badtoken`.
/// Lives on `App` for the whole process lifetime; cleared on logout (there is
/// nothing to reuse against a session that no longer exists).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TokenCache {
    csrf: HashMap<TokenKey, String>,
    watch: HashMap<TokenKey, String>,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached CSRF token for this session's `(wiki, lang)`, fetching it if
    /// this is the first such write of the session.
    pub async fn csrf_token(
        &mut self,
        client: &WikiClient,
        lang: &str,
        access_token: &str,
    ) -> Result<String> {
        let key = (client.wiki_scope(), lang.to_string());
        if let Some(t) = self.csrf.get(&key) {
            return Ok(t.clone());
        }
        let t = client.fetch_token(lang, access_token, "csrf").await?;
        self.csrf.insert(key, t.clone());
        Ok(t)
    }

    /// The cached watch token for this session's `(wiki, lang)`, fetching it
    /// if this is the first watch/unwatch of the session on that wiki.
    pub async fn watch_token(
        &mut self,
        client: &WikiClient,
        lang: &str,
        access_token: &str,
    ) -> Result<String> {
        let key = (client.wiki_scope(), lang.to_string());
        if let Some(t) = self.watch.get(&key) {
            return Ok(t.clone());
        }
        let t = client.fetch_token(lang, access_token, "watch").await?;
        self.watch.insert(key, t.clone());
        Ok(t)
    }

    /// Drops the cached CSRF token for `(wiki, lang)` — called after a
    /// `badtoken` response, so the next `csrf_token` call for that wiki
    /// fetches a fresh one instead of handing back the same stale value.
    /// Only that wiki's token is dropped; another wiki's cached token is a
    /// different session and stays valid.
    pub fn invalidate_csrf(&mut self, wiki: &str, lang: &str) {
        self.csrf.remove(&(wiki.to_string(), lang.to_string()));
    }

    /// The watch-token counterpart of [`invalidate_csrf`](Self::invalidate_csrf).
    pub fn invalidate_watch(&mut self, wiki: &str, lang: &str) {
        self.watch.remove(&(wiki.to_string(), lang.to_string()));
    }

    /// PRD FR-ACC-9 / logout: nothing cached here can outlive the session it
    /// was minted for.
    pub fn clear(&mut self) {
        self.csrf.clear();
        self.watch.clear();
    }
}

/// PRD §6.2 rule 8: recognizes the standard MediaWiki `{"error":{"code":
/// "badtoken", ...}}` response shape a write action returns when the token
/// it was sent is stale. Every other shape (success, a different error code,
/// unparseable garbage) is `false` — this predicate answers exactly one
/// question ("was that a badtoken?"), never "did the write otherwise
/// succeed?", which the caller decides separately from the same body.
pub fn is_badtoken_response(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct ErrorEnvelope {
        error: Option<ErrorCode>,
    }
    #[derive(Deserialize)]
    struct ErrorCode {
        code: String,
    }
    serde_json::from_slice::<ErrorEnvelope>(body)
        .ok()
        .and_then(|e| e.error)
        .is_some_and(|e| e.code == "badtoken")
}

// ---------------------------------------------------------------------------
// FR-ACC-2: watchlist
// ---------------------------------------------------------------------------

/// `list=watchlistraw`'s response shape: a top-level `watchlistraw` array
/// (NOT nested under `query` — unlike almost everything else this client
/// reads, `watchlistraw` rides its own top-level key on the real API too).
#[derive(Debug, Deserialize, Default)]
struct WatchlistRawResponse {
    #[serde(default)]
    watchlistraw: Vec<WatchlistRawEntry>,
}

#[derive(Debug, Deserialize)]
struct WatchlistRawEntry {
    title: String,
}

/// Parses `list=watchlistraw` into the plain title list (PRD FR-ACC-2's
/// "raw watched-pages list").
pub fn parse_watchlistraw(body: &[u8]) -> Result<Vec<String>, serde_json::Error> {
    let parsed: WatchlistRawResponse = serde_json::from_slice(body)?;
    Ok(parsed
        .watchlistraw
        .into_iter()
        .map(|e| clean_field(&e.title))
        .collect())
}

/// One recent edit to a watched page (PRD FR-ACC-2's "what changed"
/// activity feed, `list=watchlist`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WatchlistChange {
    pub title: String,
    #[serde(default)]
    pub user: String,
    /// ISO 8601 — lexicographically sortable, so [`changes_since`] can
    /// compare it against a stored cutoff with plain string comparison.
    pub timestamp: String,
    #[serde(default)]
    pub comment: Option<String>,
    pub revid: u64,
    #[serde(default)]
    pub old_revid: u64,
}

#[derive(Debug, Deserialize, Default)]
struct WatchlistChangesResponse {
    #[serde(default)]
    query: Option<WatchlistChangesQuery>,
}

#[derive(Debug, Deserialize)]
struct WatchlistChangesQuery {
    #[serde(default)]
    watchlist: Vec<WatchlistChange>,
}

/// Parses `list=watchlist` into the recent-changes list.
pub fn parse_watchlist_changes(body: &[u8]) -> Result<Vec<WatchlistChange>, serde_json::Error> {
    let parsed: WatchlistChangesResponse = serde_json::from_slice(body)?;
    let mut changes = parsed.query.map(|q| q.watchlist).unwrap_or_default();
    for c in &mut changes {
        // SEC-1: every field is rendered (`ui.rs::draw_watchlist`) — title and
        // the `timestamp · user · comment` subline. `timestamp` also feeds the
        // "since last seen" cutoff, but that comparison is a plain string
        // compare that stays correct on the cleaned value.
        c.title = clean_field(&c.title);
        c.user = clean_field(&c.user);
        c.timestamp = clean_field(&c.timestamp);
        if let Some(comment) = c.comment.as_deref() {
            c.comment = Some(clean_text(comment));
        }
    }
    Ok(changes)
}

/// The "since last seen" filter (PRD FR-ACC-2): every change whose
/// `timestamp` sorts strictly after `last_seen`, in the order the server
/// returned them. `last_seen: None` (first-ever open, or a store that
/// couldn't be read) keeps everything — there is nothing to compare against
/// yet, so nothing is filtered out. ISO 8601 timestamps compare correctly as
/// plain strings as long as both sides use the same fixed-width form (the
/// mock and the real API both do), so this needs no date parsing at all.
pub fn changes_since<'a>(
    changes: &'a [WatchlistChange],
    last_seen: Option<&str>,
) -> Vec<&'a WatchlistChange> {
    match last_seen {
        None => changes.iter().collect(),
        Some(cutoff) => changes
            .iter()
            .filter(|c| c.timestamp.as_str() > cutoff)
            .collect(),
    }
}

/// The response to a `prop=info&inprop=watched` check (PRD FR-ACC-2's `w`
/// toggle): whether the *current* session already watches this title, read
/// fresh before every toggle so the decision (`unwatch=` or not) is never
/// based on a stale local guess — the server's own state is the only source
/// of truth for "is this watched right now."
#[derive(Debug, Deserialize, Default)]
struct WatchedStatusResponse {
    #[serde(default)]
    query: Option<WatchedStatusQuery>,
}

#[derive(Debug, Deserialize)]
struct WatchedStatusQuery {
    #[serde(default)]
    pages: Vec<WatchedStatusPage>,
}

#[derive(Debug, Deserialize, Default)]
struct WatchedStatusPage {
    #[serde(default)]
    watched: bool,
}

/// Parses `prop=info&inprop=watched`'s single-page response into whether the
/// title is currently watched. Defaults to `false` (never watched) for any
/// shape that doesn't parse — the same "degrade to the safer/simpler branch,
/// never crash" posture the rest of this codebase's response parsing takes.
pub fn parse_watched_status(body: &[u8]) -> bool {
    serde_json::from_slice::<WatchedStatusResponse>(body)
        .ok()
        .and_then(|r| r.query)
        .and_then(|q| q.pages.into_iter().next())
        .map(|p| p.watched)
        .unwrap_or(false)
}

/// The outcome of an `action=watch` write, read back off its own response
/// (never inferred from what was sent) so a toggle's confirmation always
/// reflects what the server actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchOutcome {
    Watched,
    Unwatched,
}

#[derive(Debug, Deserialize, Default)]
struct WatchActionResponse {
    #[serde(default)]
    watch: Vec<WatchActionEntry>,
}

#[derive(Debug, Deserialize, Default)]
struct WatchActionEntry {
    /// Only populated by the batched form (`parse_watch_batch_outcome`) —
    /// the single-title `parse_watch_outcome` already knows which title it
    /// asked about, so it never needs to read this field back.
    #[serde(default)]
    title: String,
    #[serde(default)]
    watched: bool,
    #[serde(default)]
    unwatched: bool,
}

/// Parses `action=watch`'s response into [`WatchOutcome`]. `None` when the
/// body is a badtoken/other error (check [`is_badtoken_response`] first) or
/// otherwise doesn't parse — the caller treats that as a failed write.
pub fn parse_watch_outcome(body: &[u8]) -> Option<WatchOutcome> {
    let parsed: WatchActionResponse = serde_json::from_slice(body).ok()?;
    let entry = parsed.watch.into_iter().next()?;
    if entry.unwatched {
        Some(WatchOutcome::Unwatched)
    } else if entry.watched {
        Some(WatchOutcome::Watched)
    } else {
        None
    }
}

/// PRD §6.4's state-dir convention (`auth::auth_path`/`history::
/// history_path`): the watchlist's persisted last-seen timestamp lives in
/// `$XDG_STATE_HOME/wikitui/watchlist.json` — state, not data, since it's a
/// local bookmark into a server-side feed, not user content of its own.
pub fn watchlist_state_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("watchlist.json"))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WatchlistState {
    last_seen: Option<String>,
}

/// Loads the last-seen timestamp from `path`. Best-effort like `history.rs`'s
/// writes: a missing file, a corrupt one, or no readable path at all all
/// degrade to `None` (first-ever open — nothing is filtered out) rather than
/// an error the reader would have to do anything about.
pub fn load_last_seen(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<WatchlistState>(&text)
        .ok()
        .and_then(|s| s.last_seen)
}

/// Persists `last_seen` to `path`, creating parent directories as needed.
/// Best-effort: the caller discards the `Result` (matching `history.rs`'s
/// "losing one write is preferable to interrupting reading" posture) —
/// worst case the next open re-shows a change already seen, never a crash.
pub fn save_last_seen(path: &Path, last_seen: &str) -> std::io::Result<()> {
    let json = serde_json::to_string(&WatchlistState {
        last_seen: Some(last_seen.to_string()),
    })
    .map_err(std::io::Error::other)?;
    // Atomic write (quality-M4): a torn watchlist-state write could leave a
    // truncated file that loads as "never seen anything", re-surfacing changes
    // the reader already acknowledged.
    crate::atomicio::write_atomic(path, json.as_bytes())
}

/// The newest `timestamp` across `changes`, or `None` for an empty feed —
/// what a successful watchlist open advances `last_seen` to, so the next
/// open's "since last seen" filter starts from here rather than from the
/// moment the reader happened to press a key.
pub fn newest_timestamp(changes: &[WatchlistChange]) -> Option<&str> {
    changes.iter().map(|c| c.timestamp.as_str()).max()
}

// ---------------------------------------------------------------------------
// FR-ACC-3: notifications (Echo)
// ---------------------------------------------------------------------------

/// The unread-count badge's two buckets (PRD FR-ACC-3's "alerts / messages"
/// split, `meta=notifications&notprop=count`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct NotifCounts {
    pub alert: u32,
    pub message: u32,
}

impl NotifCounts {
    pub fn total(self) -> u32 {
        self.alert + self.message
    }
}

#[derive(Debug, Deserialize, Default)]
struct NotifCountResponse {
    #[serde(default)]
    query: Option<NotifCountQuery>,
}

#[derive(Debug, Deserialize)]
struct NotifCountQuery {
    notifications: NotifCountInner,
}

#[derive(Debug, Deserialize, Default)]
struct NotifCountInner {
    #[serde(default)]
    alert: CountField,
    #[serde(default)]
    message: CountField,
}

#[derive(Debug, Deserialize, Default)]
struct CountField {
    #[serde(default)]
    count: u32,
}

/// Parses `meta=notifications&notprop=count`.
pub fn parse_notif_count(body: &[u8]) -> Result<NotifCounts, serde_json::Error> {
    let parsed: NotifCountResponse = serde_json::from_slice(body)?;
    let inner = parsed.query.map(|q| q.notifications).unwrap_or_default();
    Ok(NotifCounts {
        alert: inner.alert.count,
        message: inner.message.count,
    })
}

/// The status-bar badge text (PRD FR-ACC-3): `None` when there is nothing
/// unread (no badge shown at all — an absent glyph reads as "you're caught
/// up," not as "notifications are broken"), else `✉{total}`.
pub fn format_badge(counts: NotifCounts) -> Option<String> {
    let total = counts.total();
    if total == 0 {
        None
    } else {
        Some(format!("\u{2709}{total}"))
    }
}

/// Which of the notifications pane's two tabs an entry belongs to (PRD
/// FR-ACC-3's "pane split into alerts / messages").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifKind {
    Alert,
    Message,
}

/// One Echo notification (PRD FR-ACC-3), from `notprop=list`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Notification {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: NotifKind,
    pub text: String,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub timestamp: String,
}

#[derive(Debug, Deserialize, Default)]
struct NotifListResponse {
    #[serde(default)]
    query: Option<NotifListQuery>,
}

#[derive(Debug, Deserialize)]
struct NotifListQuery {
    notifications: NotifListInner,
}

#[derive(Debug, Deserialize)]
struct NotifListInner {
    #[serde(default)]
    list: Vec<Notification>,
}

/// Parses `meta=notifications&notprop=list`.
pub fn parse_notif_list(body: &[u8]) -> Result<Vec<Notification>, serde_json::Error> {
    let parsed: NotifListResponse = serde_json::from_slice(body)?;
    let mut list = parsed
        .query
        .map(|q| q.notifications.list)
        .unwrap_or_default();
    for n in &mut list {
        // SEC-1: `text` is rendered as a single line (`ui.rs::
        // draw_notifications`); `timestamp` is remote too, so it is cleaned
        // even though this build doesn't currently show it.
        n.text = clean_text(&n.text);
        n.timestamp = clean_field(&n.timestamp);
    }
    Ok(list)
}

/// [`NotifCounts`] recomputed purely from a fetched notification list — used
/// after a local mark-read/mark-all-read (PRD FR-ACC-3) so the badge updates
/// instantly without a third network round trip per the module doc
/// comment's poll-cadence contract.
pub fn counts_from_list(list: &[Notification]) -> NotifCounts {
    let mut counts = NotifCounts::default();
    for n in list {
        if n.read {
            continue;
        }
        match n.kind {
            NotifKind::Alert => counts.alert += 1,
            NotifKind::Message => counts.message += 1,
        }
    }
    counts
}

// ---------------------------------------------------------------------------
// FR-ACC-4: contributions
// ---------------------------------------------------------------------------

/// One edit from `list=usercontribs` (PRD FR-ACC-4): title, timestamp,
/// comment, and the size delta the picker shows alongside each row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Contribution {
    pub title: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub comment: Option<String>,
    pub revid: u64,
    #[serde(default)]
    pub sizediff: i64,
}

#[derive(Debug, Deserialize, Default)]
struct UserContribsResponse {
    #[serde(default)]
    query: Option<UserContribsQuery>,
}

#[derive(Debug, Deserialize)]
struct UserContribsQuery {
    #[serde(default)]
    usercontribs: Vec<Contribution>,
}

/// Parses `list=usercontribs` (public — no auth needed, works for any
/// username per FR-ACC-4).
pub fn parse_usercontribs(body: &[u8]) -> Result<Vec<Contribution>, serde_json::Error> {
    let parsed: UserContribsResponse = serde_json::from_slice(body)?;
    let mut contribs = parsed.query.map(|q| q.usercontribs).unwrap_or_default();
    for c in &mut contribs {
        // SEC-1 — the sharpest case: `:contribs <user>` is public and
        // unauthenticated (any wiki, any username), so `title`/`comment` come
        // straight off an untrusted server and are rendered by
        // `ui.rs::draw_contribs`.
        c.title = clean_field(&c.title);
        c.timestamp = clean_field(&c.timestamp);
        if let Some(comment) = c.comment.as_deref() {
            c.comment = Some(clean_text(comment));
        }
    }
    Ok(contribs)
}

// ---------------------------------------------------------------------------
// FR-ACC-6: thank
// ---------------------------------------------------------------------------

/// Whether `action=thank` reported success (PRD FR-ACC-6). Checked
/// separately from [`is_badtoken_response`] on the same body, same as every
/// other write here.
pub fn thank_succeeded(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct ThankResponse {
        result: Option<ThankResult>,
    }
    #[derive(Deserialize)]
    struct ThankResult {
        #[serde(default)]
        success: u32,
    }
    serde_json::from_slice::<ThankResponse>(body)
        .ok()
        .and_then(|r| r.result)
        .is_some_and(|r| r.success != 0)
}

// ---------------------------------------------------------------------------
// FR-ACC-7: preferences (read-only)
// ---------------------------------------------------------------------------

/// The curated subset of `meta=userinfo&uiprop=options` this build shows
/// (PRD FR-ACC-7): skin, language, whether the account's email is confirmed,
/// and the edit count — never the full options blob (which runs to hundreds
/// of keys on a real account), and never written back (`action=options` is
/// never called anywhere in this codebase, matching FR-ACC-1's "never
/// `editmyoptions`" grant restriction one layer up).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserPrefs {
    pub skin: Option<String>,
    pub language: Option<String>,
    pub email_confirmed: bool,
    pub editcount: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct UserPrefsResponse {
    #[serde(default)]
    query: Option<UserPrefsQuery>,
}

#[derive(Debug, Deserialize)]
struct UserPrefsQuery {
    userinfo: UserPrefsInfo,
}

#[derive(Debug, Deserialize, Default)]
struct UserPrefsInfo {
    #[serde(default)]
    options: Option<UserPrefsOptions>,
    #[serde(default)]
    editcount: Option<u64>,
    #[serde(default)]
    emailauthenticated: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct UserPrefsOptions {
    #[serde(default)]
    skin: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

/// Parses `meta=userinfo&uiprop=options` into the curated [`UserPrefs`].
pub fn parse_userinfo_options(body: &[u8]) -> Result<UserPrefs, serde_json::Error> {
    let parsed: UserPrefsResponse = serde_json::from_slice(body)?;
    let info = parsed.query.map(|q| q.userinfo).unwrap_or_default();
    Ok(UserPrefs {
        // SEC-1: `skin`/`language` are server-controlled strings rendered by
        // `ui.rs::draw_prefs_overlay`.
        skin: info
            .options
            .as_ref()
            .and_then(|o| o.skin.as_deref())
            .map(clean_field),
        language: info
            .options
            .as_ref()
            .and_then(|o| o.language.as_deref())
            .map(clean_field),
        email_confirmed: info.emailauthenticated.is_some(),
        editcount: info.editcount,
    })
}

// ---------------------------------------------------------------------------
// FR-BM-5: Reading List sync (Extension:ReadingLists, `action=readinglists`)
// ---------------------------------------------------------------------------
//
// ## SP-7 assumptions (unverifiable live — see PRD SP-7 / Appendix A)
//
// The real extension's exact wire shape for third-party OAuth consumers is
// explicitly flagged unverified (SP-7: "verify third-party OAuth consumers
// can use `action=readinglists`; which grants the write modules require;
// list-size caps; conflict semantics"). Live Wikipedia is unreachable from
// this build's test environment, so every shape below is this project's own
// documented best-effort reading, mirrored exactly by the test mock
// (`tests/mock-server/server.py`), not a verified live behavior:
//
//   - Every response (read or write) rides one top-level `"readinglists"`
//     key, keyed further by what the command returned (`lists`, `entries`,
//     `entry`, `list`, `success`) — chosen for a single, generic parse shape
//     rather than one struct per command's own top-level key.
//   - `command=list`/`command=listentries` before the account has ever run
//     `command=setup` come back as an error with code
//     `"readinglists-db-error-not-set-up"` ([`READINGLISTS_NOT_SET_UP`]) —
//     the signal `main::fetch_readinglists_with_setup` treats the same way
//     `is_badtoken_response` treats a stale token (§6.2 rule 8): try, detect
//     that one recoverable failure, fix it (`command=setup`), retry once.
//   - `command=list`/`listentries`/`setup`/`createentry`/`deleteentry` are
//     called against the *current* `lang`'s per-wiki endpoint (PRD §6.2 rule
//     1: per-wiki endpoints only), matching every other authenticated write
//     in this codebase. Appendix A notes the real extension is "cross-wiki";
//     if that means a single central list resolves identically regardless of
//     which wiki host serves the request, this degrades to one independent
//     list per wiki host — safe (nothing crashes or double-syncs), just not
//     the cross-wiki ideal. **Multi-wiki Reading List sync is a documented
//     v1 seam**, same as multiple *named* lists (`default_list_id` always
//     picks the one default list — see its own doc comment).
//   - A `listentries` entry whose `project` doesn't match the current
//     session's own wiki origin is left alone by `main::sync_reading_list`
//     (filtered out before reconciling) rather than guessed into some other
//     `lang` bucket — the safe version of the same cross-wiki scope-cut.
//
// ## The two-way reconcile + sync-mapping model (FR-BM-5's conflict policy)
//
// FR-BM-5's conflict policy, verbatim: "server wins on order, local wins on
// tags/notes (which the server can't store — kept local-only)." Concretely:
//
//   - **Tags/notes**: never sent to the server (no field exists for them —
//     `readinglists_createentry_raw` takes only a title/project) and never
//     touched by a pull, because [`reconcile_reading_list`] only ever calls
//     an operation on a title that's *missing* from one side; a title
//     present on both sides (`matched`) is left completely untouched, tags
//     and notes included. "Local wins" here isn't an active merge decision —
//     it's that sync has no path that could ever overwrite them.
//   - **Order**: [`apply_server_order`] re-sorts local bookmarks (within the
//     synced `lang`) to the server's own `listentries` return order after
//     every reconcile; a title the server doesn't know about yet (this
//     round's fresh pushes) keeps its prior relative position, appended
//     last, until the *next* sync re-lists it with a real rank.
//   - **The sync-mapping problem**: telling "a page deleted locally after
//     being synced" (must delete server-side, never re-pull) apart from "a
//     page new to the server" (must pull) needs memory of what was
//     previously synced — a same-run diff of local-vs-server alone can't
//     distinguish them (both look like "on the server, not local"). This
//     build's model is a **persisted id-map** ([`ReadingListSyncState`],
//     `synced: Vec<SyncedEntry>`), not a tombstone list: after every
//     reconcile the map is fully recomputed as exactly "every title known to
//     be on both sides right now" (`matched` ∪ freshly pushed ∪ freshly
//     pulled) — a title that drops off *either* side (deleted locally,
//     deleted server-side, or both) simply isn't re-added, which is the
//     model's garbage collection: no tombstone ever needs explicit expiry.
//     **Limit**: a page removed from the server by a *different* client
//     between two of this client's syncs, while still bookmarked locally,
//     is indistinguishable from "never synced" — [`reconcile_reading_list`]
//     re-pushes it (favors never silently losing a local bookmark over
//     never re-creating a deliberately-server-deleted one).

/// PRD FR-BM-5 / SP-7: this mock/build's own chosen error code for "no
/// reading list exists for this user yet" — see this section's module doc
/// for why the real extension's actual code (if any) is unverified.
pub const READINGLISTS_NOT_SET_UP: &str = "readinglists-db-error-not-set-up";

/// Recognizes [`READINGLISTS_NOT_SET_UP`] the same way [`is_badtoken_response`]
/// recognizes `badtoken` — the one recoverable failure `main::
/// fetch_readinglists_with_setup` retries after running `command=setup`.
pub fn is_readinglists_not_set_up(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct ErrorEnvelope {
        error: Option<ErrorCode>,
    }
    #[derive(Deserialize)]
    struct ErrorCode {
        code: String,
    }
    serde_json::from_slice::<ErrorEnvelope>(body)
        .ok()
        .and_then(|e| e.error)
        .is_some_and(|e| e.code == READINGLISTS_NOT_SET_UP)
}

/// `command=setup`'s response: the id of the (now-available) default list,
/// when this build's assumed shape parses. `None` on any other shape
/// (including a genuine error body) — the caller treats that as "setup
/// didn't confirm anything usable," not as a crash.
pub fn parse_readinglists_setup(body: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct Resp {
        readinglists: Option<Inner>,
    }
    #[derive(Deserialize)]
    struct Inner {
        #[serde(default)]
        list: Option<u64>,
    }
    serde_json::from_slice::<Resp>(body)
        .ok()
        .and_then(|r| r.readinglists)
        .and_then(|i| i.list)
}

/// One of the account's Reading Lists (`command=list`'s `lists` array): its
/// server id, display name, and whether it's the account's default list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReadingListInfo {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub default: bool,
}

/// Parses `command=list`'s response into every list the account has.
pub fn parse_readinglists(body: &[u8]) -> Vec<ReadingListInfo> {
    #[derive(Deserialize)]
    struct Resp {
        readinglists: Option<Inner>,
    }
    #[derive(Deserialize, Default)]
    struct Inner {
        #[serde(default)]
        lists: Vec<ReadingListInfo>,
    }
    serde_json::from_slice::<Resp>(body)
        .ok()
        .and_then(|r| r.readinglists)
        .map(|i| i.lists)
        .unwrap_or_default()
}

/// Picks the list `:sync` reconciles against (PRD FR-BM-5 v1: one default
/// list only — multiple *named* lists are a documented seam, see this
/// section's module doc): the entry flagged `default`, or else simply the
/// first list returned, tolerant of a server that omits the flag entirely.
/// `None` only when the account has no lists at all (shouldn't happen right
/// after a successful `command=setup`, but kept total).
pub fn default_list_id(lists: &[ReadingListInfo]) -> Option<u64> {
    lists
        .iter()
        .find(|l| l.default)
        .or_else(|| lists.first())
        .map(|l| l.id)
}

/// One entry in a Reading List (`command=listentries`): its server id, the
/// wiki origin it belongs to (`project`, e.g. `https://en.wikipedia.org` —
/// empty when a server omits it, tolerated rather than rejected), and its
/// title.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReadingListEntry {
    pub id: u64,
    #[serde(default)]
    pub project: String,
    pub title: String,
}

/// Parses `command=listentries`.
pub fn parse_readinglist_entries(body: &[u8]) -> Vec<ReadingListEntry> {
    #[derive(Deserialize)]
    struct Resp {
        readinglists: Option<Inner>,
    }
    #[derive(Deserialize, Default)]
    struct Inner {
        #[serde(default)]
        entries: Vec<ReadingListEntry>,
    }
    let mut entries = serde_json::from_slice::<Resp>(body)
        .ok()
        .and_then(|r| r.readinglists)
        .map(|i| i.entries)
        .unwrap_or_default();
    for e in &mut entries {
        // SEC-1: a pulled entry's `title` becomes both a persisted bookmark/
        // read-later entry (`main::sync_reading_list`) and rendered text
        // (`ui.rs`), so it is cleaned before it can reach either the store or
        // the terminal. `project` is only ever compared for equality against
        // this session's own (clean) origin, never displayed, so it is left
        // as-is — a hostile value simply fails to match and is filtered out.
        e.title = clean_field(&e.title);
    }
    entries
}

/// Parses `command=createentry`'s response into the entry the server just
/// created (its assigned id, in particular — the whole point of the call).
pub fn parse_readinglists_createentry(body: &[u8]) -> Option<ReadingListEntry> {
    #[derive(Deserialize)]
    struct Resp {
        readinglists: Option<Inner>,
    }
    #[derive(Deserialize)]
    struct Inner {
        entry: Option<ReadingListEntry>,
    }
    serde_json::from_slice::<Resp>(body)
        .ok()
        .and_then(|r| r.readinglists)
        .and_then(|i| i.entry)
        .map(|mut e| {
            // SEC-1: same `ReadingListEntry` type as `parse_readinglist_entries`
            // — cleaned at the parse boundary for defense in depth even though
            // this build reads back only the server-assigned id from it.
            e.title = clean_field(&e.title);
            e
        })
}

/// Whether `command=deleteentry` reported success.
pub fn readinglists_deleteentry_succeeded(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Resp {
        readinglists: Option<Inner>,
    }
    #[derive(Deserialize, Default)]
    struct Inner {
        #[serde(default)]
        success: bool,
    }
    serde_json::from_slice::<Resp>(body)
        .ok()
        .and_then(|r| r.readinglists)
        .is_some_and(|i| i.success)
}

/// One title this client has previously reconciled to a server entry id —
/// [`ReadingListSyncState`]'s id-map row (see this section's module doc for
/// the model this implements).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedEntry {
    pub lang: String,
    pub title: String,
    pub entry_id: u64,
}

/// The Reading List sync's persisted state (PRD §6.4: `$XDG_STATE_HOME` —
/// this is a local bookmark into server-side state, not user content of its
/// own, the same reasoning `watchlist.json` already uses).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadingListSyncState {
    #[serde(default)]
    pub list_id: Option<u64>,
    #[serde(default)]
    pub synced: Vec<SyncedEntry>,
}

/// `$XDG_STATE_HOME/wikitui/readinglist-sync.json`.
pub fn readinglist_sync_state_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("readinglist-sync.json"))
}

/// Loads the Reading List sync state from `path`. Best-effort like
/// `load_last_seen`: a missing file, a corrupt one, or no readable path all
/// degrade to `ReadingListSyncState::default()` (nothing previously synced)
/// rather than an error the caller would have to handle.
pub fn load_readinglist_sync_state(path: &Path) -> ReadingListSyncState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persists the Reading List sync state to `path`, creating parent
/// directories as needed. Best-effort — the caller discards the `Result`,
/// matching `save_last_seen`'s "losing one write is preferable to
/// interrupting reading" posture.
pub fn save_readinglist_sync_state(
    path: &Path,
    state: &ReadingListSyncState,
) -> std::io::Result<()> {
    let json = serde_json::to_string(state).map_err(std::io::Error::other)?;
    // Atomic write (quality-M4): a torn reading-list sync-state write is
    // especially costly — a truncated snapshot makes the next reconcile
    // resurrect entries the reader deleted or re-churn already-synced ones.
    crate::atomicio::write_atomic(path, json.as_bytes())
}

/// The three actions one Reading List two-way reconcile decides on (PRD
/// FR-BM-5's conflict policy — see this section's module doc for the full
/// model): `push` titles need a server entry created; `pull` titles need a
/// local bookmark created; `server_delete` entry ids need deleting
/// server-side. `matched` (present on both sides already, so nothing to do)
/// is exposed too, since the caller needs it to recompute the sync-mapping
/// going forward — see [`reconcile_reading_list`]'s doc comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadingListPlan {
    pub push: Vec<String>,
    pub pull: Vec<String>,
    pub server_delete: Vec<u64>,
    pub matched: Vec<(String, u64)>,
}

/// The pure heart of FR-BM-5's two-way sync (no I/O — every network call and
/// every `BookmarkStore`/state-file write is the caller's job, `main::
/// sync_reading_list`): given the local bookmark titles for the wiki being
/// synced (in their existing local order — the caller passes a slice, not a
/// set, precisely so this stays deterministic), the server's current
/// entries for that list, and the previous sync's id-map, decides:
///
///   - a **local** title with no matching server entry → [`push`
///     ](ReadingListPlan::push) it (covers both "brand-new local bookmark"
///     and "was synced before, but the server copy is gone now" — treated
///     identically, since either way the local bookmark should exist on the
///     server and currently doesn't; see this section's module doc for why
///     that favors never losing a local bookmark).
///   - a **server** entry with no matching local title, whose title was
///     *not* in `previously_synced` → [`pull`](ReadingListPlan::pull) it
///     (new to this client, from another device/app).
///   - a **server** entry with no matching local title, whose title *was* in
///     `previously_synced` → [`server_delete`](ReadingListPlan::server_delete)
///     it (this client had it, the reader deleted the local bookmark since —
///     mirror that deletion server-side, don't resurrect it locally).
///   - a title present on **both** sides → [`matched`](ReadingListPlan::matched)
///     (already in sync; completely untouched, tags/notes included — the
///     "local wins on tags/notes" half of the conflict policy, enforced by
///     omission rather than by an active merge).
pub fn reconcile_reading_list(
    local_titles: &[String],
    server_entries: &[ReadingListEntry],
    previously_synced: &[SyncedEntry],
) -> ReadingListPlan {
    let server_id_by_title: HashMap<&str, u64> = server_entries
        .iter()
        .map(|e| (e.title.as_str(), e.id))
        .collect();
    let local_set: HashSet<&str> = local_titles.iter().map(String::as_str).collect();
    let was_synced: HashSet<&str> = previously_synced.iter().map(|s| s.title.as_str()).collect();

    let mut push = Vec::new();
    let mut matched = Vec::new();
    for title in local_titles {
        match server_id_by_title.get(title.as_str()) {
            Some(&id) => matched.push((title.clone(), id)),
            None => push.push(title.clone()),
        }
    }

    let mut pull = Vec::new();
    let mut server_delete = Vec::new();
    for entry in server_entries {
        if local_set.contains(entry.title.as_str()) {
            continue; // already accounted for in `matched` above
        }
        if was_synced.contains(entry.title.as_str()) {
            server_delete.push(entry.id);
        } else {
            pull.push(entry.title.clone());
        }
    }

    ReadingListPlan {
        push,
        pull,
        server_delete,
        matched,
    }
}

/// PRD FR-BM-5's "server wins on order": returns `local_titles` reordered so
/// every title the server also returned follows `server_titles_in_order`'s
/// ranking; a title the server doesn't (yet) know about — this sync round's
/// fresh pushes, before the *next* sync re-lists them — keeps its prior
/// relative position among the rest, appended after every server-ranked
/// title. Stable by construction (ties broken by original index), so this
/// is a total ordering with no arbitrary tie-breaking.
pub fn apply_server_order(
    server_titles_in_order: &[String],
    local_titles: &[String],
) -> Vec<String> {
    let rank: HashMap<&str, usize> = server_titles_in_order
        .iter()
        .enumerate()
        .map(|(i, t)| (t.as_str(), i))
        .collect();
    let mut indexed: Vec<(usize, &String)> = local_titles.iter().enumerate().collect();
    indexed.sort_by_key(|(i, t)| (rank.get(t.as_str()).copied().unwrap_or(usize::MAX), *i));
    indexed.into_iter().map(|(_, t)| t.clone()).collect()
}

// ---------------------------------------------------------------------------
// FR-BM-6: watchlist mirror
// ---------------------------------------------------------------------------
//
// One designated bookmark tag (config `watchlist_mirror_tag`, default
// `"watched"`) mirrors to the real watchlist. This is edit-monitoring, not
// bookmarking — PRD FR-BM-6's own wording ("clearly labeled as 'watching
// edits', separate from bookmarks") — so tagging a bookmark `watched` adds
// its page to the server watchlist, and removing the tag (or deleting the
// bookmark outright) unwatches it; it never works the other direction (a
// page watched by hand, outside this mirror, is never touched by it — see
// [`watch_mirror_diff`]'s doc comment for exactly why that needs its own
// persisted state rather than reading the live watchlist).

/// The watch-mirror's own persisted state: which titles *this mirror*
/// caused to be watched, as of its last run. Never re-derived from the live
/// watchlist (`list=watchlistraw`) — a page the reader watches by hand for
/// unrelated reasons must never be unwatched just because it isn't tagged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchMirrorState {
    #[serde(default)]
    pub mirrored: Vec<String>,
}

/// `$XDG_STATE_HOME/wikitui/watch-mirror.json`.
pub fn watch_mirror_state_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("watch-mirror.json"))
}

/// Loads the watch-mirror state from `path` — best-effort, same degrade-to-
/// default posture as [`load_readinglist_sync_state`].
pub fn load_watch_mirror_state(path: &Path) -> WatchMirrorState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persists the watch-mirror state to `path` — best-effort, same posture as
/// [`save_readinglist_sync_state`].
pub fn save_watch_mirror_state(path: &Path, state: &WatchMirrorState) -> std::io::Result<()> {
    let json = serde_json::to_string(state).map_err(std::io::Error::other)?;
    // Atomic write (quality-M4): same posture as `save_readinglist_sync_state`
    // — a torn watch-mirror snapshot must not corrupt the mirror's view.
    crate::atomicio::write_atomic(path, json.as_bytes())
}

/// What one watch-mirror application needs to do (PRD FR-BM-6): titles to
/// watch and titles to unwatch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchMirrorPlan {
    pub to_watch: Vec<String>,
    pub to_unwatch: Vec<String>,
}

/// The pure heart of FR-BM-6's mirror (no I/O): `tagged_titles` are every
/// bookmark title currently carrying the mirror tag; `previously_mirrored`
/// is [`WatchMirrorState::mirrored`] from the last run. A title just tagged
/// shows up only in [`to_watch`](WatchMirrorPlan::to_watch); a title
/// untagged (or whose bookmark was deleted outright) shows up only in
/// [`to_unwatch`](WatchMirrorPlan::to_unwatch) — the two sets are disjoint by
/// construction, since a title can't simultaneously be in `tagged_titles`
/// and be the *complement* of `tagged_titles`.
///
/// This is deliberately a diff against the mirror's own prior state, never
/// against the live watchlist: unwatching everything the live watchlist
/// shows that isn't currently tagged would also unwatch pages the reader
/// watches by hand for reasons that have nothing to do with bookmarking.
pub fn watch_mirror_diff(
    tagged_titles: &[String],
    previously_mirrored: &[String],
) -> WatchMirrorPlan {
    let tagged_set: HashSet<&str> = tagged_titles.iter().map(String::as_str).collect();
    let previously_set: HashSet<&str> = previously_mirrored.iter().map(String::as_str).collect();
    let to_watch = tagged_titles
        .iter()
        .filter(|t| !previously_set.contains(t.as_str()))
        .cloned()
        .collect();
    let to_unwatch = previously_mirrored
        .iter()
        .filter(|t| !tagged_set.contains(t.as_str()))
        .cloned()
        .collect();
    WatchMirrorPlan {
        to_watch,
        to_unwatch,
    }
}

/// Parses a batched `action=watch`/unwatch response (`titles=A|B` in one
/// request — PRD FR-BM-6's own batching, distinct from `watch_raw`'s single-
/// `title` form the `w` keybinding uses) into per-title outcomes. An entry
/// with no `title` (a response shape this build doesn't expect) is skipped
/// rather than guessed at; an unparseable body is an empty list, same
/// "degrade to nothing rather than crash" posture as `parse_watch_outcome`.
pub fn parse_watch_batch_outcome(body: &[u8]) -> Vec<(String, WatchOutcome)> {
    let Ok(parsed) = serde_json::from_slice::<WatchActionResponse>(body) else {
        return Vec::new();
    };
    parsed
        .watch
        .into_iter()
        .filter_map(|e| {
            if e.title.is_empty() {
                return None;
            }
            if e.unwatched {
                Some((e.title, WatchOutcome::Unwatched))
            } else if e.watched {
                Some((e.title, WatchOutcome::Watched))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- badtoken predicate (PRD §6.2 rule 8) -----------------------------

    #[test]
    fn is_badtoken_response_recognizes_the_standard_error_shape() {
        let body = br#"{"error":{"code":"badtoken","info":"Invalid CSRF token"}}"#;
        assert!(is_badtoken_response(body));
    }

    #[test]
    fn is_badtoken_response_rejects_other_errors_and_success() {
        assert!(!is_badtoken_response(
            br#"{"error":{"code":"notloggedin","info":"..."}}"#
        ));
        assert!(!is_badtoken_response(
            br#"{"watch":[{"ns":0,"title":"X","watched":true}]}"#
        ));
        assert!(!is_badtoken_response(b"not json at all"));
    }

    // ---- watchlistraw / watchlist parse -----------------------------------

    #[test]
    fn parse_watchlistraw_reads_the_top_level_array() {
        let body = br#"{"watchlistraw":[{"ns":0,"title":"Alan Turing"},{"ns":0,"title":"Enigma machine"}]}"#;
        let titles = parse_watchlistraw(body).unwrap();
        assert_eq!(titles, vec!["Alan Turing", "Enigma machine"]);
    }

    #[test]
    fn parse_watchlistraw_of_an_empty_list_is_empty_not_an_error() {
        let body = br#"{"watchlistraw":[]}"#;
        assert_eq!(parse_watchlistraw(body).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn parse_watchlist_changes_reads_the_recent_changes() {
        let body = br#"{"query":{"watchlist":[
            {"title":"Alan Turing","user":"Historian1","timestamp":"2026-07-10T09:00:00Z","comment":"copyedit","revid":5101,"old_revid":5100}
        ]}}"#;
        let changes = parse_watchlist_changes(body).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].title, "Alan Turing");
        assert_eq!(changes[0].user, "Historian1");
        assert_eq!(changes[0].comment.as_deref(), Some("copyedit"));
        assert_eq!(changes[0].revid, 5101);
        assert_eq!(changes[0].old_revid, 5100);
    }

    fn sample_changes() -> Vec<WatchlistChange> {
        vec![
            WatchlistChange {
                title: "Alan Turing".into(),
                user: "Historian1".into(),
                timestamp: "2026-07-10T09:00:00Z".into(),
                comment: Some("old edit".into()),
                revid: 1,
                old_revid: 0,
            },
            WatchlistChange {
                title: "Enigma machine".into(),
                user: "CryptoFan".into(),
                timestamp: "2026-07-14T15:30:00Z".into(),
                comment: Some("newer edit".into()),
                revid: 2,
                old_revid: 1,
            },
            WatchlistChange {
                title: "Alan Turing".into(),
                user: "Historian1".into(),
                timestamp: "2026-07-15T08:00:00Z".into(),
                comment: Some("newest edit".into()),
                revid: 3,
                old_revid: 2,
            },
        ]
    }

    #[test]
    fn changes_since_none_keeps_everything() {
        let changes = sample_changes();
        let all = changes_since(&changes, None);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn changes_since_a_cutoff_keeps_only_strictly_newer_entries() {
        let changes = sample_changes();
        let since = changes_since(&changes, Some("2026-07-10T09:00:00Z"));
        // The entry AT the cutoff is excluded (strictly newer only) —
        // already-seen means already-seen, not "seen again."
        assert_eq!(since.len(), 2);
        assert!(
            since
                .iter()
                .all(|c| c.timestamp.as_str() > "2026-07-10T09:00:00Z")
        );
    }

    #[test]
    fn changes_since_a_cutoff_after_everything_keeps_nothing() {
        let changes = sample_changes();
        let since = changes_since(&changes, Some("2027-01-01T00:00:00Z"));
        assert!(since.is_empty());
    }

    #[test]
    fn newest_timestamp_finds_the_max() {
        assert_eq!(
            newest_timestamp(&sample_changes()),
            Some("2026-07-15T08:00:00Z")
        );
        assert_eq!(newest_timestamp(&[]), None);
    }

    // ---- last-seen store ---------------------------------------------------

    fn temp_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "wikitui-account-test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn last_seen_round_trips_through_save_and_load() {
        let dir = temp_path("lastseen");
        let path = dir.join("watchlist.json");
        assert_eq!(load_last_seen(&path), None, "nothing saved yet");
        save_last_seen(&path, "2026-07-15T08:00:00Z").unwrap();
        assert_eq!(
            load_last_seen(&path).as_deref(),
            Some("2026-07-15T08:00:00Z")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_seen_of_a_missing_file_is_none_not_an_error() {
        let path = temp_path("missing").join("watchlist.json");
        assert_eq!(load_last_seen(&path), None);
    }

    #[test]
    fn last_seen_of_a_corrupt_file_degrades_to_none() {
        let dir = temp_path("corrupt");
        let path = dir.join("watchlist.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_last_seen(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- watched status / watch outcome parse -----------------------------

    #[test]
    fn parse_watched_status_reads_true_and_false() {
        assert!(parse_watched_status(
            br#"{"query":{"pages":[{"title":"Alan Turing","watched":true}]}}"#
        ));
        assert!(!parse_watched_status(
            br#"{"query":{"pages":[{"title":"Alan Turing","watched":false}]}}"#
        ));
        assert!(!parse_watched_status(b"garbage"));
    }

    #[test]
    fn parse_watch_outcome_distinguishes_watched_from_unwatched() {
        assert_eq!(
            parse_watch_outcome(br#"{"watch":[{"ns":0,"title":"X","watched":true}]}"#),
            Some(WatchOutcome::Watched)
        );
        assert_eq!(
            parse_watch_outcome(br#"{"watch":[{"ns":0,"title":"X","unwatched":true}]}"#),
            Some(WatchOutcome::Unwatched)
        );
    }

    #[test]
    fn parse_watch_outcome_of_an_error_body_is_none() {
        assert_eq!(
            parse_watch_outcome(br#"{"error":{"code":"badtoken","info":"x"}}"#),
            None
        );
    }

    // ---- notification count / list parse + badge --------------------------

    #[test]
    fn parse_notif_count_reads_both_buckets() {
        let body = br#"{"query":{"notifications":{"alert":{"count":3},"message":{"count":2}}}}"#;
        let counts = parse_notif_count(body).unwrap();
        assert_eq!(
            counts,
            NotifCounts {
                alert: 3,
                message: 2
            }
        );
        assert_eq!(counts.total(), 5);
    }

    #[test]
    fn format_badge_is_none_at_zero_and_shows_the_total_otherwise() {
        assert_eq!(format_badge(NotifCounts::default()), None);
        assert_eq!(
            format_badge(NotifCounts {
                alert: 2,
                message: 1
            }),
            Some("\u{2709}3".to_string())
        );
    }

    #[test]
    fn parse_notif_list_reads_alerts_and_messages() {
        let body = br#"{"query":{"notifications":{"list":[
            {"id":"101","type":"alert","text":"thanked you","read":false,"timestamp":"2026-07-14T09:00:00Z"},
            {"id":"201","type":"message","text":"new talk message","read":true,"timestamp":"2026-07-13T09:00:00Z"}
        ]}}}"#;
        let list = parse_notif_list(body).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, NotifKind::Alert);
        assert!(!list[0].read);
        assert_eq!(list[1].kind, NotifKind::Message);
        assert!(list[1].read);
    }

    #[test]
    fn counts_from_list_ignores_already_read_entries() {
        let list = vec![
            Notification {
                id: "1".into(),
                kind: NotifKind::Alert,
                text: "a".into(),
                read: false,
                timestamp: String::new(),
            },
            Notification {
                id: "2".into(),
                kind: NotifKind::Alert,
                text: "b".into(),
                read: true,
                timestamp: String::new(),
            },
            Notification {
                id: "3".into(),
                kind: NotifKind::Message,
                text: "c".into(),
                read: false,
                timestamp: String::new(),
            },
        ];
        let counts = counts_from_list(&list);
        assert_eq!(
            counts,
            NotifCounts {
                alert: 1,
                message: 1
            }
        );
    }

    // ---- usercontribs parse (own + other username) -------------------------

    #[test]
    fn parse_usercontribs_reads_title_timestamp_comment_and_sizediff() {
        let body = br#"{"query":{"usercontribs":[
            {"title":"Alan Turing","timestamp":"2026-07-15T08:00:00Z","comment":"fix citation","revid":5103,"sizediff":12},
            {"title":"Enigma machine","timestamp":"2026-07-01T10:00:00Z","comment":"typo","revid":4500,"sizediff":-4}
        ]}}"#;
        let contribs = parse_usercontribs(body).unwrap();
        assert_eq!(contribs.len(), 2);
        assert_eq!(contribs[0].title, "Alan Turing");
        assert_eq!(contribs[0].sizediff, 12);
        assert_eq!(
            contribs[1].sizediff, -4,
            "a negative size delta must survive"
        );
    }

    #[test]
    fn parse_usercontribs_of_an_unknown_user_is_an_empty_list() {
        let body = br#"{"query":{"usercontribs":[]}}"#;
        assert_eq!(parse_usercontribs(body).unwrap(), Vec::new());
    }

    // ---- SEC-1: remote account/social fields are sanitized at parse ---------

    /// `true` for any byte SEC-1 must never let survive to the terminal: a C0
    /// control other than `\n`/`\t`, DEL, a C1 control, or a bidi override/
    /// isolate. Mirrors `corpus_tests::sanitizer_property::is_forbidden`.
    fn has_no_control_bytes(s: &str) -> bool {
        s.chars().all(|c| {
            let u = c as u32;
            !((u < 0x20 && c != '\n' && c != '\t')
                || u == 0x7F
                || (0x80..=0x9F).contains(&u)
                || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        })
    }

    #[test]
    fn parse_usercontribs_strips_hostile_bytes_from_title_and_comment() {
        // `:contribs <user>` is public/unauthenticated — the sharpest surface.
        // OSC-0 window-title set, CSI SGR, and an RLO bidi override, all inside
        // the fields the contributions pane renders verbatim.
        let body = serde_json::json!({
            "query": { "usercontribs": [{
                "title": "Alan\u{1b}]0;pwned\u{7} Turing",
                "timestamp": "2026-07-15T08:00:00Z",
                "comment": "fix \u{1b}[31mcite\u{1b}[0m \u{202e}evil\u{202c}",
                "revid": 5103, "sizediff": 12
            }]}
        })
        .to_string();
        let contribs = parse_usercontribs(body.as_bytes()).unwrap();
        let c = &contribs[0];
        assert!(has_no_control_bytes(&c.title), "title: {:?}", c.title);
        assert!(c.title.contains("Alan"), "inert text must survive");
        let comment = c.comment.as_deref().unwrap();
        assert!(has_no_control_bytes(comment), "comment: {comment:?}");
        assert!(comment.contains("cite") && comment.contains("evil"));
        assert_eq!(c.sizediff, 12, "non-text fields are untouched");
    }

    #[test]
    fn parse_usercontribs_caps_a_pathological_comment_length() {
        let huge = "A".repeat(MAX_SOCIAL_TEXT_CHARS + 5_000);
        let body = format!(
            "{{\"query\":{{\"usercontribs\":[{{\"title\":\"T\",\
             \"timestamp\":\"t\",\"comment\":\"{huge}\",\"revid\":1,\"sizediff\":0}}]}}}}"
        );
        let contribs = parse_usercontribs(body.as_bytes()).unwrap();
        let comment = contribs[0].comment.as_deref().unwrap();
        assert!(
            comment.chars().count() <= MAX_SOCIAL_TEXT_CHARS + "…[truncated]".chars().count(),
            "a multi-field comment must be length-capped, got {}",
            comment.chars().count()
        );
        assert!(comment.ends_with("[truncated]"));
    }

    #[test]
    fn parse_notif_list_strips_hostile_bytes_from_text() {
        let body = serde_json::json!({
            "query": { "notifications": { "list": [{
                "id": "1", "type": "alert",
                "text": "\u{1b}]0;hijack\u{7}You were \u{202e}thanked\u{202c}",
                "read": false, "timestamp": "2026-07-14T09:00:00Z"
            }]}}
        })
        .to_string();
        let list = parse_notif_list(body.as_bytes()).unwrap();
        assert!(
            has_no_control_bytes(&list[0].text),
            "text: {:?}",
            list[0].text
        );
        assert!(list[0].text.contains("thanked"));
    }

    #[test]
    fn parse_watchlist_changes_strips_hostile_bytes_from_every_field() {
        let body = serde_json::json!({
            "query": { "watchlist": [{
                "title": "T\u{1b}[31mitle", "user": "Ba\u{9b}dUser",
                "timestamp": "2026-07-10\u{7}", "comment": "c\u{202e}omment",
                "revid": 1, "old_revid": 0
            }]}
        })
        .to_string();
        let changes = parse_watchlist_changes(body.as_bytes()).unwrap();
        let c = &changes[0];
        assert!(has_no_control_bytes(&c.title));
        assert!(has_no_control_bytes(&c.user));
        assert!(has_no_control_bytes(&c.timestamp));
        assert!(has_no_control_bytes(c.comment.as_deref().unwrap()));
    }

    #[test]
    fn parse_watchlistraw_strips_hostile_bytes_from_titles() {
        let body = serde_json::json!({
            "watchlistraw": [{ "ns": 0, "title": "Ev\u{1b}]0;x\u{7}il" }]
        })
        .to_string();
        let titles = parse_watchlistraw(body.as_bytes()).unwrap();
        assert!(has_no_control_bytes(&titles[0]), "{:?}", titles[0]);
    }

    #[test]
    fn parse_userinfo_options_strips_hostile_bytes_from_skin_and_language() {
        let body = serde_json::json!({
            "query": { "userinfo": { "id": 1, "name": "U",
                "options": { "skin": "vec\u{1b}[31mtor", "language": "e\u{202e}n" } } }
        })
        .to_string();
        let prefs = parse_userinfo_options(body.as_bytes()).unwrap();
        assert!(has_no_control_bytes(prefs.skin.as_deref().unwrap()));
        assert!(has_no_control_bytes(prefs.language.as_deref().unwrap()));
    }

    #[test]
    fn parse_readinglist_entries_strips_hostile_bytes_from_title() {
        let body = serde_json::json!({
            "readinglists": { "entries": [{
                "id": 7, "project": "http://127.0.0.1:8943",
                "title": "Al\u{1b}]0;pwned\u{7}an Turing"
            }]}
        })
        .to_string();
        let entries = parse_readinglist_entries(body.as_bytes());
        assert!(
            has_no_control_bytes(&entries[0].title),
            "{:?}",
            entries[0].title
        );
        assert!(entries[0].title.contains("an Turing"));
    }

    // ---- thank ---------------------------------------------------------------

    #[test]
    fn thank_succeeded_reads_the_success_flag() {
        assert!(thank_succeeded(br#"{"result":{"success":1}}"#));
        assert!(!thank_succeeded(br#"{"result":{"success":0}}"#));
        assert!(!thank_succeeded(
            br#"{"error":{"code":"badtoken","info":"x"}}"#
        ));
    }

    // ---- prefs (read-only userinfo options) --------------------------------

    #[test]
    fn parse_userinfo_options_reads_the_curated_subset() {
        let body = br#"{"query":{"userinfo":{"id":42,"name":"MockWikipedian",
            "options":{"skin":"vector-2022","language":"en"},
            "editcount":1234,"emailauthenticated":"2020-05-01T00:00:00Z"}}}"#;
        let prefs = parse_userinfo_options(body).unwrap();
        assert_eq!(prefs.skin.as_deref(), Some("vector-2022"));
        assert_eq!(prefs.language.as_deref(), Some("en"));
        assert!(prefs.email_confirmed);
        assert_eq!(prefs.editcount, Some(1234));
    }

    #[test]
    fn parse_userinfo_options_without_email_confirmation_is_false() {
        let body = br#"{"query":{"userinfo":{"id":42,"name":"MockWikipedian",
            "options":{"skin":"vector-2022","language":"en"},"editcount":1}}}"#;
        let prefs = parse_userinfo_options(body).unwrap();
        assert!(!prefs.email_confirmed);
    }

    // ---- TokenCache ------------------------------------------------------

    #[test]
    fn token_cache_starts_empty_and_clears_both_kinds() {
        let mut cache = TokenCache::new();
        assert_eq!(cache, TokenCache::default());
        cache.csrf.insert(("".into(), "en".into()), "c".into());
        cache.watch.insert(("".into(), "en".into()), "w".into());
        cache.clear();
        assert_eq!(cache, TokenCache::default());
    }

    #[test]
    fn token_cache_invalidate_only_clears_its_own_kind_and_scope() {
        let mut cache = TokenCache::default();
        cache.csrf.insert(("".into(), "en".into()), "c".into());
        cache.watch.insert(("".into(), "en".into()), "w".into());
        cache.invalidate_csrf("", "en");
        assert!(
            cache.csrf.is_empty(),
            "the csrf token for that scope is gone"
        );
        assert_eq!(
            cache
                .watch
                .get(&("".to_string(), "en".to_string()))
                .map(String::as_str),
            Some("w"),
            "the watch token of the same scope is untouched"
        );
    }

    /// CORR-M7: tokens are keyed by `(wiki, lang)`, so invalidating one
    /// wiki's token leaves another wiki's (same kind) cached token intact —
    /// they are distinct login sessions with distinct tokens.
    #[test]
    fn token_cache_is_keyed_per_wiki_and_lang() {
        let mut cache = TokenCache::default();
        cache.watch.insert(("".into(), "en".into()), "EN_WP".into());
        cache
            .watch
            .insert(("wiktionary".into(), "en".into()), "EN_WKT".into());
        cache.watch.insert(("".into(), "de".into()), "DE_WP".into());

        // Invalidating en-Wikipedia touches only that scope.
        cache.invalidate_watch("", "en");
        assert!(
            !cache
                .watch
                .contains_key(&("".to_string(), "en".to_string()))
        );
        assert_eq!(
            cache
                .watch
                .get(&("wiktionary".to_string(), "en".to_string()))
                .map(String::as_str),
            Some("EN_WKT"),
            "another wiki's token is a different session, kept"
        );
        assert_eq!(
            cache
                .watch
                .get(&("".to_string(), "de".to_string()))
                .map(String::as_str),
            Some("DE_WP"),
            "another lang's token is a different session, kept"
        );
    }

    // ---- FR-BM-5: ReadingLists parse + setup-needed signal ----------------

    #[test]
    fn is_readinglists_not_set_up_recognizes_the_assumed_error_code() {
        let body = br#"{"error":{"code":"readinglists-db-error-not-set-up","info":"not set up"}}"#;
        assert!(is_readinglists_not_set_up(body));
        assert!(!is_readinglists_not_set_up(
            br#"{"error":{"code":"badtoken","info":"x"}}"#
        ));
        assert!(!is_readinglists_not_set_up(
            br#"{"readinglists":{"lists":[]}}"#
        ));
    }

    #[test]
    fn parse_readinglists_setup_reads_the_new_list_id() {
        let body = br#"{"readinglists":{"list":100}}"#;
        assert_eq!(parse_readinglists_setup(body), Some(100));
        assert_eq!(parse_readinglists_setup(b"garbage"), None);
    }

    #[test]
    fn parse_readinglists_reads_every_list() {
        let body = br#"{"readinglists":{"lists":[
            {"id":100,"name":"default","default":true},
            {"id":101,"name":"later","default":false}
        ]}}"#;
        let lists = parse_readinglists(body);
        assert_eq!(lists.len(), 2);
        assert_eq!(lists[0].id, 100);
        assert!(lists[0].default);
        assert!(!lists[1].default);
    }

    #[test]
    fn parse_readinglists_of_an_error_body_is_empty_not_a_panic() {
        assert!(parse_readinglists(br#"{"error":{"code":"x","info":"y"}}"#).is_empty());
    }

    #[test]
    fn default_list_id_prefers_the_flagged_default() {
        let lists = vec![
            ReadingListInfo {
                id: 101,
                name: "later".into(),
                default: false,
            },
            ReadingListInfo {
                id: 100,
                name: "default".into(),
                default: true,
            },
        ];
        assert_eq!(default_list_id(&lists), Some(100));
    }

    #[test]
    fn default_list_id_falls_back_to_the_first_list_when_none_is_flagged() {
        let lists = vec![ReadingListInfo {
            id: 202,
            name: "mystery".into(),
            default: false,
        }];
        assert_eq!(default_list_id(&lists), Some(202));
        assert_eq!(default_list_id(&[]), None);
    }

    #[test]
    fn parse_readinglist_entries_reads_id_project_and_title() {
        let body = br#"{"readinglists":{"entries":[
            {"id":7,"project":"http://127.0.0.1:8943","title":"Alan Turing"}
        ]}}"#;
        let entries = parse_readinglist_entries(body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 7);
        assert_eq!(entries[0].project, "http://127.0.0.1:8943");
        assert_eq!(entries[0].title, "Alan Turing");
    }

    #[test]
    fn parse_readinglists_createentry_reads_the_new_entry() {
        let body = br#"{"readinglists":{"entry":{"id":9,"project":"http://x","title":"Bombe"}}}"#;
        let entry = parse_readinglists_createentry(body).unwrap();
        assert_eq!(entry.id, 9);
        assert_eq!(entry.title, "Bombe");
        assert!(parse_readinglists_createentry(b"garbage").is_none());
    }

    #[test]
    fn readinglists_deleteentry_succeeded_reads_the_flag() {
        assert!(readinglists_deleteentry_succeeded(
            br#"{"readinglists":{"success":true}}"#
        ));
        assert!(!readinglists_deleteentry_succeeded(
            br#"{"readinglists":{"success":false}}"#
        ));
        assert!(!readinglists_deleteentry_succeeded(b"garbage"));
    }

    // ---- FR-BM-5: sync state round-trip ------------------------------------

    #[test]
    fn readinglist_sync_state_round_trips_through_save_and_load() {
        let dir = temp_path("rlsync");
        let path = dir.join("readinglist-sync.json");
        assert_eq!(
            load_readinglist_sync_state(&path),
            ReadingListSyncState::default(),
            "nothing saved yet"
        );
        let state = ReadingListSyncState {
            list_id: Some(100),
            synced: vec![SyncedEntry {
                lang: "en".into(),
                title: "Alan Turing".into(),
                entry_id: 7,
            }],
        };
        save_readinglist_sync_state(&path, &state).unwrap();
        assert_eq!(load_readinglist_sync_state(&path), state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readinglist_sync_state_of_a_corrupt_file_degrades_to_default() {
        let dir = temp_path("rlsync-corrupt");
        let path = dir.join("readinglist-sync.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(
            load_readinglist_sync_state(&path),
            ReadingListSyncState::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- FR-BM-5: the two-way reconcile (PURE) — the conflict policy ------

    fn entry(id: u64, title: &str) -> ReadingListEntry {
        ReadingListEntry {
            id,
            project: "http://127.0.0.1:8943".into(),
            title: title.to_string(),
        }
    }

    fn synced(title: &str, id: u64) -> SyncedEntry {
        SyncedEntry {
            lang: "en".into(),
            title: title.to_string(),
            entry_id: id,
        }
    }

    #[test]
    fn reconcile_pushes_a_brand_new_local_bookmark() {
        let local = vec!["Alan Turing".to_string()];
        let plan = reconcile_reading_list(&local, &[], &[]);
        assert_eq!(plan.push, vec!["Alan Turing"]);
        assert!(plan.pull.is_empty());
        assert!(plan.server_delete.is_empty());
        assert!(plan.matched.is_empty());
    }

    #[test]
    fn reconcile_pulls_a_server_entry_never_seen_before() {
        let server = vec![entry(7, "Enigma machine")];
        let plan = reconcile_reading_list(&[], &server, &[]);
        assert_eq!(plan.pull, vec!["Enigma machine"]);
        assert!(plan.push.is_empty());
        assert!(plan.server_delete.is_empty());
    }

    #[test]
    fn reconcile_deletes_server_side_a_title_removed_locally_after_sync() {
        // Previously synced (entry_id 7), still on the server, but no
        // longer a local bookmark — the reader deleted it locally.
        let server = vec![entry(7, "Alan Turing")];
        let previously = vec![synced("Alan Turing", 7)];
        let plan = reconcile_reading_list(&[], &server, &previously);
        assert_eq!(plan.server_delete, vec![7]);
        assert!(
            plan.pull.is_empty(),
            "a deleted-then-synced title must never be pulled back"
        );
    }

    #[test]
    fn reconcile_leaves_an_unchanged_title_untouched_in_every_list() {
        let local = vec!["Alan Turing".to_string()];
        let server = vec![entry(7, "Alan Turing")];
        let previously = vec![synced("Alan Turing", 7)];
        let plan = reconcile_reading_list(&local, &server, &previously);
        assert!(plan.push.is_empty());
        assert!(plan.pull.is_empty());
        assert!(plan.server_delete.is_empty());
        assert_eq!(plan.matched, vec![("Alan Turing".to_string(), 7)]);
    }

    #[test]
    fn reconcile_matches_a_local_title_the_server_already_has_even_if_never_tracked() {
        // Never in `previously_synced` at all — e.g. another device pushed
        // the exact same title this client already had bookmarked. Must be
        // treated as a match (no push, no pull, no duplicate), not a push.
        let local = vec!["Alan Turing".to_string()];
        let server = vec![entry(42, "Alan Turing")];
        let plan = reconcile_reading_list(&local, &server, &[]);
        assert!(plan.push.is_empty());
        assert!(plan.pull.is_empty());
        assert_eq!(plan.matched, vec![("Alan Turing".to_string(), 42)]);
    }

    #[test]
    fn reconcile_repushes_a_synced_title_the_server_lost_independently() {
        // Was synced (entry_id 7), still a local bookmark, but the server no
        // longer has it — favor never silently losing a local bookmark.
        let local = vec!["Alan Turing".to_string()];
        let previously = vec![synced("Alan Turing", 7)];
        let plan = reconcile_reading_list(&local, &[], &previously);
        assert_eq!(plan.push, vec!["Alan Turing"]);
    }

    #[test]
    fn reconcile_a_mixed_batch_sorts_every_title_into_exactly_one_bucket() {
        let local = vec![
            "New Local".to_string(),
            "Unchanged".to_string(),
            // "Deleted Locally" intentionally absent: it was synced before.
        ];
        let server = vec![
            entry(1, "Unchanged"),
            entry(2, "Deleted Locally"),
            entry(3, "New From Server"),
        ];
        let previously = vec![synced("Unchanged", 1), synced("Deleted Locally", 2)];
        let plan = reconcile_reading_list(&local, &server, &previously);
        assert_eq!(plan.push, vec!["New Local"]);
        assert_eq!(plan.pull, vec!["New From Server"]);
        assert_eq!(plan.server_delete, vec![2]);
        assert_eq!(plan.matched, vec![("Unchanged".to_string(), 1)]);
    }

    #[test]
    fn reconcile_tags_and_notes_survive_because_matched_titles_are_never_touched() {
        // The reconcile function itself never sees tags/notes at all (only
        // titles) — this test locks the invariant that makes "local wins on
        // tags/notes" true: a matched title produces no push/pull/delete
        // action whatsoever, so nothing downstream ever has a reason to
        // overwrite the local `Bookmark`'s tags/notes fields.
        let local = vec!["Alan Turing".to_string()];
        let server = vec![entry(7, "Alan Turing")];
        let previously = vec![synced("Alan Turing", 7)];
        let plan = reconcile_reading_list(&local, &server, &previously);
        assert!(plan.push.is_empty());
        assert!(plan.pull.is_empty());
        assert!(plan.server_delete.is_empty());
    }

    // ---- FR-BM-5: "server wins on order" -----------------------------------

    #[test]
    fn apply_server_order_reorders_local_titles_to_match_the_server() {
        let server_order = vec!["B".to_string(), "A".to_string(), "C".to_string()];
        let local = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        assert_eq!(
            apply_server_order(&server_order, &local),
            vec!["B", "A", "C"]
        );
    }

    #[test]
    fn apply_server_order_appends_a_title_the_server_does_not_know_yet() {
        // "Fresh Push" was just created server-side this round and hasn't
        // been re-listed, so the server order doesn't mention it yet.
        let server_order = vec!["B".to_string(), "A".to_string()];
        let local = vec!["Fresh Push".to_string(), "A".to_string(), "B".to_string()];
        assert_eq!(
            apply_server_order(&server_order, &local),
            vec!["B", "A", "Fresh Push"],
            "an unranked title keeps its relative position, appended last"
        );
    }

    // ---- FR-BM-6: watch-mirror state + diff --------------------------------

    #[test]
    fn watch_mirror_state_round_trips() {
        let dir = temp_path("watchmirror");
        let path = dir.join("watch-mirror.json");
        assert_eq!(
            load_watch_mirror_state(&path),
            WatchMirrorState::default(),
            "nothing saved yet"
        );
        let state = WatchMirrorState {
            mirrored: vec!["Alan Turing".to_string()],
        };
        save_watch_mirror_state(&path, &state).unwrap();
        assert_eq!(load_watch_mirror_state(&path), state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_mirror_diff_a_newly_tagged_title_needs_watching() {
        let tagged = vec!["Alan Turing".to_string()];
        let plan = watch_mirror_diff(&tagged, &[]);
        assert_eq!(plan.to_watch, vec!["Alan Turing"]);
        assert!(plan.to_unwatch.is_empty());
    }

    #[test]
    fn watch_mirror_diff_an_untagged_title_needs_unwatching() {
        let previously = vec!["Alan Turing".to_string()];
        let plan = watch_mirror_diff(&[], &previously);
        assert!(plan.to_watch.is_empty());
        assert_eq!(plan.to_unwatch, vec!["Alan Turing"]);
    }

    #[test]
    fn watch_mirror_diff_an_unchanged_tag_needs_neither() {
        let titles = vec!["Alan Turing".to_string()];
        let plan = watch_mirror_diff(&titles, &titles);
        assert!(plan.to_watch.is_empty());
        assert!(plan.to_unwatch.is_empty());
    }

    #[test]
    fn watch_mirror_diff_never_unwatches_a_title_it_never_mirrored() {
        // "Manually Watched" isn't in `previously_mirrored` at all (the
        // reader watched it by hand, outside this mirror) — must never
        // appear in `to_unwatch` just because it also isn't tagged.
        let tagged: Vec<String> = vec![];
        let previously: Vec<String> = vec![];
        let plan = watch_mirror_diff(&tagged, &previously);
        assert!(plan.to_unwatch.is_empty());
    }

    #[test]
    fn parse_watch_batch_outcome_reads_every_title() {
        let body = br#"{"watch":[
            {"ns":0,"title":"A","watched":true},
            {"ns":0,"title":"B","unwatched":true}
        ]}"#;
        let outcomes = parse_watch_batch_outcome(body);
        assert_eq!(
            outcomes,
            vec![
                ("A".to_string(), WatchOutcome::Watched),
                ("B".to_string(), WatchOutcome::Unwatched),
            ]
        );
    }

    #[test]
    fn parse_watch_batch_outcome_of_garbage_is_empty() {
        assert!(parse_watch_batch_outcome(b"not json").is_empty());
    }
}
