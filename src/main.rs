mod api;
mod app;
mod attribution;
mod bookmark_export;
mod bookmarks;
mod cache;
mod cite;
mod cli;
mod command;
mod config;
mod crashguard;
mod doc;
mod doctor;
mod fetch_queue;
mod fuzzy;
mod graphics;
mod hints;
mod history;
mod image;
mod jsonl;
mod layout;
mod netqueue;
mod prefetch;
mod random;
mod research;
mod sanitize;
mod saved;
mod saved_export;
mod search_ops;
mod startpage;
mod tab;
mod target;
mod theme;
mod ui;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedSender};

use api::{SearchResult, TitleSuggestion, WikiClient};
use app::{App, BulkSaveRequest, Mode, PageSource, PendingReload};
use bookmarks::ReadLaterEntry;
use cache::{PageCache, RevalidateAction, SwrDecision};
use cite::CiteStyle;
use cli::{Cli, Commands, ConfigAction};
use config::ConfigContext;
use crashguard::{SuspendedTerminal, TerminalGuard};
use saved::{LinkSummary, Tier};
use tab::{HistoryEntry, TabId};
use theme::Theme;

/// How often the loop wakes up while the Search prompt is open, purely so
/// the typeahead debounce timer (PRD FR-SR-1) has a chance to fire even
/// when the user pauses mid-query with no new keystroke arriving. Every
/// other mode still blocks in `event::read()` indefinitely (PRD FR-ACS-2:
/// no gratuitous redraws/CPU use while idle) — this cost is scoped to
/// Search mode alone, not paid while reading.
const TYPEAHEAD_POLL: Duration = Duration::from_millis(30);

/// How many typeahead rows to request/show (PRD FR-SR-1).
const TYPEAHEAD_LIMIT: u32 = 10;

/// How often the loop wakes up while a background revalidation (PRD
/// FR-OFF-2) is in flight, so its "updated — r to reload" notice can
/// appear without the reader having to press a key first. Coarser than
/// `TYPEAHEAD_POLL` since nothing here is debounced against a keystroke —
/// just waiting for a background task — and scoped the same way: only paid
/// while `App::pending_revalidations` is nonzero, never in plain Reading
/// mode with nothing in flight (PRD FR-ACS-2's 0%-idle-CPU property).
const REVALIDATE_POLL: Duration = Duration::from_millis(100);

/// One completed (or failed) typeahead request, tagged with the query it
/// answers so the receiver can drop it if it's gone stale (`app::
/// typeahead_is_current`) — PRD FR-SR-1's in-flight cancellation.
struct TypeaheadOutcome {
    query: String,
    result: std::result::Result<Vec<TitleSuggestion>, String>,
}

/// One completed (or failed) background revalidation (PRD FR-OFF-2),
/// tagged with the originating tab's id (FR-TB-3) plus the (lang, title) it's
/// for, so the receiver can route it to the exact tab that requested it —
/// dropping it gracefully if that tab has since closed — and confirm the tab
/// still shows that article before arming a reload notice.
struct RevalidationOutcome {
    tab_id: TabId,
    lang: String,
    title: String,
    /// `None` on any network failure along the way (bare-metadata call or
    /// the follow-up fetch): NF-NET-8's "revalidation failure is silent" —
    /// the cached copy already on screen simply stands, nothing is logged
    /// or shown.
    result: Option<RevalidationResult>,
}

/// One completed (or failed) background-tab fetch (PRD FR-TB-3): the `Ctrl-Enter`
/// "open in background tab" path fetches off the event loop and delivers the
/// result here, keyed to the tab it belongs to.
struct TabLoadOutcome {
    tab_id: TabId,
    lang: String,
    title: String,
    result: std::result::Result<FetchOutcome, String>,
}

/// One completed (or failed) inline-image fetch+decode (PRD FR-RD-8),
/// delivered off the event loop like article/typeahead loads so decoding
/// never blocks the UI. `decoded` is `None` on any fetch/decode failure — the
/// alt-text placeholder simply stands (never logged or retried in-session).
struct ImageOutcome {
    src: String,
    decoded: Option<crate::image::DecodedImage>,
}

/// One completed (or failed) saved-page fetch (PRD FR-OFF-4..5), delivered
/// off the event loop like every other background fetch. The main loop applies
/// it via `apply_save_outcome`, which owns `App::saved`.
struct SaveOutcome {
    /// `Err(message)` on any network/parse failure fetching this target — the
    /// save of *this* page fails, but a bulk run continues with the rest.
    result: std::result::Result<SaveFetched, String>,
    title: String,
}

/// Everything fetched for one saved page, ready for `SavedPages::save` to pin
/// on the main thread (the store is not `Send`-shared).
struct SaveFetched {
    lang: String,
    title: String,
    revid: u64,
    tier: Tier,
    html: String,
    /// `(src, bytes)` pairs for T1+ thumbnails (empty for T0).
    thumbs: Vec<(String, Vec<u8>)>,
    /// Link-target summaries for T2 (empty otherwise).
    summaries: Vec<LinkSummary>,
    source_note: String,
}

/// One completed (or failed) `morelike:` fetch for the Related panel (PRD
/// FR-SR-6), delivered off the event loop like every other lazy fetch (see
/// `fire_related`) so opening the panel never blocks. Tagged with the
/// `(lang, title)` it answers so a result for an article the reader has
/// since navigated away from is still cache-worthy (`App::deliver_related`)
/// without pretending to update a panel that's no longer showing it.
struct RelatedOutcome {
    lang: String,
    title: String,
    result: std::result::Result<Vec<SearchResult>, String>,
}

enum RevalidationResult {
    /// The bare-metadata call reported the same revid already cached:
    /// nothing to fetch, just extend the TTL window silently.
    Unchanged,
    /// A different (or newly-discovered) revid: fresh HTML fetched and
    /// ready to write into L2.
    Changed {
        html: String,
        revid: u64,
        etag: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // `wikitui config doctor` runs before anything else touches the
    // network, the cache, or the terminal (PRD §6.7: plain stdout, no TUI
    // init) — it's a standalone diagnostic, not a mode of the reader.
    if let Some(Commands::Config {
        action: ConfigAction::Doctor,
    }) = &cli.command
    {
        let cli_overrides = cli_overrides_from(&cli);
        let env_overrides = config::EnvOverrides::from_process_env();
        let config_path =
            config::resolve_config_path(cli.config.clone(), std::env::var("WIKITUI_CONFIG").ok());
        let resolved = config::resolve(&cli_overrides, &env_overrides, config_path.as_deref());
        std::process::exit(doctor::run(&resolved));
    }

    let mut cli_overrides = cli_overrides_from(&cli);

    // The TITLE argument may be a full wikipedia.org URL or a
    // lang-prefixed title (FR-CS-6); either overrides --lang, and — same
    // precedence-independent priority as before this config system
    // existed — config file and env too.
    if let Some(raw) = &cli.title {
        let target = target::parse(raw);
        if let Some(lang) = target.lang {
            cli_overrides.lang = Some(lang);
        }
        cli.title = Some(target.title);
    }

    // No network, no TTY — just format the saved bibliography and exit.
    if let Some(style_name) = &cli.export_bibliography {
        let style = cite::CiteStyle::by_name(style_name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown citation style {:?} — choose one of: {}",
                style_name,
                cite::CiteStyle::NAMES.join(", ")
            )
        })?;
        let store = research::ResearchStore::load();
        print!("{}", cite::format_bibliography(&store.citations, style));
        return Ok(());
    }

    let env_overrides = config::EnvOverrides::from_process_env();
    let config_path =
        config::resolve_config_path(cli.config.clone(), std::env::var("WIKITUI_CONFIG").ok());
    let resolved = config::resolve(&cli_overrides, &env_overrides, config_path.as_deref());

    // §6.7: unknown keys and rejected values warn, never crash — printed
    // once, before any terminal state change (raw mode/the alternate
    // screen would otherwise swallow or mangle them). `config doctor`
    // reports the same issues in its own format, so this only fires on
    // the normal run path.
    for issue in &resolved.issues {
        eprintln!("wikitui: config: {}", issue.message);
    }
    if let Some(summary) = &resolved.migration_summary {
        eprintln!("wikitui: config: {summary}");
    }

    // NF-NET-2: the default contact needs no rebuild of the UA string; only a
    // configured `[network] contact` takes the `with_contact` path.
    let client = if resolved.network_contact.source == config::Source::Default {
        WikiClient::new(resolved.base_url_template.value.clone())?
    } else {
        WikiClient::with_contact(
            resolved.base_url_template.value.clone(),
            &resolved.network_contact.value,
        )?
    };
    let page_cache = PageCache::open(
        resolved.cache_max_mb.value.saturating_mul(1024 * 1024),
        resolved.cache_fresh_ttl_hours.value.saturating_mul(3600),
        resolved
            .cache_force_refetch_days
            .value
            .saturating_mul(86_400),
    );

    if cli.dump {
        let title = cli
            .title
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--dump requires an article title"))?;
        // A one-shot linear render (PRD FR-RD-12) has no event loop to
        // deliver a background revalidation's result into, so it never
        // spawns one — the reader gets whatever's freshest synchronously
        // (fresh cache, or a network round trip on a stale/missing one).
        let outcome = fetch_page(&client, &page_cache, &resolved.lang.value, &title).await?;
        let document = doc::parse_article_html(&title, &outcome.html);
        print!("{}", doc::render_plain(&document));
        return Ok(());
    }

    // Already validated during resolution (unknown names fall back to the
    // default with a warning above), so these can't fail here — the
    // `unwrap_or_else` is a belt-and-braces guard, not an expected path.
    let theme = Theme::by_name(&resolved.theme.value).unwrap_or_else(Theme::terminal);
    let cite_style = CiteStyle::by_name(&resolved.cite_style.value).unwrap_or(CiteStyle::Apa);
    let no_color = no_color_active();
    let accessible = accessible_active();

    let config_ctx = ConfigContext {
        cli: cli_overrides,
        env: env_overrides,
        config_path,
    };

    // A flag rather than a direct callback: the event loop blocks on
    // `event::read()`, so a background signal task can't reach into it —
    // it can only leave a note that's picked up on the next loop turn
    // (PRD §6.7: SIGHUP does the same reload as `:config reload`).
    let reload_flag = Arc::new(AtomicBool::new(false));
    spawn_sighup_listener(Arc::clone(&reload_flag));

    // Installed before the terminal is touched at all (PRD §7's "Crash"
    // row): a panic during `TerminalGuard::enter` itself has nothing to
    // restore yet, but anything after does, and the hook must already be in
    // place for it.
    crashguard::install_panic_hook();
    // `TerminalGuard` (not a bare `init_terminal`/`restore_terminal` pair)
    // owns raw mode and the alternate screen for the rest of this function:
    // its `Drop` restores both no matter how this scope is left — the
    // normal path below, an early `?` a future edit might add, or `run`
    // itself returning early — which a call-restore-after-the-fact pattern
    // can't guarantee. See `crashguard`'s module doc comment for why the
    // panic hook above is still separately necessary.
    let mut guard = TerminalGuard::enter()?;
    run(
        guard.terminal(),
        &client,
        &page_cache,
        cli,
        resolved.lang.value,
        theme,
        no_color,
        accessible,
        resolved.measure.value,
        resolved.ambiguous_wide.value,
        cite_style,
        resolved.readlater_auto_dequeue.value,
        resolved.history_retention_days.value,
        resolved.images.value,
        resolved.include_nonfree.value,
        resolved.startpage.value,
        prefetch_config_from(&resolved.prefetch),
        resolved.prefetch.enabled.value,
        config_ctx,
        reload_flag,
    )
    .await
}

/// Build the [`netqueue::SubstrateConfig`] from resolved `[prefetch]` config
/// (PRD §5.8 / FR-PF-1/5). The jitter seed is time-derived in production so
/// two processes' backoff schedules don't synchronize (tests pin their own).
fn prefetch_config_from(pf: &config::ResolvedPrefetch) -> netqueue::SubstrateConfig {
    let jitter_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED_1234)
        | 1;
    netqueue::SubstrateConfig {
        daily_byte_budget: pf.daily_mb.value.saturating_mul(1024 * 1024),
        hourly_request_budget: pf.hourly_requests.value.min(u32::MAX as u64) as u32,
        metered: netqueue::Metered::parse(&pf.metered.value).unwrap_or(netqueue::Metered::Reduced),
        top_n: pf.top_n.value as usize,
        weights: netqueue::RankWeights {
            lead: pf.weight_lead.value,
            pageviews: pf.weight_pageviews.value,
            affinity: pf.weight_affinity.value,
        },
        jitter_seed,
        ..netqueue::SubstrateConfig::default()
    }
}

/// The CLI-flag layer of PRD §6.7's precedence chain, read off the parsed
/// `Cli` once so both the `config doctor` path and the normal run path
/// build it identically.
fn cli_overrides_from(cli: &Cli) -> config::CliOverrides {
    config::CliOverrides {
        lang: cli.lang.clone(),
        theme: cli.theme.clone(),
        measure: cli.measure,
        ambiguous_width: cli.ambiguous_width,
        cite_style: cli.cite_style.clone(),
    }
}

/// PRD FR-TH-5: NO_COLOR, when present and non-empty, strips color from
/// every theme regardless of which one is selected. Shared with `doctor`'s
/// capability report so both agree on what "active" means.
pub(crate) fn no_color_active() -> bool {
    std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty())
}

/// PRD FR-ACS-6: `ACCESSIBLE=1` (any non-empty, non-"0" value) implies
/// linear-leaning behavior — in this chunk, collapse-to-list tables (the
/// layout's accessible path) regardless of terminal width. A standard the
/// PRD honors alongside `NO_COLOR`/`CLICOLOR_FORCE` (§6.7 precedence).
pub(crate) fn accessible_active() -> bool {
    std::env::var("ACCESSIBLE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// PRD §7 / the v0.5 milestone's "crash-safe terminal restore": whether a
/// deliberate test panic was requested. Gated on an env var rather than a
/// hidden keybinding so a pty-based test can trigger the real crash path —
/// terminal restore, crash report file, stderr message — just by setting
/// the child process's environment, with zero risk of an accidental
/// production panic path reachable by any key sequence a real user could
/// press. Kept in the tree deliberately (not stripped before commit) as a
/// documented testing/doctor-adjacent aid, the same category of
/// intentional escape hatch as `WIKITUI_BASE_URL` or `WIKITUI_ANIMATIONS`.
fn debug_panic_requested() -> bool {
    std::env::var("WIKITUI_DEBUG_PANIC").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Listens for SIGHUP and sets `flag`, picked up by the event loop on its
/// next turn (PRD §6.7's live-reload). No-op on non-Unix targets — there's
/// no SIGHUP there; `:config reload` still works everywhere.
#[cfg(unix)]
fn spawn_sighup_listener(flag: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let Ok(mut stream) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            return;
        };
        loop {
            if stream.recv().await.is_none() {
                return;
            }
            flag.store(true, Ordering::SeqCst);
        }
    });
}

