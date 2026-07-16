//! OAuth 2.0 authorization-code + PKCE login against Wikimedia's central
//! OAuth (PRD §5.9 "Auth decision", FR-ACC-1/9, SEC-4).
//!
//! ## Why this shape
//!
//! PRD §5.9 fixes the primary path: wikitui is a **public client** (no client
//! secret — a distributed terminal binary can't keep one), so it uses
//! authorization-code **with PKCE** (RFC 7636). The reader is sent to
//! Meta-Wiki's `/w/rest.php/oauth2/authorize`, approves, and the browser is
//! redirected to a **loopback** URI (`http://127.0.0.1:<port>/callback`) that
//! a short-lived local listener captures ([`LoopbackServer`]). If loopback
//! URIs are rejected by the provider, §5.9's documented fallback is a
//! **manual code-paste** flow ([`parse_manual_input`]) — itself unverified
//! for OAuth 2.0 (SP-2), implemented here and flagged as such.
//!
//! PKCE means no secret ever leaves the machine: a random `code_verifier`
//! ([`Pkce`]) is kept locally, only its SHA-256 `code_challenge` travels in
//! the authorization request, and the verifier is presented at token-exchange
//! time to prove we are the same client. A random `state` parameter defends
//! the callback against CSRF (§5.9 / SEC — a callback whose `state` doesn't
//! match what we sent is rejected, [`Callback`]).
//!
//! ## Token storage (SEC-4) — keychain vs. file
//!
//! Access tokens live 4 h, refresh tokens 365 d, so tokens must persist and
//! refresh transparently ([`Tokens::needs_refresh`], `AuthState::
//! valid_access_token`). SEC-4 ranks storage: **OS keychain primary, 0600
//! file fallback (warned)**. Both are exposed through the [`TokenStore`]
//! trait so the file path — the one this environment can actually exercise —
//! is unit-tested, and the keychain path ([`KeyringTokenStore`], behind the
//! off-by-default `keychain` cargo feature) is swappable behind the same
//! seam. **No passwords are ever stored** — there are none; this is OAuth.
//!
//! In this build the keychain is *unverifiable*: there is no D-Bus Secret
//! Service (nor macOS/Windows keychain) in the container, and the `keychain`
//! feature is off, so [`FileTokenStore`] (with a visible "stored in a file,
//! not a keychain" warning) is the exercised store. That is a documented
//! environment limitation, not a design choice against the keychain.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// PRD Appendix A "Auth (Wikimedia)": Meta-Wiki's central OAuth 2.0
/// endpoints. Config-overridable (§6.2 rule 2 — `[auth] authorize_url` /
/// `token_url`) so a WMF endpoint move, or pointing at a test server, is a
/// config change, not a release.
pub const DEFAULT_AUTHORIZE_URL: &str = "https://meta.wikimedia.org/w/rest.php/oauth2/authorize";
pub const DEFAULT_TOKEN_URL: &str = "https://meta.wikimedia.org/w/rest.php/oauth2/access_token";

/// PRD FR-ACC-9: where `:logout` points the reader for *server-side*
/// revocation (this client only revokes locally — it holds no consumer
/// secret to call a revoke endpoint with).
pub const MANAGE_GRANTS_URL: &str = "https://meta.wikimedia.org/wiki/Special:OAuthManageMyGrants";

/// PRD FR-ACC-1's requested grants, as the OAuth2 `scope` parameter. Grants
/// are ultimately fixed at consumer registration, but the scope is sent so an
/// over-broad consumer is still narrowed to exactly what wikitui uses.
/// **`editmyoptions` is deliberately absent** (FR-ACC-1: "never
/// `editmyoptions`") — wikitui never writes user preferences.
pub const GRANTS: &[&str] = &[
    "basic",
    "viewmywatchlist",
    "editmywatchlist",
    "viewmyprivateinfo",
    "editmyprivateinfo",
];

/// PRD FR-ACC-8: the article-editing grant. **Deliberately not in [`GRANTS`]**
/// — the ordinary login stays read-only, and editing is a *separate, explicit*
/// opt-in (`:enable-editing`) that re-runs the OAuth flow requesting this
/// extra scope. A session that never requested it can never edit (the double
/// opt-in gate's grant half — see `editing::edit_gate`).
pub const EDIT_GRANT: &str = "editpage";

/// Refresh this long *before* the access token's stated expiry rather than at
/// it — a request fired at the instant of expiry can still land a beat late
/// and 401. Five minutes comfortably covers clock skew and in-flight latency
/// against a 4 h token.
pub const REFRESH_SKEW: Duration = Duration::from_secs(300);

/// The `scope` value sent in the ordinary (read-only) authorization request
/// (space-joined per RFC 6749 §3.3).
pub fn scope_param() -> String {
    GRANTS.join(" ")
}

/// PRD FR-ACC-8: the `scope` value for the *editing* re-auth — the read-only
/// [`GRANTS`] plus [`EDIT_GRANT`]. Requested only by `:enable-editing`, never
/// by a plain `:login`.
pub fn scope_param_editing() -> String {
    let mut scopes: Vec<&str> = GRANTS.to_vec();
    scopes.push(EDIT_GRANT);
    scopes.join(" ")
}

// ---------------------------------------------------------------------------
// PKCE (RFC 7636)
// ---------------------------------------------------------------------------

/// A PKCE code-verifier/challenge pair (RFC 7636). The `verifier` is the
/// secret kept locally; the `challenge` (`S256`) is what travels in the
/// authorization request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generates a fresh pair from OS entropy. The verifier is 32 CSPRNG
    /// bytes base64url-encoded → a 43-character string drawn entirely from
    /// the RFC 7636 unreserved set (`A-Z a-z 0-9 - _`), inside the required
    /// 43..=128 length window.
    pub fn generate() -> Self {
        let verifier = random_urlsafe(32);
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// The `S256` challenge for a verifier: `base64url(sha256(ascii(verifier)))`,
/// no padding (RFC 7636 §4.2). Split out so the RFC's own test vector can
/// pin it.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    B64URL.encode(digest)
}

/// `n` bytes of OS entropy, base64url-encoded without padding — the building
/// block for both the PKCE verifier and the CSRF `state`. Uses `getrandom`
/// (a real CSPRNG), never a PRNG or the clock (PRD FR-ACC-1). A failure to
/// read entropy is treated as fatal for the value: there is no safe weaker
/// fallback for a security token, so the caller aborts the login rather than
/// proceeding with a predictable one. In practice `getrandom` on a booted
/// system does not fail.
fn random_urlsafe(n: usize) -> String {
    let mut buf = vec![0u8; n];
    // A panic here would only fire on a kernel with no entropy source at all;
    // there is no correct way to continue an OAuth flow without randomness.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    B64URL.encode(buf)
}

