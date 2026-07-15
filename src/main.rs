mod account;
mod api;
mod app;
mod attribution;
mod auth;
mod autotheme;
mod bookmark_export;
mod bookmarks;
mod cache;
mod cite;
mod cleardata;
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
mod hyperlink;
mod image;
mod interest;
mod jsonl;
mod layout;
mod migrate;
mod netqueue;
mod offline_search;
mod prefetch;
mod privacy;
mod random;
mod registry;
mod research;
mod sanitize;
mod saved;
mod saved_export;
mod search_ops;
mod session;
mod sisters;
mod split;
mod startpage;
mod stats;
mod tab;
mod talk;
mod target;
mod theme;
mod ui;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, IsTerminal, Stdout, Write as _};
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
use hyperlink::HyperlinkMode;
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
    /// The wiki scope this revalidation actually addressed (PRD FR-ML-4),
    /// captured when the fetch ran so the cache write lands under the same
    /// wiki the content came from — never a different wiki's key, even if
    /// `:wiki` switched the active wiki between firing and completion.
    wiki: String,
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
    /// The wiki scope this background tab was fetched from (PRD FR-ML-4), so
    /// the tab is stamped with — and caches under — its own wiki, not
    /// whatever the active wiki is by the time the load lands.
    wiki: String,
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
    /// The wiki this save ran on (PRD FR-ML-4's `api::wiki_scope`), carried
    /// through so `apply_save_outcome` can tag the offline-search index row
    /// correctly (PRD FR-SR-7) — `saved::SavedPages` itself has no wiki
    /// dimension yet (see `offline_search`'s module doc's documented
    /// limitation), but the index still gets this one right.
    wiki: String,
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
    /// The wiki scope this article belongs to (PRD FR-ML-4), so the session
    /// cache keys never collide with a same-titled article on another wiki.
    wiki: String,
    lang: String,
    title: String,
    result: std::result::Result<Vec<SearchResult>, String>,
}

/// One completed (or failed) link-preview summary fetch (PRD FR-NV-5),
/// delivered off the event loop like every other lazy fetch (see
/// `fire_summary`) so opening the peek popup never blocks. Tagged with the
/// `(lang, title)` it answers so a slow response for a preview the reader has
/// since closed still lands in the session cache (`App::deliver_summary`).
struct SummaryOutcome {
    lang: String,
    title: String,
    result: std::result::Result<api::SummaryData, String>,
}

/// One completed (or failed) langlinks fetch (PRD FR-ML-1/2), delivered off
/// the event loop exactly like `RelatedOutcome` — see `fire_langlinks`.
/// Tagged with the *source* article's `(lang, title)` so a result for an
/// article the reader has since navigated away from still lands in the
/// session cache (`App::deliver_langlinks`) without touching whatever is on
/// screen now.
struct LangLinksOutcome {
    /// The source article's wiki scope (PRD FR-ML-4) — see `RelatedOutcome`.
    wiki: String,
    lang: String,
    title: String,
    result: std::result::Result<Vec<api::LangLink>, String>,
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
        let mut resolved = config::resolve(&cli_overrides, &env_overrides, config_path.as_deref());
        config::apply_accessible_bundle(&mut resolved, accessible_active());
        std::process::exit(doctor::run(&resolved));
    }

    // PRD FR-PR-4: `wikitui clear-data` is the same kind of standalone,
    // TUI-free diagnostic-adjacent subcommand as `config doctor` above —
    // deleting local stores must not depend on the terminal, network, or
    // any in-process store initializing first.
    if let Some(Commands::ClearData {
        history,
        cache,
        stats,
        auth,
        all,
        yes,
    }) = &cli.command
    {
        let cli_overrides = cli_overrides_from(&cli);
        let env_overrides = config::EnvOverrides::from_process_env();
        let config_path =
            config::resolve_config_path(cli.config.clone(), std::env::var("WIKITUI_CONFIG").ok());
        let resolved = config::resolve(&cli_overrides, &env_overrides, config_path.as_deref());
        let scope = cleardata::Scope {
            history: *history,
            cache: *cache,
            stats: *stats,
            auth: *auth,
            all: *all,
        };
        std::process::exit(cleardata::run(&resolved, scope, *yes));
    }

    // PRD FR-PC-3: `wikitui stats [--explain]` is another standalone,
    // TUI-free subcommand (plain stdout, no terminal/network) — it reads the
    // local history + interest model and prints. Nothing leaves the machine.
    if let Some(Commands::Stats { explain }) = &cli.command {
        let cli_overrides = cli_overrides_from(&cli);
        let env_overrides = config::EnvOverrides::from_process_env();
        let config_path =
            config::resolve_config_path(cli.config.clone(), std::env::var("WIKITUI_CONFIG").ok());
        let resolved = config::resolve(&cli_overrides, &env_overrides, config_path.as_deref());
        std::process::exit(stats::run(&resolved, *explain));
    }

    // PRD FR-SR-7: `wikitui reindex` rebuilds the offline search index from
    // scratch — another standalone, TUI-free subcommand; no terminal or
    // network access needed to walk what's already saved/cached on disk.
    if let Some(Commands::Reindex) = &cli.command {
        let cli_overrides = cli_overrides_from(&cli);
        let env_overrides = config::EnvOverrides::from_process_env();
        let config_path =
            config::resolve_config_path(cli.config.clone(), std::env::var("WIKITUI_CONFIG").ok());
        let resolved = config::resolve(&cli_overrides, &env_overrides, config_path.as_deref());
        std::process::exit(run_reindex(&resolved));
    }

    // PRD FR-TH-8 / goal G3: `wikitui import-wiki-tui <path>` is the third
    // standalone, TUI-free subcommand alongside `config doctor` and
    // `clear-data` above — converting a config file has no business
    // touching the network, the cache, or the terminal either.
    if let Some(Commands::ImportWikiTui { path }) = &cli.command {
        let config_path =
            config::resolve_config_path(cli.config.clone(), std::env::var("WIKITUI_CONFIG").ok());
        let Some(config_dir) = config_path.as_deref().and_then(|p| p.parent()) else {
            eprintln!(
                "wikitui: import-wiki-tui: no config directory available (no --config, $WIKITUI_CONFIG, or platform config dir)"
            );
            std::process::exit(1);
        };
        match migrate::import_to_config_dir(path, config_dir) {
            Ok(written) => {
                print!("{}", migrate::summarize(&written));
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("wikitui: import-wiki-tui: {e}");
                std::process::exit(1);
            }
        }
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
        // PRD FR-ML-4: a sister-project URL/prefix (`wikt:Word`,
        // `en.wiktionary.org/wiki/Word`) starts the whole session on that
        // wiki, not just this one fetch — same "the argument overrides
        // everything else" precedence as its language prefix, just one
        // layer up (`config::resolve_wiki`'s `cli.active_wiki`).
        if let Some(project) = target.project {
            cli_overrides.active_wiki = Some(project);
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
    let mut resolved = config::resolve(&cli_overrides, &env_overrides, config_path.as_deref());
    // PRD FR-ACS-6: ACCESSIBLE=1 implies the no-motion/plain-link bundle for
    // whichever of `animations`/`hyperlinks` the reader didn't already pin
    // explicitly. Applied once, here, before anything downstream (including
    // `--dump`, which reads `resolved.terminal.hyperlinks` for its own
    // `[link: target]` fallback) sees the resolved config.
    config::apply_accessible_bundle(&mut resolved, accessible_active());

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

    // PRD FR-TH-1: user theme files, a `themes/` subdirectory sibling of
    // config.toml — loaded once, here, before the theme name is resolved
    // below (so `theme::resolve_named` can find a user theme by name) and
    // before the terminal is touched (so parse/contrast warnings print
    // alongside the config issues above, not garbled by raw mode).
    let themes_dir = config_path
        .as_deref()
        .and_then(|p| p.parent())
        .map(|d| d.join("themes"));
    let (user_themes, theme_warnings) = theme::load_user_themes(themes_dir.as_deref());
    for warning in &theme_warnings {
        eprintln!("wikitui: theme: {warning}");
    }

    // PRD FR-CS-3: build the active keymap — the selected preset (vim/emacs)
    // plus any per-key overrides from `keymap.toml` (a sibling of
    // config.toml). Parse warnings print here, alongside the config issues,
    // before the terminal is touched.
    let mut keymap = registry::Keymap::preset(&resolved.keymap_preset.value);
    if let Some(dir) = config_path.as_deref().and_then(|p| p.parent())
        && let Ok(text) = std::fs::read_to_string(dir.join("keymap.toml"))
    {
        for warning in keymap.apply_user_toml(&text) {
            eprintln!("wikitui: keymap: {warning}");
        }
    }

    // PRD FR-CS-8: onboarding shows once — only in an interactive TUI session
    // with no config file yet. Never for `--dump` (which returns below before
    // this matters), non-tty output, or when explicitly opted out.
    let show_onboarding = !cli.no_onboarding
        && std::io::IsTerminal::is_terminal(&std::io::stdout())
        && config::first_run(config_path.as_deref());

    // PRD FR-ML-4/5: the client starts already knowing the active wiki's
    // name, host template, and feature-degradation matrix — no separate
    // "probe the wiki" round trip before the first real request.
    let wiki_capabilities = api::WikiCapabilities {
        parser: api::ParserMode::parse(&resolved.wiki_capabilities.parser.value)
            .unwrap_or(api::ParserMode::Auto),
        wikifeeds: resolved.wiki_capabilities.wikifeeds.value,
        pageviews: resolved.wiki_capabilities.pageviews.value,
        pageassessments: resolved.wiki_capabilities.pageassessments.value,
    };
    // NF-NET-2: the default contact needs no rebuild of the UA string; only a
    // configured `[network] contact` takes a non-default one.
    let contact = if resolved.network_contact.source == config::Source::Default {
        api::DEFAULT_CONTACT
    } else {
        &resolved.network_contact.value
    };
    let client = WikiClient::with_wiki(
        resolved.active_wiki.value.clone(),
        resolved.base_url_template.value.clone(),
        wiki_capabilities,
        contact,
    )?;
    let page_cache = PageCache::open(
        resolved.cache_dir.value.clone(),
        resolved.cache_max_mb.value.saturating_mul(1024 * 1024),
        resolved.cache_fresh_ttl_hours.value.saturating_mul(3600),
        resolved
            .cache_force_refetch_days
            .value
            .saturating_mul(86_400),
    );
    // PRD FR-PR-3: `--incognito` takes effect from the very first fetch this
    // process makes (even `--dump`'s), not just once `App`/`run` exist.
    page_cache.set_incognito(cli.incognito);
    // PRD FR-PR-3's documented crash-recovery mitigation: "a crash may leave
    // [incognito cache entries] behind, mitigated by the session tag being
    // checked/swept at next startup" — every run sweeps leftovers from
    // whatever the *previous* run tagged, regardless of this run's own
    // incognito state, before doing anything else with the cache.
    startup_sweep_incognito_leftovers(&page_cache);

    if cli.dump {
        let title = cli
            .title
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--dump requires an article title"))?;
        // A one-shot linear render (PRD FR-RD-12) has no event loop to
        // deliver a background revalidation's result into, so it never
        // spawns one — the reader gets whatever's freshest synchronously
        // (fresh cache, or a network round trip on a stale/missing one).
        // The offline search index is a session-lived construct with no
        // reader here to search it, so `--dump` indexes into a throwaway
        // in-memory instance rather than touching the real on-disk one for
        // a process that exits immediately after printing.
        let outcome = fetch_page(
            &client,
            &page_cache,
            &offline_search::OfflineIndex::in_memory(),
            &client.wiki_scope(),
            &resolved.lang.value,
            &title,
        )
        .await?;
        let document = doc::parse_article_html(&title, &outcome.html);
        print!("{}", doc::render_plain(&document, &resolved.lang.value));
        // `--dump` never reaches `run`'s own end-of-session wipe below, so it
        // does its own — a one-shot process is still a "session" for FR-PR-3's
        // purposes.
        page_cache.wipe_incognito_entries();
        return Ok(());
    }

    // Already validated during resolution (unknown names fall back to the
    // default with a warning above), so these can't fail here — the
    // `unwrap_or_else` is a belt-and-braces guard, not an expected path.
    // `mut`: PRD FR-TH-4's auto light/dark may override this pick below,
    // once the terminal is in raw mode and only when the reader hasn't
    // pinned an explicit theme of their own (see the `auto_theme` block).
    let mut theme =
        theme::resolve_named(&resolved.theme.value, &user_themes).unwrap_or_else(Theme::terminal);
    let cite_style = CiteStyle::by_name(&resolved.cite_style.value).unwrap_or(CiteStyle::Apa);
    let no_color = no_color_active();
    let accessible = accessible_active();
    // PRD FR-TH-3: NO_COLOR (FR-TH-5's own policy override, owned by
    // `no_color_active` above) forces `Mono` regardless of `color_depth`;
    // otherwise the configured value resolves via `theme::resolve_color_depth`
    // (an explicit depth, or `auto`'s `$COLORTERM`/`$TERM` detection).
    let color_depth = if no_color {
        theme::ColorDepth::Mono
    } else {
        theme::resolve_color_depth(&resolved.terminal.color_depth.value)
    };

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

    // PRD FR-NV-9: opt-in mouse capture, toggled on before the event loop
    // ever reads an event so every input turn from the first draw onward is
    // subject to it. Best-effort (`?` would abort startup over a cosmetic
    // terminal-capability failure) — a terminal that rejects the escape
    // simply never delivers `Event::Mouse` and behaves as if `mouse=off`.
    if resolved.terminal.mouse.value {
        let _ = crashguard::set_mouse_capture(true);
    }

    // PRD FR-TH-4: query the terminal's background color, guarded by DA1 so
    // an unsupporting terminal can't hang startup, and only when the reader
    // both opted into `auto_theme` AND left `theme` itself unset — an
    // explicit `theme` (CLI/env/config file) always wins over auto-detection
    // (§6.7's general precedence: something more specific beats a blanket
    // auto-behavior). Raw mode is required to read the reply without local
    // echo/line-buffering, hence why this runs only now, after
    // `TerminalGuard::enter` — see `query_terminal_bg`'s own doc comment for
    // exactly what is and isn't verifiable about the live round trip.
    if resolved.terminal.auto_theme.value && resolved.theme.source == config::Source::Default {
        const AUTO_THEME_TIMEOUT: Duration = Duration::from_millis(200);
        if let Some(mode) = query_terminal_bg(AUTO_THEME_TIMEOUT) {
            let picked_name = match mode {
                autotheme::BgMode::Light => &resolved.terminal.theme_light.value,
                autotheme::BgMode::Dark => &resolved.terminal.theme_dark.value,
            };
            if let Some(picked) = theme::resolve_named(picked_name, &user_themes) {
                theme = picked;
            }
        }
    }

    run(
        guard.terminal(),
        &client,
        &page_cache,
        cli,
        resolved.lang.value,
        resolved.languages.value.clone(),
        theme,
        no_color,
        color_depth,
        user_themes,
        accessible,
        resolved.terminal.clone(),
        resolved.reading.clone(),
        resolved.measure.value,
        resolved.ambiguous_wide.value,
        resolved.reading_wpm.value,
        cite_style,
        resolved.readlater_auto_dequeue.value,
        resolved.history_retention_days.value,
        resolved.interest_learning.value,
        resolved.interest_half_life_days.value,
        resolved.images.value,
        resolved.include_nonfree.value,
        resolved.startpage.value,
        resolved.restore_session.value,
        prefetch_config_from(&resolved.prefetch),
        resolved.prefetch.enabled.value,
        config_ctx,
        reload_flag,
        keymap,
        show_onboarding,
        resolved.auth.clone(),
        resolved.network_contact.value.clone(),
        resolved.watchlist_mirror_tag.value.clone(),
        resolved.active_wiki.value.clone(),
        resolved.wiki_registry.clone(),
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
        // Filled in by the TITLE-argument handling just below, once the
        // argument itself has been parsed (a sister-project URL/prefix
        // can't be known until then) — matching `lang`'s own split for the
        // exact same reason.
        active_wiki: None,
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

/// PRD FR-TH-4: attempts the guarded OSC 11 + DA1 query against the real
/// terminal on `stdin`/`stdout`, bounded by `timeout` no matter how the
/// terminal behaves.
///
/// `autotheme::resolve_via_io` alone cannot bound a `read()` call that blocks
/// forever (see its own doc comment) — a real `io::stdin()` read has exactly
/// that failure mode when the terminal never sends anything more, so the
/// actual read runs on a detached background thread and this function waits
/// for it over a channel with `recv_timeout`, giving up (and abandoning that
/// thread) once `timeout` elapses.
///
/// **Documented residual risk, and why this path is opt-in only
/// (`auto_theme` defaults to `false`)**: the abandoned thread may still be
/// blocked inside `io::stdin().read()` past this function's return. If the
/// terminal eventually does reply, late-arriving bytes are consumed by
/// *that* thread's read, not by this process's normal `crossterm::event::
/// read()` loop — so they cannot land as garbled keystrokes, but they are
/// also simply lost (never retried). Kept bounded and low-risk by (a)
/// `timeout` being short — a real OSC-11-capable terminal answers in
/// single-digit milliseconds, so a reply arriving any later is already an
/// unusual/unhealthy terminal — and (b) this function only ever running when
/// the reader explicitly opted into `auto_theme`.
///
/// **Unverified in this environment**: no terminal emulator in this build's
/// pty/test harness answers a real OSC 11 query (the harness's "terminal" is
/// a scripted test driver, not a real emulator) — the exact same limitation
/// `graphics.rs` documents for its kitty/iTerm2 escape emitters. The guarded
/// read loop, the parsing, and the luminance classification are exercised
/// directly in `autotheme`'s own unit tests against in-memory mock replies
/// instead.
fn query_terminal_bg(timeout: Duration) -> Option<autotheme::BgMode> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut stdout = io::stdout();
        let outcome = autotheme::resolve_via_io(&mut stdin, &mut stdout, timeout);
        let _ = tx.send(outcome);
    });
    rx.recv_timeout(timeout + Duration::from_millis(50))
        .ok()
        .flatten()
}