#[cfg(not(unix))]
fn spawn_sighup_listener(_flag: Arc<AtomicBool>) {}

/// The result of the cache-aware fetch (PRD FR-OFF-2): the HTML to render
/// right now, where it came from (drives the ●◐○ glyph), the revid it's
/// at, and — when the serve was a stale-but-within-backstop cache hit —
/// the cached revid a background revalidation should compare against.
struct FetchOutcome {
    html: String,
    source: PageSource,
    revid: u64,
    /// `Some(cached_revid)` exactly when the caller should spawn a
    /// background revalidation (PRD FR-OFF-2's stale-while-revalidate);
    /// `None` for a fresh cache hit (nothing to check yet) or a live/
    /// offline network result (already as current as this session can
    /// make it).
    revalidate: Option<u64>,
}

/// The cache-aware fetch (PRD FR-OFF-2's serve policy): render whatever
/// cached copy exists immediately — instantly for a fresh one, and for a
/// stale-but-within-backstop one too, deferring the staleness check to a
/// background revalidation the caller spawns. Only a cache miss, or an
/// entry past the force-refetch backstop (`cache::SwrDecision::
/// ForceRefetch` — "treated as absent on open"), goes to the network
/// inline here. If the network fails, any cached copy — however stale —
/// beats the error, which is what makes offline reading work.
async fn fetch_page(
    client: &WikiClient,
    cache: &PageCache,
    lang: &str,
    title: &str,
) -> Result<FetchOutcome> {
    let cached = cache.get(lang, title);
    if let Some(page) = &cached {
        match cache.swr_decision(page.age_secs) {
            SwrDecision::Fresh => {
                return Ok(FetchOutcome {
                    html: page.html.clone(),
                    source: PageSource::Cached {
                        age_secs: page.age_secs,
                    },
                    revid: page.revid,
                    revalidate: None,
                });
            }
            SwrDecision::RevalidateInBackground => {
                return Ok(FetchOutcome {
                    html: page.html.clone(),
                    source: PageSource::Cached {
                        age_secs: page.age_secs,
                    },
                    revid: page.revid,
                    revalidate: Some(page.revid),
                });
            }
            SwrDecision::ForceRefetch => {
                // Falls through to the network-first path below; `cached`
                // remains available there as the offline-error fallback.
            }
        }
    }
    match client.fetch_article_html(lang, title).await {
        Ok(fetched) => {
            cache.put(
                lang,
                title,
                &fetched.html,
                fetched.revid,
                fetched.etag.as_deref(),
            );
            Ok(FetchOutcome {
                html: fetched.html,
                source: PageSource::Live,
                revid: fetched.revid,
                revalidate: None,
            })
        }
        Err(network_error) => match cached {
            Some(page) => Ok(FetchOutcome {
                html: page.html,
                source: PageSource::Offline {
                    age_secs: page.age_secs,
                },
                revid: page.revid,
                revalidate: None,
            }),
            None => Err(network_error),
        },
    }
}

/// Spawns the background staleness check (PRD FR-OFF-2): a cheap
/// bare-metadata call, then — only if the revid actually changed — a full
/// HTML re-fetch. Mirrors `fire_typeahead`'s shape (never runs inline in
/// the loop) but has no debounce timer to wait on; it fires right after the
/// stale-cache-hit fetch that requested it.
/// Returns whether a revalidation was newly enqueued (`false` = coalesced with
/// an identical in-flight one, NF-NET-5) so the caller only bumps
/// `pending_revalidations` when a completion is actually coming.
fn fire_revalidation(
    app: &App,
    client: &WikiClient,
    tab_id: TabId,
    lang: String,
    title: String,
    cached_revid: u64,
    tx: &UnboundedSender<RevalidationOutcome>,
) -> bool {
    // PRD FR-OFF-2 migrated onto the substrate (NF-NET-1): the worker runs it
    // at revalidation priority, after the foreground gate, serially. The
    // `BgExecutor` still delivers the `RevalidationOutcome` over `tx`, so the
    // loop applies it exactly as before. Two tabs revalidating the same
    // (lang, title) coalesce to one fetch — the outcome routes to the first
    // requester's tab; the cache write benefits both (documented limitation).
    if let Some(handle) = &app.prefetch {
        return handle.enqueue_revalidation(tab_id, lang, title, cached_revid);
    }
    // Fallback for any path with no substrate installed (unit tests, `--dump`):
    // the original ad-hoc spawn, so behavior is unchanged there.
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = revalidate(&client, &lang, &title, cached_revid).await;
        let _ = tx.send(RevalidationOutcome {
            tab_id,
            lang,
            title,
            result,
        });
    });
    true
}

/// Spawns the fetch for a background tab (PRD FR-TB-3): runs the full
/// cache-aware `fetch_page` off the event loop so opening a link into a
/// background tab never blocks the reader, and reports the outcome tagged
/// with the tab's id. The cache is cheap to clone (a directory path plus a
/// few counters).
fn fire_background_load(
    client: &WikiClient,
    cache: &PageCache,
    tab_id: TabId,
    lang: String,
    title: String,
    tx: &UnboundedSender<TabLoadOutcome>,
) {
    let client = client.clone();
    let cache = cache.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = fetch_page(&client, &cache, &lang, &title)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(TabLoadOutcome {
            tab_id,
            lang,
            title,
            result,
        });
    });
}

/// The actual bare-metadata-then-maybe-fetch sequence, isolated from the
/// channel plumbing above so it's a plain `async fn` returning `None` on
/// any network failure (NF-NET-8: revalidation failure is silent).
async fn revalidate(
    client: &WikiClient,
    lang: &str,
    title: &str,
    cached_revid: u64,
) -> Option<RevalidationResult> {
    let latest_revid = client.fetch_bare_metadata(lang, title).await.ok()?;
    match cache::revalidate_action(cached_revid, latest_revid) {
        RevalidateAction::Touch => Some(RevalidationResult::Unchanged),
        RevalidateAction::Fetch => {
            let fetched = client.fetch_article_html(lang, title).await.ok()?;
            Some(RevalidationResult::Changed {
                html: fetched.html,
                revid: fetched.revid,
                etag: fetched.etag,
            })
        }
    }
}

// -- The background substrate's executor (PRD §5.8, NF-NET-*) ---------------

/// The HTTP-doing half of the substrate ([`netqueue::Executor`]): it holds the
/// shared client/cache and turns each queued [`netqueue::Job`] into real work.
/// The substrate itself owns the respectful-client policy (gate, serial order,
/// budgets, breaker, backoff); this type only performs the request the policy
/// has already cleared, classifies the result for the breaker, and — for seed
/// jobs — hands back the follow-up article bodies to prefetch.
struct BgExecutor {
    client: WikiClient,
    cache: PageCache,
    revalidate_tx: UnboundedSender<RevalidationOutcome>,
    feed_cache: Arc<std::sync::Mutex<prefetch::FeedCache>>,
    weights: netqueue::RankWeights,
    top_n: usize,
}

impl netqueue::Executor for BgExecutor {
    fn execute(
        &self,
        job: netqueue::Job,
    ) -> impl std::future::Future<Output = netqueue::ExecResult> + Send {
        let client = self.client.clone();
        let cache = self.cache.clone();
        let tx = self.revalidate_tx.clone();
        let feed_cache = self.feed_cache.clone();
        let weights = self.weights;
        let top_n = self.top_n;
        async move {
            match job {
                netqueue::Job::Revalidate {
                    tab_id,
                    lang,
                    title,
                    cached_revid,
                } => execute_revalidation(&client, &tx, tab_id, lang, title, cached_revid).await,
                netqueue::Job::PrefetchArticle { lang, title, .. } => {
                    execute_prefetch_article(&client, &cache, &lang, &title).await
                }
                netqueue::Job::RankLinks {
                    lang,
                    article_title,
                    candidates,
                } => {
                    execute_rank_links(&client, &lang, &article_title, &candidates, weights, top_n)
                        .await
                }
                netqueue::Job::Featured { lang, date } => {
                    execute_featured(&client, &feed_cache, &lang, &date).await
                }
            }
        }
    }
}

/// Map an `api` background failure onto the substrate's outcome vocabulary so
/// the circuit breaker and Retry-After handling react correctly (NF-NET-4).
fn bg_failure_to_outcome(e: api::BgFailure) -> netqueue::Outcome {
    match e {
        api::BgFailure::RateLimited { retry_after } => {
            netqueue::Outcome::RateLimited { retry_after }
        }
        api::BgFailure::ServerError => netqueue::Outcome::ServerError,
        api::BgFailure::Network => netqueue::Outcome::Failed,
    }
}

/// FR-PF-1/2: fetch one article body into L2. Already-cached targets are
/// skipped without a request (the byte/request budget is never spent twice).
async fn execute_prefetch_article(
    client: &WikiClient,
    cache: &PageCache,
    lang: &str,
    title: &str,
) -> netqueue::ExecResult {
    if cache.get(lang, title).is_some() {
        return netqueue::ExecResult {
            outcome: netqueue::Outcome::Skipped {
                note: "already cached".to_string(),
            },
            follow_ups: Vec::new(),
        };
    }
    match client.fetch_article_html_bg(lang, title).await {
        Ok(a) => {
            // Prefetch fills L2 only — no L1 render, no images (PRD §5.8).
            cache.put(lang, title, &a.html, a.revid, a.etag.as_deref());
            netqueue::ExecResult {
                outcome: netqueue::Outcome::Done { bytes: a.bytes },
                follow_ups: Vec::new(),
            }
        }
        Err(e) => netqueue::ExecResult {
            outcome: bg_failure_to_outcome(e),
            follow_ups: Vec::new(),
        },
    }
}

/// FR-PF-1 seed: the one batched `generator=links` + `prop=pageviews` call,
/// then rank and hand back the top-N bodies as follow-up jobs (§6.2 rule 7:
/// never a per-article fanout).
async fn execute_rank_links(
    client: &WikiClient,
    lang: &str,
    article_title: &str,
    candidates: &[netqueue::LinkCandidate],
    weights: netqueue::RankWeights,
    top_n: usize,
) -> netqueue::ExecResult {
    match client.fetch_link_pageviews(lang, article_title).await {
        Ok((pageviews, bytes)) => {
            let ranked =
                prefetch::rank_links(article_title, candidates, &pageviews, weights, top_n);
            let follow_ups = ranked
                .into_iter()
                .map(|r| netqueue::Job::PrefetchArticle {
                    lang: lang.to_string(),
                    title: r.title,
                    reason: r.reason,
                    log_id: 0,
                })
                .collect();
            netqueue::ExecResult {
                outcome: netqueue::Outcome::Done { bytes },
                follow_ups,
            }
        }
        Err(e) => netqueue::ExecResult {
            outcome: bg_failure_to_outcome(e),
            follow_ups: Vec::new(),
        },
    }
}

/// FR-PF-2 seed: the one daily featured-content call. A same-day repeat is a
/// cache hit (no network); a fresh day fetches, parses, caches for B9's start
/// page, and hands back the TFA + top-10 most-read bodies.
async fn execute_featured(
    client: &WikiClient,
    feed_cache: &Arc<std::sync::Mutex<prefetch::FeedCache>>,
    lang: &str,
    date: &str,
) -> netqueue::ExecResult {
    if !feed_cache.lock().unwrap().should_fetch(date) {
        return netqueue::ExecResult {
            outcome: netqueue::Outcome::Skipped {
                note: "feed already fetched today".to_string(),
            },
            follow_ups: Vec::new(),
        };
    }
    let Some((y, m, d)) = parse_ymd(date) else {
        return netqueue::ExecResult {
            outcome: netqueue::Outcome::Failed,
            follow_ups: Vec::new(),
        };
    };
    match client.fetch_featured_feed(lang, y, m, d).await {
        Ok((body, bytes)) => {
            let feed = prefetch::FeaturedFeed::parse(&body).unwrap_or_default();
            let mut follow_ups = Vec::new();
            if let Some(tfa) = &feed.tfa {
                follow_ups.push(netqueue::Job::PrefetchArticle {
                    lang: lang.to_string(),
                    title: tfa.clone(),
                    reason: prefetch::trending_reason(None, 0),
                    log_id: 0,
                });
            }
            for (i, mr) in feed.mostread.iter().take(10).enumerate() {
                follow_ups.push(netqueue::Job::PrefetchArticle {
                    lang: lang.to_string(),
                    title: mr.title.clone(),
                    reason: prefetch::trending_reason(Some(i + 1), mr.views),
                    log_id: 0,
                });
            }
            feed_cache.lock().unwrap().store(date.to_string(), feed);
            netqueue::ExecResult {
                outcome: netqueue::Outcome::Done { bytes },
                follow_ups,
            }
        }
        Err(e) => netqueue::ExecResult {
            outcome: bg_failure_to_outcome(e),
            follow_ups: Vec::new(),
        },
    }
}

/// FR-OFF-2 on the substrate: the cheap bare check with `maxlag`, then a full
/// re-fetch only if the revid changed. Always sends exactly one
/// `RevalidationOutcome` (even on failure → silent per NF-NET-8) so the loop's
/// `pending_revalidations` counter balances.
async fn execute_revalidation(
    client: &WikiClient,
    tx: &UnboundedSender<RevalidationOutcome>,
    tab_id: TabId,
    lang: String,
    title: String,
    cached_revid: u64,
) -> netqueue::ExecResult {
    let none = Vec::new();
    match client.fetch_bare_metadata_bg(&lang, &title).await {
        Ok(latest) => match cache::revalidate_action(cached_revid, latest) {
            RevalidateAction::Touch => {
                let _ = tx.send(RevalidationOutcome {
                    tab_id,
                    lang,
                    title,
                    result: Some(RevalidationResult::Unchanged),
                });
                netqueue::ExecResult {
                    outcome: netqueue::Outcome::Done { bytes: 0 },
                    follow_ups: none,
                }
            }
            RevalidateAction::Fetch => match client.fetch_article_html_bg(&lang, &title).await {
                Ok(a) => {
                    let _ = tx.send(RevalidationOutcome {
                        tab_id,
                        lang,
                        title,
                        result: Some(RevalidationResult::Changed {
                            html: a.html,
                            revid: a.revid,
                            etag: a.etag,
                        }),
                    });
                    netqueue::ExecResult {
                        outcome: netqueue::Outcome::Done { bytes: a.bytes },
                        follow_ups: none,
                    }
                }
                Err(e) => {
                    let _ = tx.send(RevalidationOutcome {
                        tab_id,
                        lang,
                        title,
                        result: None,
                    });
                    netqueue::ExecResult {
                        outcome: bg_failure_to_outcome(e),
                        follow_ups: none,
                    }
                }
            },
        },
        Err(e) => {
            let _ = tx.send(RevalidationOutcome {
                tab_id,
                lang,
                title,
                result: None,
            });
            netqueue::ExecResult {
                outcome: bg_failure_to_outcome(e),
                follow_ups: none,
            }
        }
    }
}

/// Parse a `yyyy-mm-dd` bucket into calendar parts for the feed URL.
fn parse_ymd(date: &str) -> Option<(i32, u32, u32)> {
    let mut parts = date.split('-');
    let y = parts.next()?.parse().ok()?;
    let m = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((y, m, d))
}