/// A fresh CSRF `state` value (PRD §5.9): 32 bytes of entropy, opaque.
pub fn random_state() -> String {
    random_urlsafe(32)
}

// ---------------------------------------------------------------------------
// Authorization request
// ---------------------------------------------------------------------------

/// Builds the `/oauth2/authorize` URL (PRD §5.9): `response_type=code`, the
/// PKCE `S256` challenge, the CSRF `state`, the `redirect_uri`, `client_id`,
/// and the requested `scope`. Every value is percent-encoded so a redirect
/// URI's own `:`/`/` (and any future scope punctuation) can't corrupt the
/// query.
pub fn build_authorize_url(
    authorize_url: &str,
    client_id: &str,
    challenge: &str,
    state: &str,
    redirect_uri: &str,
) -> String {
    build_authorize_url_scoped(
        authorize_url,
        client_id,
        challenge,
        state,
        redirect_uri,
        &scope_param(),
    )
}

/// [`build_authorize_url`] with an explicit `scope` — the seam the FR-ACC-8
/// editing re-auth uses to request [`scope_param_editing`] (read-only grants
/// plus `editpage`) without a second copy of the URL-building logic. The
/// plain [`build_authorize_url`] is exactly this with the read-only
/// [`scope_param`].
pub fn build_authorize_url_scoped(
    authorize_url: &str,
    client_id: &str,
    challenge: &str,
    state: &str,
    redirect_uri: &str,
    scope: &str,
) -> String {
    let q = |s: &str| urlencoding::encode(s).into_owned();
    format!(
        "{authorize_url}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        q(client_id),
        q(redirect_uri),
        q(scope),
        q(state),
        q(challenge),
    )
}

// ---------------------------------------------------------------------------
// Callback / manual-paste parsing
// ---------------------------------------------------------------------------

/// A parsed authorization-redirect: the one-time `code` plus the `state` that
/// must match what we sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: Option<String>,
}

impl Callback {
    /// PRD §5.9 CSRF check: rejects the callback unless its `state` matches
    /// the one this login generated. A callback with *no* `state` is rejected
    /// too — the authorization request always sends one, so its absence means
    /// the response is not the one we initiated.
    pub fn verify_state(&self, expected: &str) -> Result<()> {
        match self.state.as_deref() {
            Some(got) if got == expected => Ok(()),
            Some(got) => bail!("state mismatch (CSRF guard): expected {expected:?}, got {got:?}"),
            None => bail!("authorization response carried no state parameter (CSRF guard)"),
        }
    }
}

/// Parses the query string of a redirect (`code=…&state=…`, or an
/// `error=…&error_description=…` denial). Accepts a bare query, a full URL,
/// or a `?`-prefixed fragment.
pub fn parse_callback_query(input: &str) -> Result<Callback> {
    let query = query_part(input);
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_desc = None;
    for (k, v) in form_urlencoded_pairs(query) {
        match k.as_str() {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
            "error_description" => error_desc = Some(v),
            _ => {}
        }
    }
    if let Some(err) = error {
        let detail = error_desc.map(|d| format!(": {d}")).unwrap_or_default();
        bail!("authorization denied ({err}{detail})");
    }
    let code = code.ok_or_else(|| anyhow!("no authorization code in redirect"))?;
    if code.is_empty() {
        bail!("empty authorization code in redirect");
    }
    Ok(Callback { code, state })
}

/// PRD §5.9's manual code-paste fallback: the reader pastes *either* the bare
/// authorization code *or* the whole redirect URL they were sent to. A bare
/// token (no `=`, no `?`) is taken as the code directly; anything URL-shaped
/// is parsed like a callback. Returns the same [`Callback`] shape so the
/// caller runs the identical state check and exchange.
pub fn parse_manual_input(input: &str) -> Result<Callback> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("paste the authorization code (or the full redirect URL)");
    }
    // URL-shaped (has a query, or is an http(s) URL): parse like a callback.
    if trimmed.contains('?') || trimmed.contains('=') || looks_like_url(trimmed) {
        return parse_callback_query(trimmed);
    }
    // Otherwise it's a bare code; no state travels with a hand-copied code.
    Ok(Callback {
        code: trimmed.to_string(),
        state: None,
    })
}

fn looks_like_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// The query portion of `input`: everything after the first `?`, or the whole
/// string when there is no `?` (a bare `k=v&…` fragment).
fn query_part(input: &str) -> &str {
    match input.split_once('?') {
        Some((_, q)) => q,
        None => input,
    }
}

/// A tiny `application/x-www-form-urlencoded` splitter (percent-decoding each
/// side, `+` → space) — avoids a dependency for the handful of pairs a
/// callback carries.
fn form_urlencoded_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    urlencoding::decode(&s.replace('+', " "))
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

// ---------------------------------------------------------------------------
// Loopback listener (PRD §5.9)
// ---------------------------------------------------------------------------

/// A short-lived `http://127.0.0.1:<port>/callback` listener bound to an
/// ephemeral port (PRD §5.9). The actual bound port is used to build the
/// `redirect_uri` so the OS picks a free one; the listener serves exactly one
/// request — the browser's redirect — then is dropped.
pub struct LoopbackServer {
    listener: TcpListener,
    port: u16,
}

impl LoopbackServer {
    /// Binds `127.0.0.1:0` (ephemeral). Bound to loopback only — no external
    /// interface ever sees this listener.
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("binding loopback listener")?;
        let port = listener
            .local_addr()
            .context("reading loopback port")?
            .port();
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `redirect_uri` to advertise in the authorization request.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Blocks (up to `timeout`) for the browser's redirect, returns the parsed
    /// [`Callback`], and sends the browser a small "you can close this window"
    /// page. Consumes `self` so the socket is closed on return. Meant to run
    /// inside `tokio::task::spawn_blocking` — it does real blocking I/O.
    pub fn accept_one(self, timeout: Duration) -> Result<Callback> {
        self.listener
            .set_nonblocking(true)
            .context("configuring loopback listener")?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let request_line = read_request_line(&mut stream)?;
                    let target = request_target(&request_line)
                        .ok_or_else(|| anyhow!("malformed loopback request: {request_line:?}"))?;
                    let result = parse_callback_query(&target);
                    write_browser_response(&mut stream, result.is_ok());
                    return result;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        bail!("timed out waiting for the authorization redirect");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).context("accepting loopback connection"),
            }
        }
    }
}

