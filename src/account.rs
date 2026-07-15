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

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::api::WikiClient;

// ---------------------------------------------------------------------------
// CSRF / watch tokens (PRD §6.2 rule 8)
// ---------------------------------------------------------------------------

/// Caches the two token kinds this build's writes need (`csrf` for thank/
/// echomarkread, `watch` for watch/unwatch) so a session fetches each at
/// most once — until a badtoken forces a refetch. Lives on `App` for the
/// whole process lifetime; cleared on logout (there is nothing to reuse
/// against a session that no longer exists).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TokenCache {
    csrf: Option<String>,
    watch: Option<String>,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached CSRF token, fetching it if this is the first write of the
    /// session.
    pub async fn csrf_token(
        &mut self,
        client: &WikiClient,
        lang: &str,
        access_token: &str,
    ) -> Result<String> {
        if let Some(t) = &self.csrf {
            return Ok(t.clone());
        }
        let t = client.fetch_token(lang, access_token, "csrf").await?;
        self.csrf = Some(t.clone());
        Ok(t)
    }

    /// The cached watch token, fetching it if this is the first watch/
    /// unwatch of the session.
    pub async fn watch_token(
        &mut self,
        client: &WikiClient,
        lang: &str,
        access_token: &str,
    ) -> Result<String> {
        if let Some(t) = &self.watch {
            return Ok(t.clone());
        }
        let t = client.fetch_token(lang, access_token, "watch").await?;
        self.watch = Some(t.clone());
        Ok(t)
    }

    /// Drops the cached CSRF token — called after a `badtoken` response, so
    /// the next `csrf_token` call fetches a fresh one instead of handing
    /// back the same stale value.
    pub fn invalidate_csrf(&mut self) {
        self.csrf = None;
    }

    /// The watch-token counterpart of [`invalidate_csrf`](Self::invalidate_csrf).
    pub fn invalidate_watch(&mut self) {
        self.watch = None;
    }

    /// PRD FR-ACC-9 / logout: nothing cached here can outlive the session it
    /// was minted for.
    pub fn clear(&mut self) {
        self.csrf = None;
        self.watch = None;
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
    Ok(parsed.watchlistraw.into_iter().map(|e| e.title).collect())
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
    Ok(parsed.query.map(|q| q.watchlist).unwrap_or_default())
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(&WatchlistState {
        last_seen: Some(last_seen.to_string()),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(path, json)
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
    Ok(parsed
        .query
        .map(|q| q.notifications.list)
        .unwrap_or_default())
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
    Ok(parsed.query.map(|q| q.usercontribs).unwrap_or_default())
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
        skin: info.options.as_ref().and_then(|o| o.skin.clone()),
        language: info.options.as_ref().and_then(|o| o.language.clone()),
        email_confirmed: info.emailauthenticated.is_some(),
        editcount: info.editcount,
    })
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
        cache.csrf = Some("c".to_string());
        cache.watch = Some("w".to_string());
        cache.clear();
        assert_eq!(cache, TokenCache::default());
    }

    #[test]
    fn token_cache_invalidate_only_clears_its_own_kind() {
        let mut cache = TokenCache {
            csrf: Some("c".to_string()),
            watch: Some("w".to_string()),
        };
        cache.invalidate_csrf();
        assert_eq!(cache.csrf, None);
        assert_eq!(cache.watch.as_deref(), Some("w"));
    }
}