/// PRD FR-PF-1: with an article on screen, enqueue a link-ranking seed for its
/// internal links (deduped, ≤50). A no-op unless prefetch is active (kill
/// switch on, not incognito — FR-PF-6/FR-PR-3). The cursor link is flagged so
/// ranking always keeps it (FR-PF-1).
fn schedule_link_prefetch(app: &App) {
    if !app.prefetch_active() {
        return;
    }
    let Some(handle) = &app.prefetch else {
        return;
    };
    let tab = app.active_tab();
    let Some(doc) = tab.doc.as_ref() else {
        return;
    };
    let article_title = doc.title.clone();
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut lead_position = 0usize;
    for (i, link) in tab.links.iter().enumerate() {
        let Some(title) = &link.internal_title else {
            continue;
        };
        if title == &article_title || !seen.insert(title.clone()) {
            continue;
        }
        candidates.push(netqueue::LinkCandidate {
            title: title.clone(),
            lead_position,
            is_cursor: tab.focused_link == Some(i),
        });
        lead_position += 1;
        if candidates.len() >= 50 {
            break;
        }
    }
    if candidates.is_empty() {
        return;
    }
    handle.enqueue_prefetch(netqueue::Job::RankLinks {
        lang: tab.lang.clone(),
        article_title,
        candidates,
    });
}

/// PRD FR-PF-2: enqueue the once-per-day trending seed. The `date` bucket drives
/// the feed's once-per-day cache; the substrate's gate makes it idle-only.
fn schedule_trending_prefetch(app: &App) {
    if !app.prefetch_active() {
        return;
    }
    let Some(handle) = &app.prefetch else {
        return;
    };
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    handle.enqueue_prefetch(netqueue::Job::Featured {
        lang: app.lang.clone(),
        date,
    });
}

/// Applies a completed revalidation (PRD FR-OFF-2, FR-TB-3): writes any
/// changed content into L2 (or silently touches `fetched_at` for an unchanged
/// revid) regardless of what's on screen, then routes the "updated" signal to
/// the *originating tab* by id. If that tab has closed, the completion is
/// dropped gracefully (the cache write still happened, so a future open is
/// fresh). The tab must still show that (lang, title) — otherwise it navigated
/// on within its own view and the notice would be wrong. The notice text is
/// shown only when the affected tab is the active one; a background tab arms
/// its own `pending_reload`, which is re-surfaced when the reader switches to
/// it (`App::sync_active_tab`).
fn apply_revalidation_outcome(app: &mut App, cache: &PageCache, outcome: RevalidationOutcome) {
    app.pending_revalidations = app.pending_revalidations.saturating_sub(1);
    let Some(result) = outcome.result else {
        return; // NF-NET-8: silent on failure — the cached copy stands.
    };
    match result {
        RevalidationResult::Unchanged => {
            cache.touch_fetched_at(&outcome.lang, &outcome.title);
        }
        RevalidationResult::Changed { html, revid, etag } => {
            cache.put(&outcome.lang, &outcome.title, &html, revid, etag.as_deref());
            let Some(index) = app.tab_index_by_id(outcome.tab_id) else {
                return; // the tab closed — nothing to notify.
            };
            let tab = &mut app.tabs[index];
            let still_open = tab.lang == outcome.lang
                && tab.doc.as_ref().is_some_and(|d| d.title == outcome.title);
            if still_open {
                tab.pending_reload = Some(PendingReload {
                    lang: outcome.lang,
                    title: outcome.title,
                });
                // Only surface the notice text when this tab is on screen; a
                // background tab's reload re-arms when it becomes active.
                if index == app.active {
                    app.notice = Some("updated — r to reload".to_string());
                }
            }
        }
    }
}

/// Applies a completed background-tab fetch (PRD FR-TB-3): installs the
/// document into the tab it was fetched for (by id), or drops it if that tab
/// has closed. On a network error the tab keeps a "(failed)" placeholder title
/// so the reader can see which background open didn't land. A stale-cache-hit
/// result also kicks off a revalidation keyed to that tab.
fn apply_tab_load_outcome(
    client: &WikiClient,
    app: &mut App,
    outcome: TabLoadOutcome,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
) {
    let Some(index) = app.tab_index_by_id(outcome.tab_id) else {
        return; // the background tab was closed before its fetch landed.
    };
    match outcome.result {
        Ok(fetch) => {
            let document = doc::parse_article_html(&outcome.title, &fetch.html);
            {
                let tab = &mut app.tabs[index];
                tab.loading = false;
                tab.lang = outcome.lang.clone();
                tab.page_source = fetch.source;
                tab.current_revid = fetch.revid;
                tab.install_document(document);
            }
            // PRD FR-HS-1: a background tab's fetch landing is "an article
            // successfully renders in a tab" too, same as the active tab's
            // own `App::set_document` — no referrer is captured for a
            // background open today (the tab that spawned it isn't
            // threaded through `TabLoadOutcome`), a documented limitation
            // rather than a missing feature.
            app.record_history_visit(index, None);
            // If this tab happens to be the active one (the reader switched to
            // it while it loaded), refresh the app-global view state.
            if index == app.active {
                app.layout = None;
            }
            if let Some(cached_revid) = fetch.revalidate
                && fire_revalidation(
                    app,
                    client,
                    outcome.tab_id,
                    outcome.lang,
                    outcome.title,
                    cached_revid,
                    revalidate_tx,
                )
            {
                app.pending_revalidations += 1;
            }
        }
        Err(_) => {
            let tab = &mut app.tabs[index];
            tab.loading = false;
            tab.pending_title = Some(format!("{} (failed)", outcome.title));
        }
    }
}

/// Builds the OSC 52 clipboard-set escape sequence for `text`, split out
/// from `yank_to_clipboard` so the framing can be verified directly (PRD
/// SEC-2): base64's alphabet (`A-Za-z0-9+/=`) contains no C0/C1 control
/// bytes, so no matter what `text` contains — a title is expected to already
/// be sanitized by the time it gets here (PRD SEC-1, `doc::parse_article_html`),
/// but this holds even for arbitrary content — the encoded payload between
/// the `\x1b]52;c;` prefix and the `\x07` terminator can never itself
/// contain a raw ESC/BEL byte that could break out of the sequence.
fn osc52_clipboard_sequence(text: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{encoded}\x07")
}

/// Copies text to the system clipboard via OSC 52 (PRD FR-NV-10), which
/// works over SSH because the *terminal emulator* performs the copy.
/// Terminals without OSC 52 support silently ignore the sequence — the
/// status line still reports what was yanked so the user can tell.
fn yank_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = io::stdout();
    write!(out, "{}", osc52_clipboard_sequence(text))?;
    out.flush()
}