/// Reads the HTTP request head (everything up to the blank line) and returns
/// its first line — all the callback data is in that line's request target.
/// The whole head is consumed rather than just the first line so no bytes are
/// left unread in the socket when it closes: a close with unread receive data
/// makes the OS send a TCP RST, which the browser sees as a reset *before* it
/// reads our response page. Bounded to 8 KiB (SEC-3-style defensive cap) — a
/// callback request is far smaller.
fn read_request_line(stream: &mut std::net::TcpStream) -> Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 512];
    while buf.len() < 8192 {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e).context("reading loopback request"),
        }
    }
    let text = String::from_utf8_lossy(&buf);
    Ok(text.lines().next().unwrap_or("").to_string())
}

/// Extracts the request target from `GET /callback?... HTTP/1.1`.
fn request_target(request_line: &str) -> Option<String> {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    parts.next().map(str::to_string)
}

fn write_browser_response(stream: &mut std::net::TcpStream, ok: bool) {
    let body = if ok {
        "<!doctype html><html><body style=\"font-family:sans-serif\"><h2>wikitui</h2>\
         <p>Authorization received. You can close this window and return to your terminal.</p></body></html>"
    } else {
        "<!doctype html><html><body style=\"font-family:sans-serif\"><h2>wikitui</h2>\
         <p>Authorization failed. Return to your terminal for details.</p></body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// Token model
// ---------------------------------------------------------------------------

/// The raw token endpoint response (PRD §5.9: `access_token` ~4 h,
/// `refresh_token` ~365 d, `expires_in` seconds). `refresh_token` is optional
/// because a refresh response may (per RFC 6749 §6) omit it, in which case the
/// previous one is retained.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// Persisted token state (SEC-4): the access + refresh tokens, an absolute
/// expiry (so a restart can judge freshness without the original
/// `expires_in`), and the logged-in `username` (fetched via `meta=userinfo`,
/// FR-ACC-1) so the indicator can render immediately on load without a
/// network round trip. **Never a password** — there is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Absolute expiry, unix seconds.
    pub expires_at: i64,
    pub username: String,
    /// PRD FR-ACC-8: whether this session was obtained with the `editpage`
    /// grant requested — the local record of the deliberate editing opt-in
    /// (`:enable-editing`). An ordinary read-only login leaves it `false`, so
    /// editing stays gated off. `#[serde(default)]` keeps every pre-existing
    /// `auth.json` (which never had this field) reading back as `false`
    /// (read-only), never crashing on load. On the real API the grant is
    /// ultimately enforced server-side; this flag records the client's own
    /// opt-in so `:edit` refuses locally without a doomed round trip.
    #[serde(default)]
    pub editpage: bool,
}

impl Tokens {
    /// Builds persisted tokens from a fresh exchange/refresh response, folding
    /// the relative `expires_in` into an absolute `expires_at` against `now`.
    /// `prev_refresh` carries a refresh token forward when the response omits
    /// one (RFC 6749 §6). Defaults a missing `expires_in` to 4 h (§5.9's
    /// stated access-token lifetime) rather than treating the token as
    /// immediately stale.
    pub fn from_response(
        resp: &TokenResponse,
        username: String,
        now: i64,
        prev_refresh: Option<&str>,
    ) -> Result<Self> {
        let refresh_token = resp
            .refresh_token
            .clone()
            .or_else(|| prev_refresh.map(str::to_string))
            .ok_or_else(|| anyhow!("token response carried no refresh token"))?;
        let ttl = resp.expires_in.unwrap_or(4 * 3600).max(0);
        Ok(Self {
            access_token: resp.access_token.clone(),
            refresh_token,
            expires_at: now + ttl,
            username,
            // Defaults to read-only; the FR-ACC-8 editing login sets it true
            // on the returned value (`main::finish_login`), and a refresh
            // carries it forward (`AuthState::valid_access_token`).
            editpage: false,
        })
    }

    /// Whether the access token is at (or within [`REFRESH_SKEW`] of) expiry
    /// as of `now` — the signal that `valid_access_token` must refresh before
    /// handing the token out.
    pub fn needs_refresh(&self, now: i64) -> bool {
        now + REFRESH_SKEW.as_secs() as i64 >= self.expires_at
    }
}

// ---------------------------------------------------------------------------
// Token HTTP (exchange / refresh)
// ---------------------------------------------------------------------------

/// A reqwest client for the OAuth token endpoint, carrying NF-NET-2's
/// User-Agent (every request identifies wikitui). Separate from
/// `api::WikiClient`'s client because the token host (Meta-Wiki) is fixed and
/// distinct from the per-`{lang}` wiki host.
pub fn token_http_client(contact: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(crate::api::build_user_agent(contact))
        .timeout(Duration::from_secs(10))
        .build()
        .context("building OAuth HTTP client")
}

/// Exchanges an authorization `code` for tokens (RFC 6749 §4.1.3 +
/// RFC 7636 §4.5: `grant_type=authorization_code`, the `code_verifier`, the
/// public-client `client_id`, and the same `redirect_uri`).
pub async fn exchange_code(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", code_verifier),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
    ];
    post_token(http, token_url, &form).await
}

/// Refreshes tokens (RFC 6749 §6: `grant_type=refresh_token`). Public clients
/// send `client_id` but no secret.
pub async fn refresh_tokens(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    post_token(http, token_url, &form).await
}