/// PRD FR-PR-3's crash-recovery sweep: deletes any cache entries a *previous*
/// run tagged incognito and never got to clean up (a crash, a `kill -9`,
/// anything short of reaching `run`'s own end-of-session wipe). Runs once at
/// the very start of every invocation — dump or interactive, incognito or
/// not — since the leftovers being swept belong to whatever run wrote them,
/// not to this one. Silent on the ordinary case (nothing to sweep); prints a
/// one-line note to stderr only when it actually found something, the same
/// "visible on a terminal, never in the way" posture `history.rs`'s
/// `log_write_failure` uses.
fn startup_sweep_incognito_leftovers(cache: &PageCache) {
    let report = cache.wipe_incognito_entries();
    if report.entries > 0 {
        eprintln!(
            "wikitui: incognito: swept {} leftover cache entr{} from a previous session ({} freed)",
            report.entries,
            if report.entries == 1 { "y" } else { "ies" },
            human_bytes(report.bytes)
        );
    }
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
    search_index: &offline_search::OfflineIndex,
    wiki: &str,
    lang: &str,
    title: &str,
) -> Result<FetchOutcome> {
    // The cache read (and the offline fallback) are scoped to `wiki` — the
    // wiki this open belongs to (the active wiki for a fresh open, or the
    // history entry's own wiki for back/forward), so a `:wiki`-switched open
    // of a title already cached on another wiki is a miss, never a wrong-wiki
    // hit (PRD FR-ML-4). The network write below is keyed by the wiki the
    // request *actually* hit (the client's active wiki), so fetched content
    // never lands under a different wiki's key.
    let cached = cache.get(wiki, lang, title);
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
            let wiki = client.wiki_scope();
            cache.put(
                &wiki,
                lang,
                title,
                &fetched.html,
                fetched.revid,
                fetched.etag.as_deref(),
            );
            // PRD FR-SR-7: index the freshly cached HTML for offline search
            // right where it's cached — see `offline_search`'s module doc's
            // "Populate / remove". Best-effort like the cache write itself;
            // a parse failure here never blocks the article from opening.
            index_cached_html(search_index, &wiki, lang, title, &fetched.html);
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

/// Extracts plain text from freshly cached (or saved) HTML and upserts it
/// into the offline index (PRD FR-SR-7) — the one place `main.rs` turns raw
/// Parsoid HTML into what `offline_search::OfflineIndex::index` wants,
/// shared by every cache-populating call site so the extraction step
/// (`doc::parse_article_html` then `doc::render_plain`) has exactly one
/// implementation. Best-effort like the index write itself: a malformed
/// document never blocks the read path that called this.
fn index_cached_html(
    search_index: &offline_search::OfflineIndex,
    wiki: &str,
    lang: &str,
    title: &str,
    html: &str,
) {
    let document = doc::parse_article_html(title, html);
    let plain = doc::render_plain(&document, lang);
    search_index.index(wiki, lang, title, offline_search::Kind::Cached, &plain);
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
        let wiki = client.wiki_scope();
        let result = revalidate(&client, &lang, &title, cached_revid).await;
        let _ = tx.send(RevalidationOutcome {
            tab_id,
            wiki,
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
// `search_index` (PRD FR-SR-7) is one dimension past clippy's arg ceiling,
// same shape as `cache`/`client` beside it — bundling them into a struct
// would only move the noise, not remove it.
#[allow(clippy::too_many_arguments)]
fn fire_background_load(
    client: &WikiClient,
    cache: &PageCache,
    search_index: &offline_search::OfflineIndex,
    tab_id: TabId,
    wiki: String,
    lang: String,
    title: String,
    tx: &UnboundedSender<TabLoadOutcome>,
) {
    let client = client.clone();
    let cache = cache.clone();
    let search_index = search_index.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        // The tab belongs to `wiki` (its active wiki when opened, or its
        // persisted wiki on restore) — cache reads and the tab's own scope
        // key on it, not on a wiki `:wiki` may have switched to since (PRD
        // FR-ML-4).
        let result = fetch_page(&client, &cache, &search_index, &wiki, &lang, &title)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(TabLoadOutcome {
            tab_id,
            wiki,
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
    search_index: offline_search::OfflineIndex,
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
        let search_index = self.search_index.clone();
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
                    execute_prefetch_article(&client, &cache, &search_index, &lang, &title).await
                }
                netqueue::Job::RankLinks {
                    lang,
                    article_title,
                    candidates,
                    affinity,
                } => {
                    execute_rank_links(
                        &client,
                        &lang,
                        &article_title,
                        &candidates,
                        &affinity,
                        weights,
                        top_n,
                    )
                    .await
                }
                netqueue::Job::Featured { lang, date } => {
                    execute_featured(&client, &feed_cache, &lang, &date).await
                }
                netqueue::Job::InterestMorelike { lang, seeds } => {
                    execute_interest_morelike(&client, &feed_cache, &lang, &seeds, top_n).await
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
    search_index: &offline_search::OfflineIndex,
    lang: &str,
    title: &str,
) -> netqueue::ExecResult {
    let wiki = client.wiki_scope();
    if cache.get(&wiki, lang, title).is_some() {
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
            cache.put(&wiki, lang, title, &a.html, a.revid, a.etag.as_deref());
            // PRD FR-SR-7: a prefetched article is genuinely cached content —
            // offline search should find it exactly like an interactively
            // opened one (see `offline_search`'s module doc).
            index_cached_html(search_index, &wiki, lang, title, &a.html);
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
    affinity: &std::collections::HashMap<String, f64>,
    weights: netqueue::RankWeights,
    top_n: usize,
) -> netqueue::ExecResult {
    // PRD FR-ML-5's degradation matrix: a wiki without `prop=pageviews`
    // skips the request entirely rather than making a call it knows will
    // never carry that data — `rank_links` already degrades to
    // lead-position-only when the pageviews map is empty (every candidate's
    // views default to 0, so the pageviews term is 0 too — the same math an
    // unseen title already gets), so no separate "degraded" ranking path is
    // needed here, only the skipped request.
    if !client.capabilities().pageviews {
        let ranked = prefetch::rank_links(
            article_title,
            candidates,
            &std::collections::HashMap::new(),
            affinity,
            weights,
            top_n,
        );
        let follow_ups = ranked
            .into_iter()
            .map(|r| netqueue::Job::PrefetchArticle {
                lang: lang.to_string(),
                title: r.title,
                reason: r.reason,
                log_id: 0,
            })
            .collect();
        return netqueue::ExecResult {
            outcome: netqueue::Outcome::Done { bytes: 0 },
            follow_ups,
        };
    }
    match client.fetch_link_pageviews(lang, article_title).await {
        Ok((pageviews, bytes)) => {
            let ranked = prefetch::rank_links(
                article_title,
                candidates,
                &pageviews,
                affinity,
                weights,
                top_n,
            );
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
    // PRD FR-ML-5: no Wikifeeds on this wiki means no daily feed to fetch —
    // `feed_cache` stays empty, which `App::start_page_model` already reads
    // as "the feed never showed up," falling back to
    // `StartPageModel::offline_fallback`'s simpler view (recent history /
    // saved pages), the exact same degradation an offline reader already
    // sees. Checked before `should_fetch` so a capability change mid-session
    // (`:wiki`) is picked up immediately rather than waiting for the date to
    // roll over.
    if !client.capabilities().wikifeeds {
        return netqueue::ExecResult {
            outcome: netqueue::Outcome::Skipped {
                note: "Wikifeeds not supported on this wiki".to_string(),
            },
            follow_ups: Vec::new(),
        };
    }
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

/// FR-PF-3 seed: run `morelike:` on the reader's top-affinity recent reads and
/// enqueue the results as interest candidates, intersected with the day's
/// trending most-read where that feed is available (the PRD's "morelike on
/// top-affinity reads ∩ trending"). Falls back to the raw morelike results when
/// no trending feed is cached yet, or when the intersection is empty — the
/// "at minimum, morelike on the highest-affinity recently-read articles" floor.
/// Each candidate carries the FR-PF-4 reason
/// "morelike your {topic} reading, affinity {a}".
async fn execute_interest_morelike(
    client: &WikiClient,
    feed_cache: &Arc<std::sync::Mutex<prefetch::FeedCache>>,
    lang: &str,
    seeds: &[netqueue::MorelikeSeed],
    top_n: usize,
) -> netqueue::ExecResult {
    if seeds.is_empty() {
        return netqueue::ExecResult {
            outcome: netqueue::Outcome::Skipped {
                note: "no interest seeds".to_string(),
            },
            follow_ups: Vec::new(),
        };
    }
    // The seed titles themselves are already-read; never re-prefetch them.
    let seed_titles: std::collections::HashSet<String> =
        seeds.iter().map(|s| s.title.clone()).collect();

    // (title -> reason), first seed to surface a title wins its attribution.
    let mut candidates: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut any_ok = false;
    for seed in seeds {
        let query = format!("morelike:{}", seed.title);
        // Best-effort: a failed morelike search just yields no candidates from
        // that seed. It never trips the breaker (search isn't a budgeted
        // background primitive the way pageviews/feeds are).
        if let Ok(outcome) = client.search(lang, &query, MORELIKE_CANDIDATE_LIMIT).await {
            any_ok = true;
            let reason = prefetch::morelike_reason(&seed.category, seed.affinity);
            for result in outcome.results {
                let title = result.title;
                if seed_titles.contains(&title) || !seen.insert(title.clone()) {
                    continue;
                }
                candidates.push((title, reason.clone()));
            }
        }
    }

    // FR-PF-3 "∩ trending": if the daily feed is cached, prefer candidates that
    // are also in today's most-read; fall back to the full set if that leaves
    // nothing (or the feed isn't available yet).
    let trending: std::collections::HashSet<String> = feed_cache
        .lock()
        .unwrap()
        .get()
        .map(|f| f.mostread.iter().map(|m| m.title.clone()).collect())
        .unwrap_or_default();
    if !trending.is_empty() {
        let intersected: Vec<(String, String)> = candidates
            .iter()
            .filter(|(t, _)| trending.contains(t))
            .cloned()
            .collect();
        if !intersected.is_empty() {
            candidates = intersected;
        }
    }

    if !any_ok {
        return netqueue::ExecResult {
            outcome: netqueue::Outcome::Failed,
            follow_ups: Vec::new(),
        };
    }

    let follow_ups: Vec<netqueue::Job> = candidates
        .into_iter()
        .take(top_n)
        .map(|(title, reason)| netqueue::Job::PrefetchArticle {
            lang: lang.to_string(),
            title,
            reason,
            log_id: 0,
        })
        .collect();
    netqueue::ExecResult {
        // The seed job itself fetched only search metadata (no article body);
        // the follow-up bodies count their own bytes.
        outcome: netqueue::Outcome::Done { bytes: 0 },
        follow_ups,
    }
}

/// How many `morelike:` results to consider per interest seed.
const MORELIKE_CANDIDATE_LIMIT: u32 = 10;

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
    // The wiki the metadata/HTML below is actually fetched from — the cache
    // write keyed by it can never land under another wiki's key (PRD FR-ML-4).
    let wiki = client.wiki_scope();
    match client.fetch_bare_metadata_bg(&lang, &title).await {
        Ok(latest) => match cache::revalidate_action(cached_revid, latest) {
            RevalidateAction::Touch => {
                let _ = tx.send(RevalidationOutcome {
                    tab_id,
                    wiki,
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
                        wiki,
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
                        wiki,
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
                wiki,
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
    // FR-PF-3 w3 term: snapshot each candidate's affinity from the interest
    // model here, where it lives, so the executor stays stateless. Only
    // previously-read targets have a nonzero entry; the map is empty when
    // interest learning is off/incognito (`interest_active` false), so the
    // affinity term then contributes nothing.
    let mut affinity = std::collections::HashMap::new();
    if app.interest_active() {
        // Link targets share the active tab's wiki scope (PRD FR-ML-4), so
        // affinity is read from that wiki's slice of the category cache.
        let wiki = &tab.wiki;
        for c in &candidates {
            let a = app.interest.affinity_of_title(wiki, &c.title);
            if a != 0.0 {
                affinity.insert(c.title.clone(), a);
            }
        }
    }
    handle.enqueue_prefetch(netqueue::Job::RankLinks {
        lang: tab.lang.clone(),
        article_title,
        candidates,
        affinity,
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

/// PRD FR-PF-3: enqueue the interest-driven `morelike:` candidate seed — the
/// reader's top-affinity recent reads, run through `morelike:` and intersected
/// with trending in the executor. A no-op unless prefetch is active (kill
/// switch, not incognito) *and* interest learning is on (`interest_active`);
/// the kill switch (`:set prefetch=off`) suppresses interest-driven prefetch
/// exactly like the other two sources. Seeds are capped small so the seed job
/// makes only a handful of `morelike:` searches.
fn schedule_interest_prefetch(app: &App) {
    if !app.prefetch_active() || !app.interest_active() {
        return;
    }
    let Some(handle) = &app.prefetch else {
        return;
    };
    // Recent reads in the active language, most-recent first — the pool the
    // model ranks by affinity for seeding.
    let recent: Vec<String> = app
        .history
        .recent(INTEREST_SEED_POOL)
        .into_iter()
        .filter(|v| v.lang == app.lang)
        .map(|v| v.title)
        .collect();
    let seeds = app.interest.morelike_seeds(&recent, INTEREST_MAX_SEEDS);
    if seeds.is_empty() {
        return;
    }
    let seeds: Vec<netqueue::MorelikeSeed> = seeds
        .into_iter()
        .map(|s| netqueue::MorelikeSeed {
            title: s.title,
            category: s.category,
            affinity: s.affinity,
        })
        .collect();
    handle.enqueue_prefetch(netqueue::Job::InterestMorelike {
        lang: app.lang.clone(),
        seeds,
    });
}

/// How many recent reads to consider when picking interest seeds.
const INTEREST_SEED_POOL: usize = 30;
/// How many seeds to run `morelike:` on (each is a background search).
const INTEREST_MAX_SEEDS: usize = 3;

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
            cache.touch_fetched_at(&outcome.wiki, &outcome.lang, &outcome.title);
        }
        RevalidationResult::Changed { html, revid, etag } => {
            cache.put(
                &outcome.wiki,
                &outcome.lang,
                &outcome.title,
                &html,
                revid,
                etag.as_deref(),
            );
            // PRD FR-SR-7: a revalidation-driven refetch is new cached
            // content — keep the offline index's snippet current rather
            // than serving a stale one from before the update.
            index_cached_html(
                &app.search_index,
                &outcome.wiki,
                &outcome.lang,
                &outcome.title,
                &html,
            );
            let Some(index) = app.tab_index_by_id(outcome.tab_id) else {
                return; // the tab closed — nothing to notify.
            };
            let tab = &mut app.tabs[index];
            // The notice must only fire for a tab that is genuinely showing
            // the wiki+lang+title this content is for — a same-titled article
            // on a different wiki (or lang) is a different page (PRD FR-ML-4).
            let still_open = tab.wiki == outcome.wiki
                && tab.lang == outcome.lang
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
                // PRD FR-ML-4: stamp the background tab with the wiki it was
                // fetched from (`install_document` doesn't touch app-global
                // state, so unlike `set_document` it can't derive this).
                tab.wiki = outcome.wiki.clone();
                tab.page_source = fetch.source;
                tab.current_revid = fetch.revid;
                tab.install_document(document);
            }
            // PRD FR-TB-5: a session-restore fetch (`main::restore_session_tabs`)
            // stashed the scroll/fold-set to apply once `install_document`
            // (just above) finishes resetting both to the top — apply it
            // now, then forget it; a tab that was never part of a restore
            // simply has no entry here.
            if let Some(restore) = app.pending_session_restore.remove(&outcome.tab_id) {
                let tab = &mut app.tabs[index];
                tab.scroll = restore.scroll;
                tab.folded_blocks = restore.folded_blocks;
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
            // PRD FR-TB-5: a background tab's document installing is a
            // "meaningful change" too — see `App::persist_session`'s doc
            // comment for the full trigger list.
            app.persist_session();
        }
        Err(_) => {
            let tab = &mut app.tabs[index];
            tab.loading = false;
            tab.pending_title = Some(format!("{} (failed)", outcome.title));
            // A restore that never lands has nothing left to apply.
            app.pending_session_restore.remove(&outcome.tab_id);
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
            // PRD FR-PR-3: `ba` is `toggle_bookmark`'s explicit-save sibling
            // — `ensure_bookmarked` above persists a bookmark exactly like
            // `m` does, so the same warning applies when it actually created
            // one (a note added to an *existing* bookmark isn't a new write
            // worth re-warning about).
            app.notice = Some(if was_new {
                crate::privacy::append_warning_if_needed(
                    app.incognito,
                    crate::privacy::Write::Bookmark,
                    format!("Bookmarked \"{title}\" and saved the note"),
                )
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
    let saved_offline = ensure_cached(client, cache, &app.search_index, &lang, &title).await;
    app.readlater.enqueue(ReadLaterEntry {
        title: title.clone(),
        lang,
        enqueued_at: bookmarks::now_ts(),
        priority: 0,
    });
    // PRD FR-PR-3: an explicit save (the reader named this article) — warn,
    // don't suppress; see `App::toggle_bookmark`'s doc comment.
    let base = if saved_offline {
        format!("Enqueued \"{title}\" for later")
    } else {
        format!("Enqueued \"{title}\" for later (offline save failed — will retry on open)")
    };
    app.notice = Some(crate::privacy::append_warning_if_needed(
        app.incognito,
        crate::privacy::Write::ReadLater,
        base,
    ));
}

/// Whether `(lang, title)` is already in L2, fetching it in if not. Returns
/// whether it's cached by the time this returns (a network failure here is
/// reported to the reader, not retried — the queue entry still gets
/// created either way; opening it later tries again via the normal fetch
/// path).
async fn ensure_cached(
    client: &WikiClient,
    cache: &PageCache,
    search_index: &offline_search::OfflineIndex,
    lang: &str,
    title: &str,
) -> bool {
    let wiki = client.wiki_scope();
    if cache.get(&wiki, lang, title).is_some() {
        return true;
    }
    match client.fetch_article_html(lang, title).await {
        Ok(fetched) => {
            cache.put(
                &wiki,
                lang,
                title,
                &fetched.html,
                fetched.revid,
                fetched.etag.as_deref(),
            );
            index_cached_html(search_index, &wiki, lang, title, &fetched.html);
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
    // PRD FR-PF-3: saving an article is the strongest positive signal (+5.0)
    // on its topics — the reader deliberately kept this page. Gated on
    // `interest_active` (off in incognito), so an incognito save still pins
    // the page but never teaches the model.
    app.apply_active_article_signal(crate::interest::SIGNAL_SAVE);
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
    let wiki = client.wiki_scope();
    let (html, revid) = match cache.get(&wiki, lang, title) {
        Some(page) => (page.html, page.revid),
        None => {
            let fetched = client.fetch_article_html(lang, title).await?;
            cache.put(
                &wiki,
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
        wiki,
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
                // PRD FR-SR-7: a saved page is offline-searchable the moment
                // it's pinned — see `offline_search`'s module doc's
                // "Populate / remove".
                let document = doc::parse_article_html(&f.title, &f.html);
                let plain = doc::render_plain(&document, &f.lang);
                app.search_index.index(
                    &f.wiki,
                    &f.lang,
                    &f.title,
                    offline_search::Kind::Saved,
                    &plain,
                );
                // PRD FR-PR-3: `S`/`:save` is an explicit save (the reader
                // named this article) — warn, don't suppress; see
                // `App::toggle_bookmark`'s doc comment.
                app.notice = Some(crate::privacy::append_warning_if_needed(
                    app.incognito,
                    crate::privacy::Write::OfflineSave,
                    format!(
                        "Saved \"{}\" ({}, {})",
                        record.title,
                        record.tier.label(),
                        human_bytes(record.size_total)
                    ),
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
        if ensure_cached(client, cache, &app.search_index, &q.lang, &q.title).await {
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
    languages: Vec<String>,
    theme: Theme,
    no_color: bool,
    color_depth: theme::ColorDepth,
    user_themes: Vec<theme::LoadedUserTheme>,
    accessible: bool,
    terminal_cfg: config::ResolvedTerminal,
    reading_cfg: config::ResolvedReading,
    measure: u16,
    ambiguous_wide: bool,
    reading_wpm: u32,
    cite_style: CiteStyle,
    readlater_auto_dequeue: bool,
    history_retention_days: u64,
    interest_learning: bool,
    interest_half_life_days: f64,
    images_config: Option<bool>,
    include_nonfree: bool,
    startpage_config: String,
    restore_session_config: bool,
    prefetch_config: netqueue::SubstrateConfig,
    prefetch_enabled: bool,
    config_ctx: ConfigContext,
    reload_flag: Arc<AtomicBool>,
    keymap: registry::Keymap,
    show_onboarding: bool,
    auth_cfg: config::ResolvedAuth,
    contact: String,
    watchlist_mirror_tag: String,
    active_wiki: String,
    wiki_registry: config::ResolvedWikiRegistry,
) -> Result<()> {
    let mut app = App::new(lang, theme, no_color);
    app.keymap = keymap;
    // PRD FR-ML-4/5: mirrors what `client` was already constructed with
    // (`main`'s `WikiClient::with_wiki` call) plus every other name `:wiki`
    // can switch to — see `App::wiki_registry`'s own doc comment for why
    // this lives on `App` too instead of requiring `&WikiClient` everywhere
    // a name needs resolving.
    app.active_wiki_name = active_wiki;
    app.wiki_registry = wiki_registry
        .entries
        .into_iter()
        .map(|(name, entry)| {
            let caps = entry.capabilities;
            (
                name,
                api::WikiRegistryEntry {
                    base_url_template: entry.base_url_template.value,
                    capabilities: api::WikiCapabilities {
                        parser: api::ParserMode::parse(&caps.parser.value)
                            .unwrap_or(api::ParserMode::Auto),
                        wikifeeds: caps.wikifeeds.value,
                        pageviews: caps.pageviews.value,
                        pageassessments: caps.pageassessments.value,
                    },
                },
            )
        })
        .collect();
    app.languages = languages;
    app.accessible = accessible;
    // PRD FR-TH-3: `color_depth`/`user_themes` must be in place *before*
    // `set_theme` re-adapts the already-truecolor `theme` `App::new` just
    // stored — otherwise the very first paint would be truecolor regardless
    // of the configured depth, only correcting itself on the next theme
    // change.
    app.color_depth = color_depth;
    app.user_themes = user_themes;
    app.set_theme(app.theme);
    // PRD FR-NV-9 / FR-ACS-4 / FR-RD-2: the terminal-integration settings
    // this chunk adds. `mouse_enabled` mirrors the real
    // `EnableMouseCapture`/`DisableMouseCapture` state `main` already
    // toggled (or didn't) before this function was ever called — see that
    // call site's own comment.
    app.mouse_enabled = terminal_cfg.mouse.value;
    app.no_motion = terminal_cfg.animations.value == "none";
    app.hyperlinks_mode =
        HyperlinkMode::parse(&terminal_cfg.hyperlinks.value).unwrap_or(HyperlinkMode::Auto);
    app.measure = measure;
    app.ambiguous_wide = ambiguous_wide;
    app.reading_wpm = reading_wpm;
    // PRD FR-PC-1: the `[reading]` spacing/typography options — session-
    // global starting points, same as `measure`/`ambiguous_wide` above;
    // already validated/clamped by `config::resolve`, so no fallback logic
    // is needed here (mirrors `app.measure = measure` just above).
    app.margin = reading_cfg.margin.value;
    app.text_align = layout::TextAlign::parse(&reading_cfg.text_align.value)
        .unwrap_or(layout::TextAlign::Center);
    app.paragraph_spacing = reading_cfg.paragraph_spacing.value;
    app.line_spacing = reading_cfg.line_spacing.value;
    app.word_spacing = reading_cfg.word_spacing.value;
    app.cite_style = cite_style;
    app.readlater_auto_dequeue = readlater_auto_dequeue;
    // PRD FR-RD-8/§6.3: terminal graphics capability snapshot, taken once.
    // The `no_color`/`images_enabled` decision is layered on live in
    // `App::graphics_protocol`.
    app.graphics_env = graphics::GraphicsEnv::from_process_env(std::io::stdout().is_terminal());
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
    // PRD FR-SR-7: the real, on-disk offline search index (PRD §6.4's cache
    // dir) — `App::new` defaults to an in-memory store for the same reason
    // `history` does (see `App.search_index`'s doc comment); this is the one
    // place production opens the durable one.
    app.search_index = offline_search::OfflineIndex::open();
    // PRD FR-PF-3 / FR-PR-2: the local, private interest model — loaded from
    // `$XDG_STATE/wikitui/interest.json` (a fast local read, no network) with
    // the config half-life. Like history, `App::new` defaults to an empty
    // in-memory model so tests never touch the real state dir; this line is
    // the one place production loads and persists it. `interest_path` being
    // `Some` is what enables persistence (`App::persist_interest`).
    app.interest_learning = interest_learning;
    match interest::interest_path() {
        Some(path) => {
            app.interest = interest::InterestModel::load(&path, interest_half_life_days);
            app.interest_path = Some(path);
        }
        None => {
            app.interest = interest::InterestModel::new(interest_half_life_days);
        }
    }
    // PRD FR-TB-5 (§6.4: sessions live in state): resolved unconditionally,
    // even under `--incognito` — `App::persist_session` is the one gate
    // that stops incognito writing anything new, so a later non-incognito
    // run can still restore whatever the *last* non-incognito session left
    // behind (see that function's doc comment for why incognito must not
    // also erase history it didn't ask to touch).
    app.session_path = session::resolve_session_path();

    // PRD FR-ACC-1 / §5.9: the OAuth client identity + endpoints `:login`
    // builds a flow from, and — if a token store already holds tokens from a
    // previous session — the logged-in session restored on startup. Loading
    // tokens is a fast local read (no network), so it never blocks the
    // <100 ms cold-start path (§6.8); the username rides the stored tokens so
    // the indicator shows immediately without a userinfo round trip.
    app.auth_runtime = app::AuthRuntime {
        client_id: auth_cfg.client_id.value.clone(),
        authorize_url: auth_cfg.authorize_url.value.clone(),
        token_url: auth_cfg.token_url.value.clone(),
        contact: contact.clone(),
    };
    if let Some(state) = load_auth_state(&app.auth_runtime) {
        app.auth = Some(state);
    }
    // PRD FR-ACC-2: where the watchlist's last-seen cursor persists
    // (`$XDG_STATE_HOME/wikitui/watchlist.json`), loaded now the same way
    // `session_path`/`auth_path` are — a fast local read, never blocking on
    // network.
    app.watchlist_state_path = account::watchlist_state_path();
    if let Some(path) = &app.watchlist_state_path {
        app.watchlist_last_seen = account::load_last_seen(path);
    }
    // PRD FR-BM-5/6: the Reading List sync id-map and the watch-mirror's own
    // "what did we watch" state, resolved now the same way `watchlist_
    // state_path` is just above — a fast local read, never network.
    app.watchlist_mirror_tag = watchlist_mirror_tag;
    app.readinglist_sync_state_path = account::readinglist_sync_state_path();
    app.watch_mirror_state_path = account::watch_mirror_state_path();
    // PRD FR-ACC-3's login/startup poll (see `account.rs`'s poll-cadence
    // doc): a restored session gets its unread-count badge without waiting
    // for the reader to open `:notifications` first.
    if app.auth.is_some() {
        poll_notifications_count(client, &mut app).await;
    }

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
    // PRD FR-ML-1/2: lazy langlinks fetches (the picker, and the automatic
    // "available in your preferred language" check after every fresh open)
    // — same idiom again.
    let (langlinks_tx, mut langlinks_rx) = mpsc::unbounded_channel::<LangLinksOutcome>();
    // PRD FR-NV-5: lazy page-summary fetches for the `K` link-preview popup —
    // same non-blocking idiom as `related_tx`/`langlinks_tx` above.
    let (summary_tx, mut summary_rx) = mpsc::unbounded_channel::<SummaryOutcome>();

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
        search_index: app.search_index.clone(),
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
        open_title(
            client,
            cache,
            &mut app,
            &title,
            &revalidate_tx,
            &langlinks_tx,
        )
        .await;
    } else if app.startpage_config == startpage::StartPageConfig::Resume || restore_session_config {
        // PRD FR-TB-5: `startpage = resume` (or the independent
        // `restore_session = true`) reopens the persisted tab set — every
        // tab loads lazily through the same background-tab machinery
        // `Ctrl-Enter` already uses (`restore_session_tabs`), so this never
        // blocks startup on the network. Falls back to the single-article
        // history approximation `resume` used before this chunk existed
        // when there's no session file (fresh install) or it's empty/corrupt
        // (`session::load` returns `None`) — and *that* falls through to the
        // feed-backed start page when there's no history either, exactly as
        // it always has. Never an error, never a blank screen with no
        // explanation.
        let restored = app
            .session_path
            .clone()
            .and_then(|path| session::load(&path))
            .map(|state| restore_session_tabs(client, cache, &mut app, state, &open_tx))
            .unwrap_or(false);
        if !restored
            && app.startpage_config == startpage::StartPageConfig::Resume
            && let Some(visit) = app.history.recent(1).into_iter().next()
        {
            app.lang = visit.lang.clone();
            open_title(
                client,
                cache,
                &mut app,
                &visit.title,
                &revalidate_tx,
                &langlinks_tx,
            )
            .await;
        }
    }

    // PRD FR-PF-2: seed trending prefetch once at startup. The foreground gate
    // ensures it only runs while the reader is idle; the daily feed cache makes
    // it a single call per day.
    schedule_trending_prefetch(&app);

    // PRD FR-CS-8: overlay the first-run tour on top of whatever loaded (the
    // start page in the common case). Any key dismisses it and writes the
    // default config so it never shows again.
    if show_onboarding {
        app.mode = Mode::Onboarding;
    }

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

        // PRD FR-RD-2 / SEC-2: overlay OSC 8 hyperlinks on top of the frame
        // ratatui just painted — see `emit_hyperlinks`'s own doc comment for
        // why this runs as a distinct pass *after* `draw` rather than being
        // folded into it. Best-effort: a write failure here (e.g. stdout
        // gone) is no worse than the plain styled text already on screen.
        let _ = emit_hyperlinks(&app);

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
            // PRD FR-NV-5: the link-preview popup's lazy summary fetch fills
            // in without a keypress, same as the Related panel above.
            || app.summary_loading
            // PRD FR-ML-1/2: same for any in-flight langlinks fetch — the
            // picker's own, and the automatic one fired after every fresh
            // open. Both must gate this condition: without it, the
            // automatic fetch (which powers the preferred-language hint,
            // not just the picker) would only ever surface on the reader's
            // *next* keystroke, since nothing else here is guaranteed to be
            // true right after opening a plain article with no other
            // background activity.
            || app.pending_langlinks > 0
        {
            let poll_interval = if app.mode == Mode::Search {
                TYPEAHEAD_POLL
            } else {
                REVALIDATE_POLL
            };
            if event::poll(poll_interval)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
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
                            &langlinks_tx,
                            &summary_tx,
                            terminal,
                        )
                        .await;
                    }
                    // PRD FR-NV-9: only ever arrives when `mouse` is on (see
                    // `main`'s `EnableMouseCapture` toggle) — off means this
                    // arm is simply never reached, keyboard input untouched.
                    Event::Mouse(mouse) => {
                        handle_mouse(
                            client,
                            cache,
                            &mut app,
                            mouse,
                            &revalidate_tx,
                            &open_tx,
                            &save_tx,
                            &related_tx,
                            &langlinks_tx,
                            &summary_tx,
                            terminal,
                        )
                        .await;
                    }
                    _ => {}
                }
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
                app.deliver_related(outcome.wiki, outcome.lang, outcome.title, outcome.result);
            }
            // PRD FR-NV-5: install a completed link-preview summary; fills the
            // popup in place if it's still open on this target.
            while let Ok(outcome) = summary_rx.try_recv() {
                app.deliver_summary(outcome.lang, outcome.title, outcome.result);
            }
        } else {
            // Block until an event arrives instead of redrawing on a timer —
            // an idle reader shouldn't spin the CPU or spam hide-cursor codes.
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
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
                        &langlinks_tx,
                        &summary_tx,
                        terminal,
                    )
                    .await;
                }
                Event::Mouse(mouse) => {
                    handle_mouse(
                        client,
                        cache,
                        &mut app,
                        mouse,
                        &revalidate_tx,
                        &open_tx,
                        &save_tx,
                        &related_tx,
                        &langlinks_tx,
                        &summary_tx,
                        terminal,
                    )
                    .await;
                }
                _ => {}
            }
        }

        // PRD FR-ML-1/2: drained every iteration (not only inside the
        // scoped-poll branch above) — harmless when nothing is pending
        // (`try_recv` returns immediately) and means a result is picked up
        // the moment it lands rather than waiting for the next full pass
        // through the scoped-poll branch specifically.
        while let Ok(outcome) = langlinks_rx.try_recv() {
            app.pending_langlinks = app.pending_langlinks.saturating_sub(1);
            app.deliver_langlinks(outcome.wiki, outcome.lang, outcome.title, outcome.result);
        }

        if app.should_quit {
            break;
        }
    }

    // PRD FR-HS-1's dwell time: whatever every open tab is still showing
    // stops accumulating dwell the moment the app exits.
    app.flush_all_tab_dwell();

    // PRD FR-PR-3's "cache entries tagged for wipe at session end": covers
    // every entry tagged incognito during this run, whether the session
    // ended incognito or the reader toggled `zz` off again before quitting —
    // the tag, not the app's final state, is what's swept. A panic or a
    // signal kill skips this (documented: mitigated by the startup sweep,
    // `startup_sweep_incognito_leftovers`, on the *next* run).
    cache.wipe_incognito_entries();

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
        // The wiki this morelike search actually addresses — keys the session
        // cache under the same wiki the results came from (PRD FR-ML-4).
        let wiki = client.wiki_scope();
        let query = format!("morelike:{title}");
        let result = client
            .search(&lang, &query, RELATED_LIMIT)
            .await
            .map(|outcome| outcome.results)
            .map_err(|e| e.to_string());
        let _ = tx.send(RelatedOutcome {
            wiki,
            lang,
            title,
            result,
        });
    });
}

/// PRD FR-NV-5: fetch a link target's page summary for the preview popup off
/// the event loop (never blocking), tagged so a late response still lands in
/// the session cache even if the popup has since closed — mirrors
/// `fire_related`/`fire_langlinks`.
fn fire_summary(
    client: &WikiClient,
    lang: &str,
    title: &str,
    tx: &UnboundedSender<SummaryOutcome>,
) {
    let lang = lang.to_string();
    let title = title.to_string();
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = client
            .fetch_summary_full(&lang, &title)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(SummaryOutcome {
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

/// Spawns the `prop=langlinks` fetch (PRD FR-ML-1/2) so neither the picker
/// nor the automatic preferred-language check ever blocks the event loop —
/// mirrors `fire_related`'s pattern exactly, down to the `(lang, title)`
/// tag the result carries back so a slow response for an article the reader
/// has since left still lands in the session cache.
fn fire_langlinks(
    client: &WikiClient,
    lang: &str,
    title: &str,
    tx: &UnboundedSender<LangLinksOutcome>,
) {
    let lang = lang.to_string();
    let title = title.to_string();
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let wiki = client.wiki_scope();
        let result = client
            .fetch_langlinks(&lang, &title)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(LangLinksOutcome {
            wiki,
            lang,
            title,
            result,
        });
    });
}

/// `:lang` (bare, PRD FR-ML-1): opens the picker, firing the langlinks fetch
/// only when `App::open_lang_picker` says the session cache doesn't already
/// have this article's editions — mirrors `open_related` exactly.
fn open_lang_picker(app: &mut App, client: &WikiClient, tx: &UnboundedSender<LangLinksOutcome>) {
    if app.open_lang_picker() {
        let tab = app.active_tab();
        let lang = tab.lang.clone();
        let title = tab
            .doc
            .as_ref()
            .expect("open_lang_picker guarantees a document")
            .title
            .clone();
        fire_langlinks(client, &lang, &title, tx);
        app.pending_langlinks += 1;
    }
}

/// PRD FR-ML-4/5's `:wiki <name>` switch: looks `name` up in the
/// config-resolved registry (Wikipedia, the four sister projects, and every
/// `[wiki.<name>]` section — `config::resolve_wiki`) and repoints the
/// client's host + capability matrix at it (`WikiClient::switch_wiki`).
/// Mirrors `:lang`'s "no cached edition" branch: changes the default for
/// *future* searches/opens rather than force-navigating the active tab —
/// nothing about the article on screen changes. Returns whether the switch
/// happened, so `Command::Wiki`'s caller can decide whether to also show a
/// success notice or leave the "unknown wiki" one this sets on failure.
fn switch_wiki(client: &WikiClient, app: &mut App, name: &str) -> bool {
    let Some(entry) = app.wiki_registry.get(name).cloned() else {
        app.notice = Some(format!(
            "unknown wiki {name:?} — configure [wiki.{name}] in config.toml, or :wiki to pick a known project"
        ));
        return false;
    };
    client.switch_wiki(
        name.to_string(),
        entry.base_url_template,
        entry.capabilities,
    );
    app.active_wiki_name = name.to_string();
    app.notice = Some(format!(
        "Wiki: {name} — searches and opens now use {}",
        client.wiki_origin(&app.lang)
    ));
    true
}

/// Enter on a language-picker row, and `:lang <code>`'s switch branch (PRD
/// FR-ML-1): opens `title` in `lang` as a fresh navigation in the active
/// tab — pushes the tab's current article onto its back stack (so `H`
/// returns), the same as following any other link. Delegates to
/// `open_title` rather than duplicating its fetch/install logic; the
/// fallback chain that function also runs is harmless here (it only ever
/// engages if `lang` itself turns up nothing, in which case falling back is
/// still better than an error card).
async fn switch_to_langlink(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    lang: &str,
    title: &str,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    app.lang = lang.to_string();
    open_title(client, cache, app, title, revalidate_tx, langlinks_tx).await;
}

/// `:lang <code>`'s disambiguation (PRD FR-ML-2): if the active tab's
/// article has a *cached* langlink for `code`, this is a language switch —
/// delegates to `switch_to_langlink`. Otherwise (the langlinks haven't
/// loaded yet, a network failure cached an empty list, or this article
/// simply has no edition in `code`) this keeps `:lang`'s original meaning:
/// set `code` as the default for new searches/opens. Both branches notify,
/// so which one applied is never ambiguous to the reader.
async fn set_or_switch_lang(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    code: String,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    match app.lang_link_title_for_code(&code) {
        Some(title) => {
            switch_to_langlink(
                client,
                cache,
                app,
                &code,
                &title,
                revalidate_tx,
                langlinks_tx,
            )
            .await
        }
        None => {
            app.lang = code.clone();
            app.notice = Some(format!(
                "Language: {code} — searches and new articles use {code}.wikipedia.org"
            ));
        }
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
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    app.loading = true;
    let lang = app.lang.clone();
    match client.random_titles(&lang, 1).await {
        Ok(mut titles) => match titles.pop() {
            Some(title) => {
                open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
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
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    app.loading = true;
    let lang = app.lang.clone();
    match random::pick_random_good(client, &lang).await {
        Ok(random::RandomGood::Found(title)) => {
            open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
            return;
        }
        Ok(random::RandomGood::Fallback(title)) => {
            open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
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
    // PRD FR-TH-1/3: re-scan `themes/` and re-resolve `color_depth` too — a
    // reload might add a new user theme file or change the configured depth,
    // and `theme` (reloaded right below) needs both already in place to
    // resolve/adapt correctly.
    let themes_dir = app
        .config_ctx
        .config_path
        .as_deref()
        .and_then(|p| p.parent())
        .map(|d| d.join("themes"));
    let (user_themes, theme_warnings) = theme::load_user_themes(themes_dir.as_deref());
    app.user_themes = user_themes;
    app.color_depth = theme::resolve_color_depth(&resolved.terminal.color_depth.value);
    if let Some(theme) = theme::resolve_named(&resolved.theme.value, &app.user_themes) {
        app.set_theme(theme);
    }
    app.measure = resolved.measure.value;
    app.ambiguous_wide = resolved.ambiguous_wide.value;
    app.reading_wpm = resolved.reading_wpm.value;
    // PRD FR-PC-1: reload the `[reading]` spacing/typography defaults too —
    // same live-apply seam as measure/ambiguous_wide just above. A `:set`
    // made this session is not specially preserved here (unlike `images`
    // below): these are cosmetic reading preferences, not a persisted
    // network/privacy posture, so a reload simply re-adopts the file.
    app.margin = resolved.reading.margin.value;
    app.text_align = layout::TextAlign::parse(&resolved.reading.text_align.value)
        .unwrap_or(layout::TextAlign::Center);
    app.paragraph_spacing = resolved.reading.paragraph_spacing.value;
    app.line_spacing = resolved.reading.line_spacing.value;
    app.word_spacing = resolved.reading.word_spacing.value;
    app.readlater_auto_dequeue = resolved.readlater_auto_dequeue.value;
    // PRD FR-ML-2: the fallback chain and picker-pinning both read this
    // live, like measure/ambiguous_wide above — no restart needed to pick
    // up an edited `languages = [...]`.
    app.languages = resolved.languages.value;
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
    let total_warnings = resolved.issues.len() + theme_warnings.len();
    app.notice = Some(if total_warnings == 0 {
        "Config reloaded".to_string()
    } else {
        format!("Config reloaded ({total_warnings} warning(s) — see `wikitui config doctor`)")
    });
}

/// Fetch and open `title` as a fresh navigation (pushes the current article
/// onto the back stack — see `App::open_document`). Used for the initial
/// CLI title, search results, and following a link. When the fetch served a
/// stale-but-within-backstop cache hit, spawns the PRD FR-OFF-2 background
/// revalidation `revalidate_tx` will eventually report back.
///
/// PRD FR-ML-2's fallback chain: tries `app.lang` first — whatever brought
/// us here, an explicit `lang:Title`/`--lang` override, a bookmark's own
/// recorded language, or just the session default — then any other
/// configured `languages` in their order, each only if the one before it
/// came back with nothing (a network error or a missing article). A reader
/// who never set `languages` gets exactly one attempt, identical to
/// pre-FR-ML-2 behavior. On success `app.lang` becomes whichever edition
/// actually resolved, so the tab/history/status all reflect it truthfully
/// rather than the language that was originally asked for.
///
/// PRD FR-ML-1/2: once the article installs, fires a langlinks fetch for it
/// off the event loop (`fire_langlinks`) — never blocking this navigation —
/// so the `:lang` picker is warm and the "available in your preferred
/// language" hint can surface without the reader pressing anything first.
/// PRD FR-DL-5: the shared "follow this internal link" entry point for
/// Enter, the registry's `Action::FollowLink`, and a resolved link hint
/// (`HintFollowAction::Foreground`) — the one place that checks whether
/// `title` is already a known redlink (`App::is_redlink`, either Parsoid's
/// `class="new"` pre-marking or the batched info check) before ever
/// attempting a fetch. A confirmed redlink shows §7's "doesn't exist yet"
/// card directly — no network round trip to (re)discover what this session
/// already knows — anything else falls through to the ordinary `open_title`
/// navigation. Deliberately not applied to the *background*-tab-open paths
/// (Ctrl-Enter, `Action::OpenBackgroundTab`): those already tolerate a 404
/// via the ordinary error path, and routing a background tab through a
/// foreground card would fight the "doesn't move focus" contract FR-TB-3
/// gives that action — a documented, narrower scope than the foreground
/// follow paths this covers.
async fn follow_internal_link(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    title: &str,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    if app.is_redlink(title) {
        app.show_redlink_card(app.lang.clone(), title.to_string());
        return;
    }
    open_title(client, cache, app, title, revalidate_tx, langlinks_tx).await;
}

/// PRD FR-ACC-5: `T` (Reading context) or `:talk` flips the active tab
/// between an article and its talk page (`talk::toggle_target`'s
/// `Talk:{title}` convention — see that module's doc comment for the v1.0
/// localized-namespace seam). A talk page is just another ordinary page, so
/// this reuses the exact link-follow path (`follow_internal_link`) rather
/// than a second fetch/install pipeline: same redlink guard, same cache/
/// prefetch/badge machinery, same back-stack push — `H` after toggling
/// returns to whichever page was on screen before, exactly like following
/// any other internal link.
async fn toggle_talk_page(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    let Some(current_title) = app.active_tab().doc.as_ref().map(|d| d.title.clone()) else {
        app.status = "No article open".to_string();
        return;
    };
    let entering_talk = talk::from_talk(&current_title).is_none();
    let target = talk::toggle_target(&current_title);
    follow_internal_link(client, cache, app, &target, revalidate_tx, langlinks_tx).await;
    // A status/notice cue that this is the talk page, not the article
    // (PRD FR-ACC-5) — on top of the ordinary status line, which already
    // shows the real (now `Talk:`-prefixed) title. Only on the way in:
    // flipping back to the article needs no such notice, the title alone
    // already says so.
    if entering_talk && app.active_tab().doc.is_some() {
        app.notice = Some("Viewing the talk page — T to return".to_string());
    }
}

/// PRD FR-DL-3/FR-DL-5: after an article installs, opportunistically fetches
/// its quality-assessment badge and checks its own outgoing links for
/// redlinks the parse-time `class="new"` signal didn't already catch — both
/// single batched calls (never a fanout: `WikiClient::page_assessments`
/// takes one title, `fetch_missing_links` takes one source and gets every
/// one of its links in the same request) — and both session-cached
/// (`App::quality_cache`/`checked_redlink_sources`) so a re-visited article
/// costs nothing the second time.
///
/// Awaited inline rather than backgrounded through a channel+`tokio::spawn`
/// (contrast `fire_langlinks`, deliberately fire-and-forget): doing the same
/// for these two would mean threading a third `_tx`/`*Outcome` pair through
/// every one of `open_title`/`open_history_entry`'s many call sites for a
/// pair of small, already-batched, already-tested requests — this chunk
/// judged that plumbing cost not worth it against the extra latency of
/// awaiting them inline. Routing both through the netqueue substrate
/// (PRD's "at low priority") instead of a dedicated call is the documented
/// seam for a later pass, not a limitation of the API methods themselves.
/// The redlink check is additionally skipped outright when prefetch is off
/// (kill switch or incognito) — FR-DL-5's "skippable on budget", realized as
/// "skippable when the reader already said no to background traffic."
async fn enrich_article(client: &WikiClient, app: &mut App, lang: &str, title: &str) {
    // Every session-state key here is scoped to the wiki the article was
    // fetched from (PRD FR-ML-4), so a same-titled article on another wiki
    // gets its own quality badge / redlink set, never this one's.
    let wiki = client.wiki_scope();
    let key = (wiki.clone(), lang.to_string(), title.to_string());
    // PRD FR-ML-5: a wiki without PageAssessments never gets the lookup
    // attempted, so `quality_cache` simply never gains an entry for it — the
    // same visible result (no badge) as a wiki that has the extension but no
    // assessment for this one title.
    if client.capabilities().pageassessments
        && !app.quality_cache.contains_key(&key)
        && let Ok(assessments) = client.page_assessments(lang, &[title.to_string()]).await
        && let Some(class) = assessments.get(title)
    {
        app.quality_cache.insert(key.clone(), *class);
    }

    if app.prefetch_active() && !app.checked_redlink_sources.contains(&key) {
        app.checked_redlink_sources.insert(key.clone());
        if let Ok(missing) = client.fetch_missing_links(lang, title).await {
            app.confirmed_redlinks.extend(
                missing
                    .into_iter()
                    .map(|t| (wiki.clone(), lang.to_string(), t)),
            );
        }
    }

    // PRD FR-PF-3: learn from this read. On a first read this session we fetch
    // the article's categories (one batched `prop=categories` call) and apply
    // the "open" signal; on a revisit the categories are already cached, so
    // we apply the open signal without re-fetching. Then schedule interest-
    // driven `morelike:` prefetch. All gated on `interest_active` (learning on
    // and not incognito), so incognito reads leave the model untouched.
    if app.interest_active() {
        let raw = if app.interest.knows_categories(&wiki, title) {
            Vec::new()
        } else {
            client
                .fetch_categories(lang, &[title.to_string()])
                .await
                .ok()
                .and_then(|mut m| m.remove(title))
                .unwrap_or_default()
        };
        app.note_article_read(&wiki, title, &raw);
        schedule_interest_prefetch(app);
    }
}

async fn open_title(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    title: &str,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    app.loading = true;
    let chain = app::fallback_chain(&app.lang, &app.languages);

    let mut last_err = None;
    for lang in &chain {
        // NF-NET-1: hold the foreground gate across this interactive fetch
        // so the background worker yields — a prefetch already draining
        // never delays the article the reader is waiting on.
        let outcome = {
            let _fg = app
                .prefetch
                .as_ref()
                .map(netqueue::SubstrateHandle::foreground_guard);
            fetch_page(
                client,
                cache,
                &app.search_index,
                &client.wiki_scope(),
                lang,
                title,
            )
            .await
        };
        match outcome {
            Ok(outcome) => {
                // PRD §5.7: a pinned saved copy is the intended offline
                // artifact, so it takes precedence over a stale cache serve
                // — but never over a live/fresh copy, which is genuinely
                // newer.
                if matches!(outcome.source, PageSource::Offline { .. })
                    && app.saved.is_saved(lang, title)
                {
                    app.loading = false;
                    open_saved(app, lang, title);
                    return;
                }
                let document = doc::parse_article_html(title, &outcome.html);
                let article_title = document.title.clone();
                app.lang = lang.clone();
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
                        lang.clone(),
                        title.to_string(),
                        cached_revid,
                        revalidate_tx,
                    ) {
                        app.pending_revalidations += 1;
                    }
                }
                // PRD FR-PF-1: with the article shown, rank its links and
                // prefetch the top-N bodies into L2 (gated on the kill
                // switch + incognito).
                schedule_link_prefetch(app);
                fire_langlinks(client, lang, title, langlinks_tx);
                app.pending_langlinks += 1;
                // PRD FR-DL-3/FR-DL-5: quality badge + redlink info, both
                // session-cached and batched — see `enrich_article`'s doc
                // comment for why this is awaited inline rather than
                // threaded through yet another background channel.
                enrich_article(client, app, lang, &article_title).await;
                app.loading = false;
                return;
            }
            Err(e) => last_err = Some(e),
        }
    }
    // Every language in the chain came back with nothing.
    app.loading = false;
    let lang = chain.first().cloned().unwrap_or_else(|| app.lang.clone());
    if app.saved.is_saved(&lang, title) {
        open_saved(app, &lang, title);
    } else {
        // §7's "Offline, uncached link": the network failed and nothing is
        // cached, in every language tried. Offer the queue-for-fetch /
        // search-saved card (FR-OFF-6) for the chain's first (primary)
        // language, same as the pre-fallback-chain error path.
        app.status = format!(
            "Error: {}",
            last_err.expect("the chain always has at least one entry")
        );
        app.show_offline_card(lang, title.to_string());
    }
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
        // Read L2 in the history entry's *own* wiki scope (PRD FR-ML-4), not
        // the current active wiki — a back/forward hop to an article read on
        // another wiki before a `:wiki` switch still finds its cached copy.
        fetch_page(
            client,
            cache,
            &app.search_index,
            &entry.wiki,
            &entry.lang,
            &entry.title,
        )
        .await
    };
    match outcome {
        Ok(outcome) => {
            let document = doc::parse_article_html(&entry.title, &outcome.html);
            let article_title = document.title.clone();
            let entry_lang = entry.lang.clone();
            let entry_wiki = entry.wiki.clone();
            {
                let tab = app.active_tab_mut();
                tab.page_source = outcome.source;
                tab.current_revid = outcome.revid;
            }
            // PRD FR-NV-8: back/forward restores its own remembered scroll
            // just below, so it must not also raise the resume toast.
            app.suppress_resume_once = true;
            app.set_document(document);
            // `set_document` stamps the tab with the *active* wiki; this
            // navigation is to the entry's own wiki, so restore that (PRD
            // FR-ML-4) alongside the scroll below.
            app.active_tab_mut().wiki = entry_wiki;
            // Restore the scroll position we left this page at (set_document
            // reset it to the top); the draw clamps it to the article's real
            // extent, which is unchanged since it's the same article.
            app.active_tab_mut().scroll = entry.scroll;
            // PRD FR-TB-5: `set_document` just persisted the session with
            // scroll reset to 0 (its own doc comment covers why); this
            // restores the *real* scroll a moment later, so it needs its own
            // save to actually land on disk rather than the stale zero.
            app.persist_session();
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
            // PRD FR-DL-3/FR-DL-5: session-cached, so a back/forward hop to
            // an article already visited this session costs no extra
            // request — see `enrich_article`'s doc comment.
            enrich_article(client, app, &entry_lang, &article_title).await;
        }
        Err(e) => {
            app.status = format!("Error: {e}");
        }
    }
    app.loading = false;
}

/// PRD FR-TB-5: reopens a persisted tab set at startup. Every tab's content
/// loads lazily through the exact same background-tab machinery `Ctrl-Enter`
/// background opens use (`fire_background_load`/`apply_tab_load_outcome`),
/// so restore never blocks startup on the network — each tab shows its
/// target title + "…" in the tab bar until its fetch (cache-first, so
/// usually instant) lands, exactly like any other background tab. Scroll and
/// fold state can't be applied until that fetch installs a document
/// (`Tab::install_document` always resets both) — `apply_tab_load_outcome`
/// applies them from `app.pending_session_restore` the moment each tab's
/// fetch completes.
///
/// The first persisted tab reuses `App::new`'s already-existing first tab
/// (id `0`) rather than allocating a new one and discarding it — `App`'s
/// invariant is "always ≥ 1 tab," so there is always exactly one already
/// there to repurpose. Returns `false` (nothing restored, caller falls back)
/// for an empty session — a session that was saved as zero tabs should
/// never happen in practice (`App` never runs with zero tabs), but a
/// hand-edited or future-format file could still produce one.
fn restore_session_tabs(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    state: session::SessionState,
    open_tx: &UnboundedSender<TabLoadOutcome>,
) -> bool {
    if state.tabs.is_empty() {
        return false;
    }
    for (i, saved) in state.tabs.iter().enumerate() {
        let tab_id = if i == 0 {
            app.tabs[0].id
        } else {
            app.push_blank_tab(saved.lang.clone())
        };
        let Some(idx) = app.tab_index_by_id(tab_id) else {
            continue; // unreachable in practice: just allocated or reused.
        };
        {
            let tab = &mut app.tabs[idx];
            tab.lang = saved.lang.clone();
            // PRD FR-ML-4: a restored tab keeps the wiki it was saved on.
            tab.wiki = saved.wiki.clone();
            tab.back_stack = saved.back_stack.clone();
            tab.forward_stack = saved.forward_stack.clone();
        }
        if let Some(title) = &saved.title {
            {
                let tab = &mut app.tabs[idx];
                tab.loading = true;
                tab.pending_title = Some(title.clone());
            }
            app.pending_session_restore.insert(
                tab_id,
                app::PendingSessionRestore {
                    scroll: saved.scroll,
                    folded_blocks: saved.folded_blocks.iter().copied().collect(),
                },
            );
            fire_background_load(
                client,
                cache,
                &app.search_index,
                tab_id,
                saved.wiki.clone(),
                saved.lang.clone(),
                title.clone(),
                open_tx,
            );
        }
    }
    app.active = state.active.min(app.tabs.len() - 1);
    app.sync_active_tab();
    true
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
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
    summary_tx: &UnboundedSender<SummaryOutcome>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) {
    // The single choke point for `App::notice`'s "shown until the next
    // keypress" lifetime (PRD FR-CS-4-adjacent — see `ui::status_bar_text`):
    // every call to `handle_key` is a keypress, so every call starts by
    // discarding whatever notice the *previous* one left behind, before any
    // mode-specific dispatch runs. A handler below is free to set a fresh
    // `app.notice` after this point — Research's citation save, the redlink
    // card's yank, a bookmark toggle, "External link: ..." — and that one
    // survives to be drawn this frame, then gets cleared in turn by the next
    // keypress's call. This used to happen piecemeal (only inside
    // `Mode::Reading`'s own key handling), so a notice set while in Research,
    // on the redlink card, or anywhere else that isn't Reading never got
    // cleared at all and would linger stale once the reader returned to a
    // mode that does show it.
    app.notice = None;

    // `Q`'s one-keypress quit confirmation (PRD Appendix B) is intercepted
    // before any mode dispatch so no other binding can leak through: `y`
    // confirms the quit, anything else cancels it.
    if app.pending_quit_confirm {
        app.pending_quit_confirm = false;
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

    // PRD FR-CS-1: Ctrl-p opens the command palette from the reading view.
    // Additive — Ctrl-p was previously unbound there; the text-input modes
    // (Search's own Ctrl-p moves the suggestion) are deliberately excluded.
    if palette_allowed(app.mode)
        && !app.pending_g
        && !app.pending_b
        && !app.pending_r
        && !app.pending_z
        && code == KeyCode::Char('p')
        && modifiers.contains(KeyModifiers::CONTROL)
    {
        app.open_palette();
        return;
    }

    // PRD FR-CS-3: the active keymap's *override* layer (a user keymap.toml or
    // the emacs preset) is consulted ahead of the hardcoded arms below. The
    // vim default has an empty override layer, so `runtime_action` returns
    // `None` for every default binding and this is a no-op — `handle_key`'s
    // existing dispatch and behavior are untouched. Only a rebinding surfaces
    // here. Scoped to the reading view (where `dispatch_action` is well
    // defined) and to single keys — the g/b/r/z chord latches are resolved
    // below, so we skip while one is pending.
    if !app.pending_g
        && !app.pending_b
        && !app.pending_r
        && !app.pending_z
        && let Some(ctx) = override_context(app)
        && let Some(chord) = registry::Chord::from_key(code, modifiers)
        && let Some(action) = app.keymap.runtime_action(ctx, &chord)
    {
        dispatch_action(
            action,
            client,
            cache,
            app,
            revalidate_tx,
            open_tx,
            save_tx,
            related_tx,
            langlinks_tx,
            terminal,
        )
        .await;
        return;
    }

    match app.mode {
        // PRD FR-CS-4: the help overlay is scrollable so it can never clip,
        // however tall the generated cheatsheet. Navigation keys scroll;
        // Esc/?/q/Enter close (returning to the view it was opened over).
        Mode::Help => {
            let visible = terminal
                .size()
                .map(|s| s.height.saturating_sub(4))
                .unwrap_or(20);
            let max_scroll = (ui::help_view_len(app) as u16).saturating_sub(visible);
            let set = |v: u16| v.min(max_scroll);
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') | KeyCode::Char('q') => {
                    app.mode = app.prior_mode;
                    app.help_scroll = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    app.help_scroll = set(app.help_scroll.saturating_add(1))
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.help_scroll = app.help_scroll.saturating_sub(1)
                }
                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.help_scroll = set(app.help_scroll.saturating_add(10))
                }
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.help_scroll = app.help_scroll.saturating_sub(10)
                }
                KeyCode::Char(' ') => app.help_scroll = set(app.help_scroll.saturating_add(10)),
                KeyCode::Char('g') => app.help_scroll = 0,
                KeyCode::Char('G') => app.help_scroll = max_scroll,
                _ => {}
            }
        }
        // PRD FR-CS-1's command palette: fuzzy-filter as you type, Enter runs
        // the highlighted command in the context it was opened from, Esc
        // cancels. Ctrl-n/p and the arrows move the selection (mirroring the
        // typeahead dropdown's grammar).
        Mode::Palette => match code {
            KeyCode::Esc => {
                app.mode = app.palette_prior_mode;
                app.palette_input.clear();
            }
            KeyCode::Enter => {
                let action = app.palette_selection();
                app.mode = app.palette_prior_mode;
                app.palette_input.clear();
                if let Some(action) = action {
                    dispatch_action(
                        action,
                        client,
                        cache,
                        app,
                        revalidate_tx,
                        open_tx,
                        save_tx,
                        related_tx,
                        langlinks_tx,
                        terminal,
                    )
                    .await;
                }
            }
            KeyCode::Up => app.palette_move(-1),
            KeyCode::Down => app.palette_move(1),
            KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => app.palette_move(1),
            KeyCode::Char('p') if modifiers.contains(KeyModifiers::CONTROL) => app.palette_move(-1),
            KeyCode::Backspace => {
                app.palette_input.pop();
                app.palette_selected = 0;
            }
            KeyCode::Char(c) => {
                app.palette_input.push(c);
                app.palette_selected = 0;
            }
            _ => {}
        },
        // PRD FR-CS-8's first-run onboarding: any key dismisses the tour and
        // writes the default config so it never shows again.
        Mode::Onboarding => {
            app.mode = Mode::Reading;
            finish_onboarding(app);
        }
        // PRD FR-PF-4: the prefetch-log panel is read-only — any key closes it.
        Mode::PrefetchLog => app.close_prefetch_log(),
        // PRD FR-PF-3 / FR-PC-3: the interest and stats panels are read-only
        // inspectors — any key closes them, same as the prefetch log.
        Mode::Interests => app.close_interests(),
        Mode::Stats => app.close_stats(),
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
                    open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
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
                // PRD FR-TH-1: `:theme`/`:set theme=` also validate against
                // any loaded user theme names, not just the six built-ins.
                let user_theme_names: Vec<String> =
                    app.user_themes.iter().map(|t| t.name.clone()).collect();
                match command::parse_with_user_themes(&input, &user_theme_names) {
                    Ok(cmd) => {
                        execute_command(
                            terminal,
                            client,
                            cache,
                            app,
                            cmd,
                            revalidate_tx,
                            save_tx,
                            related_tx,
                            langlinks_tx,
                        )
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
        // PRD §5.9's manual code-paste prompt: type/paste the code (or full
        // redirect URL), Enter exchanges it, Esc cancels the login.
        Mode::Login => match code {
            KeyCode::Esc => {
                app.mode = Mode::Reading;
                app.pending_login = None;
                app.login_input.clear();
                app.status = String::new();
                app.notice = Some("Login canceled".to_string());
            }
            KeyCode::Enter => {
                if app.login_input.trim().is_empty() {
                    app.notice =
                        Some("Paste the authorization code first (or Esc to cancel)".to_string());
                } else {
                    submit_login_paste(client, app).await;
                }
            }
            KeyCode::Backspace => {
                app.login_input.pop();
            }
            KeyCode::Char(c) => {
                app.login_input.push(c);
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
                            follow_internal_link(
                                client,
                                cache,
                                app,
                                &title,
                                revalidate_tx,
                                langlinks_tx,
                            )
                            .await;
                        }
                        Some(app::HintFollowAction::Background(title)) => {
                            let lang = app.lang.clone();
                            let id = app.open_background_tab(title.clone(), lang.clone());
                            fire_background_load(
                                client,
                                cache,
                                &app.search_index,
                                id,
                                client.wiki_scope(),
                                lang,
                                title,
                                open_tx,
                            );
                            app.status = "Opening in a background tab…".to_string();
                        }
                        Some(app::HintFollowAction::External(href)) => {
                            // A `notice`, not `status` — a link is almost
                            // always focused right after following a hint,
                            // and the focused-link line would otherwise hide
                            // this behind Reading's own status line (the bug
                            // three prior chunks independently hit — see
                            // `ui::status_bar_text`).
                            app.notice = Some(format!("External link: {href}"));
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
                    if app.results_offline {
                        // PRD FR-SR-7: an offline result opens from the
                        // local saved/cache stores only — never a network
                        // fetch (see `open_offline_result`'s doc comment).
                        let wiki = app.active_wiki_scope().to_string();
                        let lang = app.lang.clone();
                        open_offline_result(cache, app, &wiki, &lang, &result.title);
                    } else {
                        open_title(
                            client,
                            cache,
                            app,
                            &result.title,
                            revalidate_tx,
                            langlinks_tx,
                        )
                        .await;
                    }
                } else if let Some(suggestion) = app.search_suggestion.clone() {
                    // PRD FR-SR-4 / §7's zero-results row: "Did you mean X?
                    // (Enter to search)" — re-runs the search with the
                    // suggested spelling. `search_suggestion` is always
                    // `None` for offline results (`run_offline_search`), so
                    // this arm only ever fires for an online zero-results
                    // screen.
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
                    open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-ML-1's language switcher: a selectable list of the current
        // article's langlinks, same j/k/Enter/Esc grammar as every other
        // picker in this match; `/` enters the live fuzzy filter.
        Mode::LangPicker => match code {
            KeyCode::Esc => app.close_lang_picker(),
            KeyCode::Char('j') | KeyCode::Down => app.cycle_lang(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_lang(false),
            KeyCode::Char('/') => app.mode = Mode::LangFilter,
            KeyCode::Enter => {
                if let Some((lang, title)) = app.lang_picker_target() {
                    switch_to_langlink(
                        client,
                        cache,
                        app,
                        &lang,
                        &title,
                        revalidate_tx,
                        langlinks_tx,
                    )
                    .await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // The picker's `/` filter (PRD FR-ML-1): every keystroke narrows the
        // live row list the draw already reads from `app.lang_filter_input`;
        // Enter/Esc both just return to navigating the (already-filtered)
        // picker — mirrors `Mode::BookmarkFilter`.
        Mode::LangFilter => match code {
            KeyCode::Esc | KeyCode::Enter => {
                app.mode = Mode::LangPicker;
                app.selected_lang = 0;
            }
            KeyCode::Backspace => {
                app.lang_filter_input.pop();
                app.selected_lang = 0;
            }
            KeyCode::Char(c) => {
                app.lang_filter_input.push(c);
                app.selected_lang = 0;
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
        // PRD FR-ML-4's bare `:wiki` picker: Wikipedia + the four sister
        // projects. Enter switches (same state change `:wiki <name>` makes),
        // Esc cancels. No fetch to kick off — the list never changes.
        Mode::WikiPicker => match code {
            KeyCode::Esc => app.mode = Mode::Reading,
            KeyCode::Char('j') | KeyCode::Down => app.cycle_wiki_pick(true),
            KeyCode::Char('k') | KeyCode::Up => app.cycle_wiki_pick(false),
            KeyCode::Enter => {
                if let Some(name) = app.wiki_picker_target() {
                    switch_wiki(client, app, &name);
                }
                app.mode = Mode::Reading;
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
                    open_title(
                        client,
                        cache,
                        app,
                        &bookmark.title,
                        revalidate_tx,
                        langlinks_tx,
                    )
                    .await;
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
                    open_title(
                        client,
                        cache,
                        app,
                        &entry.title,
                        revalidate_tx,
                        langlinks_tx,
                    )
                    .await;
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
                    open_title(
                        client,
                        cache,
                        app,
                        &visit.title,
                        revalidate_tx,
                        langlinks_tx,
                    )
                    .await;
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
        // PRD FR-DL-5 / §7's "Redlink followed" card: `s` searches for a
        // similar title (dismissing the card into the results it finds), `y`
        // yanks the wiki's own create-page URL, Esc dismisses.
        Mode::RedlinkCard => match code {
            KeyCode::Char('s') => {
                if let Some((_, title)) = app.redlink_card_target.clone() {
                    app.close_redlink_card();
                    app.search_input = title;
                    run_search(client, app).await;
                }
            }
            KeyCode::Char('y') => {
                if let Some(url) = app.redlink_create_url() {
                    app.notice = Some(match yank_to_clipboard(&url) {
                        Ok(()) => format!("Yanked {url}"),
                        Err(e) => format!("Yank failed: {e}"),
                    });
                }
            }
            KeyCode::Esc => app.close_redlink_card(),
            _ => {}
        },
        // PRD FR-NV-4/5's `K` peek popup: `Ctrl-o` (Appendix B "returns") and
        // Esc close it; Enter follows the previewed internal link in this tab.
        Mode::Peek => match code {
            KeyCode::Char('o') if modifiers.contains(KeyModifiers::CONTROL) => app.close_peek(),
            KeyCode::Esc => app.close_peek(),
            KeyCode::Enter => {
                if let Some((lang, title)) = app.peek_open_target() {
                    app.close_peek();
                    app.lang = lang;
                    follow_internal_link(client, cache, app, &title, revalidate_tx, langlinks_tx)
                        .await;
                }
            }
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
                    open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
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
        // PRD §10 / Appendix B's `:info` overlay: read-only, Esc dismisses.
        Mode::Info => {
            if code == KeyCode::Esc {
                app.close_info();
            }
        }
        // PRD FR-ACC-2's watchlist pane: same two-tab navigation shape as
        // `Mode::OnThisDay` above (j/k move, Tab/h/l switch tab, Enter opens).
        Mode::Watchlist => match code {
            KeyCode::Esc => app.close_watchlist(),
            KeyCode::Char('j') | KeyCode::Down => app.watchlist_move(1),
            KeyCode::Char('k') | KeyCode::Up => app.watchlist_move(-1),
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => app.watchlist_next_tab(),
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => app.watchlist_prev_tab(),
            KeyCode::Enter => {
                if let Some(title) = app.watchlist_open_target() {
                    app.close_watchlist();
                    open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
                } else {
                    app.status = "Nothing to open".to_string();
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-ACC-3's notifications pane: `d` marks the focused entry
        // read, `A` marks every entry read (both are the one real write this
        // mode makes — a fresh network round trip only for the write itself,
        // never a re-poll of the count, per `account.rs`'s poll-cadence doc).
        Mode::Notifications => match code {
            KeyCode::Esc => app.close_notifications(),
            KeyCode::Char('j') | KeyCode::Down => app.notif_move(1),
            KeyCode::Char('k') | KeyCode::Up => app.notif_move(-1),
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => app.notif_next_tab(),
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => app.notif_prev_tab(),
            KeyCode::Char('d') => mark_notification_read(client, app).await,
            KeyCode::Char('A') => mark_all_notifications_read(client, app).await,
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-ACC-4/6's contributions view: Enter opens the edited
        // article, `t` thanks the focused edit (logged in only).
        Mode::Contribs => match code {
            KeyCode::Esc => app.close_contribs(),
            KeyCode::Char('j') | KeyCode::Down => app.contribs_move(1),
            KeyCode::Char('k') | KeyCode::Up => app.contribs_move(-1),
            KeyCode::Enter => {
                if let Some(title) = app.contribs_open_target() {
                    app.close_contribs();
                    open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
                } else {
                    app.status = "Nothing to open".to_string();
                }
            }
            KeyCode::Char('t') => cmd_thank(client, app).await,
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        // PRD FR-ACC-7's read-only prefs card: same overlay idiom as
        // `Mode::Info` above.
        Mode::Prefs => {
            if code == KeyCode::Esc {
                app.close_prefs();
            }
        }
        Mode::Reading => {
            // Ctrl-w window-command chord (PRD FR-TB-4, Appendix B): the
            // Ctrl-w latch's second key. `v` splits, `w`/`h`/`l` move focus
            // between panes, `c`/`o`/`q` close the split. Consumes the second
            // key (an unrecognized one is a no-op, vim-style) — resolved here
            // before any other binding so `Ctrl-w v` never falls through to
            // `v`'s own (absent) meaning.
            if app.pending_ctrl_w {
                app.pending_ctrl_w = false;
                if let KeyCode::Char(c) = code {
                    match c {
                        'v' => match app.open_split(app.last_content_area.width) {
                            Ok(()) => {
                                app.notice = Some(
                                    "split — Ctrl-w w switches panes, :set scrollbind syncs, \
                                     :only closes"
                                        .to_string(),
                                )
                            }
                            Err(reason) => app.notice = Some(reason),
                        },
                        'w' => {
                            if !app.focus_split_other() {
                                app.notice = Some("no split — Ctrl-w v to split".to_string());
                            }
                        }
                        'h' => {
                            app.focus_split_pane(0);
                        }
                        'l' => {
                            app.focus_split_pane(1);
                        }
                        'c' | 'o' | 'q' => {
                            if !app.close_split() {
                                app.notice = Some("not split".to_string());
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
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
                            open_random_article(client, cache, app, revalidate_tx, langlinks_tx)
                                .await;
                            return;
                        }
                        app::GPrefixAction::Related => {
                            open_related(app, client, related_tx);
                            return;
                        }
                        // PRD FR-NV-4: `gK` jumps to the References section.
                        app::GPrefixAction::References => {
                            app.jump_to_references();
                            return;
                        }
                        // PRD FR-ACC-2: `gW` opens the watchlist pane.
                        app::GPrefixAction::Watchlist => {
                            open_watchlist(client, app).await;
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
            // z-prefix chord (PRD FR-PR-3, Appendix B): `zz` toggles
            // incognito. `cache.set_incognito` keeps the page cache's tagging
            // (see `cache::PageCache`'s doc comment) in sync with the flag the
            // rest of the app reads off `App`, so a toggle mid-session applies
            // to the very next fetch, not just ones after a restart.
            if app.pending_z {
                app.pending_z = false;
                if let KeyCode::Char(c) = code {
                    match app::resolve_z_prefix(c) {
                        app::ZPrefixAction::ToggleIncognito => {
                            app.incognito = !app.incognito;
                            cache.set_incognito(app.incognito);
                            app.notice = Some(if app.incognito {
                                "incognito: on — no history, no stats, no prefetch \
                                 (explicit saves still persist, with a warning)"
                                    .to_string()
                            } else {
                                "incognito: off".to_string()
                            });
                            return;
                        }
                        // PRD FR-NV-3 section folding.
                        app::ZPrefixAction::ToggleFold => {
                            app.toggle_fold_at_cursor();
                            return;
                        }
                        app::ZPrefixAction::FoldAll => {
                            app.fold_all();
                            return;
                        }
                        app::ZPrefixAction::UnfoldAll => {
                            app.unfold_all();
                            return;
                        }
                        app::ZPrefixAction::PassThrough => {} // handle `code` normally below.
                    }
                }
            }

            // PRD FR-OFF-2's "updated — r to reload" notice means this
            // keypress, if it's `r`, reloads instead of arming the
            // read-later/Research prefix — see the `r` arm's own comment for
            // why the two share a key.
            let had_pending_reload = app.active_tab().pending_reload.is_some();
            // PRD FR-NV-8's resume toast, captured for the `r`-precedence
            // decision below (see the `r` arms). It ranks *below* the SWR "r
            // to reload" (a fresher update always wins) and *above* the
            // read-later/Research `r`-prefix.
            let had_pending_resume = app.pending_resume.is_some();
            // `r`-prefix chord (PRD FR-BM-3's `rl`), reached only when no
            // reload is pending (see above): `rl` enqueues for later, any
            // other second key falls back to `r`'s own original meaning
            // (open Research mode) — see `app::resolve_r_prefix`'s doc
            // comment for why this, unlike `g`/`b`, consumes the second key
            // rather than reprocessing it as its own binding.
            if app.pending_r {
                app.pending_r = false;
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
            // PRD FR-NV-8: the resume toast is non-blocking — any key other
            // than `r` dismisses it (the `r` arms below consume it first).
            if !matches!(code, KeyCode::Char('r')) {
                app.pending_resume = None;
            }
            match code {
                // PRD Appendix B: `q` closes the current tab (quitting if it
                // was the last); `Q` quits outright behind a one-keypress
                // y/n confirm (armed here, resolved at the top of handle_key).
                KeyCode::Char('q') => {
                    // PRD FR-TB-4: while split, `q` closes the split (the
                    // current "window"), keeping the tabs — like Ctrl-w c.
                    // Only when unsplit does it close the tab (quitting if it
                    // was the last).
                    if app.split.is_some() {
                        app.close_split();
                    } else if app.close_active_tab() {
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
                            open_title(client, cache, app, &title, revalidate_tx, langlinks_tx)
                                .await;
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
                            fire_background_load(
                                client,
                                cache,
                                &app.search_index,
                                id,
                                client.wiki_scope(),
                                lang,
                                title,
                                open_tx,
                            );
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
                                follow_internal_link(
                                    client,
                                    cache,
                                    app,
                                    &title,
                                    revalidate_tx,
                                    langlinks_tx,
                                )
                                .await
                            }
                            // A `notice`, not `status` — the very link this
                            // reports on is the one still focused, so the
                            // focused-link line would otherwise hide it.
                            None => app.notice = Some(format!("External link: {}", link.href)),
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
                // Ctrl-t cycles the theme (PRD FR-TH-2): bare `T` moved to
                // the talk-page toggle below to match PRD Appendix B's
                // "Article: T talk page" sketch — see `toggle_talk_page`'s
                // doc comment for the conflict this resolves. `:theme
                // <name>`/`:set theme=` remain unchanged.
                KeyCode::Char('t') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.cycle_theme()
                }
                KeyCode::Char('t') if app.active_tab().doc.is_none() => app.reroll_til(),
                KeyCode::Char('t') => {
                    if app.active_tab().sections.is_empty() {
                        app.status = "No sections on this page".to_string();
                    } else {
                        app.mode = Mode::Toc;
                    }
                }
                // PRD FR-ACC-5: flip article ↔ talk page.
                KeyCode::Char('T') => {
                    toggle_talk_page(client, cache, app, revalidate_tx, langlinks_tx).await
                }
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
                // PRD FR-NV-8: `r` on the resume toast jumps to the saved
                // position — ranked below the SWR reload above (a fresher
                // update wins) and above the read-later/Research `r`-prefix
                // below (`had_pending_resume` was captured before the toast
                // was cleared).
                KeyCode::Char('r') if had_pending_resume => {
                    app.resume_to_saved_position();
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
                // PRD FR-NV-4/5: `K` peeks the focused link — a reference
                // marker opens the footnote peek (resolved locally, no
                // network), an internal link opens the link preview (its
                // summary fetched lazily off the event loop if not cached).
                KeyCode::Char('K') => {
                    if let Some((lang, title)) = app.open_peek_at_focus() {
                        fire_summary(client, &lang, &title, summary_tx);
                    }
                }
                // PRD §10 / Appendix B's "Article: i article info/attribution".
                KeyCode::Char('i') => {
                    app.open_info();
                }
                // PRD FR-TB-4 / Appendix B: Ctrl-w arms the window-command
                // chord (resolved at the top of this arm on the next key).
                // Guarded against the plain `w` watch-toggle below, matching
                // this match's "more specific/guarded binding first" idiom.
                KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.pending_ctrl_w = true;
                }
                // PRD FR-ACC-2 / Appendix B's "Library: w watch/unwatch".
                KeyCode::Char('w') => cmd_watch_toggle(client, app).await,
                // Arm the g-/b-/z-prefix latches (their second key is
                // consumed at the top of this arm on the next keypress).
                KeyCode::Char('g') => app.pending_g = true,
                KeyCode::Char('b') => app.pending_b = true,
                KeyCode::Char('z') => app.pending_z = true,
                KeyCode::Char('G') => app.scroll_to_bottom(),
                KeyCode::Esc => {
                    app.pending_g = false;
                    app.pending_b = false;
                    app.pending_r = false;
                    app.pending_z = false;
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
    if !matches!(code, KeyCode::Char('z')) {
        app.pending_z = false;
    }
}

/// How many lines one mouse-wheel notch scrolls the reading view or moves a
/// picker's selection (PRD FR-NV-9) — implemented by synthesizing that many
/// `Down`/`Up` keypresses through `handle_key` itself (see `handle_mouse`),
/// so wheel scrolling behaves identically, in every mode, to holding the
/// arrow key: no separate scroll-amount logic to keep in sync with whatever
/// `j`/`k`/`Down`/`Up` already do there.
const MOUSE_WHEEL_STEP: u8 = 3;

/// PRD FR-NV-9's mouse dispatch, mirroring `handle_key`'s own shape and
/// parameter list. Every action here is implemented by literally reusing an
/// existing keyboard action (a synthesized `Down`/`Up`/`Enter` through
/// `handle_key`, or the same `App::switch_to_tab` the tab picker's own Enter
/// calls) — never a second, mouse-only code path — which is what makes PRD
/// FR-ACS-3's "every mouse action has a keyboard equivalent" true by
/// construction rather than by convention.
#[allow(clippy::too_many_arguments)]
async fn handle_mouse(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    mouse: MouseEvent,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    open_tx: &UnboundedSender<TabLoadOutcome>,
    save_tx: &UnboundedSender<SaveOutcome>,
    related_tx: &UnboundedSender<RelatedOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
    summary_tx: &UnboundedSender<SummaryOutcome>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) {
    // Defense in depth, not the real gate: the real gate is that
    // `Event::Mouse` is never produced at all unless `main` sent
    // `EnableMouseCapture`, which it only does when `mouse` is on (PRD
    // FR-ACS-3: mouse support must be strictly additive, never load-bearing).
    if !app.mouse_enabled {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollDown => {
            for _ in 0..MOUSE_WHEEL_STEP {
                handle_key(
                    client,
                    cache,
                    app,
                    KeyCode::Down,
                    KeyModifiers::NONE,
                    revalidate_tx,
                    open_tx,
                    save_tx,
                    related_tx,
                    langlinks_tx,
                    summary_tx,
                    terminal,
                )
                .await;
            }
        }
        MouseEventKind::ScrollUp => {
            for _ in 0..MOUSE_WHEEL_STEP {
                handle_key(
                    client,
                    cache,
                    app,
                    KeyCode::Up,
                    KeyModifiers::NONE,
                    revalidate_tx,
                    open_tx,
                    save_tx,
                    related_tx,
                    langlinks_tx,
                    summary_tx,
                    terminal,
                )
                .await;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            handle_left_click(
                client,
                cache,
                app,
                mouse.column,
                mouse.row,
                revalidate_tx,
                open_tx,
                save_tx,
                related_tx,
                langlinks_tx,
                summary_tx,
                terminal,
            )
            .await;
        }
        // Right/middle click, drag, and plain movement carry no meaning here
        // (PRD FR-NV-9 names scroll, link-follow, TOC entries, and the tab
        // bar — nothing else) and are silently ignored, exactly like an
        // unbound key.
        _ => {}
    }
}

/// A left click's dispatch (PRD FR-NV-9): the tab bar first (clickable
/// regardless of the current mode — there is no picker selection state for
/// it, so a hit switches tabs directly), then whatever the click landed on
/// inside the content area, scoped to the views this chunk wires up (the
/// reading view's links, the TOC, and full-text search results — see
/// `ui::results_row_to_index`/`ui::single_line_list_row_to_index`'s own doc
/// comments for exactly why the long tail of other pickers isn't included:
/// a click on a *scrolled* list can't be safely mapped to an entry without
/// ratatui exposing the scroll offset it chose, so those stay keyboard-only
/// for now — never a regression, since the keyboard path already covers
/// every one of them).
#[allow(clippy::too_many_arguments)]
async fn handle_left_click(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    col: u16,
    row: u16,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    open_tx: &UnboundedSender<TabLoadOutcome>,
    save_tx: &UnboundedSender<SaveOutcome>,
    related_tx: &UnboundedSender<RelatedOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
    summary_tx: &UnboundedSender<SummaryOutcome>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) {
    if let Some(bar) = app.last_tab_bar_area
        && row == bar.y
        && col >= bar.x
        && col < bar.x.saturating_add(bar.width)
    {
        let labels: Vec<ui::TabLabel> = app
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| ui::TabLabel {
                number: i + 1,
                title: t.display_title(),
                loading: t.loading,
                active: i == app.active,
            })
            .collect();
        if let Some(idx) = ui::tab_bar_hit_test(&labels, bar.width as usize, (col - bar.x) as usize)
        {
            app.switch_to_tab(idx); // same call the tab picker's own Enter makes.
        }
        return;
    }

    let area = app.last_content_area;
    let inside = row >= area.y
        && row < area.y.saturating_add(area.height)
        && col >= area.x
        && col < area.x.saturating_add(area.width);
    if !inside {
        return; // clicking outside every known clickable region is a no-op.
    }

    // "Select the clicked entry, then synthesize Enter" reuses each mode's
    // existing Enter handling verbatim (`follow_internal_link`'s branch for
    // Reading, `App::jump_to_section` for Toc, `open_title` for Results) —
    // see this function's own doc comment for why that's deliberate.
    match app.mode {
        Mode::Reading => {
            let Some(link_idx) = app.layout.as_ref().and_then(|layout| {
                let scroll = app.active_tab().scroll as usize;
                let line_index = scroll + (row - area.y) as usize;
                let col_in_line = (col - area.x) as usize;
                layout.link_at(line_index, col_in_line, app.ambiguous_wide)
            }) else {
                return; // clicked on plain text, not a link — no-op.
            };
            app.active_tab_mut().focused_link = Some(link_idx);
            handle_key(
                client,
                cache,
                app,
                KeyCode::Enter,
                KeyModifiers::NONE,
                revalidate_tx,
                open_tx,
                save_tx,
                related_tx,
                langlinks_tx,
                summary_tx,
                terminal,
            )
            .await;
        }
        Mode::Toc => {
            let len = app.active_tab().sections.len();
            let Some(idx) = ui::single_line_list_row_to_index(area, len, row) else {
                return;
            };
            app.active_tab_mut().selected_section = idx;
            handle_key(
                client,
                cache,
                app,
                KeyCode::Enter,
                KeyModifiers::NONE,
                revalidate_tx,
                open_tx,
                save_tx,
                related_tx,
                langlinks_tx,
                summary_tx,
                terminal,
            )
            .await;
        }
        Mode::Results => {
            let Some(idx) = ui::results_row_to_index(app, area, row) else {
                return;
            };
            app.selected_result = idx;
            handle_key(
                client,
                cache,
                app,
                KeyCode::Enter,
                KeyModifiers::NONE,
                revalidate_tx,
                open_tx,
                save_tx,
                related_tx,
                langlinks_tx,
                summary_tx,
                terminal,
            )
            .await;
        }
        _ => {}
    }
}

/// PRD FR-RD-2 / SEC-2: overlays OSC 8 hyperlinks directly onto the terminal
/// for every visible link in the active tab's reading view, immediately
/// after `terminal.draw` has painted the frame — see `hyperlink`'s module doc
/// comment for exactly why this must be a distinct pass rather than escape
/// bytes injected into a `ratatui::text::Span`'s own content (ratatui's own
/// width accounting would corrupt on the latter). Nothing here writes
/// through `Frame`/`Buffer`/`Span` at all: a `MoveTo` positions the terminal
/// cursor (hidden throughout — this app never calls `Frame::set_cursor_position`)
/// at a link's first cell, the zero-width OSC 8 open sequence is printed,
/// then the same for one past its last cell with the close sequence — never
/// touching a single visible glyph ratatui already placed.
fn emit_hyperlinks(app: &App) -> io::Result<()> {
    if app.mode != Mode::Reading {
        return Ok(());
    }
    let env = hyperlink::HyperlinkEnv {
        is_tty: std::io::stdout().is_terminal(),
        accessible: app.accessible,
    };
    if !hyperlink::active(app.hyperlinks_mode, env) {
        return Ok(());
    }
    let Some(layout) = app.layout.as_ref() else {
        return Ok(());
    };
    let area = app.last_content_area;
    if area.width == 0 || area.height == 0 {
        return Ok(());
    }
    let tab = app.active_tab();
    let scroll = tab.scroll as usize;
    let top = scroll;
    let bottom = (scroll + area.height as usize).min(layout.lines.len());
    if top >= bottom {
        return Ok(());
    }

    use crossterm::cursor::MoveTo;
    use crossterm::queue;
    use crossterm::style::Print;
    let mut out = io::stdout();
    for (row_offset, line_index) in (top..bottom).enumerate() {
        let line = &layout.lines[line_index];
        let row = area.y + row_offset as u16;
        let mut col = 0usize;
        for span in &line.spans {
            let w = layout::display_width(&span.text, app.ambiguous_wide);
            if let layout::SpanKind::Link(occ) = span.kind
                && let Some(link) = tab.links.get(occ)
            {
                let url = doc::resolve_link_url(&link.href, &tab.lang);
                // SEC-2: only a validated https(s) target ever becomes an
                // OSC 8 escape — anything else keeps its plain styled text,
                // already painted, completely untouched.
                if let Some(safe) = hyperlink::sanitize_uri(&url) {
                    let start_col = area.x + col as u16;
                    let end_col = area.x + (col + w) as u16;
                    if end_col <= area.x + area.width {
                        queue!(
                            out,
                            MoveTo(start_col, row),
                            Print(hyperlink::osc8_open(safe))
                        )?;
                        queue!(out, MoveTo(end_col, row), Print(hyperlink::OSC8_CLOSE))?;
                    }
                }
            }
            col += w;
        }
    }
    out.flush()
}

/// The modes `Ctrl-p` opens the command palette from (PRD FR-CS-1). The
/// reading view only: from there the palette's commands (search, toc, random,
/// theme, ...) are all meaningful, and `Ctrl-p` was previously unbound, so
/// this is purely additive. The text-input modes are excluded (Search's own
/// `Ctrl-p` moves the suggestion).
fn palette_allowed(mode: Mode) -> bool {
    matches!(mode, Mode::Reading)
}

/// The [`registry::KeyContext`] whose keymap *override* layer applies to a
/// raw keypress (PRD FR-CS-3). Only the reading view: there `dispatch_action`
/// covers every action, so a rebinding is safe to route through it. Returns
/// `None` everywhere else, leaving those modes' hardcoded handlers untouched
/// (picker/search key remapping is a documented seam).
fn override_context(app: &App) -> Option<registry::KeyContext> {
    match app.mode {
        Mode::Reading => Some(if app.active_tab().doc.is_none() {
            registry::KeyContext::StartPage
        } else {
            registry::KeyContext::Reading
        }),
        _ => None,
    }
}

/// PRD FR-CS-8: dismissing the first-run tour writes `config.toml` with
/// commented defaults so onboarding never shows again (its presence is the
/// first-run sentinel — see `config::first_run`). A write failure is
/// non-fatal: the tour simply reappears next run, surfaced as a notice.
fn finish_onboarding(app: &mut App) {
    match config::write_default_config(app.config_ctx.config_path.as_deref()) {
        Ok(true) => app.status = "Welcome — defaults written to your config file".to_string(),
        Ok(false) => {} // a file already exists (or no config dir); nothing to do
        Err(e) => app.notice = Some(format!("Could not write the default config: {e}")),
    }
}

/// Execute a registry [`registry::Action`] (PRD §5.13): the single dispatch
/// point the command palette (FR-CS-1) and the keymap override layer
/// (FR-CS-3) both drive through. Each arm performs exactly what
/// `handle_key`'s corresponding hardcoded binding does, so routing a command
/// here — from a palette row or a rebound key — is behavior-identical to
/// pressing its default key.
#[allow(clippy::too_many_arguments)]
async fn dispatch_action(
    action: registry::Action,
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    open_tx: &UnboundedSender<TabLoadOutcome>,
    save_tx: &UnboundedSender<SaveOutcome>,
    related_tx: &UnboundedSender<RelatedOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) {
    use registry::Action;
    match action {
        Action::ScrollDown => app.scroll_by(1),
        Action::ScrollUp => app.scroll_by(-1),
        Action::HalfPageDown => app.scroll_by(10),
        Action::HalfPageUp => app.scroll_by(-10),
        Action::PageDown => app.scroll_by(15),
        Action::ScrollTop => app.scroll_to_top(),
        Action::ScrollBottom => app.scroll_to_bottom(),
        Action::ScrollTablesLeft => app.scroll_tables(-1),
        Action::ScrollTablesRight => app.scroll_tables(1),
        Action::LinkCycleNext => app.cycle_link(true),
        Action::LinkCyclePrev => app.cycle_link(false),
        Action::LinkHints => app.enter_hint_mode(false),
        Action::LinkHintsBackground => app.enter_hint_mode(true),
        Action::FollowLink => {
            let link = app
                .active_tab()
                .focused_link
                .and_then(|i| app.active_tab().links.get(i))
                .cloned();
            if let Some(link) = link {
                match link.internal_title {
                    Some(title) => {
                        follow_internal_link(
                            client,
                            cache,
                            app,
                            &title,
                            revalidate_tx,
                            langlinks_tx,
                        )
                        .await
                    }
                    // A `notice`, not `status` — same reasoning as the
                    // hardcoded `Enter` arm this mirrors.
                    None => app.notice = Some(format!("External link: {}", link.href)),
                }
            }
        }
        Action::OpenBackgroundTab => {
            let link = app
                .active_tab()
                .focused_link
                .and_then(|i| app.active_tab().links.get(i))
                .cloned();
            match link.and_then(|l| l.internal_title) {
                Some(title) => {
                    let lang = app.lang.clone();
                    let id = app.open_background_tab(title.clone(), lang.clone());
                    fire_background_load(
                        client,
                        cache,
                        &app.search_index,
                        id,
                        client.wiki_scope(),
                        lang,
                        title,
                        open_tx,
                    );
                    app.status = "Opening in a background tab…".to_string();
                }
                None => app.status = "No internal link focused to open in a tab".to_string(),
            }
        }
        Action::Back => {
            if let Some(entry) = app.navigate_back_target() {
                open_history_entry(client, cache, app, entry, revalidate_tx).await;
            } else {
                app.status = "No earlier page in history".to_string();
            }
        }
        Action::Forward => {
            if let Some(entry) = app.navigate_forward_target() {
                open_history_entry(client, cache, app, entry, revalidate_tx).await;
            } else {
                app.status = "No later page in history".to_string();
            }
        }
        Action::BackStackPicker => {
            if app.active_tab().back_stack.is_empty() {
                app.status = "No history to show in this tab".to_string();
            } else {
                app.selected_history = app.active_tab().back_stack.len() - 1;
                app.mode = Mode::HistoryPicker;
            }
        }
        Action::ReadingHistory => app.open_reading_history_picker(),
        Action::NextTab => app.next_tab(),
        Action::PrevTab => app.prev_tab(),
        Action::TabPicker => {
            app.selected_tab_pick = app.active;
            app.mode = Mode::TabPicker;
        }
        Action::ReopenClosedTab => {
            if !app.reopen_closed_tab() {
                app.status = "No recently closed tabs to reopen".to_string();
            }
        }
        Action::CloseTab => {
            if app.close_active_tab() {
                app.should_quit = true;
            }
        }
        Action::Toc => {
            if app.active_tab().sections.is_empty() {
                app.status = "No sections on this page".to_string();
            } else {
                app.mode = Mode::Toc;
            }
        }
        Action::CycleTheme => app.cycle_theme(),
        Action::TalkToggle => {
            toggle_talk_page(client, cache, app, revalidate_tx, langlinks_tx).await
        }
        Action::Search => {
            app.mode = Mode::Search;
            app.search_input.clear();
            app.typeahead.clear();
            app.search_suggestion = None;
            app.search_debounce_at = None;
        }
        Action::CommandLine => {
            app.mode = Mode::Command;
            app.command_input.clear();
        }
        Action::Help => {
            app.prior_mode = app.mode;
            app.help_scroll = 0;
            app.mode = Mode::Help;
        }
        Action::Palette => app.open_palette(),
        Action::YankUrl => {
            if let Some(url) = app.yank_url() {
                app.status = match yank_to_clipboard(&url) {
                    Ok(()) => format!("Yanked {url}"),
                    Err(e) => format!("Yank failed: {e}"),
                };
            } else {
                app.status = "Open an article first".to_string();
            }
        }
        Action::YankMarkdown => {
            if let Some(link) = app.yank_markdown() {
                app.status = match yank_to_clipboard(&link) {
                    Ok(()) => format!("Yanked {link}"),
                    Err(e) => format!("Yank failed: {e}"),
                };
            } else {
                app.status = "Open an article first".to_string();
            }
        }
        Action::ReadLater => enqueue_read_later(client, cache, app).await,
        Action::Research => {
            if app.active_tab().doc.is_some() {
                app.mode = Mode::Research;
            } else {
                app.status = "Open an article first".to_string();
            }
        }
        Action::Library => app.open_library(),
        Action::BookmarkToggle => app.toggle_bookmark(),
        Action::BookmarkPicker => app.open_bookmark_picker(),
        Action::Annotate => annotate_current_article(terminal, app),
        Action::FindInPage => {
            app.mode = Mode::Find;
            app.clear_find();
        }
        Action::FindNext => {
            if app.active_tab().find_matches.is_empty() {
                app.status = "No active search — Ctrl-f to find in this page".to_string();
            } else {
                app.find_next();
            }
        }
        Action::FindPrev => {
            if app.active_tab().find_matches.is_empty() {
                app.status = "No active search — Ctrl-f to find in this page".to_string();
            } else {
                app.find_prev();
            }
        }
        Action::SaveOffline => start_current_save(client, cache, app, Tier::T0, save_tx),
        Action::RandomArticle => {
            open_random_article(client, cache, app, revalidate_tx, langlinks_tx).await
        }
        Action::RelatedPanel => open_related(app, client, related_tx),
        Action::LangPicker => open_lang_picker(app, client, langlinks_tx),
        // PRD FR-ML-4: same picker `:wiki` (bare) opens.
        Action::WikiPicker => app.open_wiki_picker(),
        Action::Home => app.go_home(),
        Action::Today => fetch_on_this_day(client, app).await,
        Action::Info => {
            app.open_info();
        }
        // PRD FR-ACC-1 / §5.9: the palette/keybind entry point mirrors bare
        // `:login` (the loopback flow).
        Action::Login => cmd_login_loopback(terminal, client, app).await,
        // PRD FR-ACC-9.
        Action::Logout => cmd_logout(app),
        // PRD FR-ACC-2.
        Action::WatchToggle => cmd_watch_toggle(client, app).await,
        Action::WatchlistOpen => open_watchlist(client, app).await,
        // PRD FR-ACC-3.
        Action::NotificationsOpen => open_notifications(client, app).await,
        // PRD FR-ACC-4: the palette/keybind entry always shows the
        // logged-in user's own contributions — `:contribs <username>` (a
        // parsed argument, not reachable from a bare Action) is the only way
        // to view someone else's.
        Action::ContribsOpen => open_contribs(client, app, None).await,
        // PRD FR-ACC-7.
        Action::PrefsOpen => open_prefs(client, app).await,
        Action::Quit => {
            app.pending_quit_confirm = true;
            app.notice = Some("really quit? (y/n)".to_string());
        }
        // Picker-generic actions are handled inline by each picker's own arm;
        // they are never routed here (not palette-exposed, and the override
        // layer is scoped to the reading view).
        Action::MoveDown | Action::MoveUp | Action::Select | Action::Close => {}
    }
}

/// PRD SEC-4: the token store — the OS keychain (feature-gated) with the 0600
/// file fallback. One helper so startup, `:login`, and `:logout` agree on
/// where tokens live.
fn build_token_store() -> Option<Box<dyn auth::TokenStore>> {
    auth::default_store(auth::auth_path())
}

/// Loads a persisted logged-in session (PRD FR-ACC-1) from the token store,
/// wrapping it in an `AuthState` ready to refresh on demand. `None` when
/// logged out, no store resolves, or the stored blob is unreadable (a corrupt
/// `auth.json` degrades to logged-out, not a crash).
fn load_auth_state(runtime: &app::AuthRuntime) -> Option<auth::AuthState> {
    let store = build_token_store()?;
    let tokens = store.load().ok().flatten()?;
    let http = auth::token_http_client(&runtime.contact).ok()?;
    Some(auth::AuthState::new(
        tokens,
        runtime.client_id.clone(),
        runtime.token_url.clone(),
        http,
        store,
    ))
}

/// PRD §5.9 / FR-ACC-1: the loopback OAuth login. Generates PKCE + a CSRF
/// `state`, binds a short-lived `127.0.0.1:<port>/callback` listener, opens
/// the browser to the authorization URL (best-effort), waits for the
/// redirect, then completes the exchange. Blocking (bounded 180 s) by design:
/// login is an intentional "wait for the browser" step. The URL is *drawn*
/// before the wait so a headless reader can copy it (or `:login paste`
/// instead).
async fn cmd_login_loopback(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &WikiClient,
    app: &mut App,
) {
    if !app.auth_runtime.is_configured() {
        app.notice = Some(
            "Login unavailable: register an OAuth consumer and set [auth] client_id in config.toml"
                .to_string(),
        );
        return;
    }
    if let Some(existing) = app.auth.as_ref() {
        // Already logged in: re-verify the session (the authed action that
        // transparently refreshes a near-expired token, PRD §5.9).
        let name = existing.username().to_string();
        app.notice = Some(format!("Already logged in as {name} — checking session…"));
        reverify_session(client, app).await;
        return;
    }

    let server = match auth::LoopbackServer::bind() {
        Ok(s) => s,
        Err(e) => {
            app.notice = Some(format!("Login failed: {e}"));
            return;
        }
    };
    let port = server.port();
    let redirect_uri = server.redirect_uri();
    let pkce = auth::Pkce::generate();
    let state = auth::random_state();
    let url = auth::build_authorize_url(
        &app.auth_runtime.authorize_url,
        &app.auth_runtime.client_id,
        &pkce.challenge,
        &state,
        &redirect_uri,
    );
    let opened = auth::open_browser(&url);
    app.status = format!("Waiting for browser authorization on 127.0.0.1:{port} …");
    app.notice = Some(if opened {
        format!("Opened your browser to authorize. If nothing opened, visit: {url}")
    } else {
        format!("Open this URL to authorize (loopback capture is running): {url}")
    });
    // Paint the URL before blocking on the redirect.
    let _ = terminal.draw(|f| ui::draw(f, app));

    let wait =
        tokio::task::spawn_blocking(move || server.accept_one(Duration::from_secs(180))).await;
    let callback = match wait {
        Ok(Ok(cb)) => cb,
        Ok(Err(e)) => {
            app.status = String::new();
            app.notice = Some(format!("Login canceled: {e}"));
            return;
        }
        Err(e) => {
            app.notice = Some(format!("Login failed: {e}"));
            return;
        }
    };
    if let Err(e) = callback.verify_state(&state) {
        app.status = String::new();
        app.notice = Some(format!("Login rejected: {e}"));
        return;
    }
    finish_login(client, app, &pkce.verifier, &callback.code, &redirect_uri).await;
}

/// PRD §5.9's manual code-paste fallback setup: builds the authorization URL
/// (advertising a loopback redirect the reader copies the code out of — there
/// is no documented oob for OAuth 2.0, SP-2), opens the browser best-effort,
/// and enters the paste prompt ([`Mode::Login`]). The exchange runs when the
/// reader submits the pasted code (`submit_login_paste`).
fn cmd_login_paste(app: &mut App) {
    if !app.auth_runtime.is_configured() {
        app.notice = Some(
            "Login unavailable: register an OAuth consumer and set [auth] client_id in config.toml"
                .to_string(),
        );
        return;
    }
    if let Some(existing) = app.auth.as_ref() {
        app.notice = Some(format!("Already logged in as {}", existing.username()));
        return;
    }
    let redirect_uri = "http://127.0.0.1/callback".to_string();
    let pkce = auth::Pkce::generate();
    let state = auth::random_state();
    let url = auth::build_authorize_url(
        &app.auth_runtime.authorize_url,
        &app.auth_runtime.client_id,
        &pkce.challenge,
        &state,
        &redirect_uri,
    );
    let _ = auth::open_browser(&url);
    app.pending_login = Some(app::PendingLogin {
        verifier: pkce.verifier,
        state,
        redirect_uri,
        authorize_url: url.clone(),
    });
    app.login_input.clear();
    app.mode = Mode::Login;
    app.status =
        format!("Authorize in your browser, then paste the code or full redirect URL: {url}");
}

/// Completes the manual-paste login: parses the pasted code/URL, runs the
/// CSRF `state` check when the paste carried one, and exchanges the code.
async fn submit_login_paste(client: &WikiClient, app: &mut App) {
    let Some(pending) = app.pending_login.clone() else {
        app.mode = Mode::Reading;
        return;
    };
    let input = app.login_input.clone();
    let callback = match auth::parse_manual_input(&input) {
        Ok(cb) => cb,
        Err(e) => {
            app.notice = Some(format!("Paste rejected: {e}"));
            return;
        }
    };
    // A pasted full redirect URL carries its own `state` — verify it (CSRF);
    // a bare hand-copied code has none, which is accepted (the reader vouches
    // for it) per §5.9's manual fallback.
    if callback.state.is_some()
        && let Err(e) = callback.verify_state(&pending.state)
    {
        app.notice = Some(format!("Paste rejected: {e}"));
        return;
    }
    finish_login(
        client,
        app,
        &pending.verifier,
        &callback.code,
        &pending.redirect_uri,
    )
    .await;
}

/// Completes an OAuth login once an authorization `code` is in hand (from the
/// loopback capture or a manual paste): exchanges the code for tokens (the
/// PKCE `verifier` proves the client), fetches the username via authenticated
/// `meta=userinfo`, stores the tokens (SEC-4), and installs the session.
async fn finish_login(
    client: &WikiClient,
    app: &mut App,
    verifier: &str,
    code: &str,
    redirect_uri: &str,
) {
    let rt = app.auth_runtime.clone();
    let http = match auth::token_http_client(&rt.contact) {
        Ok(h) => h,
        Err(e) => {
            app.notice = Some(format!("Login failed: {e}"));
            return;
        }
    };
    let resp = match auth::exchange_code(
        &http,
        &rt.token_url,
        &rt.client_id,
        code,
        verifier,
        redirect_uri,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            app.notice = Some(format!("Login failed: {e}"));
            return;
        }
    };
    let username = match client.fetch_userinfo(&app.lang, &resp.access_token).await {
        Ok(name) => name,
        Err(e) => {
            app.notice = Some(format!("Login failed: could not confirm account: {e}"));
            return;
        }
    };
    let now = chrono::Utc::now().timestamp();
    let tokens = match auth::Tokens::from_response(&resp, username.clone(), now, None) {
        Ok(t) => t,
        Err(e) => {
            app.notice = Some(format!("Login failed: {e}"));
            return;
        }
    };
    let Some(store) = build_token_store() else {
        app.notice =
            Some("Login failed: no token store available (no keychain, no state dir)".to_string());
        return;
    };
    let file_fallback = store.is_file_fallback();
    let store_desc = store.describe();
    if let Err(e) = store.save(&tokens) {
        app.notice = Some(format!("Login failed: could not store tokens: {e}"));
        return;
    }
    app.auth = Some(auth::AuthState::new(
        tokens,
        rt.client_id.clone(),
        rt.token_url.clone(),
        http,
        store,
    ));
    app.pending_login = None;
    app.login_input.clear();
    app.mode = Mode::Reading;
    app.status = String::new();
    // SEC-4: name the storage honestly — a file fallback is less safe than a
    // keychain and the reader should know.
    app.notice = Some(if file_fallback {
        format!("Logged in as {username}  (tokens stored in {store_desc}, not an OS keychain)")
    } else {
        format!("Logged in as {username}  (tokens stored in {store_desc})")
    });
    // PRD FR-ACC-3's login/startup poll (see `account.rs`'s poll-cadence
    // doc): the unread badge shows up right away, without waiting for the
    // reader to open `:notifications` first.
    poll_notifications_count(client, app).await;
}

/// PRD §5.9's transparent refresh in action: re-fetches `meta=userinfo`
/// through `valid_access_token`, which refreshes a near-expired token first
/// (hitting the token endpoint's refresh grant) and persists the new tokens.
/// A refresh failure logs the reader out gracefully (§7 "Login: OAuth
/// failure/expiry").
async fn reverify_session(client: &WikiClient, app: &mut App) {
    let now = chrono::Utc::now().timestamp();
    let Some(auth_state) = app.auth.as_mut() else {
        return;
    };
    let token = match auth_state.valid_access_token(now).await {
        Ok(t) => t,
        Err(e) => {
            // The refresh failed (revoked/expired refresh token): drop the
            // dead session and delete the local tokens.
            let _ = auth_state.logout();
            app.auth = None;
            app.notice = Some(format!("Session expired — logged out ({e})"));
            return;
        }
    };
    match client.fetch_userinfo(&app.lang, &token).await {
        Ok(name) => app.notice = Some(format!("Logged in as {name}")),
        Err(e) => app.notice = Some(format!("Session check failed: {e}")),
    }
}

/// PRD FR-ACC-9 / FR-PR-4: local logout — deletes the stored tokens and drops
/// the session, then points the reader at `Special:OAuthManageMyGrants` for
/// server-side revocation (this public client holds no secret to revoke with
/// itself).
fn cmd_logout(app: &mut App) {
    match app.auth.take() {
        Some(auth_state) => {
            let who = auth_state.username().to_string();
            let deleted = auth_state.logout();
            app.mode = Mode::Reading;
            // PRD §6.2 rule 8: a session's cached csrf/watch tokens are
            // meaningless once it ends — a future login gets its own.
            app.tokens.clear();
            app.notif_counts = account::NotifCounts::default();
            let base = match deleted {
                Ok(()) => format!(
                    "Logged out {who}. Revoke server-side at {}",
                    auth::MANAGE_GRANTS_URL
                ),
                Err(e) => format!(
                    "Logged out {who} (local token delete warning: {e}). Revoke server-side at {}",
                    auth::MANAGE_GRANTS_URL
                ),
            };
            app.notice = Some(base);
        }
        None => app.notice = Some("Not logged in".to_string()),
    }
}

// ---------------------------------------------------------------------------
// PRD FR-ACC-2/3/4/6/7: watchlist, notifications, contributions, thank, prefs
// ---------------------------------------------------------------------------

/// The shared login gate every feature in this section routes through
/// (PRD's "degrade to a friendly login prompt, never an error, and never
/// make an authed request" contract). Returns `None` — having made no
/// request at all — when logged out; otherwise a valid (transparently
/// refreshed) access token. A refresh failure logs the reader out, mirroring
/// `reverify_session`'s own handling of the same failure mode.
async fn require_login(app: &mut App, feature_prompt: &str) -> Option<String> {
    let now = chrono::Utc::now().timestamp();
    let Some(auth_state) = app.auth.as_mut() else {
        app.notice = Some(format!("Log in to {feature_prompt} (:login)"));
        return None;
    };
    match auth_state.valid_access_token(now).await {
        Ok(token) => Some(token),
        Err(e) => {
            let _ = auth_state.logout();
            app.auth = None;
            app.notice = Some(format!("Session expired — logged out ({e})"));
            None
        }
    }
}

/// PRD §6.2 rule 8's badtoken retry, for `action=watch`: fetches (or reuses)
/// the cached watch token, attempts the write, and — only if that response
/// is a `badtoken` error — invalidates the cache, fetches one fresh token,
/// and retries exactly once more. `access_token` is the OAuth Bearer token
/// (already resolved by `require_login`); `write_token` is the csrf/watch
/// token this one action needs.
async fn watch_with_retry(
    app: &mut App,
    client: &WikiClient,
    lang: &str,
    access_token: &str,
    title: &str,
    unwatch: bool,
) -> Result<Vec<u8>> {
    let write_token = app.tokens.watch_token(client, lang, access_token).await?;
    let body = client
        .watch_raw(lang, access_token, title, &write_token, unwatch)
        .await?;
    if !account::is_badtoken_response(&body) {
        return Ok(body);
    }
    app.tokens.invalidate_watch();
    let fresh = app.tokens.watch_token(client, lang, access_token).await?;
    client
        .watch_raw(lang, access_token, title, &fresh, unwatch)
        .await
}

/// PRD FR-ACC-2's `:watchlist` / `gW`: fetches the raw watched-pages list and
/// the "since last seen" activity feed, then advances the persisted
/// last-seen timestamp to the newest change just shown (so the *next* open
/// only shows what's new since this one). Logged-out shows the login prompt
/// and makes no request at all.
async fn open_watchlist(client: &WikiClient, app: &mut App) {
    let Some(token) = require_login(app, "view your watchlist").await else {
        return;
    };
    app.enter_watchlist();
    let lang = app.lang.clone();
    let raw = match client.fetch_watchlistraw(&lang, &token).await {
        Ok(body) => account::parse_watchlistraw(&body).unwrap_or_default(),
        Err(e) => {
            app.status = format!("Couldn't load the watchlist: {e}");
            Vec::new()
        }
    };
    let changes = match client.fetch_watchlist_changes(&lang, &token, 50).await {
        Ok(body) => account::parse_watchlist_changes(&body).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let since: Vec<account::WatchlistChange> =
        account::changes_since(&changes, app.watchlist_last_seen.as_deref())
            .into_iter()
            .cloned()
            .collect();
    // Advance the cursor to the newest change in THIS fetch (not just the
    // "since" subset) — a page with no new changes today must not make
    // tomorrow's open re-show today's changes it never had.
    if let Some(newest) = account::newest_timestamp(&changes) {
        let newest = newest.to_string();
        if let Some(path) = &app.watchlist_state_path {
            let _ = account::save_last_seen(path, &newest);
        }
        app.watchlist_last_seen = Some(newest);
    }
    app.watchlist_raw = raw;
    app.watchlist_changes = since;
    app.status = "j/k: move  Tab/h/l: switch  Enter: open  Esc: close".to_string();
}

/// PRD FR-ACC-2's `w`: toggles watch/unwatch on the article currently on
/// screen. Reads the server's own current-watched status first (never a
/// stale local guess) to decide which way to toggle, then writes through
/// `watch_with_retry`.
async fn cmd_watch_toggle(client: &WikiClient, app: &mut App) {
    let Some(access_token) = require_login(app, "watch articles").await else {
        return;
    };
    let Some(doc) = app.active_tab().doc.as_ref() else {
        app.status = "Open an article first".to_string();
        return;
    };
    let title = doc.title.clone();
    let lang = app.active_tab().lang.clone();
    let currently_watched = client
        .fetch_watched_status(&lang, &access_token, &title)
        .await
        .unwrap_or(false);
    let result = watch_with_retry(
        app,
        client,
        &lang,
        &access_token,
        &title,
        currently_watched, // toggle: unwatch iff it's currently watched
    )
    .await;
    app.notice = Some(match result {
        Ok(body) => match account::parse_watch_outcome(&body) {
            Some(account::WatchOutcome::Watched) => format!("Watching {title}"),
            Some(account::WatchOutcome::Unwatched) => format!("Unwatched {title}"),
            None => format!("Watch toggle failed for {title}"),
        },
        Err(e) => format!("Watch toggle failed: {e}"),
    });
}

/// PRD FR-ACC-3's `:notifications`: fetches the unread-count badge and the
/// full alerts/messages list. Logged-out shows the login prompt and makes no
/// request.
async fn open_notifications(client: &WikiClient, app: &mut App) {
    let Some(token) = require_login(app, "view your notifications").await else {
        return;
    };
    app.enter_notifications();
    let lang = app.lang.clone();
    // Per `account.rs`'s poll-cadence doc: the count is polled again here
    // (opening the pane), on top of the login/startup poll — never on a
    // timer.
    if let Ok(body) = client.fetch_notifications_count(&lang, &token).await
        && let Ok(counts) = account::parse_notif_count(&body)
    {
        app.notif_counts = counts;
    }
    match client.fetch_notifications_list(&lang, &token).await {
        Ok(body) => {
            let list = account::parse_notif_list(&body).unwrap_or_default();
            app.notif_alerts = list
                .iter()
                .filter(|n| n.kind == account::NotifKind::Alert)
                .cloned()
                .collect();
            app.notif_messages = list
                .iter()
                .filter(|n| n.kind == account::NotifKind::Message)
                .cloned()
                .collect();
            app.status = "j/k: move  Tab/h/l: switch  d: mark read  A: mark all read  Esc: close"
                .to_string();
        }
        Err(e) => app.status = format!("Couldn't load notifications: {e}"),
    }
}

/// PRD §6.2 rule 8's badtoken retry for `action=echomarkread`: fetches (or
/// reuses) the cached csrf token, attempts the mark-read, and retries once
/// more on a `badtoken` response — the csrf-token counterpart of
/// `watch_with_retry`. `all` marks every notification read; otherwise `ids`
/// (non-empty) marks just those.
async fn echomarkread_with_retry(
    app: &mut App,
    client: &WikiClient,
    lang: &str,
    access_token: &str,
    all: bool,
    ids: &[String],
) -> Result<Vec<u8>> {
    let write_token = app.tokens.csrf_token(client, lang, access_token).await?;
    let body = client
        .echomarkread_raw(lang, access_token, &write_token, all, ids)
        .await?;
    if !account::is_badtoken_response(&body) {
        return Ok(body);
    }
    app.tokens.invalidate_csrf();
    let fresh = app.tokens.csrf_token(client, lang, access_token).await?;
    client
        .echomarkread_raw(lang, access_token, &fresh, all, ids)
        .await
}

/// PRD FR-ACC-3's `d`: marks the focused notification read, then updates the
/// badge locally from the in-memory lists (no extra network round trip).
async fn mark_notification_read(client: &WikiClient, app: &mut App) {
    let Some(id) = app.notif_focused_id() else {
        app.status = "Nothing to mark read".to_string();
        return;
    };
    let Some(access_token) = require_login(app, "manage your notifications").await else {
        return;
    };
    let lang = app.lang.clone();
    let ids = [id.clone()];
    let result = echomarkread_with_retry(app, client, &lang, &access_token, false, &ids).await;
    match result {
        Ok(_) => {
            app.mark_notif_read_locally(&id);
            app.notice = Some("Marked read".to_string());
        }
        Err(e) => app.notice = Some(format!("Mark-read failed: {e}")),
    }
}

/// PRD FR-ACC-3's `A`: marks every notification read.
async fn mark_all_notifications_read(client: &WikiClient, app: &mut App) {
    let Some(access_token) = require_login(app, "manage your notifications").await else {
        return;
    };
    let lang = app.lang.clone();
    let result = echomarkread_with_retry(app, client, &lang, &access_token, true, &[]).await;
    match result {
        Ok(_) => {
            app.mark_all_notifs_read_locally();
            app.notice = Some("Marked all read".to_string());
        }
        Err(e) => app.notice = Some(format!("Mark-all-read failed: {e}")),
    }
}

/// PRD FR-ACC-4's `:contribs [username]`: `username` defaults to the
/// logged-in user (showing the login prompt, and making no request, if
/// that's requested but no session exists); an explicit username works
/// logged out too, since `usercontribs` is public.
async fn open_contribs(client: &WikiClient, app: &mut App, username: Option<String>) {
    let resolved = match username {
        Some(name) => name,
        None => match app.logged_in_username() {
            Some(name) => name.to_string(),
            None => {
                app.notice = Some(
                    "Log in to view your contributions, or specify one: :contribs <username>"
                        .to_string(),
                );
                return;
            }
        },
    };
    app.enter_contribs(resolved.clone());
    let lang = app.lang.clone();
    match client.fetch_usercontribs(&lang, &resolved, 50).await {
        Ok(body) => {
            app.contribs = account::parse_usercontribs(&body).unwrap_or_default();
            app.status = "j/k: move  Enter: open  t: thank  Esc: close".to_string();
        }
        Err(e) => app.status = format!("Couldn't load contributions for {resolved:?}: {e}"),
    }
}

/// PRD §6.2 rule 8's badtoken retry for `action=thank` — the csrf-token
/// counterpart of `watch_with_retry`, mirroring `echomarkread_with_retry`.
async fn thank_with_retry(
    app: &mut App,
    client: &WikiClient,
    lang: &str,
    access_token: &str,
    revid: u64,
) -> Result<Vec<u8>> {
    let write_token = app.tokens.csrf_token(client, lang, access_token).await?;
    let body = client
        .thank_raw(lang, access_token, revid, &write_token)
        .await?;
    if !account::is_badtoken_response(&body) {
        return Ok(body);
    }
    app.tokens.invalidate_csrf();
    let fresh = app.tokens.csrf_token(client, lang, access_token).await?;
    client.thank_raw(lang, access_token, revid, &fresh).await
}

/// PRD FR-ACC-6's `t` (from the contributions view): thanks the focused
/// edit's revision — purely positive, one-way, never reciprocated.
async fn cmd_thank(client: &WikiClient, app: &mut App) {
    let Some(revid) = app.contribs_focused_revid() else {
        app.status = "Nothing to thank".to_string();
        return;
    };
    let Some(access_token) = require_login(app, "thank an editor").await else {
        return;
    };
    let lang = app.lang.clone();
    let result = thank_with_retry(app, client, &lang, &access_token, revid).await;
    app.notice = Some(match result {
        Ok(body) if account::thank_succeeded(&body) => "Thanked".to_string(),
        Ok(_) => "Thank failed".to_string(),
        Err(e) => format!("Thank failed: {e}"),
    });
}

/// PRD FR-ACC-7's `:prefs`: the read-only preferences card.
async fn open_prefs(client: &WikiClient, app: &mut App) {
    let Some(token) = require_login(app, "view your preferences").await else {
        return;
    };
    app.enter_prefs();
    let lang = app.lang.clone();
    match client.fetch_userinfo_options(&lang, &token).await {
        Ok(body) => match account::parse_userinfo_options(&body) {
            Ok(prefs) => {
                app.prefs = Some(prefs);
                app.status = "Esc: close".to_string();
            }
            Err(e) => app.status = format!("Couldn't parse preferences: {e}"),
        },
        Err(e) => app.status = format!("Couldn't load preferences: {e}"),
    }
}

// ---------------------------------------------------------------------------
// PRD FR-BM-5/6: Reading List sync and the watchlist mirror
// ---------------------------------------------------------------------------
//
// Two independent, login-gated, opt-in operations against two distinct
// backends (`account.rs`'s module doc for each has the full model + every
// SP-7 assumption). `:sync` runs both, one after the other, and reports each
// half on its own — never conflated into a single count, per FR-BM-6's own
// "separate from bookmarks" wording. `:mirror-watchlist` runs only the
// watch-mirror half, for a reader who wants that applied right after tagging
// a bookmark without touching Reading List sync at all.

/// PRD FR-BM-5 / SP-7's assumed "needs setup" retry: calls `command=list`;
/// if the response is the not-set-up error this build's mock/reading
/// documents (`account::READINGLISTS_NOT_SET_UP`), runs `command=setup`
/// once and retries `list` exactly once more — the same "try, detect the
/// one known recoverable failure, fix it, retry exactly once" shape as
/// `watch_with_retry`'s badtoken handling (§6.2 rule 8), applied to this
/// build's own ReadingLists assumption instead of a real badtoken.
async fn fetch_readinglists_with_setup(
    app: &mut App,
    client: &WikiClient,
    lang: &str,
    access_token: &str,
) -> Result<Vec<account::ReadingListInfo>> {
    let body = client.fetch_readinglists(lang, access_token).await?;
    if !account::is_readinglists_not_set_up(&body) {
        return Ok(account::parse_readinglists(&body));
    }
    let token = app.tokens.csrf_token(client, lang, access_token).await?;
    let setup_body = client
        .readinglists_setup_raw(lang, access_token, &token)
        .await?;
    let setup_list_id = account::parse_readinglists_setup(&setup_body);
    let retried = client.fetch_readinglists(lang, access_token).await?;
    let lists = account::parse_readinglists(&retried);
    if !lists.is_empty() {
        return Ok(lists);
    }
    // Tolerant of a server that confirms setup but returns no lists on the
    // very next call (a plausible SP-7 shape surprise): fall back to the id
    // `command=setup` itself reported, if any, rather than treating this as
    // "the account has no reading lists."
    Ok(setup_list_id
        .into_iter()
        .map(|id| account::ReadingListInfo {
            id,
            name: "default".to_string(),
            default: true,
        })
        .collect())
}

/// PRD FR-BM-5's `:sync` reading-list half: two-way reconcile against the
/// account's default Reading List (see `account.rs`'s ReadingLists module
/// doc for the full conflict-policy + sync-mapping writeup this implements).
/// Returns a short human report ("Reading List: pushed N, pulled M") —
/// never aborts the rest of `:sync` on a partial failure; a push/pull/
/// delete that itself fails is simply skipped (not retried, not counted),
/// so a flaky write degrades to "fewer than expected synced this run"
/// rather than losing the whole operation.
async fn sync_reading_list(
    client: &WikiClient,
    app: &mut App,
    lang: &str,
    access_token: &str,
) -> String {
    let lists = match fetch_readinglists_with_setup(app, client, lang, access_token).await {
        Ok(lists) => lists,
        Err(e) => return format!("Reading List sync failed: {e}"),
    };
    let Some(list_id) = account::default_list_id(&lists) else {
        return "Reading List sync failed: account has no reading lists".to_string();
    };
    let entries_body = match client
        .fetch_readinglist_entries(lang, access_token, list_id)
        .await
    {
        Ok(b) => b,
        Err(e) => return format!("Reading List sync failed: {e}"),
    };
    // PRD FR-BM-5's documented cross-wiki scope-cut (`account.rs`'s module
    // doc): an entry from a different project than this session's own wiki
    // is left alone rather than guessed into some other `lang`'s bucket. An
    // entry that omits `project` entirely is tolerated (shape-surprise
    // tolerance), not rejected.
    let expected_project = client.wiki_origin(lang);
    let server_entries: Vec<account::ReadingListEntry> =
        account::parse_readinglist_entries(&entries_body)
            .into_iter()
            .filter(|e| e.project.is_empty() || e.project == expected_project)
            .collect();

    let state_path = app.readinglist_sync_state_path.clone();
    let mut state = state_path
        .as_deref()
        .map(account::load_readinglist_sync_state)
        .unwrap_or_default();

    let local_titles: Vec<String> = app
        .bookmarks
        .bookmarks
        .iter()
        .filter(|b| b.lang == lang)
        .map(|b| b.title.clone())
        .collect();

    let plan = account::reconcile_reading_list(&local_titles, &server_entries, &state.synced);

    let mut pushed = 0u32;
    let mut new_pushed: Vec<(String, u64)> = Vec::new();
    for title in &plan.push {
        let Ok(token) = app.tokens.csrf_token(client, lang, access_token).await else {
            continue;
        };
        if let Ok(body) = client
            .readinglists_createentry_raw(
                lang,
                access_token,
                list_id,
                &expected_project,
                title,
                &token,
            )
            .await
            && let Some(created) = account::parse_readinglists_createentry(&body)
        {
            new_pushed.push((title.clone(), created.id));
            pushed += 1;
        }
    }

    let mut pulled = 0u32;
    let mut new_pulled: Vec<(String, u64)> = Vec::new();
    for entry in &server_entries {
        if plan.pull.contains(&entry.title) {
            // PRD FR-BM-5 conflict policy: a pull only ever creates a
            // bookmark that didn't already exist locally (that's exactly
            // what `plan.pull` means), so there is no existing tags/note to
            // clobber — `ensure_bookmarked` starts both empty, same as `m`.
            app.bookmarks.ensure_bookmarked(lang, &entry.title, None);
            new_pulled.push((entry.title.clone(), entry.id));
            pulled += 1;
        }
    }

    // A delete that doesn't actually land must NOT drop out of the sync
    // mapping the way a *successful* one does: if it did, the next reconcile
    // would see this entry as "on the server, not local, never synced" and
    // wrongly PULL it back instead of retrying the delete — resurrecting a
    // bookmark the reader deliberately removed. `delete_failed` preserves
    // exactly those rows so they stay tracked as `was_synced` for next time.
    let mut delete_failed: Vec<(String, u64)> = Vec::new();
    for entry_id in &plan.server_delete {
        let title = server_entries
            .iter()
            .find(|e| e.id == *entry_id)
            .map(|e| e.title.clone());
        let Ok(token) = app.tokens.csrf_token(client, lang, access_token).await else {
            if let Some(title) = title {
                delete_failed.push((title, *entry_id));
            }
            continue;
        };
        let succeeded = matches!(
            client
                .readinglists_deleteentry_raw(lang, access_token, *entry_id, &token)
                .await,
            Ok(body) if account::readinglists_deleteentry_succeeded(&body)
        );
        if !succeeded && let Some(title) = title {
            delete_failed.push((title, *entry_id));
        }
    }

    // PRD FR-BM-5 "server wins on order": reorder local bookmarks for this
    // lang to match the server's own entry order — this round's fresh
    // pushes (not yet re-listed) keep their prior relative order, appended
    // last (see `account::apply_server_order`'s doc comment).
    let server_order: Vec<String> = server_entries.iter().map(|e| e.title.clone()).collect();
    let local_titles_after: Vec<String> = app
        .bookmarks
        .bookmarks
        .iter()
        .filter(|b| b.lang == lang)
        .map(|b| b.title.clone())
        .collect();
    let new_order = account::apply_server_order(&server_order, &local_titles_after);
    let _ = app.bookmarks.reorder(lang, &new_order);

    // Recompute the sync-mapping: everything matched (untouched this round),
    // freshly pushed, and freshly pulled — an entry that was deleted
    // server-side (or vanished from both sides between runs) isn't in any
    // of these three and drops out of the map here, exactly the garbage
    // collection the id-map model needs (`account.rs`'s module doc).
    let mut new_synced: Vec<account::SyncedEntry> = plan
        .matched
        .iter()
        .map(|(title, id)| account::SyncedEntry {
            lang: lang.to_string(),
            title: title.clone(),
            entry_id: *id,
        })
        .chain(new_pushed.iter().map(|(title, id)| account::SyncedEntry {
            lang: lang.to_string(),
            title: title.clone(),
            entry_id: *id,
        }))
        .chain(new_pulled.iter().map(|(title, id)| account::SyncedEntry {
            lang: lang.to_string(),
            title: title.clone(),
            entry_id: *id,
        }))
        .chain(
            delete_failed
                .iter()
                .map(|(title, id)| account::SyncedEntry {
                    lang: lang.to_string(),
                    title: title.clone(),
                    entry_id: *id,
                }),
        )
        .collect();
    // A different lang's mapping rows are untouched — this build syncs one
    // wiki per `:sync` (multi-wiki Reading List sync is a documented seam).
    let mut kept: Vec<account::SyncedEntry> = state
        .synced
        .iter()
        .filter(|s| s.lang != lang)
        .cloned()
        .collect();
    kept.append(&mut new_synced);
    state.synced = kept;
    state.list_id = Some(list_id);
    if let Some(path) = &state_path {
        let _ = account::save_readinglist_sync_state(path, &state);
    }

    format!("Reading List: pushed {pushed}, pulled {pulled}")
}

/// PRD FR-BM-6's watch-mirror application (shared by `:sync` and
/// `:mirror-watchlist`): diffs the bookmarks currently carrying `app.
/// watchlist_mirror_tag` against what this mirror itself watched last time
/// (persisted state — never the live watchlist; `account::watch_mirror_
/// diff`'s doc comment explains why), then applies the two batched
/// `action=watch` calls this needs. Only a title *confirmed* watched/
/// unwatched this round updates the persisted "mirrored" set — one whose
/// write failed stays exactly where it was, so the next run retries it
/// instead of silently forgetting it needed action.
async fn mirror_watchlist(
    client: &WikiClient,
    app: &mut App,
    lang: &str,
    access_token: &str,
) -> String {
    let mirror_tag = app.watchlist_mirror_tag.clone();
    let tagged_titles: Vec<String> = app
        .bookmarks
        .bookmarks
        .iter()
        .filter(|b| b.lang == lang && b.tags.iter().any(|t| t.eq_ignore_ascii_case(&mirror_tag)))
        .map(|b| b.title.clone())
        .collect();

    let state_path = app.watch_mirror_state_path.clone();
    let mut state = state_path
        .as_deref()
        .map(account::load_watch_mirror_state)
        .unwrap_or_default();

    let plan = account::watch_mirror_diff(&tagged_titles, &state.mirrored);

    let mut confirmed_watched: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !plan.to_watch.is_empty()
        && let Ok(token) = app.tokens.watch_token(client, lang, access_token).await
        && let Ok(body) = client
            .watch_batch_raw(lang, access_token, &plan.to_watch, &token, false)
            .await
    {
        confirmed_watched = account::parse_watch_batch_outcome(&body)
            .into_iter()
            .filter(|(_, o)| *o == account::WatchOutcome::Watched)
            .map(|(t, _)| t)
            .collect();
    }
    let mut confirmed_unwatched: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if !plan.to_unwatch.is_empty()
        && let Ok(token) = app.tokens.watch_token(client, lang, access_token).await
        && let Ok(body) = client
            .watch_batch_raw(lang, access_token, &plan.to_unwatch, &token, true)
            .await
    {
        confirmed_unwatched = account::parse_watch_batch_outcome(&body)
            .into_iter()
            .filter(|(_, o)| *o == account::WatchOutcome::Unwatched)
            .map(|(t, _)| t)
            .collect();
    }

    let unwatched_count = confirmed_unwatched.len();
    let previously: std::collections::HashSet<String> = state.mirrored.iter().cloned().collect();
    state.mirrored = tagged_titles
        .iter()
        .filter(|t| previously.contains(t.as_str()) || confirmed_watched.contains(t.as_str()))
        .cloned()
        .collect();
    if let Some(path) = &state_path {
        let _ = account::save_watch_mirror_state(path, &state);
    }

    format!(
        "Watch mirror: {} watched, {unwatched_count} unwatched",
        confirmed_watched.len()
    )
}

/// PRD FR-BM-5/6's `:sync`: runs the Reading List two-way reconcile, then
/// applies the watchlist mirror — two distinct backends, reported on
/// separate clauses of one notice line, never conflated (per FR-BM-6's own
/// "separate from bookmarks" wording).
async fn cmd_sync(client: &WikiClient, app: &mut App) {
    let Some(access_token) = require_login(app, "sync your reading list").await else {
        return;
    };
    let lang = app.lang.clone();
    let rl_report = sync_reading_list(client, app, &lang, &access_token).await;
    let wm_report = mirror_watchlist(client, app, &lang, &access_token).await;
    app.notice = Some(format!("{rl_report}  ·  {wm_report}"));
}

/// PRD FR-BM-6's `:mirror-watchlist`: applies just the watch-mirror half of
/// `:sync`, without touching Reading List sync at all.
async fn cmd_mirror_watchlist(client: &WikiClient, app: &mut App) {
    let Some(access_token) = require_login(app, "mirror your watchlist").await else {
        return;
    };
    let lang = app.lang.clone();
    let report = mirror_watchlist(client, app, &lang, &access_token).await;
    app.notice = Some(report);
}

/// PRD FR-ACC-3's login/startup poll (see `account.rs`'s poll-cadence doc):
/// fetches the unread-count badge once, right after a session is
/// established. Best-effort — a failure here just leaves the badge absent
/// until the reader opens `:notifications`, never a login-blocking error.
async fn poll_notifications_count(client: &WikiClient, app: &mut App) {
    let Some(auth_state) = app.auth.as_mut() else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let Ok(token) = auth_state.valid_access_token(now).await else {
        return;
    };
    let lang = app.lang.clone();
    if let Ok(body) = client.fetch_notifications_count(&lang, &token).await
        && let Ok(counts) = account::parse_notif_count(&body)
    {
        app.notif_counts = counts;
    }
}

/// Executes a parsed `:` command. Parsing already validated arguments
/// (theme/style/lang names), so the arms here mostly delegate to existing
/// features.
#[allow(clippy::too_many_arguments)]
async fn execute_command(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    cmd: command::Command,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    save_tx: &UnboundedSender<SaveOutcome>,
    related_tx: &UnboundedSender<RelatedOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    use command::{Command, LoginMode, RandomSpec, SaveSpec};
    match cmd {
        Command::Open(raw) => {
            // Same grammar as the CLI TITLE argument: URLs, lang-prefixed
            // titles, and now sister-project URLs/interwiki prefixes
            // (PRD FR-ML-4) all work here too.
            let target = target::parse(&raw);
            if let Some(project) = target.project {
                switch_wiki(client, app, &project);
            }
            if let Some(lang) = target.lang {
                app.lang = lang;
            }
            open_title(
                client,
                cache,
                app,
                &target.title,
                revalidate_tx,
                langlinks_tx,
            )
            .await;
        }
        Command::Theme(name) => {
            if let Some(theme) = theme::resolve_named(&name, &app.user_themes) {
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
            // PRD FR-TB-4: sync-scroll a split's two panes in lockstep.
            "scrollbind" => app.set_scrollbind(value == "on"),
            // PRD FR-TH-2: live theme switch (value already validated by the
            // parser, so `by_name` cannot fail here).
            "theme" => {
                if let Some(theme) = theme::resolve_named(&value, &app.user_themes) {
                    app.set_theme(theme);
                    app.notice = Some(format!("theme={value}"));
                }
            }
            // PRD FR-RD-9 / FR-PC-4: re-measure and re-center. Dropping the
            // cached layout forces `ensure_layout` to recompute at the new
            // width (measure feeds layout, not just paint) — same seam
            // `apply_config_reload` uses.
            "measure" => {
                if let Ok(n) = value.parse::<u16>() {
                    app.measure = n;
                    app.layout = None;
                    app.notice = Some(format!("measure={n}"));
                }
            }
            // PRD FR-RD-10: East-Asian-Ambiguous width feeds layout too.
            "ambiguous_width" => {
                app.ambiguous_wide = value == "2";
                app.layout = None;
                app.notice = Some(format!("ambiguous_width={value}"));
            }
            // PRD FR-RD-11: the WPM divisor feeds the layout's header line
            // (`layout_options`), so it needs the same relayout seam.
            "reading_wpm" => {
                if let Ok(n) = value.parse::<u32>() {
                    app.reading_wpm = n;
                    app.layout = None;
                    app.notice = Some(format!("reading_wpm={n}"));
                }
            }
            // PRD FR-NV-9: toggles both the app-side flag (which every
            // mouse-handling call site reads) and the real terminal mouse-
            // capture state, so the two never drift apart — see
            // `App::set_mouse`'s own doc comment. Best-effort: a terminal
            // that rejects the escape just never delivers `Event::Mouse`,
            // same as `mouse=off`.
            "mouse" => {
                let on = value == "on";
                let _ = crashguard::set_mouse_capture(on);
                app.set_mouse(on);
            }
            // PRD FR-ACS-4: no-motion mode (value already validated by the
            // parser to be `full`/`none`).
            "animations" => app.set_no_motion(value == "none"),
            // PRD FR-RD-2 / SEC-2: OSC 8 hyperlink emission mode (value
            // already validated by the parser).
            "hyperlinks" => {
                if let Some(mode) = HyperlinkMode::parse(&value) {
                    app.set_hyperlinks_mode(mode);
                }
            }
            // PRD FR-PC-1: the honest "spacing options", session-global.
            // Every one of these feeds `layout_options`, so each needs the
            // same relayout seam `measure`/`ambiguous_width` use above.
            "text_align" => {
                if let Some(align) = layout::TextAlign::parse(&value) {
                    app.text_align = align;
                    app.layout = None;
                    app.notice = Some(format!("text_align={value}"));
                }
            }
            "margin" => {
                if let Ok(n) = value.parse::<u16>() {
                    app.margin = n;
                    app.layout = None;
                    app.notice = Some(format!("margin={n}"));
                }
            }
            "paragraph_spacing" => {
                if let Ok(n) = value.parse::<u8>() {
                    app.paragraph_spacing = n;
                    app.layout = None;
                    app.notice = Some(format!("paragraph_spacing={n}"));
                }
            }
            "line_spacing" => {
                if let Ok(n) = value.parse::<u8>() {
                    app.line_spacing = n;
                    app.layout = None;
                    app.notice = Some(format!("line_spacing={n}"));
                }
            }
            "word_spacing" => {
                if let Ok(n) = value.parse::<u8>() {
                    app.word_spacing = n;
                    app.layout = None;
                    app.notice = Some(format!("word_spacing={n}"));
                }
            }
            other => {
                app.notice = Some(format!(
                    "unknown :set key {other:?} (try: theme, images, prefetch, scrollbind, measure, ambiguous_width, reading_wpm, mouse, animations, hyperlinks, text_align, margin, paragraph_spacing, line_spacing, word_spacing)"
                ));
            }
        },
        // PRD FR-PC-4: `:set-tab` — same keys `TAB_SCOPED_KEYS` allows,
        // scoped to the active tab only (`App::set_tab_override`).
        Command::SetTab { key, value } => app.set_tab_override(&key, value.as_deref()),
        Command::PrefetchLog => app.open_prefetch_log(),
        Command::Interests => app.open_interests(),
        Command::NotInterested => app.mark_not_interested(),
        Command::Stats => app.open_stats(),
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
                if let Some(project) = target.project {
                    switch_wiki(client, app, &project);
                }
                if let Some(lang) = target.lang {
                    app.lang = lang;
                }
                open_title(
                    client,
                    cache,
                    app,
                    &target.title,
                    revalidate_tx,
                    langlinks_tx,
                )
                .await;
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
            open_random_article(client, cache, app, revalidate_tx, langlinks_tx).await
        }
        Command::Random(RandomSpec::Good) => {
            open_random_good_article(client, cache, app, revalidate_tx, langlinks_tx).await
        }
        // PRD FR-SR-6: same action as the `gR` keybinding.
        Command::Related => open_related(app, client, related_tx),
        // PRD FR-ACC-5: same action as the `T` keybinding.
        Command::Talk => toggle_talk_page(client, cache, app, revalidate_tx, langlinks_tx).await,
        // PRD §10 / Appendix B: same action as the `i` keybinding.
        Command::Info => {
            app.open_info();
        }
        // PRD FR-ML-1/2.
        Command::Lang(None) => open_lang_picker(app, client, langlinks_tx),
        Command::Lang(Some(code)) => {
            set_or_switch_lang(client, cache, app, code, revalidate_tx, langlinks_tx).await
        }
        // PRD FR-ML-4/5.
        Command::Wiki(None) => app.open_wiki_picker(),
        Command::Wiki(Some(name)) => {
            switch_wiki(client, app, &name);
        }
        // PRD FR-SR-7: flips the explicit offline-search toggle; the next
        // `run_search` (Tab in Mode::Search, or a redlink card's `s`) reads
        // it fresh, so this never has to reach into an in-flight search.
        Command::SearchOffline => {
            app.force_offline_search = !app.force_offline_search;
            app.notice = Some(if app.force_offline_search {
                "Offline search on — the search box now queries saved/cached pages only".to_string()
            } else {
                "Offline search off — the search box uses the API again".to_string()
            });
        }
        // PRD FR-ACC-1 / §5.9.
        Command::Login(LoginMode::Loopback) => cmd_login_loopback(terminal, client, app).await,
        Command::Login(LoginMode::Paste) => cmd_login_paste(app),
        // PRD FR-ACC-9.
        Command::Logout => cmd_logout(app),
        // PRD FR-ACC-2. Same action as `gW`.
        Command::Watchlist => open_watchlist(client, app).await,
        // PRD FR-ACC-3.
        Command::Notifications => open_notifications(client, app).await,
        // PRD FR-ACC-4.
        Command::Contribs(username) => open_contribs(client, app, username).await,
        // PRD FR-ACC-7.
        Command::Prefs => open_prefs(client, app).await,
        // PRD FR-BM-5/6.
        Command::Sync => cmd_sync(client, app).await,
        Command::MirrorWatchlist => cmd_mirror_watchlist(client, app).await,
        // PRD FR-TB-4: `:vsplit` / `:only`. The width check uses the last
        // drawn content area (one-frame-stale, exactly like the mouse-hit
        // rects) — accurate for the current terminal size at command time.
        Command::VSplit => match app.open_split(app.last_content_area.width) {
            Ok(()) => {
                app.notice = Some(
                    "split — Ctrl-w w switches panes, :set scrollbind syncs, :only closes"
                        .to_string(),
                )
            }
            Err(reason) => app.notice = Some(reason),
        },
        Command::Only => {
            if !app.close_split() {
                app.notice = Some("not split".to_string());
            }
        }
        // PRD FR-ML-3: the bilingual side-by-side view.
        Command::Bilingual => cmd_bilingual(client, cache, app, revalidate_tx, langlinks_tx).await,
        Command::Quit => app.should_quit = true,
    }
}

/// PRD FR-ML-3 `:bilingual`: open the current article in a split alongside the
/// same article in another language, resolved via langlinks (B11's
/// `fetch_langlinks`). The other-language edition is picked by
/// `App::bilingual_target` (first preferred `languages` entry with an edition,
/// else the first langlink); langlinks are fetched synchronously here if not
/// already cached so the command is reliable regardless of the passive
/// on-open prefetch's timing. The other-language article is fetched into a
/// fresh tab (the right pane); focus stays on the original-language left pane,
/// with the "interwiki articles are not translations" notice (FR-ML-3 UX copy).
async fn cmd_bilingual(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    revalidate_tx: &UnboundedSender<RevalidationOutcome>,
    langlinks_tx: &UnboundedSender<LangLinksOutcome>,
) {
    if app.split.is_some() {
        app.notice = Some(":only to close the current split first".to_string());
        return;
    }
    let Some((cur_lang, cur_title)) = app
        .active_tab()
        .doc
        .as_ref()
        .map(|d| (app.active_tab().lang.clone(), d.title.clone()))
    else {
        app.notice = Some("open an article first, then :bilingual".to_string());
        return;
    };
    if !split::fits(app.last_content_area.width) {
        app.notice = Some(format!(
            "terminal too narrow to split (need ≥ {} cols)",
            split::MIN_SPLIT_WIDTH
        ));
        return;
    }
    // Warm the langlinks cache synchronously if the on-open fetch hasn't
    // landed yet — an explicit command may block briefly on one small call.
    if app.bilingual_target().is_none()
        && let Ok(links) = client.fetch_langlinks(&cur_lang, &cur_title).await
    {
        app.deliver_langlinks(
            client.wiki_scope(),
            cur_lang.clone(),
            cur_title.clone(),
            Ok(links),
        );
    }
    let Some((code, title)) = app.bilingual_target() else {
        app.notice = Some("no other-language edition available for this article".to_string());
        return;
    };
    let original_id = app.active_tab().id;
    // Fetch the other-language article into a fresh (now active) tab.
    app.new_foreground_tab();
    let other_id = app.active_tab().id;
    app.lang = code.clone();
    open_title(client, cache, app, &title, revalidate_tx, langlinks_tx).await;
    if app.active_tab().doc.is_none() {
        // The other edition failed to load — drop the throwaway tab and
        // restore focus to the original article, unsplit.
        app.close_tab(app.active);
        if let Some(idx) = app.tab_index_by_id(original_id) {
            app.switch_to_tab(idx);
        }
        app.notice = Some(format!("couldn't load the {code} edition"));
        return;
    }
    app.begin_bilingual_split(original_id, other_id);
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
    // PRD FR-SR-7's explicit toggle (`:search-offline`): go straight to the
    // local index without ever attempting the API, even while online.
    if app.force_offline_search {
        run_offline_search(app);
        return;
    }
    app.loading = true;
    match client.search(&app.lang, &app.search_input, 20).await {
        Ok(outcome) => {
            app.results_offline = false;
            app.results = outcome.results;
            app.search_suggestion = outcome.suggestion;
            app.selected_result = 0;
            app.mode = Mode::Results;
            // PRD FR-DL-3: every result title not already cached this
            // session, batched into ONE `prop=pageassessments` call (§6.2
            // rule 10 / NF-NET-5) — never one request per row. A search with
            // zero results, or where every title is already cached, skips
            // the request entirely.
            let lang = app.lang.clone();
            // The search ran on the client's active wiki, so its result badges
            // are keyed under that wiki's scope (PRD FR-ML-4).
            let wiki = client.wiki_scope();
            let uncached: Vec<String> = app
                .results
                .iter()
                .map(|r| r.title.clone())
                .filter(|t| {
                    !app.quality_cache
                        .contains_key(&(wiki.clone(), lang.clone(), t.clone()))
                })
                .collect();
            // PRD FR-ML-5: same capability gate as `enrich_article` — a wiki
            // without PageAssessments never gets the batched lookup
            // attempted, so no result row ever shows a badge for it.
            if !uncached.is_empty()
                && client.capabilities().pageassessments
                && let Ok(assessments) = client.page_assessments(&lang, &uncached).await
            {
                app.quality_cache.extend(
                    assessments
                        .into_iter()
                        .map(|(title, class)| ((wiki.clone(), lang.clone(), title), class)),
                );
            }
        }
        Err(e) => {
            // PRD FR-SR-7 / NF-NET-8: the API failed or timed out (the
            // client's own 5s timeout, or a genuinely offline network) —
            // fall back to the local index instead of a bare error, labeled
            // so the reader knows these results aren't live.
            app.notice = Some(format!(
                "Search unavailable ({e}) — showing offline results"
            ));
            run_offline_search(app);
            return;
        }
    }
    app.loading = false;
}

/// PRD FR-SR-7's offline path: queries the local FTS index instead of the
/// API, scoped to the active wiki + language (matching online search's own
/// per-wiki scope — see `offline_search::OfflineIndex::search`'s doc
/// comment). Shares `Mode::Results`/`app.results` with the online path —
/// `ui::draw_results` needs no offline-specific rendering, because
/// `offline_search`'s snippet already carries the same `<span
/// class="searchmatch">` markup the online `search/page` endpoint's
/// excerpts use, so `ui::parse_searchmatch` highlights it unchanged. The one
/// visible difference is `app.results_offline`, which relabels the results
/// header (PRD §7's "offline-results section") and routes `Mode::Results`'
/// Enter to `open_offline_result`'s local-only serve instead of
/// `open_title`'s network-first one.
fn run_offline_search(app: &mut App) {
    let wiki = app.active_wiki_scope().to_string();
    let lang = app.lang.clone();
    let hits = app.search_index.search(&wiki, &lang, &app.search_input, 20);
    app.results = hits
        .into_iter()
        .map(|hit| SearchResult {
            title: hit.title,
            // FR-SR-7's "labels results '(offline)'" — carried per-result
            // (not just in the header) so the provenance (a pinned save vs.
            // an evictable cache hit) is visible in the same description
            // line an online result's Wikidata one-liner would occupy.
            description: Some(format!("(offline · {})", hit.kind.label())),
            excerpt: Some(hit.snippet),
            size: None,
            wordcount: None,
            timestamp: None,
        })
        .collect();
    app.results_offline = true;
    // FR-SR-4's "did you mean" is an online-only affordance (it comes from
    // the API's own `suggestion` field) — never carried over from whatever
    // the last online search happened to leave behind.
    app.search_suggestion = None;
    app.selected_result = 0;
    app.mode = Mode::Results;
    app.loading = false;
}

/// Serves an offline search result for reading (PRD FR-SR-7's "Enter opens
/// the article"): tries the pinned saved store first (§5.7's precedence — a
/// pinned copy beats a merely-cached one, same rule `open_title` already
/// applies), then the evictable L2 cache. Neither store is consulted over
/// the network — an offline search result is, by construction, a promise
/// this content already exists locally, so opening it must never attempt a
/// fetch (that would defeat the point of an *offline* result, and could
/// hang or error on a genuinely dead connection).
///
/// A miss — the content was evicted or removed since the search ran, PRD
/// FR-SR-7's accepted "eviction-driven staleness" (see `offline_search`'s
/// module doc) — reports the gap and prunes the stale row so the next
/// search doesn't offer it again.
fn open_offline_result(cache: &PageCache, app: &mut App, wiki: &str, lang: &str, title: &str) {
    if app.saved.is_saved(lang, title) {
        open_saved(app, lang, title);
        return;
    }
    if let Some(page) = cache.get(wiki, lang, title) {
        let document = doc::parse_article_html(title, &page.html);
        {
            let tab = app.active_tab_mut();
            tab.page_source = PageSource::Offline {
                age_secs: page.age_secs,
            };
            tab.current_revid = page.revid;
        }
        app.open_document(document);
        return;
    }
    app.notice = Some(format!(
        "\"{title}\" is no longer available offline — removed from the search index"
    ));
    app.search_index.remove(wiki, lang, title);
}

/// `wikitui reindex` (PRD FR-SR-7): rebuilds the offline search index from
/// scratch over every currently saved page (`saved::SavedPages`) and every
/// page the L2 cache currently holds (`cache::PageCache::list_entries`),
/// reducing each to plain text the same way the interactive populate hooks
/// do (`doc::parse_article_html` + `doc::render_plain`). A standalone,
/// TUI-free subcommand like `stats`/`clear-data` — no terminal or network
/// access needed to walk what's already on disk. Saved pages are indexed
/// under the default wiki scope (see `offline_search`'s module doc's
/// documented limitation: `saved::SavedPages` itself has no wiki dimension
/// yet); cached pages carry their own real wiki scope
/// (`cache::PageCache::list_entries`'s stored `wiki` field).
fn run_reindex(resolved: &config::ResolvedConfig) -> i32 {
    let saved = saved::SavedPages::load();
    let cache = cache::PageCache::open(
        resolved.cache_dir.value.clone(),
        resolved.cache_max_mb.value.saturating_mul(1024 * 1024),
        resolved.cache_fresh_ttl_hours.value.saturating_mul(3600),
        resolved
            .cache_force_refetch_days
            .value
            .saturating_mul(86_400),
    );
    let index = offline_search::OfflineIndex::open();

    let mut docs = Vec::new();
    for record in saved.list() {
        if let Some(content) = saved.get(&record.lang, &record.title) {
            let document = doc::parse_article_html(&record.title, &content.html);
            let plain = doc::render_plain(&document, &record.lang);
            docs.push((
                String::new(),
                record.lang.clone(),
                record.title.clone(),
                offline_search::Kind::Saved,
                plain,
            ));
        }
    }
    for (wiki, lang, title) in cache.list_entries() {
        if let Some(page) = cache.get(&wiki, &lang, &title) {
            let document = doc::parse_article_html(&title, &page.html);
            let plain = doc::render_plain(&document, &lang);
            docs.push((wiki, lang, title, offline_search::Kind::Cached, plain));
        }
    }

    let saved_count = saved.list().len();
    let cached_count = cache.list_entries().len();
    let report = index.reindex(docs);
    if index.is_empty() {
        println!("wikitui: reindex: nothing to index — no saved or cached pages found");
    } else {
        println!(
            "wikitui: reindex: {} document(s) indexed ({saved_count} saved, {cached_count} cached), {} row(s) now in the index",
            report.indexed,
            index.len()
        );
    }
    0
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

    /// A dead-port client with an explicit capability matrix (PRD FR-ML-5) —
    /// used by the degradation-decision tests below, where the whole point
    /// is that a gated capability means the network is never touched at
    /// all: if the gate were bypassed, the unreachable port would surface
    /// as a `BgFailure`/error instead of the graceful degradation this
    /// module documents.
    fn test_client_with_capabilities(capabilities: api::WikiCapabilities) -> WikiClient {
        WikiClient::with_wiki(
            "test-wiki".to_string(),
            "http://127.0.0.1:1/{lang}".to_string(),
            capabilities,
            api::DEFAULT_CONTACT,
        )
        .unwrap()
    }

    /// PRD FR-ML-5's degradation matrix: a wiki without Wikifeeds never even
    /// attempts the daily feed request — `execute_featured` reports
    /// `Skipped` immediately instead of hitting the network (which, against
    /// this dead-port client, would otherwise surface as a failure).
    #[tokio::test]
    async fn execute_featured_skips_without_touching_the_network_when_wikifeeds_is_unsupported() {
        let client = test_client_with_capabilities(api::WikiCapabilities {
            parser: api::ParserMode::Auto,
            wikifeeds: false,
            pageviews: true,
            pageassessments: true,
        });
        let feed_cache = Arc::new(std::sync::Mutex::new(prefetch::FeedCache::default()));
        let result = execute_featured(&client, &feed_cache, "en", "2026-07-15").await;
        match result.outcome {
            netqueue::Outcome::Skipped { note } => {
                assert!(note.contains("Wikifeeds"), "{note:?}")
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert!(result.follow_ups.is_empty());
    }

    /// PRD FR-ML-5's degradation matrix: a wiki without `prop=pageviews`
    /// never attempts the batched pageviews request — ranking still runs,
    /// on lead position alone (the same math an unseen title's pageviews
    /// term already reduces to: `rank_links` treats an absent title as 0
    /// views, so this isn't a separate code path, just an empty map).
    #[tokio::test]
    async fn execute_rank_links_falls_back_to_lead_only_ranking_when_pageviews_is_unsupported() {
        let client = test_client_with_capabilities(api::WikiCapabilities {
            parser: api::ParserMode::Auto,
            wikifeeds: true,
            pageviews: false,
            pageassessments: true,
        });
        let candidates = vec![
            netqueue::LinkCandidate {
                title: "Second link".to_string(),
                lead_position: 1,
                is_cursor: false,
            },
            netqueue::LinkCandidate {
                title: "First link".to_string(),
                lead_position: 0,
                is_cursor: false,
            },
        ];
        let result = execute_rank_links(
            &client,
            "en",
            "Source Article",
            &candidates,
            &std::collections::HashMap::new(),
            netqueue::RankWeights::default(),
            10,
        )
        .await;
        match result.outcome {
            netqueue::Outcome::Done { bytes } => assert_eq!(bytes, 0),
            other => panic!("expected Done{{bytes:0}}, got {other:?}"),
        }
        // Lead position alone (no pageviews term) ranks the earlier link
        // first — deterministic without any network data.
        let titles: Vec<&str> = result
            .follow_ups
            .iter()
            .map(|j| match j {
                netqueue::Job::PrefetchArticle { title, reason, .. } => {
                    assert!(
                        !reason.contains("views/day"),
                        "no pageviews term expected: {reason:?}"
                    );
                    title.as_str()
                }
                other => panic!("expected PrefetchArticle, got {other:?}"),
            })
            .collect();
        assert_eq!(titles, vec!["First link", "Second link"]);
    }

    /// PRD FR-ML-5's degradation matrix: a wiki without PageAssessments
    /// never attempts the lookup, so `quality_cache` gains no entry for the
    /// article — the same visible result as "no badge" (`enrich_article`'s
    /// own doc comment covers why this is folded into the same gate as the
    /// redlink/interest work rather than a separate function).
    #[tokio::test]
    async fn enrich_article_skips_the_quality_lookup_when_pageassessments_is_unsupported() {
        let client = test_client_with_capabilities(api::WikiCapabilities {
            parser: api::ParserMode::Auto,
            wikifeeds: false,
            pageviews: false,
            pageassessments: false,
        });
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.interest_learning = false; // isolates this test from the categories fetch
        enrich_article(&client, &mut app, "en", "Some Article").await;
        assert!(app.quality_cache.is_empty());
    }

    /// PRD FR-ML-4/5's `:wiki <name>` switch: a name in the registry
    /// repoints both `app.active_wiki_name` and the shared client state
    /// (`WikiClient::active_wiki_name`/`host`) at once.
    #[test]
    fn switch_wiki_updates_app_and_client_for_a_known_name() {
        let client = WikiClient::new(config::DEFAULT_BASE_URL_TEMPLATE.to_string()).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.wiki_registry.insert(
            "archwiki".to_string(),
            api::WikiRegistryEntry {
                base_url_template: "https://wiki.archlinux.org".to_string(),
                capabilities: api::WikiCapabilities {
                    parser: api::ParserMode::Legacy,
                    wikifeeds: false,
                    pageviews: false,
                    pageassessments: false,
                },
            },
        );

        assert!(switch_wiki(&client, &mut app, "archwiki"));
        assert_eq!(app.active_wiki_name, "archwiki");
        assert_eq!(client.active_wiki_name(), "archwiki");
        assert_eq!(client.wiki_origin("en"), "https://wiki.archlinux.org");
        assert_eq!(client.capabilities().parser, api::ParserMode::Legacy);
        assert!(app.notice.as_deref().unwrap().contains("archwiki"));
    }

    /// An unrecognized name leaves everything unchanged and explains why,
    /// rather than silently doing nothing (PRD FR-ML-5's config-driven
    /// registry: the reader needs to know to add a `[wiki.<name>]` section).
    #[test]
    fn switch_wiki_reports_an_unknown_name_without_changing_state() {
        let client = WikiClient::new(config::DEFAULT_BASE_URL_TEMPLATE.to_string()).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        assert!(!switch_wiki(&client, &mut app, "not-configured"));
        assert_eq!(app.active_wiki_name, "wikipedia");
        assert_eq!(client.active_wiki_name(), "wikipedia");
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("unknown wiki \"not-configured\"")
        );
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
                wiki: String::new(),
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
                wiki: String::new(),
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

    // ---- Session auto-restore (PRD FR-TB-5) --------------------------------

    fn sample_session_state() -> session::SessionState {
        session::SessionState {
            active: 1,
            tabs: vec![
                session::SessionTab {
                    lang: "en".to_string(),
                    wiki: String::new(),
                    title: Some("Alan Turing".to_string()),
                    scroll: 12,
                    folded_blocks: vec![2],
                    current_revid: 1001,
                    back_stack: vec![HistoryEntry {
                        wiki: String::new(),
                        lang: "en".to_string(),
                        title: "Start page".to_string(),
                        scroll: 0,
                    }],
                    forward_stack: Vec::new(),
                },
                session::SessionTab {
                    lang: "en".to_string(),
                    wiki: String::new(),
                    title: Some("Enigma machine".to_string()),
                    scroll: 3,
                    folded_blocks: Vec::new(),
                    current_revid: 1002,
                    back_stack: Vec::new(),
                    forward_stack: Vec::new(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn restore_session_tabs_reopens_every_tab_lazily_and_sets_the_active_index() {
        let client = test_client();
        let cache = PageCache::disabled();
        let (open_tx, _rx) = mpsc::unbounded_channel::<TabLoadOutcome>();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let original_tab0_id = app.tabs[0].id;

        let restored =
            restore_session_tabs(&client, &cache, &mut app, sample_session_state(), &open_tx);

        assert!(restored);
        assert_eq!(app.tabs.len(), 2, "both persisted tabs were reopened");
        assert_eq!(
            app.tabs[0].id, original_tab0_id,
            "the first persisted tab reuses App::new's already-existing tab, not a fresh one"
        );
        // Every tab shows its target title while loading (PRD FR-TB-3's
        // background-tab indicator) — never a network call blocking startup.
        assert!(app.tabs[0].loading);
        assert_eq!(app.tabs[0].pending_title.as_deref(), Some("Alan Turing"));
        assert!(app.tabs[1].loading);
        assert_eq!(app.tabs[1].pending_title.as_deref(), Some("Enigma machine"));
        // Back/forward stacks apply immediately (install_document doesn't
        // touch them, so there's no need to defer these like scroll/folds).
        assert_eq!(app.tabs[0].back_stack.len(), 1);
        assert_eq!(app.tabs[0].back_stack[0].title, "Start page");
        // Scroll/folds are stashed for `apply_tab_load_outcome` to apply once
        // each fetch actually installs a document.
        let tab0_id = app.tabs[0].id;
        let tab1_id = app.tabs[1].id;
        let restore0 = app
            .pending_session_restore
            .get(&tab0_id)
            .expect("tab 0's restore is pending");
        assert_eq!(restore0.scroll, 12);
        assert!(restore0.folded_blocks.contains(&2));
        let restore1 = app
            .pending_session_restore
            .get(&tab1_id)
            .expect("tab 1's restore is pending");
        assert_eq!(restore1.scroll, 3);
        // The persisted active index (1) survives the restore.
        assert_eq!(app.active, 1);
    }

    #[tokio::test]
    async fn restore_session_tabs_applies_stashed_scroll_and_folds_once_the_fetch_lands() {
        let client = test_client();
        let cache = PageCache::disabled();
        let (revalidate_tx, _rrx) = mpsc::unbounded_channel::<RevalidationOutcome>();
        let (open_tx, _rx) = mpsc::unbounded_channel::<TabLoadOutcome>();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);

        restore_session_tabs(&client, &cache, &mut app, sample_session_state(), &open_tx);
        let tab0_id = app.tabs[0].id;

        // Simulate tab 0's background fetch landing (bypassing the network —
        // exactly like `apply_tab_load_outcome`'s other unit tests above).
        apply_tab_load_outcome(
            &client,
            &mut app,
            TabLoadOutcome {
                tab_id: tab0_id,
                wiki: String::new(),
                lang: "en".to_string(),
                title: "Alan Turing".to_string(),
                result: Ok(FetchOutcome {
                    html: "<html><body><h2>A</h2><p>x</p><h2>B</h2><p>y</p></body></html>"
                        .to_string(),
                    source: PageSource::Live,
                    revid: 1001,
                    revalidate: None,
                }),
            },
            &revalidate_tx,
        );

        let tab = app.tabs.iter().find(|t| t.id == tab0_id).unwrap();
        assert_eq!(tab.scroll, 12, "the persisted scroll was applied");
        assert!(
            tab.folded_blocks.contains(&2),
            "the persisted fold was applied"
        );
        assert!(
            !app.pending_session_restore.contains_key(&tab0_id),
            "the pending entry is consumed once applied"
        );
    }

    #[tokio::test]
    async fn restore_session_tabs_on_an_empty_session_returns_false_and_touches_nothing() {
        let client = test_client();
        let cache = PageCache::disabled();
        let (open_tx, _rx) = mpsc::unbounded_channel::<TabLoadOutcome>();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let original_tab0_id = app.tabs[0].id;

        let restored = restore_session_tabs(
            &client,
            &cache,
            &mut app,
            session::SessionState::default(),
            &open_tx,
        );

        assert!(!restored, "an empty session restores nothing");
        assert_eq!(app.tabs.len(), 1, "the original single tab is untouched");
        assert_eq!(app.tabs[0].id, original_tab0_id);
    }

    #[tokio::test]
    async fn restore_session_tabs_clamps_an_out_of_range_active_index() {
        let client = test_client();
        let cache = PageCache::disabled();
        let (open_tx, _rx) = mpsc::unbounded_channel::<TabLoadOutcome>();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let mut state = sample_session_state();
        state.active = 99; // a corrupt/future-format file could have this.

        restore_session_tabs(&client, &cache, &mut app, state, &open_tx);

        assert_eq!(
            app.active,
            app.tabs.len() - 1,
            "an out-of-range active index clamps to the last tab, never panics"
        );
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
                wiki: String::new(),
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

    // ---- PRD FR-ACC-2/3/4/6/7: watchlist, notifications, contribs, thank, prefs

    /// `require_login` is the one gate every feature in this section shares:
    /// logged out, it must show the login prompt and touch nothing else —
    /// no mode change, no network (it takes no `client` at all, so there is
    /// nothing it even *could* call).
    #[tokio::test]
    async fn require_login_prompts_and_returns_none_when_logged_out() {
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let token = require_login(&mut app, "view your watchlist").await;
        assert!(token.is_none());
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to view your watchlist (:login)")
        );
    }

    /// PRD FR-ACC-2: logged out, `:watchlist` shows the login prompt and
    /// makes no request — proven by `test_client()` pointing at a port
    /// nothing listens on (`http://127.0.0.1:1`): if the gate were bypassed,
    /// the fetch would fail and land a *different* ("Couldn't load the
    /// watchlist: ...") message instead, and `enter_watchlist` would have
    /// already flipped the mode before that failure was even known.
    #[tokio::test]
    async fn open_watchlist_logged_out_shows_the_login_prompt_and_makes_no_request() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_watchlist(&client, &mut app).await;
        assert_eq!(app.mode, Mode::Reading, "the pane must never open");
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to view your watchlist (:login)")
        );
        assert!(app.watchlist_raw.is_empty());
        assert!(app.watchlist_changes.is_empty());
    }

    #[tokio::test]
    async fn cmd_watch_toggle_logged_out_shows_the_login_prompt_and_makes_no_request() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        cmd_watch_toggle(&client, &mut app).await;
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to watch articles (:login)")
        );
    }

    #[tokio::test]
    async fn open_notifications_logged_out_shows_the_login_prompt_and_makes_no_request() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_notifications(&client, &mut app).await;
        assert_eq!(app.mode, Mode::Reading, "the pane must never open");
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to view your notifications (:login)")
        );
        assert!(app.notif_alerts.is_empty());
        assert!(app.notif_messages.is_empty());
    }

    #[tokio::test]
    async fn mark_notification_read_logged_out_shows_the_login_prompt() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // A focused notification would normally come from an open pane;
        // this asserts the login gate is checked regardless.
        mark_notification_read(&client, &mut app).await;
        assert_eq!(app.status, "Nothing to mark read", "no focused entry yet");
        app.notif_alerts.push(account::Notification {
            id: "1".to_string(),
            kind: account::NotifKind::Alert,
            text: "x".to_string(),
            read: false,
            timestamp: String::new(),
        });
        mark_notification_read(&client, &mut app).await;
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to manage your notifications (:login)")
        );
        assert!(!app.notif_alerts[0].read, "nothing was actually marked");
    }

    #[tokio::test]
    async fn open_contribs_bare_logged_out_shows_the_login_prompt_and_makes_no_request() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_contribs(&client, &mut app, None).await;
        assert_eq!(app.mode, Mode::Reading, "the view must never open");
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Log in to view your contributions"),
            "{:?}",
            app.notice
        );
        assert!(app.contribs.is_empty());
    }

    /// PRD FR-ACC-4: an explicit username is public — it must NOT show the
    /// login prompt even logged out, and it DOES open the view (the fetch
    /// itself fails against `test_client()`'s unreachable port, which is a
    /// distinct, expected failure mode from being gated).
    #[tokio::test]
    async fn open_contribs_with_a_username_is_never_gated_by_login() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_contribs(&client, &mut app, Some("OtherEditor".to_string())).await;
        assert_eq!(
            app.mode,
            Mode::Contribs,
            "a named user's contribs always open"
        );
        assert_eq!(app.contribs_username, "OtherEditor");
        assert!(
            app.notice.is_none(),
            "no login prompt for a public, explicitly-named user's contributions"
        );
    }

    #[tokio::test]
    async fn cmd_thank_logged_out_shows_the_login_prompt() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        cmd_thank(&client, &mut app).await;
        assert_eq!(app.status, "Nothing to thank", "no focused edit yet");
        app.contribs.push(account::Contribution {
            title: "Alan Turing".to_string(),
            timestamp: String::new(),
            comment: None,
            revid: 42,
            sizediff: 1,
        });
        cmd_thank(&client, &mut app).await;
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to thank an editor (:login)")
        );
    }

    #[tokio::test]
    async fn open_prefs_logged_out_shows_the_login_prompt_and_makes_no_request() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        open_prefs(&client, &mut app).await;
        assert_eq!(app.mode, Mode::Reading, "the card must never open");
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to view your preferences (:login)")
        );
        assert!(app.prefs.is_none());
    }

    /// PRD FR-ACC-3's login/startup poll must be a silent no-op when logged
    /// out — never an error, never a request (there's no `auth_state` to
    /// even get a token from).
    #[tokio::test]
    async fn poll_notifications_count_is_a_no_op_when_logged_out() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        poll_notifications_count(&client, &mut app).await;
        assert_eq!(app.notif_counts, account::NotifCounts::default());
    }

    // ---- PRD §6.2 rule 8: CSRF/watch token fetch + badtoken retry ---------

    /// A tiny scripted server: replies to each accepted connection in turn
    /// with the next `responses` entry, all HTTP 200 — real MediaWiki
    /// reports action-API errors (including `badtoken`) as 200 with an
    /// `{"error":...}` body, never a 4xx (see `api::WikiClient::
    /// authed_post_form`'s doc comment), so every scripted reply here uses
    /// that same shape.
    fn spawn_scripted_server(responses: Vec<&'static str>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for body in responses {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut discard = [0u8; 4096];
                    let _ = stream.read(&mut discard);
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                }
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// End-to-end proof that `watch_with_retry` really performs PRD §6.2
    /// rule 8's contract: the first write with the (freshly fetched) token
    /// comes back `badtoken`, which must invalidate the cache, fetch
    /// exactly one fresh token, and retry exactly once more — landing on
    /// success with the SECOND token, not the first. Four scripted
    /// responses in order: token fetch, failed write, re-fetched token,
    /// successful write; a 5th (unscripted) request would hang the test
    /// (the listener thread only serves four), so a wrong retry count fails
    /// loudly rather than silently passing.
    #[tokio::test]
    async fn watch_with_retry_refetches_once_on_badtoken_and_succeeds() {
        let base = spawn_scripted_server(vec![
            r#"{"query":{"tokens":{"watchtoken":"STALE"}}}"#,
            r#"{"error":{"code":"badtoken","info":"stale token"}}"#,
            r#"{"query":{"tokens":{"watchtoken":"FRESH"}}}"#,
            r#"{"watch":[{"ns":0,"title":"Alan Turing","watched":true}]}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let body = watch_with_retry(&mut app, &client, "en", "access-tok", "Alan Turing", false)
            .await
            .unwrap();
        assert_eq!(
            account::parse_watch_outcome(&body),
            Some(account::WatchOutcome::Watched)
        );
        // The cache now holds the token from the retry's re-fetch, not the
        // stale one — reading it makes no further request (already cached),
        // so this doesn't need a 5th scripted response.
        let cached = app
            .tokens
            .watch_token(&client, "en", "access-tok")
            .await
            .unwrap();
        assert_eq!(cached, "FRESH");
    }

    /// The csrf-token counterpart of the watch test above, exercising
    /// `echomarkread_with_retry` (shared by mark-read/mark-all-read/thank).
    #[tokio::test]
    async fn echomarkread_with_retry_refetches_the_csrf_token_once_on_badtoken() {
        let base = spawn_scripted_server(vec![
            r#"{"query":{"tokens":{"csrftoken":"STALE"}}}"#,
            r#"{"error":{"code":"badtoken","info":"stale token"}}"#,
            r#"{"query":{"tokens":{"csrftoken":"FRESH"}}}"#,
            r#"{"query":{"echomarkread":{"result":"success"}}}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let ids = ["1".to_string()];
        let body = echomarkread_with_retry(&mut app, &client, "en", "access-tok", false, &ids)
            .await
            .unwrap();
        assert!(!account::is_badtoken_response(&body));
        let cached = app
            .tokens
            .csrf_token(&client, "en", "access-tok")
            .await
            .unwrap();
        assert_eq!(cached, "FRESH");
    }

    /// A non-badtoken failure (e.g. a permission error) must NOT trigger a
    /// retry — retrying only ever makes sense for a stale token, never for
    /// any other error, per §6.2 rule 8's "never loop" contract.
    #[tokio::test]
    async fn watch_with_retry_does_not_retry_a_non_badtoken_error() {
        let base = spawn_scripted_server(vec![
            r#"{"query":{"tokens":{"watchtoken":"T1"}}}"#,
            r#"{"error":{"code":"permissiondenied","info":"blocked"}}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let body = watch_with_retry(&mut app, &client, "en", "access-tok", "X", false)
            .await
            .unwrap();
        assert!(!account::is_badtoken_response(&body));
        // Confirms only 2 requests were made (token + one write): a 3rd
        // request against this two-response server would hang, so reaching
        // this assertion at all proves no retry was attempted.
    }

    // ---------------------------------------------------------------------
    // PRD FR-BM-5/6: Reading List sync + watchlist mirror
    // ---------------------------------------------------------------------

    fn temp_state_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "wikitui-main-test-{tag}-{}-{}.json",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn cmd_sync_logged_out_shows_the_login_prompt_and_makes_no_request() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        cmd_sync(&client, &mut app).await;
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to sync your reading list (:login)")
        );
    }

    #[tokio::test]
    async fn cmd_mirror_watchlist_logged_out_shows_the_login_prompt_and_makes_no_request() {
        let client = test_client();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        cmd_mirror_watchlist(&client, &mut app).await;
        assert_eq!(
            app.notice.as_deref(),
            Some("Log in to mirror your watchlist (:login)")
        );
    }

    /// PRD FR-BM-5 / SP-7's setup-if-needed retry: `command=list` comes back
    /// "not set up," which must run `command=setup` exactly once and retry
    /// `list` exactly once more — mirroring `watch_with_retry_refetches_
    /// once_on_badtoken_and_succeeds`'s "exactly N scripted responses, a
    /// 5th would hang" proof technique.
    #[tokio::test]
    async fn fetch_readinglists_with_setup_runs_setup_once_when_not_set_up() {
        let base = spawn_scripted_server(vec![
            r#"{"error":{"code":"readinglists-db-error-not-set-up","info":"not set up"}}"#,
            r#"{"query":{"tokens":{"csrftoken":"T1"}}}"#,
            r#"{"readinglists":{"list":100}}"#,
            r#"{"readinglists":{"lists":[{"id":100,"name":"default","default":true}]}}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let lists = fetch_readinglists_with_setup(&mut app, &client, "en", "access-tok")
            .await
            .unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].id, 100);
        assert!(lists[0].default);
    }

    #[tokio::test]
    async fn fetch_readinglists_with_setup_skips_setup_when_already_set_up() {
        let base = spawn_scripted_server(vec![
            r#"{"readinglists":{"lists":[{"id":100,"name":"default","default":true}]}}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        let lists = fetch_readinglists_with_setup(&mut app, &client, "en", "access-tok")
            .await
            .unwrap();
        assert_eq!(lists.len(), 1);
        // Only one scripted response exists — a setup call here would hang,
        // so reaching this assertion proves setup was skipped.
    }

    /// End-to-end proof of FR-BM-5's push half: one brand-new local bookmark,
    /// nothing on the server yet — `:sync` must create a server entry and
    /// report "pushed 1, pulled 0".
    #[tokio::test]
    async fn sync_reading_list_pushes_a_new_local_bookmark_and_reports_it() {
        let base = spawn_scripted_server(vec![
            r#"{"readinglists":{"lists":[{"id":100,"name":"default","default":true}]}}"#,
            r#"{"readinglists":{"entries":[]}}"#,
            r#"{"query":{"tokens":{"csrftoken":"T1"}}}"#,
            r#"{"readinglists":{"entry":{"id":55,"project":"x","title":"Alan Turing"}}}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.bookmarks = bookmarks::BookmarkStore::in_memory();
        app.bookmarks.toggle("en", "Alan Turing", None);
        let state_path = temp_state_path("sync-push");
        app.readinglist_sync_state_path = Some(state_path.clone());

        let report = sync_reading_list(&client, &mut app, "en", "access-tok").await;
        assert_eq!(report, "Reading List: pushed 1, pulled 0");

        let state = account::load_readinglist_sync_state(&state_path);
        assert_eq!(state.list_id, Some(100));
        assert_eq!(state.synced.len(), 1);
        assert_eq!(state.synced[0].title, "Alan Turing");
        assert_eq!(state.synced[0].entry_id, 55);
        let _ = std::fs::remove_file(&state_path);
    }

    /// FR-BM-5's conflict policy, at the orchestration level (not just the
    /// pure `reconcile_reading_list` unit test): a local bookmark that's
    /// ALSO already on the server (a "matched" title) must come out of
    /// `:sync` with its tags/notes completely unchanged — sync has no path
    /// that could touch them, since a matched title is never pushed,
    /// pulled, or deleted.
    #[tokio::test]
    async fn sync_reading_list_never_touches_tags_or_notes_on_a_matched_title() {
        let base = spawn_scripted_server(vec![
            r#"{"readinglists":{"lists":[{"id":100,"name":"default","default":true}]}}"#,
            // An empty `project` is tolerated (kept, not scope-filtered) —
            // see `sync_reading_list`'s doc comment — so this fixture
            // doesn't need to know the scripted server's own port.
            r#"{"readinglists":{"entries":[{"id":7,"project":"","title":"Alan Turing"}]}}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.bookmarks = bookmarks::BookmarkStore::in_memory();
        app.bookmarks.toggle("en", "Alan Turing", None);
        app.bookmarks
            .set_tags("en", "Alan Turing", vec!["crypto".to_string()]);
        app.bookmarks
            .set_note("en", "Alan Turing", Some("great read".to_string()));
        let state_path = temp_state_path("sync-matched");
        app.readinglist_sync_state_path = Some(state_path.clone());

        let report = sync_reading_list(&client, &mut app, "en", "access-tok").await;
        assert_eq!(report, "Reading List: pushed 0, pulled 0");

        let b = app.bookmarks.find("en", "Alan Turing").unwrap();
        assert_eq!(b.tags, vec!["crypto"]);
        assert_eq!(b.note.as_deref(), Some("great read"));
        let _ = std::fs::remove_file(&state_path);
    }

    /// FR-BM-5's other conflict-policy half, at the orchestration level: a
    /// bookmark deleted locally *after* a previous sync synced it must be
    /// deleted server-side on the next sync, never pulled back — the
    /// classic sync-mapping problem this build solves with a persisted
    /// id-map (`account::ReadingListSyncState`).
    #[tokio::test]
    async fn sync_reading_list_deletes_server_side_a_bookmark_removed_locally_since_the_last_sync()
    {
        let base = spawn_scripted_server(vec![
            r#"{"readinglists":{"lists":[{"id":100,"name":"default","default":true}]}}"#,
            r#"{"readinglists":{"entries":[{"id":7,"project":"x","title":"Alan Turing"}]}}"#,
            r#"{"query":{"tokens":{"csrftoken":"T1"}}}"#,
            r#"{"readinglists":{"success":true}}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        // No local bookmarks at all — "Alan Turing" was deleted locally.
        app.bookmarks = bookmarks::BookmarkStore::in_memory();
        let state_path = temp_state_path("sync-delete");
        account::save_readinglist_sync_state(
            &state_path,
            &account::ReadingListSyncState {
                list_id: Some(100),
                synced: vec![account::SyncedEntry {
                    lang: "en".to_string(),
                    title: "Alan Turing".to_string(),
                    entry_id: 7,
                }],
            },
        )
        .unwrap();
        app.readinglist_sync_state_path = Some(state_path.clone());

        let report = sync_reading_list(&client, &mut app, "en", "access-tok").await;
        assert_eq!(
            report, "Reading List: pushed 0, pulled 0",
            "a server-delete is neither a push nor a pull"
        );
        assert!(
            !app.bookmarks.is_bookmarked("en", "Alan Turing"),
            "the deleted title must never be pulled back"
        );

        let state = account::load_readinglist_sync_state(&state_path);
        assert!(
            state.synced.is_empty(),
            "a successfully server-deleted title drops out of the sync map"
        );
        let _ = std::fs::remove_file(&state_path);
    }

    /// PRD FR-BM-6's mirror, at the orchestration level: one newly-tagged
    /// bookmark needs watching, one previously-mirrored title that's no
    /// longer tagged needs unwatching — both batched (one `action=watch`
    /// POST per direction), one shared `watch` token fetched only once.
    #[tokio::test]
    async fn mirror_watchlist_batches_a_watch_and_an_unwatch_in_one_pass() {
        let base = spawn_scripted_server(vec![
            r#"{"query":{"tokens":{"watchtoken":"WT"}}}"#,
            r#"{"watch":[{"ns":0,"title":"Alan Turing","watched":true}]}"#,
            r#"{"watch":[{"ns":0,"title":"Old Watch","unwatched":true}]}"#,
        ]);
        let client = WikiClient::new(base).unwrap();
        let mut app = App::new("en".to_string(), Theme::terminal(), false);
        app.bookmarks = bookmarks::BookmarkStore::in_memory();
        app.bookmarks.toggle("en", "Alan Turing", None);
        app.bookmarks
            .set_tags("en", "Alan Turing", vec!["watched".to_string()]);
        let state_path = temp_state_path("watchmirror");
        account::save_watch_mirror_state(
            &state_path,
            &account::WatchMirrorState {
                mirrored: vec!["Old Watch".to_string()],
            },
        )
        .unwrap();
        app.watch_mirror_state_path = Some(state_path.clone());

        let report = mirror_watchlist(&client, &mut app, "en", "access-tok").await;
        assert_eq!(report, "Watch mirror: 1 watched, 1 unwatched");

        let state = account::load_watch_mirror_state(&state_path);
        assert_eq!(state.mirrored, vec!["Alan Turing".to_string()]);
        let _ = std::fs::remove_file(&state_path);
    }
}