/// `ba` (PRD FR-BM-2): opens `$EDITOR` on a temp Markdown file seeded with
/// the bookmark's current note, suspending the TUI for the duration (SEC-5:
/// the editor gets the temp file's PATH as argv — never the note's content,
/// which only ever flows through the file itself). Auto-bookmarks the
/// current article first if it wasn't already saved, since annotating an
/// article implies wanting to keep it. Synchronous (not `async`): the editor
/// is an interactive child process the reader is directly waiting on, the
/// same "block this task, not the whole process" tradeoff `event::read()`
/// already makes in `run`'s own loop.
fn annotate_current_article(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) {
    let Some(doc) = app.active_tab().doc.as_ref() else {
        app.status = "Open an article first".to_string();
        return;
    };
    let title = doc.title.clone();
    let lang = app.active_tab().lang.clone();
    let revid = app.active_tab().current_revid;
    let revid = (revid != 0).then_some(revid);

    let Ok(editor_spec) = std::env::var("EDITOR") else {
        app.notice = Some("set $EDITOR to annotate".to_string());
        return;
    };
    let Some((program, args)) = bookmarks::parse_editor_command(&editor_spec) else {
        app.notice = Some("set $EDITOR to annotate".to_string());
        return;
    };

    let was_new = app.bookmarks.ensure_bookmarked(&lang, &title, revid);
    let existing_note = app
        .bookmarks
        .find(&lang, &title)
        .and_then(|b| b.note.clone());

    let tmp = std::env::temp_dir().join(format!(
        "wikitui-annotate-{}-{}.md",
        std::process::id(),
        ANNOTATE_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, bookmarks::seed_note_content(existing_note.as_deref())).is_err() {
        app.notice = Some("Could not create a temp file to annotate".to_string());
        return;
    }

    let status = {
        // RAII: leaves the alternate screen/raw mode for exactly the
        // lifetime of this block, restored on drop regardless of which
        // branch below runs next (SuspendedTerminal's own doc comment).
        let _suspended = SuspendedTerminal::suspend(terminal);
        std::process::Command::new(&program)
            .args(&args)
            .arg(&tmp)
            .status()
    };

    let saved_note = match status {
        Ok(exit) if exit.success() => std::fs::read_to_string(&tmp)
            .ok()
            .map(|raw| bookmarks::note_from_editor_output(&raw)),
        // A nonzero exit (user aborted the editor) or a failure to even
        // launch it (bad $EDITOR) both keep the old note untouched.
        Ok(_) | Err(_) => None,
    };
    let _ = std::fs::remove_file(&tmp);

    match saved_note {
        Some(note) => {
            app.bookmarks.set_note(&lang, &title, note);
            app.notice = Some(if was_new {
                format!("Bookmarked \"{title}\" and saved the note")
            } else {
                "Note saved".to_string()
            });
        }
        None => {
            app.notice = Some("Editor exited without saving — note unchanged".to_string());
        }
    }
}

/// Per-call-unique temp filenames for [`annotate_current_article`] within
/// one process (mirroring `jsonl::atomic_rewrite`'s own counter) — a second
/// `ba` before the first's temp file is cleaned up (shouldn't happen given
/// the editor is spawned synchronously, but costs nothing to guard) must
/// never collide with it.
static ANNOTATE_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `rl` (PRD FR-BM-3): enqueues `app.read_later_target()` — the focused
/// link's target, or the article itself with nothing focused — rejecting a
/// duplicate `(lang, title)` with a notice instead of a second queue slot.
/// "Enqueue triggers T0 offline save": makes sure the target is in the L2
/// cache before returning, so an immediate `:readlater` Enter still has
/// something to serve even offline.
async fn enqueue_read_later(client: &WikiClient, cache: &PageCache, app: &mut App) {
    let Some((lang, title)) = app.read_later_target() else {
        app.notice = Some("Open an article first".to_string());
        return;
    };
    if app.readlater.contains(&lang, &title) {
        app.notice = Some(format!("\"{title}\" is already in the read-later queue"));
        return;
    }

    // T0 offline save (FR-BM-3). FR-OFF-4's full tiered/pinned save (quota
    // tracking, integrity checks) is a later chunk — this is the minimal
    // seam read-later needs now: an ordinary L2 cache write via the same
    // fetch path every other open already uses, just with no tab attached.
    let saved_offline = ensure_cached(client, cache, &lang, &title).await;
    app.readlater.enqueue(ReadLaterEntry {
        title: title.clone(),
        lang,
        enqueued_at: bookmarks::now_ts(),
        priority: 0,
    });
    app.notice = Some(if saved_offline {
        format!("Enqueued \"{title}\" for later")
    } else {
        format!("Enqueued \"{title}\" for later (offline save failed — will retry on open)")
    });
}

/// Whether `(lang, title)` is already in L2, fetching it in if not. Returns
/// whether it's cached by the time this returns (a network failure here is
/// reported to the reader, not retried — the queue entry still gets
/// created either way; opening it later tries again via the normal fetch
/// path).
async fn ensure_cached(client: &WikiClient, cache: &PageCache, lang: &str, title: &str) -> bool {
    if cache.get(lang, title).is_some() {
        return true;
    }
    match client.fetch_article_html(lang, title).await {
        Ok(fetched) => {
            cache.put(
                lang,
                title,
                &fetched.html,
                fetched.revid,
                fetched.etag.as_deref(),
            );
            true
        }
        Err(_) => false,
    }
}

// -- Saved pages (PRD FR-OFF-4..7) ----------------------------------------

/// PRD FR-OFF-4 T1 thumbnail byte cap (per image is already bounded by
/// `api::fetch_image`'s own read cap) — the count is bounded by
/// `saved::MAX_T1_THUMBS`.
const MAX_SAVE_THUMB_BYTES: usize = 2 * 1024 * 1024;

/// `S` / `:save [t0|t1|t2]`: pin the article on screen at `tier`. A single
/// page needs no cost preview (that is for bulk saves, FR-OFF-5) — it fires
/// straight onto the background save queue.
fn start_current_save(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    tier: Tier,
    save_tx: &UnboundedSender<SaveOutcome>,
) {
    let Some(doc) = app.active_tab().doc.as_ref() else {
        app.notice = Some("Open an article first".to_string());
        return;
    };
    let title = doc.title.clone();
    let lang = app.active_tab().lang.clone();
    fire_save_job(
        client,
        cache,
        app,
        vec![(lang, title)],
        tier,
        "S save".to_string(),
        save_tx,
    );
}

/// Resolve a bulk save's targets into a cost preview + y/n confirmation (PRD
/// FR-OFF-5): nothing is fetched until the reader confirms. An empty target
/// set reports so instead of arming a pointless confirm.
fn request_bulk_save(app: &mut App, label: String, tier: Tier, targets: Vec<(String, String)>) {
    if targets.is_empty() {
        app.notice = Some(format!("Nothing to save for {label}"));
        return;
    }
    app.notice = Some(app::bulk_cost_preview(&label, targets.len(), tier));
    app.pending_bulk_save = Some(BulkSaveRequest {
        label,
        tier,
        targets,
    });
}

/// Spawn the serial, non-blocking background save (PRD FR-OFF-5's "background
/// queue"): one task walks `targets` in order, fetching each and posting a
/// `SaveOutcome`, so a big save never blocks the reader and the store is only
/// touched on the main thread (`apply_save_outcome`). `pending_saves` keeps
/// the loop polling until every outcome has landed.
fn fire_save_job(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    targets: Vec<(String, String)>,
    tier: Tier,
    source_note: String,
    save_tx: &UnboundedSender<SaveOutcome>,
) {
    let count = targets.len();
    app.pending_saves += count as u32;
    app.notice = Some(format!("Saving {count} page(s) ({})…", tier.label()));
    let client = client.clone();
    let cache = cache.clone();
    let tx = save_tx.clone();
    let include_nonfree = app.include_nonfree;
    tokio::spawn(async move {
        for (lang, title) in targets {
            let result = fetch_for_save(
                &client,
                &cache,
                &lang,
                &title,
                tier,
                include_nonfree,
                &source_note,
            )
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(SaveOutcome {
                result,
                title: title.clone(),
            });
        }
    });
}

/// Fetch everything one saved page needs at `tier`: the HTML (cache-first, so
/// re-saving the article on screen doesn't re-hit the network), plus — per
/// tier — thumbnails (T1+) and link-target summaries (T2). Thumbnail bytes are
/// the same sanitized image sources shown transiently in the reading view; the
/// non-free *export* exclusion (§10) is enforced at export time, not here (see
/// `saved.rs`'s module doc).
async fn fetch_for_save(
    client: &WikiClient,
    cache: &PageCache,
    lang: &str,
    title: &str,
    tier: Tier,
    _include_nonfree: bool,
    source_note: &str,
) -> Result<SaveFetched> {
    // HTML: prefer the cache (the article on screen is already there) and fall
    // back to a fresh fetch for a target that has never been opened.
    let (html, revid) = match cache.get(lang, title) {
        Some(page) => (page.html, page.revid),
        None => {
            let fetched = client.fetch_article_html(lang, title).await?;
            cache.put(
                lang,
                title,
                &fetched.html,
                fetched.revid,
                fetched.etag.as_deref(),
            );
            (fetched.html, fetched.revid)
        }
    };

    let mut thumbs = Vec::new();
    let mut summaries = Vec::new();

    if matches!(tier, Tier::T1 | Tier::T2) {
        let document = doc::parse_article_html(title, &html);
        let mut srcs: Vec<String> = Vec::new();
        for block in &document.blocks {
            if let doc::Block::Image { src: Some(src), .. } = block
                && !srcs.contains(src)
            {
                srcs.push(src.clone());
            }
        }
        for src in srcs.into_iter().take(saved::MAX_T1_THUMBS) {
            if let Ok(bytes) = client.fetch_image(&src).await
                && bytes.len() <= MAX_SAVE_THUMB_BYTES
            {
                thumbs.push((src, bytes));
            }
        }
    }

    if matches!(tier, Tier::T2) {
        let document = doc::parse_article_html(title, &html);
        let mut seen: Vec<String> = Vec::new();
        for link in doc::collect_links(&document) {
            if let Some(target) = link.internal_title
                && !seen.contains(&target)
            {
                seen.push(target);
            }
        }
        for target in seen.into_iter().take(saved::MAX_T2_SUMMARIES) {
            if let Ok(extract) = client.fetch_summary(lang, &target).await {
                summaries.push(LinkSummary {
                    title: target,
                    extract,
                });
            }
        }
    }

    Ok(SaveFetched {
        lang: lang.to_string(),
        title: title.to_string(),
        revid,
        tier,
        html,
        thumbs,
        summaries,
        source_note: source_note.to_string(),
    })
}

/// Apply one completed save on the main thread (PRD FR-OFF-4): pin it into the
/// store (index + content), or report the failure. Decrements `pending_saves`
/// so the loop can stop polling once a run finishes.
fn apply_save_outcome(app: &mut App, outcome: SaveOutcome) {
    app.pending_saves = app.pending_saves.saturating_sub(1);
    match outcome.result {
        Ok(f) => match app.saved.save(
            &f.lang,
            &f.title,
            f.revid,
            f.tier,
            &f.html,
            &f.thumbs,
            f.summaries,
            &f.source_note,
        ) {
            Ok(record) => {
                app.notice = Some(format!(
                    "Saved \"{}\" ({}, {})",
                    record.title,
                    record.tier.label(),
                    human_bytes(record.size_total)
                ));
            }
            Err(e) => app.notice = Some(format!("Save failed for \"{}\": {e}", f.title)),
        },
        Err(e) => app.notice = Some(format!("Save failed for \"{}\": {e}", outcome.title)),
    }
}

/// A compact byte-size string for save notices ("30 KB", "1.4 MB").
fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Serve a pinned saved page for reading (PRD §5.7): decompress from the saved
/// store and install it with the ▣ Saved indicator, no network involved. The
/// `:saved` browser's Enter and the offline fallback both route here.
fn open_saved(app: &mut App, lang: &str, title: &str) {
    match app.saved.get(lang, title) {
        Some(content) => {
            let document = doc::parse_article_html(title, &content.html);
            {
                let tab = app.active_tab_mut();
                tab.page_source = PageSource::Saved {
                    age_secs: content.age_secs,
                };
                tab.current_revid = content.revid;
            }
            app.open_document(document);
        }
        None => {
            app.notice = Some(format!("Saved page \"{title}\" is unavailable or corrupt"));
            app.mode = Mode::Reading;
        }
    }
}

/// `:fetch-queue` (PRD FR-OFF-6): drain the offline fetch queue now — fetch
/// every queued title into the cache, dropping the ones that land. Runs
/// synchronously on the trigger (a deliberate, explicit action; the mock makes
/// it instant). Auto-drain the moment connectivity returns is a documented
/// seam — it needs a connectivity signal wikitui doesn't yet have.
async fn drain_fetch_queue(client: &WikiClient, cache: &PageCache, app: &mut App) {
    let queued = app.fetch_queue.snapshot();
    if queued.is_empty() {
        app.notice = Some("Fetch queue is empty".to_string());
        return;
    }
    let (mut ok, mut fail) = (0u32, 0u32);
    for q in queued {
        if ensure_cached(client, cache, &q.lang, &q.title).await {
            app.fetch_queue.remove(&q.lang, &q.title);
            ok += 1;
        } else {
            fail += 1;
        }
    }
    app.notice = Some(format!("Fetch queue: {ok} fetched, {fail} still pending"));
}

#[allow(clippy::too_many_arguments)]
async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &WikiClient,
    cache: &PageCache,
    cli: Cli,
    lang: String,
    theme: Theme,
    no_color: bool,
    accessible: bool,
    measure: u16,
    ambiguous_wide: bool,
    cite_style: CiteStyle,
    readlater_auto_dequeue: bool,
    history_retention_days: u64,
    images_config: Option<bool>,
    include_nonfree: bool,
    startpage_config: String,
    prefetch_config: netqueue::SubstrateConfig,
    prefetch_enabled: bool,
    config_ctx: ConfigContext,
    reload_flag: Arc<AtomicBool>,
) -> Result<()> {
    let mut app = App::new(lang, theme, no_color);
    app.accessible = accessible;
    app.measure = measure;
    app.ambiguous_wide = ambiguous_wide;
    app.cite_style = cite_style;
    app.readlater_auto_dequeue = readlater_auto_dequeue;
    // PRD FR-RD-8/§6.3: terminal graphics capability snapshot, taken once.
    // The `no_color`/`images_enabled` decision is layered on live in
    // `App::graphics_protocol`.
    app.graphics_env = {
        use std::io::IsTerminal;
        graphics::GraphicsEnv::from_process_env(std::io::stdout().is_terminal())
    };
    app.images_override = images_config;
    app.include_nonfree = include_nonfree;
    // Already validated during resolution (an invalid value fell back to
    // "feed" with a warning above) — same belt-and-braces default as
    // `theme`/`cite_style` just above this function.
    app.startpage_config = startpage::StartPageConfig::parse(&startpage_config).unwrap_or_default();
    app.config_ctx = config_ctx;
    app.incognito = cli.incognito;
    // The real, on-disk reading history (PRD §6.4) — `App::new` defaults to
    // an in-memory store precisely so this line, not construction, is the
    // one place the real state directory gets touched (see `App.history`'s
    // doc comment). `retention_prune` runs once here, at startup, per PRD
    // FR-HS-4.
    app.history = history::History::open();
    app.history.retention_prune(history_retention_days);

    // Delivers typeahead responses, background revalidation outcomes, and
    // background-tab fetch results back to the loop (PRD FR-SR-1 / FR-OFF-2 /
    // FR-TB-3): all run on spawned tasks, never inline, so none can stall
    // redraws or keystrokes.
    let (typeahead_tx, mut typeahead_rx) = mpsc::unbounded_channel::<TypeaheadOutcome>();
    let (revalidate_tx, mut revalidate_rx) = mpsc::unbounded_channel::<RevalidationOutcome>();
    let (open_tx, mut open_rx) = mpsc::unbounded_channel::<TabLoadOutcome>();
    // PRD FR-RD-8: inline-image fetch+decode results (lazy, never blocking).
    let (image_tx, mut image_rx) = mpsc::unbounded_channel::<ImageOutcome>();
    // PRD FR-OFF-4..5: background saved-page fetch results (serial, non-blocking).
    let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveOutcome>();
    // PRD FR-SR-6: lazy `morelike:` fetches for the Related panel — never
    // blocking, same idiom as `typeahead_tx`/`image_tx` above.
    let (related_tx, mut related_rx) = mpsc::unbounded_channel::<RelatedOutcome>();

    // PRD §5.8 / NF-NET-1: the one background substrate. A single serial
    // worker drains its priority queue; revalidation (FR-OFF-2) is migrated
    // onto it here so there is one coherent background story with prefetch
    // (FR-PF-1/2). Background-tab loads, saved-page fetches, and read-later
    // warming remain ad-hoc spawns for now — documented follow-up.
    let substrate = netqueue::SubstrateHandle::new(prefetch_config);
    substrate.set_enabled(prefetch_enabled);
    app.prefetch = Some(substrate.clone());
    // Shared with `BgExecutor` below so the start page (FR-DL-1) and TIL
    // widget (FR-DL-7) can read the parsed feed straight off `App` without a
    // second request — the exact seam `prefetch::FeedCache::get` documents.
    let feed_cache = Arc::new(std::sync::Mutex::new(prefetch::FeedCache::default()));
    app.feed_cache = Some(feed_cache.clone());
    let executor = BgExecutor {
        client: client.clone(),
        cache: cache.clone(),
        revalidate_tx: revalidate_tx.clone(),
        feed_cache,
        weights: substrate.weights(),
        top_n: substrate.top_n(),
    };
    tokio::spawn(substrate.clone().run(executor));

    if let Some(query) = cli.search {
        app.search_input = query;
        run_search(client, &mut app).await;
    } else if let Some(title) = cli.title {
        open_title(client, cache, &mut app, &title, &revalidate_tx).await;
    } else if app.startpage_config == startpage::StartPageConfig::Resume {
        // PRD FR-DL-1's `startpage = resume`: proper session restore is a
        // later chunk (B18) — until then this resolves to the single
        // cheapest approximation already lying around at startup, the most
        // recent reading-history entry (`History::recent` is already loaded
        // for the visited-styling feature, so this costs nothing extra).
        // No history yet (fresh install, or incognito never recorded any)
        // falls straight through to the feed-backed start page, same as
        // `startpage = feed` — never an error, never a blank screen with no
        // explanation.
        if let Some(visit) = app.history.recent(1).into_iter().next() {
            app.lang = visit.lang.clone();
            open_title(client, cache, &mut app, &visit.title, &revalidate_tx).await;
        }
    }

    // PRD FR-PF-2: seed trending prefetch once at startup. The foreground gate
    // ensures it only runs while the reader is idle; the daily feed cache makes
    // it a single call per day.
    schedule_trending_prefetch(&app);

    loop {
        // Checked once per turn rather than mid-`event::read()`, which
        // blocks on real input and can't be interrupted without
        // restructuring the whole loop: SIGHUP's reload takes effect on
        // the *next* keypress, not instantly. Documented tradeoff, not a
        // bug — §6.7 only requires SIGHUP to trigger "the same reload."
        if reload_flag.swap(false, Ordering::SeqCst) {
            apply_config_reload(&mut app);
        }

        // PRD FR-DL-1: snapshotted *before* `draw` rather than re-read fresh
        // for the poll-vs-block decision below — the daily-feed arrival is a
        // background mutex write racing this loop with no channel/notify to
        // sequence against (unlike `pending_revalidations`/`any_tab_loading`,
        // which only ever change via this loop's own channel drains further
        // down). Feed state is monotonic (arrives once, never reverts), so an
        // older-or-equal snapshot here is always safe: if it says "still
        // pending", `draw` below (reading the live state a moment later) may
        // in fact already show the resolved page — harmless, just one extra
        // poll tick before the loop notices. If it instead read *fresh*
        // *after* `draw`, the opposite race is possible and IS harmful: the
        // feed can complete in the gap between `draw`'s internal read and
        // this check's, so `draw` paints "loading" while the fresher check
        // says "resolved" and picks the block-on-`event::read` branch —
        // parking the loop forever with the settled start page never drawn
        // until a keypress. Reproduced via pty verification before this
        // comment was written; see the on-this-day/start-page pty script.
        let start_page_still_loading = app.active_tab().doc.is_none() && app.start_page_pending();

        terminal.draw(|f| ui::draw(f, &mut app))?;

        // PRD FR-RD-8: after each draw, lazily kick off fetches for any
        // not-yet-loaded images the active document references (a no-op when
        // images are off / no graphics protocol / a text theme). Idempotent —
        // it spawns exactly one fetch per source URL.
        request_visible_images(client, &mut app, &image_tx);
        // PRD FR-DL-1's picture of the day: the same lazy fetch+decode path,
        // keyed by the feed's thumbnail URL instead of a document's image
        // block — see `request_start_page_image`'s doc comment.
        request_start_page_image(client, &mut app, &image_tx);

        // PRD §7 / v0.5's "crash-safe terminal restore": a deliberate,
        // inert-by-default panic trigger for exercising the crash path
        // (terminal restore, crash report, stderr message) end to end
        // without waiting for a real bug. See `debug_panic_requested`'s doc
        // comment for why this is an env var kept in the tree rather than a
        // keybinding removed before commit.
        if debug_panic_requested() {
            panic!(
                "WIKITUI_DEBUG_PANIC requested a deliberate panic for crash-safety testing (PRD §7)"
            );
        }

        // Three things wake the loop on a timer instead of blocking forever
        // in `event::read()`: Search mode's typeahead debounce (FR-SR-1), an
        // in-flight background revalidation (FR-OFF-2), and a background tab
        // still loading (FR-TB-3) — so its "…"→title transition and the
        // revalidation/reload notices land without a keypress. Everywhere else
        // — plain Reading mode, nothing in flight — keeps the block-until-input
        // behavior (PRD FR-ACS-2: no gratuitous redraws/CPU use while idle).
        if app.mode == Mode::Search
            || app.pending_revalidations > 0
            || app.any_tab_loading()
            || app.image_store.any_loading()
            || app.pending_saves > 0
            // PRD FR-PF-4: keep the prefetch-log panel refreshing live while
            // it is open (scoped to that mode only, so idle Reading still
            // blocks on input — FR-ACS-2).
            || app.mode == Mode::PrefetchLog
            // PRD FR-DL-1: the start page's skeleton fills in without a
            // keypress once the daily feed arrives — scoped to exactly the
            // "showing the start page and still waiting" window, so idle
            // Reading with an article open still blocks on input. Uses the
            // pre-`draw` snapshot above, not a fresh read — see its comment.
            || start_page_still_loading
            // PRD FR-SR-6: the Related panel's lazy `morelike:` fetch
            // completes and redraws without a keypress, mirroring the other
            // lazy-fetch cases above.
            || app.related_loading
        {
            let poll_interval = if app.mode == Mode::Search {
                TYPEAHEAD_POLL
            } else {
                REVALIDATE_POLL
            };
            if event::poll(poll_interval)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                handle_key(
                    client,
                    cache,
                    &mut app,
                    key.code,
                    key.modifiers,
                    &revalidate_tx,
                    &open_tx,
                    &save_tx,
                    &related_tx,
                    terminal,
                )
                .await;
            }
            if app.mode == Mode::Search {
                if let Some(deadline) = app.search_debounce_at
                    && app::debounce_due(deadline, Instant::now())
                {
                    app.search_debounce_at = None;
                    fire_typeahead(client, &app, &typeahead_tx);
                }
                // Non-blocking drain: a response for a query the user has
                // since typed past (`typeahead_is_current` says no) is
                // silently discarded — PRD FR-SR-1's in-flight cancellation.
                while let Ok(outcome) = typeahead_rx.try_recv() {
                    if app::typeahead_is_current(&outcome.query, &app.search_input)
                        && let Ok(suggestions) = outcome.result
                    {
                        app.typeahead = suggestions;
                        app.selected_suggestion = 0;
                    }
                }
            }
            while let Ok(outcome) = revalidate_rx.try_recv() {
                apply_revalidation_outcome(&mut app, cache, outcome);
            }
            while let Ok(outcome) = open_rx.try_recv() {
                apply_tab_load_outcome(client, &mut app, outcome, &revalidate_tx);
            }
            // PRD FR-RD-8: install decoded inline images; each triggers one
            // relayout so its half-block box appears (`App::deliver_image`).
            while let Ok(outcome) = image_rx.try_recv() {
                app.deliver_image(outcome.src, outcome.decoded);
            }
            // PRD FR-OFF-4..5: pin completed saved-page fetches on the main
            // thread (the store isn't shared with the spawned task).
            while let Ok(outcome) = save_rx.try_recv() {
                apply_save_outcome(&mut app, outcome);
            }
            // PRD FR-SR-6: install a completed `morelike:` fetch into the
            // session cache (and, if the panel is still open on the same
            // article, the live view too).
            while let Ok(outcome) = related_rx.try_recv() {
                app.deliver_related(outcome.lang, outcome.title, outcome.result);
            }
        } else if let Event::Key(key) = event::read()? {
            // Block until an event arrives instead of redrawing on a timer —
            // an idle reader shouldn't spin the CPU or spam hide-cursor codes.
            if key.kind == KeyEventKind::Press {
                handle_key(
                    client,
                    cache,
                    &mut app,
                    key.code,
                    key.modifiers,
                    &revalidate_tx,
                    &open_tx,
                    &save_tx,
                    &related_tx,
                    terminal,
                )
                .await;
            }
        }

        if app.should_quit {
            break;
        }
    }

    // PRD FR-HS-1's dwell time: whatever every open tab is still showing
    // stops accumulating dwell the moment the app exits.
    app.flush_all_tab_dwell();

    Ok(())
}