async fn post_token(
    http: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    // Built by hand rather than via `RequestBuilder::form` so the body shape
    // is explicit and reqwest's optional url-encoding feature isn't required.
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let resp = http
        .post(token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .context("sending token request")?;
    let status = resp.status();
    let body = resp.text().await.context("reading token response")?;
    if !status.is_success() {
        // The endpoint returns a JSON `{error, error_description}` on failure
        // (RFC 6749 §5.2); surface it verbatim but bounded, never the raw
        // token on a partial success.
        bail!("token endpoint returned {status}: {}", body.trim());
    }
    serde_json::from_str::<TokenResponse>(&body)
        .with_context(|| format!("parsing token response: {}", body.trim()))
}

// ---------------------------------------------------------------------------
// Token store (SEC-4)
// ---------------------------------------------------------------------------

/// The storage seam behind SEC-4's "keychain primary, 0600 file fallback":
/// both backends implement this so the file path is unit-testable and the
/// keychain path is swappable without touching the login flow. `describe`
/// feeds the UI's honest "where your tokens live" line.
pub trait TokenStore: Send {
    fn load(&self) -> Result<Option<Tokens>>;
    fn save(&self, tokens: &Tokens) -> Result<()>;
    fn delete(&self) -> Result<()>;
    /// A short human label of where tokens are kept, e.g. `"a 0600 file
    /// (/…/auth.json)"` or `"the OS keychain"`.
    fn describe(&self) -> String;
    /// Whether this store is the (less-safe) file fallback — drives the
    /// visible SEC-4 warning at login time.
    fn is_file_fallback(&self) -> bool;
}

/// PRD SEC-4's 0600 file fallback. JSON at `auth.json`, `0600` on unix (owner
/// read/write only) so another local user can't read the tokens. Always
/// available; the portable default where no keychain exists.
pub struct FileTokenStore {
    path: PathBuf,
}

impl FileTokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl TokenStore for FileTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let tokens = serde_json::from_str::<Tokens>(&text)
                    .with_context(|| format!("parsing {}", self.path.display()))?;
                Ok(Some(tokens))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(tokens).context("serializing tokens")?;
        write_private(&self.path, json.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {}", self.path.display())),
        }
    }

    fn describe(&self) -> String {
        format!("a 0600 file ({})", self.path.display())
    }

    fn is_file_fallback(&self) -> bool {
        true
    }
}

/// Writes `bytes` to `path` with `0600` permissions on unix, atomically. On
/// unix this goes through [`crate::atomicio::write_atomic_mode`]: the tokens
/// are written to a temp file created at `0600` and then renamed over `path`,
/// so they are never present on disk at a laxer mode for even an instant
/// (CORR-L6). The earlier approach set the mode only on *create* and chmod'd a
/// pre-existing file *after* writing — a window in which a rewritten
/// `auth.json` sat at its old (possibly `0644`) mode holding fresh tokens. The
/// rename also makes the rewrite crash-atomic, so a mid-write crash can never
/// truncate the token file and log the reader out. Non-unix falls back to a
/// plain write (the keychain is the real store there anyway).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        crate::atomicio::write_atomic_mode(path, bytes, 0o600)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// PRD SEC-4's primary store: the OS keychain, via the `keyring` crate. Behind
/// the off-by-default `keychain` feature (see Cargo.toml) — this build cannot
/// exercise it (no secret service in the container), but it compiles against
/// keyring's stable `Entry` API so a desktop packager who enables the feature
/// gets real keychain storage. Tokens are stored as the same JSON blob the
/// file store uses, under one keychain entry.
#[cfg(feature = "keychain")]
pub struct KeyringTokenStore {
    service: String,
    account: String,
}

#[cfg(feature = "keychain")]
impl KeyringTokenStore {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &self.account).context("opening keychain entry")
    }
}

#[cfg(feature = "keychain")]
impl TokenStore for KeyringTokenStore {
    fn load(&self) -> Result<Option<Tokens>> {
        match self.entry()?.get_password() {
            Ok(json) => Ok(Some(
                serde_json::from_str(&json).context("parsing keychain tokens")?,
            )),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading keychain entry"),
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        let json = serde_json::to_string(tokens).context("serializing tokens")?;
        self.entry()?
            .set_password(&json)
            .context("writing keychain entry")
    }

    fn delete(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("deleting keychain entry"),
        }
    }

    fn describe(&self) -> String {
        "the OS keychain".to_string()
    }

    fn is_file_fallback(&self) -> bool {
        false
    }
}

/// The `auth.json` path (PRD §6.4): the state dir, alongside `history.sqlite`
/// and `session.json`. **State, not data** — tokens must never ride the
/// git/syncthing sync the `$XDG_DATA_HOME` stores are designed for (§6.4:
/// data is "git/syncthing-friendly"; tokens are secrets that must stay on one
/// machine). `None` when no platform state directory resolves (matches
/// `history_path`'s own silent-degradation contract).
pub fn auth_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    let dir = dirs
        .state_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dirs.data_dir().join("state"));
    Some(dir.join("auth.json"))
}

/// Selects the token store per SEC-4: the OS keychain when the `keychain`
/// feature is built *and* usable, else the 0600 file at [`auth_path`].
/// Returns `None` only when no file path could be resolved *and* no keychain
/// is available (headless with no state dir) — in which case login is
/// unavailable and the caller says so.
pub fn default_store(path: Option<PathBuf>) -> Option<Box<dyn TokenStore>> {
    #[cfg(feature = "keychain")]
    {
        // Probe the keychain by attempting to open an entry; a backend that
        // isn't present (no secret service) fails here and we fall through to
        // the file store rather than losing login entirely.
        let ks = KeyringTokenStore::new("wikitui", "oauth-tokens");
        if ks.load().is_ok() {
            return Some(Box::new(ks));
        }
    }
    path.map(|p| Box::new(FileTokenStore::new(p)) as Box<dyn TokenStore>)
}

// ---------------------------------------------------------------------------
// AuthState — the live logged-in session
// ---------------------------------------------------------------------------

/// A live logged-in session (the `App.auth` value, PRD FR-ACC-1): the current
/// [`Tokens`], the endpoint/client identity needed to refresh them, an HTTP
/// client for the refresh, and the [`TokenStore`] the refreshed tokens are
/// persisted back to. `valid_access_token` is the one accessor the rest of
/// the app uses — it transparently refreshes a near-expired token before
/// handing it out.
pub struct AuthState {
    tokens: Tokens,
    client_id: String,
    token_url: String,
    http: reqwest::Client,
    store: Box<dyn TokenStore>,
}

impl AuthState {
    pub fn new(
        tokens: Tokens,
        client_id: String,
        token_url: String,
        http: reqwest::Client,
        store: Box<dyn TokenStore>,
    ) -> Self {
        Self {
            tokens,
            client_id,
            token_url,
            http,
            store,
        }
    }

    /// The logged-in username (PRD FR-ACC-1) — drives the status-bar
    /// indicator and `:login`/`:logout` copy.
    pub fn username(&self) -> &str {
        &self.tokens.username
    }

    /// PRD FR-ACC-8: whether this session holds the `editpage` grant (the
    /// grant half of the editing double opt-in gate — see
    /// `editing::edit_gate`). `false` for an ordinary read-only login.
    pub fn has_editpage(&self) -> bool {
        self.tokens.editpage
    }