/// Spawns the actual HTTP request for one typeahead debounce firing (PRD
/// FR-SR-1) so it can never block the event loop; the query travels with
/// the result so a reply that arrives after the user has typed further is
/// recognizable as stale at the receiving end (`app::typeahead_is_current`)
/// instead of clobbering a newer, still-relevant dropdown.
fn fire_typeahead(client: &WikiClient, app: &App, tx: &UnboundedSender<TypeaheadOutcome>) {
    let query = app.search_input.clone();
    let lang = app.lang.clone();
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = client
            .search_title(&lang, &query, TYPEAHEAD_LIMIT)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(TypeaheadOutcome { query, result });
    });
}

/// Spawns the `morelike:{title}` search behind the Related panel (PRD
/// FR-SR-6) so opening it never blocks — mirrors `fire_typeahead`'s pattern.
/// The panel shows its own "loading" status (`App::open_related`) until the
/// result lands via `related_rx` and `App::deliver_related` installs it.
const RELATED_LIMIT: u32 = 10;

fn fire_related(
    client: &WikiClient,
    lang: &str,
    title: &str,
    tx: &UnboundedSender<RelatedOutcome>,
) {
    let lang = lang.to_string();
    let title = title.to_string();
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let query = format!("morelike:{title}");
        let result = client
            .search(&lang, &query, RELATED_LIMIT)
            .await
            .map(|outcome| outcome.results)
            .map_err(|e| e.to_string());
        let _ = tx.send(RelatedOutcome {
            lang,
            title,
            result,
        });
    });
}

/// Opens the Related panel (PRD FR-SR-6: `gR` / `:related`), firing the
/// `morelike:` fetch only when `App::open_related` says the session cache
/// doesn't already have this article's results.
fn open_related(app: &mut App, client: &WikiClient, tx: &UnboundedSender<RelatedOutcome>) {
    if app.open_related() {
        let tab = app.active_tab();
        let lang = tab.lang.clone();
        // `open_related` only returns `true` when the active tab has a
        // document — see its own doc comment.
        let title = tab
            .doc
            .as_ref()
            .expect("open_related guarantees a document")
            .title
            .clone();
        fire_related(client, &lang, &title, tx);
    }
}

/// `gr` / `:random` (PRD FR-SR-5): opens a random main-namespace article.
/// A single quick round trip, so — like `run_search`/`open_title` — this
/// blocks the event loop for its duration rather than going through the
/// lazy channel pattern; `app.loading` drives the same "Loading…" status
/// those already show.
async fn open_random_article(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
) {
    app.loading = true;
    let lang = app.lang.clone();
    match client.random_titles(&lang, 1).await {
        Ok(mut titles) => match titles.pop() {
            Some(title) => {
                open_title(client, cache, app, &title, revalidate_tx).await;
                return;
            }
            None => app.status = "Random article: the wiki returned nothing".to_string(),
        },
        Err(e) => app.status = format!("Random article failed: {e}"),
    }
    app.loading = false;
}

/// `:random good` (PRD FR-SR-5): batches 10 random titles against one
/// `pageassessments` query (`random::pick_random_good`) and opens the first
/// title assessed ≥ GA, falling back to opening the batch's own first title
/// (with a notice) when none qualify — §7's "graceful degradation, never an
/// error screen" posture.
async fn open_random_good_article(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
) {
    app.loading = true;
    let lang = app.lang.clone();
    match random::pick_random_good(client, &lang).await {
        Ok(random::RandomGood::Found(title)) => {
            open_title(client, cache, app, &title, revalidate_tx).await;
            return;
        }
        Ok(random::RandomGood::Fallback(title)) => {
            open_title(client, cache, app, &title, revalidate_tx).await;
            app.notice = Some(
                "no good article found in this batch — opened a random article instead".to_string(),
            );
            return;
        }
        Ok(random::RandomGood::Empty) => {
            app.status = "Random good article: the wiki returned nothing".to_string();
        }
        Err(e) => app.status = format!("Random good article failed: {e}"),
    }
    app.loading = false;
}

/// Lazily fetch+decode inline images the active document references (PRD
/// FR-RD-8), one spawned task per source URL. A no-op unless images are
/// enabled *and* the terminal has a graphics protocol — so a text theme
/// (`images = false`) never touches the network for images (FR-TH-7), which
/// is the property the text-theme pty check asserts. Idempotent: the store's
/// `contains` guard means a source already loading/loaded/failed is never
/// re-fetched. Gallery thumbnails are not fetched (galleries render as
/// caption text in this build).
fn request_visible_images(client: &WikiClient, app: &mut App, tx: &UnboundedSender<ImageOutcome>) {
    if !app.images_enabled() || matches!(app.graphics_protocol(), graphics::GraphicsProtocol::None)
    {
        return;
    }
    let mut to_load: Vec<String> = Vec::new();
    if let Some(doc) = app.active_tab().doc.as_ref() {
        for block in &doc.blocks {
            if let doc::Block::Image { src: Some(src), .. } = block
                && !app.image_store.contains(src)
                && !to_load.contains(src)
            {
                to_load.push(src.clone());
            }
        }
    }
    for src in to_load {
        app.image_store.mark_loading(src.clone());
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let decoded = match client.fetch_image(&src).await {
                Ok(bytes) => crate::image::decode_image(&bytes),
                Err(_) => None,
            };
            let _ = tx.send(ImageOutcome { src, decoded });
        });
    }
}

/// PRD FR-DL-1's picture of the day: fetches+decodes the feed's thumbnail
/// through the exact same pipeline `request_visible_images` uses for inline
/// article images (`image::decode_image`, `App::image_store`,
/// `App::deliver_image` on the result — no new application code needed,
/// only this fetch trigger) so a text theme (`images = false`) never touches
/// the network for it either, same as an article's own images. A no-op
/// unless the start page is actually showing (a document is open in the
/// active tab) or the feed hasn't produced a thumbnail URL yet.
fn request_start_page_image(
    client: &WikiClient,
    app: &mut App,
    tx: &UnboundedSender<ImageOutcome>,
) {
    if app.active_tab().doc.is_some()
        || !app.images_enabled()
        || matches!(app.graphics_protocol(), graphics::GraphicsProtocol::None)
    {
        return;
    }
    let Some(src) = app.start_page_model().potd_thumb_url else {
        return;
    };
    if app.image_store.contains(&src) {
        return;
    }
    app.image_store.mark_loading(src.clone());
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let decoded = match client.fetch_image(&src).await {
            Ok(bytes) => crate::image::decode_image(&bytes),
            Err(_) => None,
        };
        let _ = tx.send(ImageOutcome { src, decoded });
    });
}

/// `:config reload` and SIGHUP both land here (PRD §6.7): re-resolve
/// against the exact CLI/env overrides pinned at startup — so they still
/// outrank the file after a reload — and live-apply theme, measure, and
/// ambiguous_wide. Network/storage settings (cache size/TTL, base URL) are
/// documented as restart-only, so `client`/`cache` are deliberately left
/// untouched here.
fn apply_config_reload(app: &mut App) {
    let resolved = config::resolve(
        &app.config_ctx.cli,
        &app.config_ctx.env,
        app.config_ctx.config_path.as_deref(),
    );
    if let Some(theme) = Theme::by_name(&resolved.theme.value) {
        app.set_theme(theme);
    }
    app.measure = resolved.measure.value;
    app.ambiguous_wide = resolved.ambiguous_wide.value;
    app.readlater_auto_dequeue = resolved.readlater_auto_dequeue.value;
    // PRD FR-TH-7: a reload can flip the `images` config override; a `:set`
    // made this session is a per-run override that the config file's value
    // does not silently undo, so only take the file's value when it set one.
    if resolved.images.source != config::Source::Default {
        app.images_override = resolved.images.value;
        app.note_image_state_change();
    }
    app.include_nonfree = resolved.include_nonfree.value;
    // Measure/ambiguous_wide feed layout, not just paint — drop the cached
    // layout so the next `ensure_layout` recomputes instead of reusing a
    // stale one keyed on the old options.
    app.layout = None;
    app.notice = Some(if resolved.issues.is_empty() {
        "Config reloaded".to_string()
    } else {
        format!(
            "Config reloaded ({} warning(s) — see `wikitui config doctor`)",
            resolved.issues.len()
        )
    });
}

/// Fetch and open `title` as a fresh navigation (pushes the current article
/// onto the back stack — see `App::open_document`). Used for the initial
/// CLI title, search results, and following a link. When the fetch served a
/// stale-but-within-backstop cache hit, spawns the PRD FR-OFF-2 background
/// revalidation `revalidate_tx` will eventually report back.
async fn open_title(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    title: &str,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
) {
    app.loading = true;
    let lang = app.lang.clone();
    // NF-NET-1: hold the foreground gate across this interactive fetch so the
    // background worker yields — a prefetch already draining never delays the
    // article the reader is waiting on.
    let outcome = {
        let _fg = app
            .prefetch
            .as_ref()
            .map(netqueue::SubstrateHandle::foreground_guard);
        fetch_page(client, cache, &lang, title).await
    };
    match outcome {
        Ok(outcome) => {
            // PRD §5.7: a pinned saved copy is the intended offline artifact,
            // so it takes precedence over a stale cache serve — but never over
            // a live/fresh copy, which is genuinely newer.
            if matches!(outcome.source, PageSource::Offline { .. })
                && app.saved.is_saved(&lang, title)
            {
                app.loading = false;
                open_saved(app, &lang, title);
                return;
            }
            let document = doc::parse_article_html(title, &outcome.html);
            {
                let tab = app.active_tab_mut();
                tab.page_source = outcome.source;
                tab.current_revid = outcome.revid;
            }
            app.open_document(document);
            if let Some(cached_revid) = outcome.revalidate {
                let tab_id = app.active_tab().id;
                if fire_revalidation(
                    app,
                    client,
                    tab_id,
                    lang,
                    title.to_string(),
                    cached_revid,
                    revalidate_tx,
                ) {
                    app.pending_revalidations += 1;
                }
            }
            // PRD FR-PF-1: with the article shown, rank its links and prefetch
            // the top-N bodies into L2 (gated on the kill switch + incognito).
            schedule_link_prefetch(app);
        }
        Err(e) => {
            // §7's "Offline, uncached link": the network failed and nothing is
            // cached. If the page is pinned, serve that (▣); otherwise offer
            // the queue-for-fetch / search-saved card (FR-OFF-6).
            app.loading = false;
            if app.saved.is_saved(&lang, title) {
                open_saved(app, &lang, title);
            } else {
                app.status = format!("Error: {e}");
                app.show_offline_card(lang, title.to_string());
            }
            return;
        }
    }
    app.loading = false;
}

/// Fetch and install a back/forward (or `gb`-jump) history entry into the
/// active tab without touching its stacks — `App::navigate_*`/
/// `jump_to_back_entry` already adjusted them — and restore the entry's saved
/// scroll position (PRD FR-TB-2 v1.0: "preserving scroll state"). Fetches in
/// the entry's own language so cross-language history restores correctly.
async fn open_history_entry(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    entry: HistoryEntry,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
) {
    app.loading = true;
    app.lang = entry.lang.clone();
    let outcome = {
        let _fg = app
            .prefetch
            .as_ref()
            .map(netqueue::SubstrateHandle::foreground_guard);
        fetch_page(client, cache, &entry.lang, &entry.title).await
    };
    match outcome {
        Ok(outcome) => {
            let document = doc::parse_article_html(&entry.title, &outcome.html);
            {
                let tab = app.active_tab_mut();
                tab.page_source = outcome.source;
                tab.current_revid = outcome.revid;
            }
            app.set_document(document);
            // Restore the scroll position we left this page at (set_document
            // reset it to the top); the draw clamps it to the article's real
            // extent, which is unchanged since it's the same article.
            app.active_tab_mut().scroll = entry.scroll;
            if let Some(cached_revid) = outcome.revalidate {
                let tab_id = app.active_tab().id;
                if fire_revalidation(
                    app,
                    client,
                    tab_id,
                    entry.lang,
                    entry.title,
                    cached_revid,
                    revalidate_tx,
                ) {
                    app.pending_revalidations += 1;
                }
            }
            schedule_link_prefetch(app);
        }
        Err(e) => {
            app.status = format!("Error: {e}");
        }
    }
    app.loading = false;
}

#[allow(clippy::too_many_arguments)]
async fn handle_key(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    open_tx: &UnboundedSender<TabLoadOutcome>,
    save_tx: &UnboundedSender<SaveOutcome>,
    related_tx: &UnboundedSender<RelatedOutcome>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) {
    // `Q`'s one-keypress quit confirmation (PRD Appendix B) is intercepted
    // before any mode dispatch so no other binding can leak through: `y`
    // confirms the quit, anything else cancels it.
    if app.pending_quit_confirm {
        app.pending_quit_confirm = false;
        app.notice = None;
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => app.should_quit = true,
            _ => app.status = "Quit cancelled".to_string(),
        }
        return;
    }

    // PRD FR-OFF-5's bulk-save cost-preview confirmation, intercepted the same
    // way: `y` proceeds with the resolved target list on the background save
    // queue, anything else cancels. Nothing was fetched until this point.
    if let Some(request) = app.pending_bulk_save.take() {
        app.notice = None;
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let source_note = format!("bulk: {}", request.label);
                fire_save_job(
                    client,
                    cache,
                    app,
                    request.targets,
                    request.tier,
                    source_note,
                    save_tx,
                );
            }
            _ => app.notice = Some("Bulk save cancelled".to_string()),
        }
        return;
    }

    match app.mode {
        Mode::Help => {
            // Any key closes the help overlay.
            app.mode = app.prior_mode;
        }
        // PRD FR-PF-4: the prefetch-log panel is read-only — any key closes it.
        Mode::PrefetchLog => app.close_prefetch_log(),
        // PRD Appendix B's search keybindings: Enter opens the highlighted
        // typeahead suggestion directly (FR-SR-1); Tab runs a full-text
        // search of the typed query instead (FR-SR-2's mode toggle) — the
        // two are deliberately separate actions, not Enter-falls-back-to-
        // search, so the dropdown and full-text results never fight over
        // what Enter means.
        // PRD FR-SR-3b: the operator cheat-sheet overlay takes over the whole
        // prompt while it's up — only `?`/Esc close it, every other key is
        // swallowed rather than typed into `search_input` (unlike the
        // generic `Mode::Help`'s "any key closes", which doesn't fit a
        // prompt still mid-edit).
        Mode::Search if app.search_operator_help => match code {
            KeyCode::Char('?') | KeyCode::Esc => app.search_operator_help = false,
            _ => {}
        },
        Mode::Search => match code {
            KeyCode::Esc => {
                app.mode = Mode::Reading;
                app.search_input.clear();
                app.typeahead.clear();
                app.search_debounce_at = None;
            }
            KeyCode::Enter => {
                if let Some(suggestion) = app.typeahead.get(app.selected_suggestion).cloned() {
                    let title = suggestion.title;
                    app.typeahead.clear();
                    app.search_debounce_at = None;
                    open_title(client, cache, app, &title, revalidate_tx).await;
                } else {
                    app.status = "No suggestion selected — Tab searches full text".to_string();
                }
            }
            // PRD FR-SR-3c: Tab first tries operator-*name* completion
            // (`morel` -> `morelike:`); only when the last word isn't an
            // unambiguous operator prefix does Tab keep its other meaning,
            // full-text search (FR-SR-2).
            KeyCode::Tab => {
                if let Some(completed) = search_ops::complete_operator_name(&app.search_input) {
                    app.search_input = completed;
                    app.queue_typeahead();
                } else if !app.search_input.trim().is_empty() {
                    run_search(client, app).await;
                }
            }
            KeyCode::Up => app.move_suggestion(false),
            KeyCode::Down => app.move_suggestion(true),
            KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
                app.move_suggestion(true)
            }
            KeyCode::Char('p') if modifiers.contains(KeyModifiers::CONTROL) => {
                app.move_suggestion(false)
            }
            KeyCode::Backspace => {
                app.search_input.pop();
                app.queue_typeahead();
            }
            // PRD FR-SR-3b: `?` opens the operator cheat-sheet instead of
            // being typed — CirrusSearch operators never need a literal `?`
            // in the query, so reserving it (lazygit-style, PRD §2.2) costs
            // nothing real queries would use.
            KeyCode::Char('?') => app.search_operator_help = true,
            KeyCode::Char(c) => {
                app.search_input.push(c);
                app.queue_typeahead();
            }
            _ => {}
        },
        Mode::Command => match code {
            KeyCode::Esc => {
                app.mode = Mode::Reading;
                app.command_input.clear();
            }
            KeyCode::Enter => {
                let input = app.command_input.clone();
                app.command_input.clear();
                app.mode = Mode::Reading;
                match command::parse(&input) {
                    Ok(cmd) => {
                        execute_command(client, cache, app, cmd, revalidate_tx, save_tx, related_tx)
                            .await
                    }
                    Err(message) => app.notice = Some(message),
                }
            }
            KeyCode::Backspace => {
                app.command_input.pop();
            }
            KeyCode::Char(c) => {
                app.command_input.push(c);
            }
            _ => {}
        },
        Mode::Find => match code {
            KeyCode::Esc => {
                app.mode = Mode::Reading;
                app.clear_find();
            }
            KeyCode::Enter => app.mode = Mode::Reading,
            KeyCode::Backspace => {
                app.active_tab_mut().find_input.pop();
                app.update_find();
            }
            KeyCode::Char(c) => {
                app.active_tab_mut().find_input.push(c);
                app.update_find();
            }
            _ => {}
        },
        // PRD FR-NV-1's link hints: Esc cancels, backspace un-narrows, and
        // any other character either narrows the visible set or (once it
        // completes exactly one label) resolves and follows it. The actual
        // fetch/background-tab-open happens here, not in `App`, because it
        // needs the network/channel handles `handle_key` already has —
        // `App::resolve_hint_action` decides WHAT to do (testable without
        // touching either), this just carries it out.
        Mode::Hint => match code {
            KeyCode::Esc => app.exit_hint_mode(),
            KeyCode::Backspace => {
                app.hint_input.pop();
            }
            KeyCode::Char(c) => {
                if let hints::HintOutcome::Resolved(link_idx) = app.narrow_hint_input(c) {
                    let action = app.resolve_hint_action(link_idx);
                    app.exit_hint_mode();
                    match action {
                        Some(app::HintFollowAction::Foreground(title)) => {
                            open_title(client, cache, app, &title, revalidate_tx).await;
                        }
                        Some(app::HintFollowAction::Background(title)) => {
                            let lang = app.lang.clone();
                            let id = app.open_background_tab(title.clone(), lang.clone());
                            fire_background_load(client, cache, id, lang, title, open_tx);
                            app.status = "Opening in a background tab…".to_string();
                        }
                        Some(app::HintFollowAction::External(href)) => {
                            app.status = format!("External link: {href}");
                        }
                        None => {}
                    }
                }
            }
            _ => {}
        },
        Mode::Research => match code {
            KeyCode::Esc => app.mode = Mode::Reading,
            KeyCode::Char('j') | KeyCode::Down => app.cycle_citation(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_citation(false),
            KeyCode::Enter | KeyCode::Char('s') => app.save_selected_citation(),
            KeyCode::Char('R') => app.open_library(),
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        Mode::Library => match code {
            KeyCode::Esc => app.close_library(),
            KeyCode::Char('j') | KeyCode::Down => app.cycle_library(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_library(false),
            KeyCode::Char('s') => app.cycle_cite_style(),
            KeyCode::Char('d') => app.delete_selected_library(),
            KeyCode::Char('e') => app.export_bibliography(),
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        Mode::Results => match code {
            KeyCode::Esc => {
                app.mode = Mode::Search;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !app.results.is_empty() {
                    app.selected_result = (app.selected_result + 1).min(app.results.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.selected_result = app.selected_result.saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(result) = app.results.get(app.selected_result).cloned() {
                    open_title(client, cache, app, &result.title, revalidate_tx).await;
                } else if let Some(suggestion) = app.search_suggestion.clone() {
                    // PRD FR-SR-4 / §7's zero-results row: "Did you mean X?
                    // (Enter to search)" — re-runs the search with the
                    // suggested spelling.
                    app.search_input = suggestion;
                    run_search(client, app).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-SR-6's Related panel: a selectable `morelike:` list, same
        // j/k/Enter/Esc grammar as every other picker in this match.
        Mode::Related => match code {
            KeyCode::Esc => app.close_related(),
            KeyCode::Char('j') | KeyCode::Down => app.related_move(1),
            KeyCode::Char('k') | KeyCode::Up => app.related_move(-1),
            KeyCode::Enter => {
                if let Some(title) = app.related_open_target() {
                    open_title(client, cache, app, &title, revalidate_tx).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        Mode::Toc => match code {
            KeyCode::Esc => app.mode = Mode::Reading,
            KeyCode::Char('j') | KeyCode::Down => {
                let len = app.active_tab().sections.len();
                if len > 0 {
                    let next = (app.active_tab().selected_section + 1).min(len - 1);
                    app.active_tab_mut().selected_section = next;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let prev = app.active_tab().selected_section.saturating_sub(1);
                app.active_tab_mut().selected_section = prev;
            }
            KeyCode::Enter => app.jump_to_section(app.active_tab().selected_section),
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-TB-1's `bb` tab picker: a selectable list of open tabs.
        // Enter switches, `d` closes the highlighted tab, Esc cancels.
        Mode::TabPicker => match code {
            KeyCode::Esc => app.mode = Mode::Reading,
            KeyCode::Char('j') | KeyCode::Down => {
                if !app.tabs.is_empty() {
                    app.selected_tab_pick = (app.selected_tab_pick + 1).min(app.tabs.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.selected_tab_pick = app.selected_tab_pick.saturating_sub(1);
            }
            KeyCode::Enter => {
                let index = app.selected_tab_pick;
                app.switch_to_tab(index); // sync_active_tab lands us in Reading
            }
            KeyCode::Char('d') => {
                // Closing the last tab quits (same invariant as `q`).
                if app.close_tab(app.selected_tab_pick) {
                    app.should_quit = true;
                } else {
                    // Stay in the picker so several tabs can be closed in a row.
                    app.mode = Mode::TabPicker;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-NV-7's `gb` back-stack picker: the active tab's history
        // trail. Enter jumps to that entry (browser-style), Esc cancels.
        Mode::HistoryPicker => match code {
            KeyCode::Esc => app.mode = Mode::Reading,
            KeyCode::Char('j') | KeyCode::Down => {
                let len = app.active_tab().back_stack.len();
                if len > 0 {
                    app.selected_history = (app.selected_history + 1).min(len - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.selected_history = app.selected_history.saturating_sub(1);
            }
            KeyCode::Enter => {
                let index = app.selected_history;
                app.mode = Mode::Reading;
                if let Some(entry) = app.jump_to_back_entry(index) {
                    open_history_entry(client, cache, app, entry, revalidate_tx).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-BM-1's `B` / `:bookmarks` picker: a selectable list, `/`
        // enters the live filter (`Mode::BookmarkFilter`), `t` the inline
        // tag editor (`Mode::BookmarkTagEdit`), `d` deletes, Enter opens in
        // this tab (consistent with Results/history — no new-tab surprise).
        Mode::BookmarkPicker => match code {
            KeyCode::Esc => app.close_bookmark_picker(),
            KeyCode::Char('j') | KeyCode::Down => app.cycle_bookmark(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_bookmark(false),
            KeyCode::Char('/') => app.mode = Mode::BookmarkFilter,
            KeyCode::Char('t') => app.begin_tag_edit(),
            KeyCode::Char('d') => app.delete_selected_bookmark(),
            KeyCode::Enter => {
                if let Some(bookmark) = app.selected_bookmark_entry().cloned() {
                    app.lang = bookmark.lang.clone();
                    open_title(client, cache, app, &bookmark.title, revalidate_tx).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // The bookmark picker's `/` filter (PRD FR-BM-1's tag-expression +
        // fuzzy-title grammar, `bookmarks::parse_filter`): every keystroke
        // narrows the live list `handle_key`'s draw already reads from
        // `app.bookmark_filter_input`; Enter/Esc both just return to
        // navigating the (already-filtered) picker.
        Mode::BookmarkFilter => match code {
            KeyCode::Esc | KeyCode::Enter => {
                app.mode = Mode::BookmarkPicker;
                app.selected_bookmark = 0;
            }
            KeyCode::Backspace => {
                app.bookmark_filter_input.pop();
                app.selected_bookmark = 0;
            }
            KeyCode::Char(c) => {
                app.bookmark_filter_input.push(c);
                app.selected_bookmark = 0;
            }
            _ => {}
        },
        // The picker's `t` inline tag editor (PRD FR-BM-1): a small prompt,
        // comma/space-separated, that replaces the selected bookmark's tag
        // set outright on Enter (`bookmarks::parse_tags`).
        Mode::BookmarkTagEdit => match code {
            KeyCode::Esc => app.mode = Mode::BookmarkPicker,
            KeyCode::Enter => app.commit_tag_edit(),
            KeyCode::Backspace => {
                app.bookmark_tag_input.pop();
            }
            KeyCode::Char(c) => app.bookmark_tag_input.push(c),
            _ => {}
        },
        // `:readlater`'s queue view (PRD FR-BM-3): Enter opens and — per
        // `readlater_auto_dequeue` — removes the entry; `d` removes without
        // opening.
        Mode::ReadLaterPicker => match code {
            KeyCode::Esc => app.close_readlater_picker(),
            KeyCode::Char('j') | KeyCode::Down => app.cycle_readlater(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_readlater(false),
            KeyCode::Char('d') => app.remove_selected_readlater(),
            KeyCode::Enter => {
                if let Some(entry) = app.take_selected_readlater() {
                    app.lang = entry.lang.clone();
                    open_title(client, cache, app, &entry.title, revalidate_tx).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // `Ctrl-h` / `:history`'s persistent reading-history picker (PRD
        // FR-HS-1): `/` enters the live filter (`Mode::ReadingHistoryFilter`),
        // `d` deletes that article's whole history, Enter opens in this tab
        // (consistent with every other picker — no new-tab surprise).
        Mode::ReadingHistory => match code {
            KeyCode::Esc => app.close_reading_history_picker(),
            KeyCode::Char('j') | KeyCode::Down => app.cycle_history_pick(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_history_pick(false),
            KeyCode::Char('/') => app.mode = Mode::ReadingHistoryFilter,
            KeyCode::Char('d') => app.delete_selected_history(),
            KeyCode::Enter => {
                if let Some(visit) = app
                    .history_pick_matches
                    .get(app.history_pick_selected)
                    .cloned()
                {
                    app.lang = visit.lang.clone();
                    open_title(client, cache, app, &visit.title, revalidate_tx).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // The reading-history picker's `/` filter: every keystroke narrows
        // `history_pick_filter` and rebuilds `history_pick_matches` right
        // away (unlike the bookmark filter's on-the-fly `visible_bookmarks`,
        // this list is a materialized, recency/fuzzy-ranked query result —
        // rebuilding it on every draw instead of every keystroke would mean
        // re-querying at 60 Hz for no reason). Enter/Esc both return to
        // `Mode::ReadingHistory` with the filter still applied.
        Mode::ReadingHistoryFilter => match code {
            KeyCode::Esc | KeyCode::Enter => app.mode = Mode::ReadingHistory,
            KeyCode::Backspace => {
                app.history_pick_filter.pop();
                app.refresh_history_matches();
            }
            KeyCode::Char(c) => {
                app.history_pick_filter.push(c);
                app.refresh_history_matches();
            }
            _ => {}
        },
        // PRD §5.7 / FR-OFF-4's saved-pages browser: Enter offline-serves the
        // pinned copy (▣), `d` un-pins, Esc closes.
        Mode::SavedPicker => match code {
            KeyCode::Esc => app.close_saved_picker(),
            KeyCode::Char('j') | KeyCode::Down => app.cycle_saved(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_saved(false),
            KeyCode::Char('d') => app.delete_selected_saved(),
            KeyCode::Enter => {
                if let Some((lang, title)) = app.selected_saved_target() {
                    app.lang = lang.clone();
                    app.mode = Mode::Reading;
                    open_saved(app, &lang, &title);
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // §7's "Offline, uncached link" card: `f` queues the target for
        // fetch-when-online, `s` opens the saved-pages browser, Esc dismisses.
        Mode::OfflineCard => match code {
            KeyCode::Char('f') => {
                app.queue_offline_target();
            }
            KeyCode::Char('s') => {
                app.close_offline_card();
                app.open_saved_picker();
            }
            KeyCode::Esc => app.close_offline_card(),
            _ => {}
        },
        // PRD FR-DL-2's `:today` panel: j/k move within the current type
        // tab, Tab/Shift-Tab (and h/l, since the tabs are laid out
        // horizontally) switch type, Enter opens the focused entry's linked
        // article.
        Mode::OnThisDay => match code {
            KeyCode::Esc => app.close_on_this_day(),
            KeyCode::Char('j') | KeyCode::Down => app.otd_move(1),
            KeyCode::Char('k') | KeyCode::Up => app.otd_move(-1),
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => app.otd_next_tab(),
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => app.otd_prev_tab(),
            KeyCode::Enter => {
                if let Some(title) = app.otd_open_target() {
                    open_title(client, cache, app, &title, revalidate_tx).await;
                } else {
                    app.status = "This entry links no article".to_string();
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        Mode::Reading => {
            // g-prefix chords (PRD Appendix B): the g-latch's second key.
            // `gg` top, `gt`/`gT` next/prev tab (FR-TB-1), `gb` back-stack
            // picker (FR-NV-7), `gh` home / start page (FR-DL-1), `gr`
            // random article (FR-SR-5), `gR` the Related panel (FR-SR-6).
            // An unrecognized second key falls through to its normal
            // binding (matching how `gj` still scrolls) — `resolve_g_prefix`
            // makes that dispatch decision testably without a live terminal.
            if app.pending_g {
                app.pending_g = false;
                if let KeyCode::Char(c) = code {
                    match app::resolve_g_prefix(c) {
                        app::GPrefixAction::Top => {
                            app.scroll_to_top();
                            return;
                        }
                        app::GPrefixAction::NextTab => {
                            app.next_tab();
                            return;
                        }
                        app::GPrefixAction::PrevTab => {
                            app.prev_tab();
                            return;
                        }
                        app::GPrefixAction::BackStack => {
                            if app.active_tab().back_stack.is_empty() {
                                app.status = "No history to show in this tab".to_string();
                            } else {
                                app.selected_history = app.active_tab().back_stack.len() - 1;
                                app.mode = Mode::HistoryPicker;
                            }
                            return;
                        }
                        app::GPrefixAction::Home => {
                            app.go_home();
                            return;
                        }
                        app::GPrefixAction::Random => {
                            open_random_article(client, cache, app, revalidate_tx).await;
                            return;
                        }
                        app::GPrefixAction::Related => {
                            open_related(app, client, related_tx);
                            return;
                        }
                        app::GPrefixAction::PassThrough => {} // handle this key normally below.
                    }
                }
            }
            // b-prefix chord: `bb` opens the tab picker (FR-TB-1), `ba`
            // annotates the current article's bookmark (FR-BM-2). Any other
            // second key cancels the latch and is handled normally (falls
            // through to `code`'s own binding below, matching the g-prefix's
            // dead-prefix fallback).
            if app.pending_b {
                app.pending_b = false;
                if let KeyCode::Char(c) = code {
                    match app::resolve_b_prefix(c) {
                        app::BPrefixAction::TabPicker => {
                            app.selected_tab_pick = app.active;
                            app.mode = Mode::TabPicker;
                            return;
                        }
                        app::BPrefixAction::Annotate => {
                            annotate_current_article(terminal, app);
                            return;
                        }
                        app::BPrefixAction::PassThrough => {} // handle `code` normally below.
                    }
                }
            }

            // Captured before `app.notice` is cleared below: PRD FR-OFF-2's
            // "updated — r to reload" notice means this keypress, if it's
            // `r`, reloads instead of arming the read-later/Research prefix
            // — see the `r` arm's own comment for why the two share a key.
            let had_pending_reload = app.active_tab().pending_reload.is_some();
            // `r`-prefix chord (PRD FR-BM-3's `rl`), reached only when no
            // reload is pending (see above): `rl` enqueues for later, any
            // other second key falls back to `r`'s own original meaning
            // (open Research mode) — see `app::resolve_r_prefix`'s doc
            // comment for why this, unlike `g`/`b`, consumes the second key
            // rather than reprocessing it as its own binding.
            if app.pending_r {
                app.pending_r = false;
                app.notice = None;
                if let KeyCode::Char(c) = code {
                    match app::resolve_r_prefix(c) {
                        app::RPrefixAction::ReadLater => {
                            enqueue_read_later(client, cache, app).await;
                        }
                        app::RPrefixAction::OpenResearch => {
                            if app.active_tab().doc.is_some() {
                                app.mode = Mode::Research;
                            } else {
                                app.status = "Open an article first".to_string();
                            }
                        }
                    }
                } else if app.active_tab().doc.is_some() {
                    app.mode = Mode::Research;
                } else {
                    app.status = "Open an article first".to_string();
                }
                return;
            }
            app.notice = None;
            match code {
                // PRD Appendix B: `q` closes the current tab (quitting if it
                // was the last); `Q` quits outright behind a one-keypress
                // y/n confirm (armed here, resolved at the top of handle_key).
                KeyCode::Char('q') => {
                    if app.close_active_tab() {
                        app.should_quit = true;
                    }
                }
                KeyCode::Char('Q') => {
                    app.pending_quit_confirm = true;
                    app.notice = Some("really quit? (y/n)".to_string());
                }
                KeyCode::Char('/') => {
                    app.mode = Mode::Search;
                    app.search_input.clear();
                    app.typeahead.clear();
                    app.search_suggestion = None;
                    app.search_debounce_at = None;
                }
                KeyCode::Char('?') => {
                    app.prior_mode = app.mode;
                    app.mode = Mode::Help;
                }
                // PRD FR-DL-1: with no document open in this tab, the content
                // area is the start page, not an article — j/k/Tab/Shift-Tab
                // move its selection instead of scrolling/cycling links, and
                // `t` rerolls the TIL widget (FR-DL-7) instead of opening the
                // TOC (which has nothing to show anyway with no document).
                // Checked once, up front of each binding's normal arm, rather
                // than duplicating the whole match — matches how the g-/b-/
                // r-prefix latches above already special-case their own
                // "what does this mode mean right now" branch.
                KeyCode::Char('j') | KeyCode::Down if app.active_tab().doc.is_none() => {
                    app.start_page_move(1)
                }
                KeyCode::Char('k') | KeyCode::Up if app.active_tab().doc.is_none() => {
                    app.start_page_move(-1)
                }
                KeyCode::Char('j') | KeyCode::Down => app.scroll_by(1),
                KeyCode::Char('k') | KeyCode::Up => app.scroll_by(-1),
                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_by(10)
                }
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_by(-10)
                }
                KeyCode::Char(' ') => app.scroll_by(15),
                // PRD FR-RD-4's horizontal table scroll: `[`/`]` shift the
                // shared column window of every wide table in the article
                // left/right (documented "simplest coherent model" — one
                // offset for the page, clamped to the widest table).
                KeyCode::Char(']') => app.scroll_tables(1),
                KeyCode::Char('[') => app.scroll_tables(-1),
                KeyCode::Tab if app.active_tab().doc.is_none() => app.start_page_move(1),
                KeyCode::BackTab if app.active_tab().doc.is_none() => app.start_page_move(-1),
                KeyCode::Tab => app.cycle_link(true),
                KeyCode::BackTab => app.cycle_link(false),
                // PRD FR-NV-1: `f` labels every visible link and follows the
                // typed one in this tab; `F` does the same but resolves into
                // a background tab (see `Mode::Hint`'s own arm below for the
                // typing/resolution side). Guarded against Ctrl so it doesn't
                // shadow Ctrl-f's own (later, unrelated) arm below — matching
                // this match's existing convention of checking the more
                // specific/guarded binding first (see Ctrl-Enter vs Enter).
                KeyCode::Char('f') if !modifiers.contains(KeyModifiers::CONTROL) => {
                    app.enter_hint_mode(false)
                }
                KeyCode::Char('F') => app.enter_hint_mode(true),
                // PRD FR-TB-3: Ctrl-Enter opens the focused internal link in a
                // BACKGROUND tab — the fetch fires immediately via the tab-load
                // channel and focus does NOT move. (Budget-aware prefetch
                // scheduling for these is a later chunk; for now it fetches
                // eagerly.) Checked before the plain-Enter arm below.
                // PRD FR-DL-1: Enter on the start page opens the focused
                // item (TFA/most-read/news/on-this-day/TIL) in this tab —
                // Ctrl-Enter's "open in background tab" has no start-page
                // equivalent, so this is checked before that arm too.
                KeyCode::Enter if app.active_tab().doc.is_none() => {
                    match app.start_page_open_target() {
                        Some((lang, title)) => {
                            if let Some(lang) = lang {
                                app.lang = lang;
                            }
                            open_title(client, cache, app, &title, revalidate_tx).await;
                        }
                        None => app.status = "Nothing focused to open".to_string(),
                    }
                }
                KeyCode::Enter if modifiers.contains(KeyModifiers::CONTROL) => {
                    let link = app
                        .active_tab()
                        .focused_link
                        .and_then(|i| app.active_tab().links.get(i))
                        .cloned();
                    match link.and_then(|l| l.internal_title) {
                        Some(title) => {
                            let lang = app.lang.clone();
                            let id = app.open_background_tab(title.clone(), lang.clone());
                            fire_background_load(client, cache, id, lang, title, open_tx);
                            app.status = "Opening in a background tab…".to_string();
                        }
                        None => {
                            app.status = "No internal link focused to open in a tab".to_string();
                        }
                    }
                }
                KeyCode::Enter => {
                    let link = app
                        .active_tab()
                        .focused_link
                        .and_then(|i| app.active_tab().links.get(i))
                        .cloned();
                    if let Some(link) = link {
                        match link.internal_title {
                            Some(title) => {
                                open_title(client, cache, app, &title, revalidate_tx).await
                            }
                            None => app.status = format!("External link: {}", link.href),
                        }
                    }
                }
                KeyCode::Char('H') => {
                    if let Some(entry) = app.navigate_back_target() {
                        open_history_entry(client, cache, app, entry, revalidate_tx).await;
                    } else {
                        app.status = "No earlier page in history".to_string();
                    }
                }
                KeyCode::Char('L') => {
                    if let Some(entry) = app.navigate_forward_target() {
                        open_history_entry(client, cache, app, entry, revalidate_tx).await;
                    } else {
                        app.status = "No later page in history".to_string();
                    }
                }
                // `u` reopens the last closed tab (PRD FR-TB-1). Distinct from
                // Ctrl-u (half-page up), which is a guarded arm above.
                KeyCode::Char('u') => {
                    if !app.reopen_closed_tab() {
                        app.status = "No recently closed tabs to reopen".to_string();
                    }
                }
                KeyCode::Char('t') if app.active_tab().doc.is_none() => app.reroll_til(),
                KeyCode::Char('t') => {
                    if app.active_tab().sections.is_empty() {
                        app.status = "No sections on this page".to_string();
                    } else {
                        app.mode = Mode::Toc;
                    }
                }
                KeyCode::Char('T') => app.cycle_theme(),
                // PRD FR-OFF-4 / Appendix B ("S save offline"): pin the
                // current article at the default depth (T0). `:save t1|t2`
                // pins deeper; `:saved` browses the store.
                KeyCode::Char('S') => start_current_save(client, cache, app, Tier::T0, save_tx),
                KeyCode::Char(':') => {
                    app.mode = Mode::Command;
                    app.command_input.clear();
                }
                KeyCode::Char('y') => {
                    if let Some(url) = app.yank_url() {
                        app.status = match yank_to_clipboard(&url) {
                            Ok(()) => format!("Yanked {url}"),
                            Err(e) => format!("Yank failed: {e}"),
                        };
                    } else {
                        app.status = "Open an article first".to_string();
                    }
                }
                KeyCode::Char('Y') => {
                    if let Some(link) = app.yank_markdown() {
                        app.status = match yank_to_clipboard(&link) {
                            Ok(()) => format!("Yanked {link}"),
                            Err(e) => format!("Yank failed: {e}"),
                        };
                    } else {
                        app.status = "Open an article first".to_string();
                    }
                }
                // PRD FR-OFF-2's "r to reload", Research mode's `r`
                // entrypoint, and FR-BM-3's `rl` read-later chord all share
                // this key: when a background revalidation just posted an
                // update notice, `r` reloads the article on the spot (the
                // notice already told the reader what `r` means right now),
                // taking precedence over the prefix below entirely — a
                // reload is a one-keystroke action, never a chord. Only when
                // no reload is pending does `r` become the two-keystroke
                // `r`-prefix latch (handled at the top of this arm, mirroring
                // `pending_g`/`pending_b`) rather than opening Research mode
                // on the spot as it once did.
                KeyCode::Char('r') if had_pending_reload => {
                    app.reload_from_pending_update(cache);
                }
                KeyCode::Char('r') => {
                    app.pending_r = true;
                }
                KeyCode::Char('m') => app.toggle_bookmark(),
                KeyCode::Char('B') => app.open_bookmark_picker(),
                KeyCode::Char('R') => app.open_library(),
                // PRD FR-HS-1: Ctrl-h opens the persistent reading-history
                // picker (distinct from `gb`'s per-tab back-stack picker).
                KeyCode::Char('h') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.open_reading_history_picker();
                }
                KeyCode::Char('f') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.mode = Mode::Find;
                    app.clear_find();
                }
                KeyCode::Char('n') => {
                    if app.active_tab().find_matches.is_empty() {
                        app.status = "No active search — Ctrl-f to find in this page".to_string();
                    } else {
                        app.find_next();
                    }
                }
                KeyCode::Char('N') => {
                    if app.active_tab().find_matches.is_empty() {
                        app.status = "No active search — Ctrl-f to find in this page".to_string();
                    } else {
                        app.find_prev();
                    }
                }
                // Arm the g-/b-prefix latches (their second key is consumed at
                // the top of this arm on the next keypress).
                KeyCode::Char('g') => app.pending_g = true,
                KeyCode::Char('b') => app.pending_b = true,
                KeyCode::Char('G') => app.scroll_to_bottom(),
                KeyCode::Esc => {
                    app.pending_g = false;
                    app.pending_b = false;
                    app.pending_r = false;
                    app.clear_find();
                }
                _ => {}
            }
        }
    }

    if !matches!(code, KeyCode::Char('g')) {
        app.pending_g = false;
    }
    if !matches!(code, KeyCode::Char('b')) {
        app.pending_b = false;
    }
    if !matches!(code, KeyCode::Char('r')) {
        app.pending_r = false;
    }
}

/// Executes a parsed `:` command. Parsing already validated arguments
/// (theme/style/lang names), so the arms here mostly delegate to existing
/// features.
#[allow(clippy::too_many_arguments)]
async fn execute_command(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    cmd: command::Command,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    save_tx: &UnboundedSender<SaveOutcome>,
    related_tx: &UnboundedSender<RelatedOutcome>,
) {
    use command::{Command, RandomSpec, SaveSpec};
    match cmd {
        Command::Open(raw) => {
            // Same grammar as the CLI TITLE argument: URLs and
            // lang-prefixed titles work here too.
            let target = target::parse(&raw);
            if let Some(lang) = target.lang {
                app.lang = lang;
            }
            open_title(client, cache, app, &target.title, revalidate_tx).await;
        }
        Command::Lang(code) => {
            app.lang = code;
            app.notice = Some(format!(
                "Language: {} — searches and new articles use {}.wikipedia.org",
                app.lang, app.lang
            ));
        }
        Command::Theme(name) => {
            if let Some(theme) = Theme::by_name(&name) {
                app.set_theme(theme);
                app.notice = Some(format!("Theme: {name}"));
            }
        }
        Command::Set { key, value } => match key.as_str() {
            "images" => {
                let on = value == "on";
                app.set_images(on);
                app.notice = Some(format!("images={value}"));
            }
            // PRD FR-PF-6 kill switch.
            "prefetch" => app.set_prefetch(value == "on"),
            other => {
                app.notice = Some(format!(
                    "unknown :set key {other:?} (try: images, prefetch)"
                ));
            }
        },
        Command::PrefetchLog => app.open_prefetch_log(),
        Command::Style(name) => {
            if let Some(style) = cite::CiteStyle::by_name(&name) {
                app.cite_style = style;
                app.notice = Some(format!("Citation style: {}", style.label()));
            }
        }
        Command::Library => app.open_library(),
        Command::Research => {
            if app.active_tab().doc.is_some() {
                app.mode = Mode::Research;
            } else {
                app.notice = Some("Open an article first".to_string());
            }
        }
        Command::Toc => {
            if app.active_tab().sections.is_empty() {
                app.notice = Some("No sections on this page".to_string());
            } else {
                app.mode = Mode::Toc;
            }
        }
        Command::Export(style) => {
            if let Some(style) = style.and_then(|s| cite::CiteStyle::by_name(&s)) {
                app.cite_style = style;
            }
            app.export_bibliography();
        }
        Command::Help => {
            app.prior_mode = Mode::Reading;
            app.mode = Mode::Help;
        }
        Command::ConfigReload => apply_config_reload(app),
        // PRD FR-TB-1's tab ex-commands.
        Command::TabClose => {
            if app.close_active_tab() {
                app.should_quit = true;
            }
        }
        Command::TabNew(title) => {
            app.new_foreground_tab();
            if let Some(raw) = title {
                let target = target::parse(&raw);
                if let Some(lang) = target.lang {
                    app.lang = lang;
                }
                open_title(client, cache, app, &target.title, revalidate_tx).await;
            } else {
                app.notice = Some("New tab".to_string());
            }
        }
        Command::Tabs => {
            app.selected_tab_pick = app.active;
            app.mode = Mode::TabPicker;
        }
        Command::Bookmarks => app.open_bookmark_picker(),
        Command::BookmarksExport { format, path } => {
            let path = path.map(std::path::PathBuf::from);
            app.export_bookmarks(&format, path.as_deref());
        }
        Command::ReadLater => app.open_readlater_picker(),
        Command::History => app.open_reading_history_picker(),
        Command::HistoryClear(scope) => app.clear_history(scope),
        // PRD FR-OFF-4..7's `:save …`.
        Command::Save(spec) => match spec {
            SaveSpec::Current(tier) => start_current_save(client, cache, app, tier, save_tx),
            SaveSpec::Tag(tag) => {
                let targets: Vec<(String, String)> = app
                    .bookmarks
                    .bookmarks
                    .iter()
                    .filter(|b| b.tags.iter().any(|t| t.eq_ignore_ascii_case(&tag)))
                    .map(|b| (b.lang.clone(), b.title.clone()))
                    .collect();
                request_bulk_save(app, format!("tag #{tag}"), Tier::T0, targets);
            }
            SaveSpec::Tabs => {
                let targets: Vec<(String, String)> = app
                    .tabs
                    .iter()
                    .filter_map(|t| t.doc.as_ref().map(|d| (t.lang.clone(), d.title.clone())))
                    .collect();
                let label = format!("{} open tabs", targets.len());
                request_bulk_save(app, label, Tier::T0, targets);
            }
            SaveSpec::Category(cat) => {
                let lang = app.lang.clone();
                match client.fetch_category_members(&lang, &cat, 500).await {
                    Ok(members) => {
                        let targets: Vec<(String, String)> =
                            members.into_iter().map(|t| (lang.clone(), t)).collect();
                        let name = cat
                            .trim_start_matches("Category:")
                            .trim_start_matches("category:");
                        request_bulk_save(app, format!("Category:{name}"), Tier::T0, targets);
                    }
                    Err(e) => app.notice = Some(format!("Couldn't list category members: {e}")),
                }
            }
            SaveSpec::Export { format, path } => {
                let path = path.map(std::path::PathBuf::from);
                app.export_saved_page(&format, path.as_deref());
            }
        },
        Command::Saved => app.open_saved_picker(),
        Command::FetchQueue => drain_fetch_queue(client, cache, app).await,
        // PRD FR-DL-1: same action as the `gh` keybinding.
        Command::Start => app.go_home(),
        // PRD FR-DL-2.
        Command::Today => fetch_on_this_day(client, app).await,
        // PRD FR-SR-5: same actions as the `gr` keybinding.
        Command::Random(RandomSpec::Any) => {
            open_random_article(client, cache, app, revalidate_tx).await
        }
        Command::Random(RandomSpec::Good) => {
            open_random_good_article(client, cache, app, revalidate_tx).await
        }
        // PRD FR-SR-6: same action as the `gR` keybinding.
        Command::Related => open_related(app, client, related_tx),
        Command::Quit => app.should_quit = true,
    }
}

/// PRD FR-DL-2's `:today`: fetches all five on-this-day types up front
/// (foreground, one call each — see `api::WikiClient::fetch_onthisday_feed`'s
/// doc comment for why this isn't a `netqueue` job) and opens the panel.
/// Simplest coherent model for a P1 feature: lazy-per-tab-switch is a
/// documented later optimization if five small calls per `:today` ever
/// matters. A type whose fetch fails degrades to an empty list for that tab
/// rather than blocking the others or erroring the whole panel — the same
/// "graceful, not an error screen" posture the start page's offline
/// fallback uses.
async fn fetch_on_this_day(client: &WikiClient, app: &mut App) {
    app.open_on_this_day();
    let lang = app.lang.clone();
    // The month/day the panel shows — resolved once here (the one place in
    // this call this module reads the wall clock) and threaded down as
    // plain data from here on, matching the codebase's "never read the
    // clock in logic" convention (`prefetch::FeedCache` takes its date the
    // same way).
    let now = chrono::Utc::now();
    let (month, day) = (now.format("%m").to_string(), now.format("%d").to_string());
    let (month, day): (u32, u32) = (month.parse().unwrap_or(1), day.parse().unwrap_or(1));

    let mut any_ok = false;
    for t in startpage::OtdType::ALL {
        let entries = match client
            .fetch_onthisday_feed(&lang, t.wire_name(), month, day)
            .await
        {
            Ok(body) => match prefetch::parse_onthisday(&body, t.wire_name()) {
                Ok(entries) => {
                    any_ok = true;
                    entries
                }
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        };
        app.otd.set(t, entries);
    }
    app.status = if any_ok {
        "j/k: move  Tab/S-Tab or h/l: switch type  Enter: open  Esc: close".to_string()
    } else {
        "On this day is unavailable right now — Esc to close".to_string()
    };
}

async fn run_search(client: &WikiClient, app: &mut App) {
    app.loading = true;
    match client.search(&app.lang, &app.search_input, 20).await {
        Ok(outcome) => {
            app.results = outcome.results;
            app.search_suggestion = outcome.suggestion;
            app.selected_result = 0;
            app.mode = Mode::Results;
        }
        Err(e) => {
            app.status = format!("Search error: {e}");
            app.mode = Mode::Reading;
        }
    }
    app.loading = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PRD SEC-2: a hostile title containing raw ESC/BEL bytes must not be
    /// able to break out of the OSC 52 payload. In normal operation the
    /// title is already sanitized (PRD SEC-1) by the time it reaches here,
    /// but this property must hold unconditionally — base64 simply has no
    /// way to represent a control byte in its output alphabet.
    #[test]
    fn osc52_sequence_cannot_carry_a_raw_esc_or_bel_from_hostile_content() {
        let hostile = "\x1b]0;pwned\x07Evil\x1bTitle\x07with\x1bcontrol\x07bytes";
        let seq = osc52_clipboard_sequence(hostile);

        assert!(seq.starts_with("\x1b]52;c;"), "{seq:?}");
        assert!(seq.ends_with('\x07'), "{seq:?}");
        assert_eq!(
            seq.matches('\x1b').count(),
            1,
            "exactly one ESC — the sequence's own opener, not one contributed by content: {seq:?}"
        );
        assert_eq!(
            seq.matches('\x07').count(),
            1,
            "exactly one BEL — the sequence's own terminator, not one contributed by content: {seq:?}"
        );

        // Everything between the fixed prefix and the trailing BEL is pure
        // base64 output — its alphabet (`A-Za-z0-9+/=`) cannot itself
        // contain a control byte no matter what was encoded.
        let payload = &seq["\x1b]52;c;".len()..seq.len() - 1];
        assert!(
            payload
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')),
            "payload must be pure base64: {payload:?}"
        );

        // And round-tripping it back must recover the exact original bytes
        // (proving nothing was silently mutated on the way in either).
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(decoded, hostile.as_bytes());
    }

    #[test]
    fn osc52_sequence_round_trips_ordinary_text() {
        let seq = osc52_clipboard_sequence("Alan Turing");
        use base64::Engine as _;
        let payload = &seq["\x1b]52;c;".len()..seq.len() - 1];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(decoded, b"Alan Turing");
    }

    // ---- Background completion routing by tab id (PRD FR-TB-3, FR-OFF-2) --

    fn test_client() -> WikiClient {
        // A never-contacted client: the routing tests set `revalidate: None`
        // so no request is ever made through it.
        WikiClient::new("http://127.0.0.1:1/{lang}".to_string()).unwrap()
    }

    #[test]
    fn background_load_completion_installs_into_the_right_tab_by_id() {
        let client = test_client();
        let (revalidate_tx, _rx) = mpsc::unbounded_channel::<RevalidationOutcome>();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let id = app.open_background_tab("Enigma machine".to_string(), "en".to_string());
        assert!(app.tabs.iter().find(|t| t.id == id).unwrap().loading);

        apply_tab_load_outcome(
            &client,
            &mut app,
            TabLoadOutcome {
                tab_id: id,
                lang: "en".to_string(),
                title: "Enigma machine".to_string(),
                result: Ok(FetchOutcome {
                    html: "<html><body><p>rotor cipher</p></body></html>".to_string(),
                    source: PageSource::Live,
                    revid: 42,
                    revalidate: None,
                }),
            },
            &revalidate_tx,
        );

        let tab = app.tabs.iter().find(|t| t.id == id).unwrap();
        assert!(!tab.loading, "the background tab is no longer loading");
        assert_eq!(tab.current_revid, 42);
        assert_eq!(tab.doc.as_ref().unwrap().title, "Enigma machine");
    }

    #[test]
    fn background_completion_for_a_closed_tab_is_dropped() {
        let client = test_client();
        let (revalidate_tx, _rx) = mpsc::unbounded_channel::<RevalidationOutcome>();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let id = app.open_background_tab("Enigma machine".to_string(), "en".to_string());
        // Close the background tab before its fetch lands.
        let index = app.tab_index_by_id(id).unwrap();
        app.close_tab(index);
        assert!(app.tab_index_by_id(id).is_none());

        // Applying the now-stale completion must not panic or resurrect it.
        apply_tab_load_outcome(
            &client,
            &mut app,
            TabLoadOutcome {
                tab_id: id,
                lang: "en".to_string(),
                title: "Enigma machine".to_string(),
                result: Ok(FetchOutcome {
                    html: "<html><body><p>x</p></body></html>".to_string(),
                    source: PageSource::Live,
                    revid: 1,
                    revalidate: None,
                }),
            },
            &revalidate_tx,
        );
        assert_eq!(app.tabs.len(), 1, "no tab was resurrected");
    }

    #[test]
    fn revalidation_notice_shows_only_for_the_active_tab_and_rearms_on_switch() {
        let cache = PageCache::disabled();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // Tab 0 (active) holds "A"; tab 1 (background) holds "B".
        app.active_tab_mut().current_revid = 1;
        app.set_document(doc::parse_article_html(
            "A",
            "<html><body><p>a</p></body></html>",
        ));
        let tab0 = app.active_tab().id;
        app.new_foreground_tab();
        app.set_document(doc::parse_article_html(
            "B",
            "<html><body><p>b</p></body></html>",
        ));
        let tab1 = app.active_tab().id;
        // Back to tab 0.
        app.switch_to_tab(app.tab_index_by_id(tab0).unwrap());

        // A revalidation for the BACKGROUND tab 1 must update its state but not
        // pop a notice on the active tab 0.
        apply_revalidation_outcome(
            &mut app,
            &cache,
            RevalidationOutcome {
                tab_id: tab1,
                lang: "en".to_string(),
                title: "B".to_string(),
                result: Some(RevalidationResult::Changed {
                    html: "<html><body><p>b2</p></body></html>".to_string(),
                    revid: 2,
                    etag: None,
                }),
            },
        );
        assert!(
            app.notice.is_none(),
            "a background tab's update must not notify the active tab"
        );
        let bg = app.tab_index_by_id(tab1).unwrap();
        assert!(
            app.tabs[bg].pending_reload.is_some(),
            "the background tab still records the pending reload"
        );

        // Switching to tab 1 re-arms the notice.
        app.switch_to_tab(bg);
        assert_eq!(app.notice.as_deref(), Some("updated — r to reload"));
    }
}