    /// PRD FR-ACC-8: marks this live session as editing-enabled and persists
    /// it. The symmetric counterpart of [`has_editpage`](Self::has_editpage),
    /// kept as the seam an *upgrade-in-place* editing re-auth would use;
    /// today's `:enable-editing` re-runs the whole OAuth flow and installs a
    /// fresh `AuthState` whose tokens already carry the grant
    /// (`main::finish_login`), so this is exercised by its own unit test
    /// rather than the login path — same "documented, tested, kept" posture as
    /// `WikiCapabilities::full`.
    #[allow(dead_code)]
    pub fn set_editpage(&mut self, editpage: bool) -> Result<()> {
        self.tokens.editpage = editpage;
        self.store.save(&self.tokens)
    }

    /// A valid bearer token, refreshing first if it is at/near expiry (PRD
    /// §5.9 transparent refresh). On a successful refresh the new tokens are
    /// persisted to the store. `now` is passed in (never read from the clock
    /// here) so the refresh decision is testable — the caller supplies
    /// `chrono::Utc::now().timestamp()`.
    pub async fn valid_access_token(&mut self, now: i64) -> Result<String> {
        if self.tokens.needs_refresh(now) {
            let resp = refresh_tokens(
                &self.http,
                &self.token_url,
                &self.client_id,
                &self.tokens.refresh_token,
            )
            .await
            .context("refreshing access token")?;
            let mut refreshed = Tokens::from_response(
                &resp,
                self.tokens.username.clone(),
                now,
                Some(&self.tokens.refresh_token),
            )?;
            // PRD FR-ACC-8: a refresh preserves the editing opt-in — a
            // near-expiry token refresh must not silently downgrade an
            // editing session back to read-only.
            refreshed.editpage = self.tokens.editpage;
            // Persist before returning so a crash right after refresh doesn't
            // lose the new refresh token (the old one may now be invalid).
            self.store
                .save(&refreshed)
                .context("persisting refreshed tokens")?;
            self.tokens = refreshed;
        }
        Ok(self.tokens.access_token.clone())
    }

    /// PRD FR-ACC-9 / FR-PR-4: local logout — deletes the stored tokens. The
    /// caller drops the `AuthState` afterward and links the reader to
    /// [`MANAGE_GRANTS_URL`] for server-side revocation.
    pub fn logout(&self) -> Result<()> {
        self.store.delete()
    }
}

/// Best-effort "open this URL in the system browser" (PRD §5.9). Honors
/// `$WIKITUI_BROWSER` first (a single command that receives the URL as its one
/// argument — also the test seam for driving the loopback), then the
/// platform opener (`xdg-open`/`open`), then `$BROWSER`. Returns whether a
/// launcher was successfully spawned; a `false` return is the headless case
/// where the caller must show the URL for the reader to paste.
pub fn open_browser(url: &str) -> bool {
    let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(cmd) = std::env::var("WIKITUI_BROWSER")
        && !cmd.trim().is_empty()
    {
        candidates.push((cmd, Vec::new()));
    }
    if cfg!(target_os = "macos") {
        candidates.push(("open".to_string(), Vec::new()));
    } else if cfg!(target_os = "windows") {
        candidates.push((
            "cmd".to_string(),
            vec!["/C".to_string(), "start".to_string()],
        ));
    } else {
        candidates.push(("xdg-open".to_string(), Vec::new()));
    }
    if let Ok(browser) = std::env::var("BROWSER")
        && !browser.trim().is_empty()
    {
        candidates.push((browser, Vec::new()));
    }
    for (program, args) in candidates {
        let spawned = std::process::Command::new(&program)
            .args(&args)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PKCE (RFC 7636) ----------------------------------------------

    /// RFC 7636 Appendix B's canonical `S256` test vector — locks the exact
    /// `code_challenge` derivation, the single most security-critical
    /// computation in the flow.
    #[test]
    fn pkce_s256_matches_the_rfc7636_test_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = challenge_for(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_verifier_is_unreserved_and_correct_length() {
        let pkce = Pkce::generate();
        // RFC 7636 §4.1: 43..=128 chars from the unreserved set.
        assert!(
            (43..=128).contains(&pkce.verifier.len()),
            "len {}",
            pkce.verifier.len()
        );
        assert!(
            pkce.verifier.chars().all(|c| c.is_ascii_alphanumeric()
                || c == '-'
                || c == '_'
                || c == '.'
                || c == '~'),
            "verifier has reserved chars: {}",
            pkce.verifier
        );
        assert_eq!(pkce.challenge, challenge_for(&pkce.verifier));
    }

    #[test]
    fn two_generated_pkce_pairs_differ() {
        // A weak/constant RNG would collide; a real CSPRNG effectively never
        // does across two calls.
        assert_ne!(Pkce::generate().verifier, Pkce::generate().verifier);
        assert_ne!(random_state(), random_state());
    }

    // ---- authorize URL ------------------------------------------------

    #[test]
    fn authorize_url_carries_every_required_parameter() {
        let url = build_authorize_url(
            DEFAULT_AUTHORIZE_URL,
            "my-client",
            "CHALLENGE123",
            "STATE456",
            "http://127.0.0.1:5555/callback",
        );
        assert!(url.starts_with(DEFAULT_AUTHORIZE_URL));
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("client_id=my-client"), "{url}");
        assert!(url.contains("code_challenge=CHALLENGE123"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("state=STATE456"), "{url}");
        // redirect_uri percent-encoded (the `:` and `/` must not corrupt the
        // query).
        assert!(
            url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback"),
            "{url}"
        );
        // Scopes present, editmyoptions absent (FR-ACC-1).
        assert!(url.contains("viewmywatchlist"), "{url}");
        assert!(url.contains("editmyprivateinfo"), "{url}");
        assert!(!url.contains("editmyoptions"), "{url}");
    }

    #[test]
    fn scope_param_lists_the_grants_without_editmyoptions() {
        let scope = scope_param();
        for g in GRANTS {
            assert!(scope.contains(g), "{scope} missing {g}");
        }
        assert!(!scope.contains("editmyoptions"));
    }

    #[test]
    fn read_only_scope_never_includes_editpage() {
        // PRD FR-ACC-8: the ordinary login must stay read-only — the edit
        // grant is opt-in only, never requested by a plain `:login`.
        assert!(!scope_param().contains(EDIT_GRANT));
    }

    #[test]
    fn editing_scope_adds_editpage_to_the_read_only_grants() {
        let scope = scope_param_editing();
        for g in GRANTS {
            assert!(scope.contains(g), "{scope} missing {g}");
        }
        assert!(scope.contains(EDIT_GRANT), "{scope}");
        assert!(!scope.contains("editmyoptions"));
        // The editing scope is a strict superset of the read-only one.
        assert!(scope.len() > scope_param().len());
    }

    #[test]
    fn editing_authorize_url_requests_the_editpage_scope() {
        let url = build_authorize_url_scoped(
            DEFAULT_AUTHORIZE_URL,
            "my-client",
            "CHALLENGE",
            "STATE",
            "http://127.0.0.1:5555/callback",
            &scope_param_editing(),
        );
        assert!(url.contains("editpage"), "{url}");
        // A plain read-only authorize URL never does.
        let plain = build_authorize_url(
            DEFAULT_AUTHORIZE_URL,
            "my-client",
            "CHALLENGE",
            "STATE",
            "http://127.0.0.1:5555/callback",
        );
        assert!(!plain.contains("editpage"), "{plain}");
    }

    // ---- state / CSRF -------------------------------------------------

    #[test]
    fn matching_state_passes_and_mismatch_is_rejected() {
        let cb = Callback {
            code: "c".to_string(),
            state: Some("abc".to_string()),
        };
        assert!(cb.verify_state("abc").is_ok());
        assert!(cb.verify_state("different").is_err());
    }

    #[test]
    fn callback_without_state_is_rejected_by_the_csrf_guard() {
        let cb = Callback {
            code: "c".to_string(),
            state: None,
        };
        assert!(cb.verify_state("abc").is_err());
    }

    // ---- callback / manual-paste parsing ------------------------------

    #[test]
    fn parse_callback_query_extracts_code_and_state_from_a_full_url() {
        let cb =
            parse_callback_query("http://127.0.0.1:8080/callback?code=abc123&state=xyz").unwrap();
        assert_eq!(cb.code, "abc123");
        assert_eq!(cb.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parse_callback_query_surfaces_an_oauth_error_denial() {
        let err = parse_callback_query("?error=access_denied&error_description=User+said+no")
            .unwrap_err()
            .to_string();
        assert!(err.contains("access_denied"), "{err}");
        assert!(err.contains("User said no"), "{err}");
    }

    #[test]
    fn parse_callback_query_without_a_code_is_an_error() {
        assert!(parse_callback_query("?state=only").is_err());
    }

    #[test]
    fn manual_paste_accepts_a_bare_code() {
        let cb = parse_manual_input("  plaincode123  ").unwrap();
        assert_eq!(cb.code, "plaincode123");
        assert_eq!(cb.state, None);
    }

    #[test]
    fn manual_paste_accepts_a_full_redirect_url() {
        let cb = parse_manual_input("https://127.0.0.1:9/callback?code=fromurl&state=st").unwrap();
        assert_eq!(cb.code, "fromurl");
        assert_eq!(cb.state.as_deref(), Some("st"));
    }

    #[test]
    fn manual_paste_accepts_a_bare_query_fragment() {
        let cb = parse_manual_input("code=q&state=s").unwrap();
        assert_eq!(cb.code, "q");
        assert_eq!(cb.state.as_deref(), Some("s"));
    }

    #[test]
    fn manual_paste_rejects_empty_input() {
        assert!(parse_manual_input("   ").is_err());
    }

    // ---- token expiry / model -----------------------------------------

    #[test]
    fn tokens_need_refresh_within_the_skew_window() {
        let t = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1_000_000,
            username: "u".into(),
            editpage: false,
        };
        // Well before expiry: fresh.
        assert!(!t.needs_refresh(1_000_000 - 3600));
        // Inside the 5-minute skew: refresh.
        assert!(t.needs_refresh(1_000_000 - 60));
        // Past expiry: refresh.
        assert!(t.needs_refresh(1_000_100));
    }

    #[test]
    fn tokens_from_response_folds_expires_in_and_carries_refresh_forward() {
        let resp = TokenResponse {
            access_token: "new-access".into(),
            refresh_token: None,
            expires_in: Some(14400),
        };
        let t = Tokens::from_response(&resp, "Alice".into(), 1000, Some("old-refresh")).unwrap();
        assert_eq!(t.access_token, "new-access");
        // Response omitted a refresh token → the previous one is retained.
        assert_eq!(t.refresh_token, "old-refresh");
        assert_eq!(t.expires_at, 1000 + 14400);
        assert_eq!(t.username, "Alice");
    }

    #[test]
    fn tokens_from_response_without_any_refresh_token_errors() {
        let resp = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: Some(1),
        };
        assert!(Tokens::from_response(&resp, "u".into(), 0, None).is_err());
    }

    // ---- file token store (SEC-4) -------------------------------------

    fn temp_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "wikitui-auth-test-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn sample_tokens() -> Tokens {
        Tokens {
            access_token: "access-xyz".into(),
            refresh_token: "refresh-abc".into(),
            expires_at: 2_000_000_000,
            username: "Wikipedian".into(),
            editpage: false,
        }
    }

    #[test]
    fn file_store_round_trips_save_load_delete() {
        let dir = temp_path("roundtrip");
        let path = dir.join("auth.json");
        let store = FileTokenStore::new(path.clone());

        assert_eq!(store.load().unwrap(), None, "absent before save");
        store.save(&sample_tokens()).unwrap();
        assert_eq!(store.load().unwrap(), Some(sample_tokens()));
        store.delete().unwrap();
        assert_eq!(store.load().unwrap(), None, "gone after delete");
        // Deleting an already-absent file is a no-op, never an error.
        assert!(store.delete().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_store_writes_0600_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_path("perms");
        let path = dir.join("auth.json");
        let store = FileTokenStore::new(path.clone());
        store.save(&sample_tokens()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.json must be owner-only, got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CORR-L6: rewriting `auth.json` over a pre-existing world-readable file
    /// must end at `0600` — and, structurally, the tokens only ever reach the
    /// real path via a rename of a temp created at `0600`, so they are never
    /// present at the laxer mode mid-write.
    #[cfg(unix)]
    #[test]
    fn rewriting_over_a_pre_existing_0644_file_ends_at_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_path("perms-rewrite");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        // The exact starting state the old create-mode + late-chmod approach
        // left fresh tokens momentarily exposed in.
        std::fs::write(&path, b"stale world-readable tokens").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let store = FileTokenStore::new(path.clone());
        store.save(&sample_tokens()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a rewritten auth.json must be 0600, never left at 0644, got {mode:o}"
        );
        assert_eq!(
            store.load().unwrap(),
            Some(sample_tokens()),
            "the atomic rename must have replaced the file with the new tokens"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_store_describe_names_the_file_fallback() {
        let store = FileTokenStore::new(PathBuf::from("/tmp/x/auth.json"));
        assert!(store.is_file_fallback());
        assert!(store.describe().contains("0600"));
    }

    #[test]
    fn no_password_field_is_ever_serialized() {
        // SEC-4: OAuth means no passwords — the persisted shape must have no
        // slot for one. This locks the serialized keys.
        let json = serde_json::to_string(&sample_tokens()).unwrap();
        assert!(!json.to_lowercase().contains("password"), "{json}");
        assert!(json.contains("access_token"));
        assert!(json.contains("refresh_token"));
    }

    // ---- loopback listener --------------------------------------------

    /// End-to-end loopback capture: bind, connect as a "browser", send the
    /// redirect GET, and assert the server parses code+state and replies with
    /// a browser page. Exercises the real socket path `:login` uses.
    #[test]
    fn loopback_server_captures_code_and_state_from_a_redirect() {
        let server = LoopbackServer::bind().unwrap();
        let port = server.port();
        assert!(server.redirect_uri().contains(&port.to_string()));

        let handle = std::thread::spawn(move || server.accept_one(Duration::from_secs(5)));

        // Simulate the browser hitting the loopback redirect.
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to loopback");
        stream
            .write_all(
                b"GET /callback?code=loopcode&state=loopstate HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("200 OK"), "{response}");
        assert!(response.contains("wikitui"), "{response}");

        let callback = handle.join().unwrap().unwrap();
        assert_eq!(callback.code, "loopcode");
        assert_eq!(callback.state.as_deref(), Some("loopstate"));
    }

    #[test]
    fn loopback_server_times_out_when_no_redirect_arrives() {
        let server = LoopbackServer::bind().unwrap();
        let result = server.accept_one(Duration::from_millis(150));
        assert!(result.is_err());
    }

    #[test]
    fn request_target_extracts_the_path_and_query() {
        assert_eq!(
            request_target("GET /callback?code=x HTTP/1.1").as_deref(),
            Some("/callback?code=x")
        );
        assert_eq!(request_target("garbage").as_deref(), None);
    }

    // ---- token endpoint HTTP (exchange / refresh) ---------------------
    //
    // A throwaway single-shot HTTP server (std sockets, no framework) so the
    // exchange/refresh request *shape* and the refresh *logic* are asserted
    // against a real socket, not a stubbed client — the same posture
    // `api.rs`'s own `background_request_carries_user_agent_and_maxlag` uses.

    #[test]
    fn auth_state_logout_deletes_the_stored_tokens() {
        // PRD FR-ACC-9: local logout removes the tokens from the store.
        let http = token_http_client("c").unwrap();
        let dir = temp_path("logout");
        let path = dir.join("auth.json");
        let store = Box::new(FileTokenStore::new(path.clone())) as Box<dyn TokenStore>;
        store.save(&sample_tokens()).unwrap();
        let auth = AuthState::new(
            sample_tokens(),
            "id".into(),
            "http://127.0.0.1:1".into(),
            http,
            store,
        );
        assert!(path.exists());
        assert_eq!(auth.username(), "Wikipedian");
        auth.logout().unwrap();
        assert!(!path.exists(), "logout must delete the stored tokens");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct CapturedRequest {
        body: String,
        user_agent: String,
    }

    /// Serves `count` sequential requests, each answered with `response_json`,
    /// capturing every request's body + User-Agent. Returns the bound URL and
    /// a receiver of the captured requests.
    fn spawn_token_mock(
        count: usize,
        response_json: &'static str,
        status_line: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..count {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let captured = read_http_request(&mut stream);
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_json}",
                    response_json.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = tx.send(captured);
            }
        });
        (url, rx)
    }

    /// Reads a full HTTP request (headers + Content-Length body).
    fn read_http_request(stream: &mut std::net::TcpStream) -> CapturedRequest {
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        // Read until the header terminator, then enough for the body.
        loop {
            let n = stream.read(&mut tmp).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(hdr_end) = text.find("\r\n\r\n") {
                let content_len = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let body_start = hdr_end + 4;
                if buf.len() >= body_start + content_len {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let user_agent = headers
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase().strip_prefix("user-agent:").map(|_| {
                    l.split_once(':')
                        .map(|x| x.1)
                        .unwrap_or("")
                        .trim()
                        .to_string()
                })
            })
            .unwrap_or_default();
        CapturedRequest {
            body: body.to_string(),
            user_agent,
        }
    }

    #[tokio::test]
    async fn exchange_code_sends_pkce_verifier_and_grant() {
        let (url, rx) = spawn_token_mock(
            1,
            r#"{"access_token":"AT","refresh_token":"RT","expires_in":14400,"token_type":"Bearer"}"#,
            "HTTP/1.1 200 OK",
        );
        let http = token_http_client("mailto:test@example.org").unwrap();
        let resp = exchange_code(
            &http,
            &url,
            "the-client",
            "auth-code-1",
            "the-verifier",
            "http://127.0.0.1:1/callback",
        )
        .await
        .unwrap();
        assert_eq!(resp.access_token, "AT");
        assert_eq!(resp.refresh_token.as_deref(), Some("RT"));

        let req = rx.recv().unwrap();
        // PRD FR-ACC-1: authorization_code grant + the PKCE verifier + the
        // public-client id + the redirect_uri all ride the exchange.
        assert!(
            req.body.contains("grant_type=authorization_code"),
            "{}",
            req.body
        );
        assert!(req.body.contains("code=auth-code-1"), "{}", req.body);
        assert!(
            req.body.contains("code_verifier=the-verifier"),
            "{}",
            req.body
        );
        assert!(req.body.contains("client_id=the-client"), "{}", req.body);
        assert!(req.body.contains("redirect_uri="), "{}", req.body);
        // NF-NET-2: the OAuth client identifies itself too.
        assert!(
            req.user_agent.starts_with("wikitui/"),
            "UA: {}",
            req.user_agent
        );
    }

    #[tokio::test]
    async fn refresh_tokens_sends_the_refresh_grant() {
        let (url, rx) = spawn_token_mock(
            1,
            r#"{"access_token":"AT2","refresh_token":"RT2","expires_in":14400}"#,
            "HTTP/1.1 200 OK",
        );
        let http = token_http_client("mailto:test@example.org").unwrap();
        let resp = refresh_tokens(&http, &url, "the-client", "old-refresh")
            .await
            .unwrap();
        assert_eq!(resp.access_token, "AT2");
        let req = rx.recv().unwrap();
        assert!(
            req.body.contains("grant_type=refresh_token"),
            "{}",
            req.body
        );
        assert!(
            req.body.contains("refresh_token=old-refresh"),
            "{}",
            req.body
        );
        assert!(!req.body.contains("code_verifier"), "{}", req.body);
    }

    #[tokio::test]
    async fn token_endpoint_error_is_surfaced_not_parsed_as_success() {
        let (url, _rx) = spawn_token_mock(
            1,
            r#"{"error":"invalid_grant","error_description":"bad code"}"#,
            "HTTP/1.1 400 Bad Request",
        );
        let http = token_http_client("c").unwrap();
        let err = exchange_code(&http, &url, "id", "code", "verifier", "uri")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid_grant"), "{err}");
    }

    #[tokio::test]
    async fn valid_access_token_refreshes_a_near_expired_token_and_persists() {
        let (url, rx) = spawn_token_mock(
            1,
            r#"{"access_token":"REFRESHED","refresh_token":"NEW_RT","expires_in":14400}"#,
            "HTTP/1.1 200 OK",
        );
        let http = token_http_client("c").unwrap();
        let dir = temp_path("refresh");
        let path = dir.join("auth.json");
        let store = Box::new(FileTokenStore::new(path.clone())) as Box<dyn TokenStore>;

        let now = 1_000_000i64;
        let expired = Tokens {
            access_token: "OLD".into(),
            refresh_token: "OLD_RT".into(),
            expires_at: now + 10, // inside the refresh skew
            username: "Alice".into(),
            editpage: false,
        };
        store.save(&expired).unwrap();
        let mut auth = AuthState::new(expired, "id".into(), url, http, store);

        let token = auth.valid_access_token(now).await.unwrap();
        assert_eq!(token, "REFRESHED", "a near-expired token is refreshed");
        // The refresh actually hit the endpoint.
        let req = rx.recv().unwrap();
        assert!(
            req.body.contains("grant_type=refresh_token"),
            "{}",
            req.body
        );
        // The new tokens are persisted (username carried forward).
        let reloaded = FileTokenStore::new(path.clone()).load().unwrap().unwrap();
        assert_eq!(reloaded.access_token, "REFRESHED");
        assert_eq!(reloaded.refresh_token, "NEW_RT");
        assert_eq!(reloaded.username, "Alice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refresh_preserves_the_editpage_opt_in() {
        // PRD FR-ACC-8: a near-expiry refresh must not downgrade an editing
        // session back to read-only.
        let (url, _rx) = spawn_token_mock(
            1,
            r#"{"access_token":"REFRESHED","refresh_token":"NEW_RT","expires_in":14400}"#,
            "HTTP/1.1 200 OK",
        );
        let http = token_http_client("c").unwrap();
        let dir = temp_path("editrefresh");
        let path = dir.join("auth.json");
        let store = Box::new(FileTokenStore::new(path.clone())) as Box<dyn TokenStore>;
        let now = 1_000_000i64;
        let editing = Tokens {
            access_token: "OLD".into(),
            refresh_token: "OLD_RT".into(),
            expires_at: now + 10,
            username: "Editor".into(),
            editpage: true,
        };
        store.save(&editing).unwrap();
        let mut auth = AuthState::new(editing, "id".into(), url, http, store);
        assert!(auth.has_editpage());
        auth.valid_access_token(now).await.unwrap();
        assert!(auth.has_editpage(), "editpage must survive a refresh");
        let reloaded = FileTokenStore::new(path.clone()).load().unwrap().unwrap();
        assert!(reloaded.editpage, "persisted tokens keep the opt-in");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn set_editpage_upgrades_and_persists_the_session() {
        // PRD FR-ACC-8: the editing re-auth marks the live session and its
        // stored tokens editing-enabled.
        let http = token_http_client("c").unwrap();
        let dir = temp_path("setedit");
        let path = dir.join("auth.json");
        let store = Box::new(FileTokenStore::new(path.clone())) as Box<dyn TokenStore>;
        let now = 1_000_000i64;
        let read_only = Tokens {
            access_token: "AT".into(),
            refresh_token: "RT".into(),
            expires_at: now + 4 * 3600,
            username: "Reader".into(),
            editpage: false,
        };
        store.save(&read_only).unwrap();
        let mut auth = AuthState::new(
            read_only,
            "id".into(),
            "http://127.0.0.1:1".into(),
            http,
            store,
        );
        assert!(!auth.has_editpage());
        auth.set_editpage(true).unwrap();
        assert!(auth.has_editpage());
        let reloaded = FileTokenStore::new(path.clone()).load().unwrap().unwrap();
        assert!(reloaded.editpage);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn valid_access_token_returns_a_fresh_token_without_refreshing() {
        // No mock needed: a fresh token must not touch the network at all.
        let http = token_http_client("c").unwrap();
        let store = Box::new(FileTokenStore::new(temp_path("fresh").join("auth.json")))
            as Box<dyn TokenStore>;
        let now = 1_000_000i64;
        let fresh = Tokens {
            access_token: "STILL_GOOD".into(),
            refresh_token: "RT".into(),
            expires_at: now + 4 * 3600,
            username: "Bob".into(),
            editpage: false,
        };
        let mut auth = AuthState::new(fresh, "id".into(), "http://127.0.0.1:1".into(), http, store);
        assert_eq!(auth.valid_access_token(now).await.unwrap(), "STILL_GOOD");
    }

    #[tokio::test]
    async fn valid_access_token_surfaces_a_refresh_failure() {
        let (url, _rx) = spawn_token_mock(
            1,
            r#"{"error":"invalid_grant"}"#,
            "HTTP/1.1 400 Bad Request",
        );
        let http = token_http_client("c").unwrap();
        let store = Box::new(FileTokenStore::new(temp_path("reffail").join("auth.json")))
            as Box<dyn TokenStore>;
        let now = 1_000_000i64;
        let expired = Tokens {
            access_token: "OLD".into(),
            refresh_token: "OLD_RT".into(),
            expires_at: now - 1,
            username: "u".into(),
            editpage: false,
        };
        let mut auth = AuthState::new(expired, "id".into(), url, http, store);
        // Refresh failure surfaces as an error — the caller logs the reader
        // out gracefully (PRD §7 "Login: OAuth failure/expiry").
        assert!(auth.valid_access_token(now).await.is_err());
    }
}
