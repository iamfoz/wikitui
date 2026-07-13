mod api;
mod app;
mod cache;
mod cite;
mod cli;
mod command;
mod config;
mod crashguard;
mod doc;
mod doctor;
mod layout;
mod research;
mod sanitize;
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

use api::{TitleSuggestion, WikiClient};
use app::{App, Mode, PageSource};
use cache::PageCache;
use cite::CiteStyle;
use cli::{Cli, Commands, ConfigAction};
use config::ConfigContext;
use crashguard::TerminalGuard;
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

/// One completed (or failed) typeahead request, tagged with the query it
/// answers so the receiver can drop it if it's gone stale (`app::
/// typeahead_is_current`) — PRD FR-SR-1's in-flight cancellation.
struct TypeaheadOutcome {
    query: String,
    result: std::result::Result<Vec<TitleSuggestion>, String>,
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

    let client = WikiClient::new(resolved.base_url_template.value.clone())?;
    let page_cache = PageCache::open(
        resolved.cache_max_mb.value.saturating_mul(1024 * 1024),
        resolved.cache_fresh_ttl_hours.value.saturating_mul(3600),
    );

    if cli.dump {
        let title = cli
            .title
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--dump requires an article title"))?;
        let (html, _) = fetch_page(&client, &page_cache, &resolved.lang.value, &title).await?;
        let document = doc::parse_article_html(&title, &html);
        print!("{}", doc::render_plain(&document));
        return Ok(());
    }

    // Already validated during resolution (unknown names fall back to the
    // default with a warning above), so these can't fail here — the
    // `unwrap_or_else` is a belt-and-braces guard, not an expected path.
    let theme = Theme::by_name(&resolved.theme.value).unwrap_or_else(Theme::terminal);
    let cite_style = CiteStyle::by_name(&resolved.cite_style.value).unwrap_or(CiteStyle::Apa);
    let no_color = no_color_active();

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
        resolved.measure.value,
        resolved.ambiguous_wide.value,
        cite_style,
        config_ctx,
        reload_flag,
    )
    .await
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

/// The cache-aware fetch (PRD FR-OFF-2's MVP serve policy): a fresh-enough
/// cached copy skips the network entirely; otherwise fetch and cache; and
/// if the network fails, any cached copy — however stale — beats the
/// error, which is what makes offline reading work. Returns the HTML and
/// where it came from.
async fn fetch_page(
    client: &WikiClient,
    cache: &PageCache,
    lang: &str,
    title: &str,
) -> Result<(String, PageSource)> {
    let cached = cache.get(lang, title);
    if let Some(page) = &cached
        && cache.is_fresh(page.age_secs)
    {
        return Ok((
            page.html.clone(),
            PageSource::Cached {
                age_secs: page.age_secs,
            },
        ));
    }
    match client.fetch_article_html(lang, title).await {
        Ok((_, html)) => {
            cache.put(lang, title, &html);
            Ok((html, PageSource::Live))
        }
        Err(network_error) => match cached {
            Some(page) => Ok((
                page.html,
                PageSource::Offline {
                    age_secs: page.age_secs,
                },
            )),
            None => Err(network_error),
        },
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

#[allow(clippy::too_many_arguments)]
async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &WikiClient,
    cache: &PageCache,
    cli: Cli,
    lang: String,
    theme: Theme,
    no_color: bool,
    measure: u16,
    ambiguous_wide: bool,
    cite_style: CiteStyle,
    config_ctx: ConfigContext,
    reload_flag: Arc<AtomicBool>,
) -> Result<()> {
    let mut app = App::new(lang, theme, no_color);
    app.measure = measure;
    app.ambiguous_wide = ambiguous_wide;
    app.cite_style = cite_style;
    app.config_ctx = config_ctx;

    if let Some(query) = cli.search {
        app.search_input = query;
        run_search(client, &mut app).await;
    } else if let Some(title) = cli.title {
        open_title(client, cache, &mut app, &title).await;
    }

    // Delivers typeahead responses back to the loop (PRD FR-SR-1): fetches
    // run on spawned tasks, never inline in the loop, so a slow request
    // can't stall redraws or keystrokes — see `fire_typeahead`.
    let (typeahead_tx, mut typeahead_rx) = mpsc::unbounded_channel::<TypeaheadOutcome>();

    loop {
        // Checked once per turn rather than mid-`event::read()`, which
        // blocks on real input and can't be interrupted without
        // restructuring the whole loop: SIGHUP's reload takes effect on
        // the *next* keypress, not instantly. Documented tradeoff, not a
        // bug — §6.7 only requires SIGHUP to trigger "the same reload."
        if reload_flag.swap(false, Ordering::SeqCst) {
            apply_config_reload(&mut app);
        }

        terminal.draw(|f| ui::draw(f, &mut app))?;

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

        if app.mode == Mode::Search {
            // The only mode that wakes on a timer instead of blocking
            // forever in `event::read()` — the typeahead debounce needs the
            // loop to notice time passing even with no new keystroke.
            // Everywhere else keeps the original block-until-input
            // behavior (PRD FR-ACS-2: no gratuitous redraws/CPU use idle).
            if event::poll(TYPEAHEAD_POLL)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                handle_key(client, cache, &mut app, key.code, key.modifiers).await;
            }
            if let Some(deadline) = app.search_debounce_at
                && app::debounce_due(deadline, Instant::now())
            {
                app.search_debounce_at = None;
                fire_typeahead(client, &app, &typeahead_tx);
            }
            // Non-blocking drain: a response for a query the user has since
            // typed past (`typeahead_is_current` says no) is silently
            // discarded — PRD FR-SR-1's in-flight cancellation.
            while let Ok(outcome) = typeahead_rx.try_recv() {
                if app::typeahead_is_current(&outcome.query, &app.search_input)
                    && let Ok(suggestions) = outcome.result
                {
                    app.typeahead = suggestions;
                    app.selected_suggestion = 0;
                }
            }
        } else if let Event::Key(key) = event::read()? {
            // Block until an event arrives instead of redrawing on a timer —
            // an idle reader shouldn't spin the CPU or spam hide-cursor codes.
            if key.kind == KeyEventKind::Press {
                handle_key(client, cache, &mut app, key.code, key.modifiers).await;
            }
        }

        if app.should_quit {
            break;
        }
    }

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
        app.theme = theme;
    }
    app.measure = resolved.measure.value;
    app.ambiguous_wide = resolved.ambiguous_wide.value;
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
/// CLI title, search results, and following a link.
async fn open_title(client: &WikiClient, cache: &PageCache, app: &mut App, title: &str) {
    app.loading = true;
    match fetch_page(client, cache, &app.lang, title).await {
        Ok((html, source)) => {
            let document = doc::parse_article_html(title, &html);
            app.page_source = source;
            app.open_document(document);
        }
        Err(e) => {
            app.status = format!("Error: {e}");
            app.mode = Mode::Reading;
        }
    }
    app.loading = false;
}

/// Fetch and install `title` without touching the back/forward stacks —
/// used for `H`/`L` navigation, which already adjusted the stacks via
/// `App::navigate_back_target`/`navigate_forward_target`.
async fn open_title_from_history(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    title: &str,
) {
    app.loading = true;
    match fetch_page(client, cache, &app.lang, title).await {
        Ok((html, source)) => {
            let document = doc::parse_article_html(title, &html);
            app.page_source = source;
            app.set_document(document);
        }
        Err(e) => {
            app.status = format!("Error: {e}");
        }
    }
    app.loading = false;
}

async fn handle_key(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    match app.mode {
        Mode::Help => {
            // Any key closes the help overlay.
            app.mode = app.prior_mode;
        }
        // PRD Appendix B's search keybindings: Enter opens the highlighted
        // typeahead suggestion directly (FR-SR-1); Tab runs a full-text
        // search of the typed query instead (FR-SR-2's mode toggle) — the
        // two are deliberately separate actions, not Enter-falls-back-to-
        // search, so the dropdown and full-text results never fight over
        // what Enter means.
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
                    open_title(client, cache, app, &title).await;
                } else {
                    app.status = "No suggestion selected — Tab searches full text".to_string();
                }
            }
            KeyCode::Tab => {
                if !app.search_input.trim().is_empty() {
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
                    Ok(cmd) => execute_command(client, cache, app, cmd).await,
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
                app.find_input.pop();
                app.update_find();
            }
            KeyCode::Char(c) => {
                app.find_input.push(c);
                app.update_find();
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
                    open_title(client, cache, app, &result.title).await;
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
        Mode::Toc => match code {
            KeyCode::Esc => app.mode = Mode::Reading,
            KeyCode::Char('j') | KeyCode::Down => {
                if !app.sections.is_empty() {
                    app.selected_section = (app.selected_section + 1).min(app.sections.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.selected_section = app.selected_section.saturating_sub(1);
            }
            KeyCode::Enter => app.jump_to_section(app.selected_section),
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        Mode::Reading => {
            app.notice = None;
            match code {
                KeyCode::Char('q') => app.should_quit = true,
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
                KeyCode::Char('j') | KeyCode::Down => app.scroll_by(1),
                KeyCode::Char('k') | KeyCode::Up => app.scroll_by(-1),
                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_by(10)
                }
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_by(-10)
                }
                KeyCode::Char(' ') => app.scroll_by(15),
                KeyCode::Tab => app.cycle_link(true),
                KeyCode::BackTab => app.cycle_link(false),
                KeyCode::Enter => {
                    if let Some(link) = app.focused_link.and_then(|i| app.links.get(i)).cloned() {
                        match link.internal_title {
                            Some(title) => open_title(client, cache, app, &title).await,
                            None => app.status = format!("External link: {}", link.href),
                        }
                    }
                }
                KeyCode::Char('H') => {
                    if let Some(title) = app.navigate_back_target() {
                        open_title_from_history(client, cache, app, &title).await;
                    } else {
                        app.status = "No earlier page in history".to_string();
                    }
                }
                KeyCode::Char('L') => {
                    if let Some(title) = app.navigate_forward_target() {
                        open_title_from_history(client, cache, app, &title).await;
                    } else {
                        app.status = "No later page in history".to_string();
                    }
                }
                KeyCode::Char('t') => {
                    if app.sections.is_empty() {
                        app.status = "No sections on this page".to_string();
                    } else {
                        app.mode = Mode::Toc;
                    }
                }
                KeyCode::Char('T') => app.cycle_theme(),
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
                KeyCode::Char('r') => {
                    if app.doc.is_some() {
                        app.mode = Mode::Research;
                    } else {
                        app.status = "Open an article first".to_string();
                    }
                }
                KeyCode::Char('R') => app.open_library(),
                KeyCode::Char('f') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.mode = Mode::Find;
                    app.clear_find();
                }
                KeyCode::Char('n') => {
                    if app.find_matches.is_empty() {
                        app.status = "No active search — Ctrl-f to find in this page".to_string();
                    } else {
                        app.find_next();
                    }
                }
                KeyCode::Char('N') => {
                    if app.find_matches.is_empty() {
                        app.status = "No active search — Ctrl-f to find in this page".to_string();
                    } else {
                        app.find_prev();
                    }
                }
                KeyCode::Char('g') => {
                    if app.pending_g {
                        app.scroll_to_top();
                        app.pending_g = false;
                    } else {
                        app.pending_g = true;
                    }
                }
                KeyCode::Char('G') => app.scroll_to_bottom(),
                KeyCode::Esc => {
                    app.pending_g = false;
                    app.clear_find();
                }
                _ => {}
            }
        }
    }

    if !matches!(code, KeyCode::Char('g')) {
        app.pending_g = false;
    }
}

/// Executes a parsed `:` command. Parsing already validated arguments
/// (theme/style/lang names), so the arms here mostly delegate to existing
/// features.
async fn execute_command(
    client: &WikiClient,
    cache: &PageCache,
    app: &mut App,
    cmd: command::Command,
) {
    use command::Command;
    match cmd {
        Command::Open(raw) => {
            // Same grammar as the CLI TITLE argument: URLs and
            // lang-prefixed titles work here too.
            let target = target::parse(&raw);
            if let Some(lang) = target.lang {
                app.lang = lang;
            }
            open_title(client, cache, app, &target.title).await;
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
                app.theme = theme;
                app.notice = Some(format!("Theme: {name}"));
            }
        }
        Command::Style(name) => {
            if let Some(style) = cite::CiteStyle::by_name(&name) {
                app.cite_style = style;
                app.notice = Some(format!("Citation style: {}", style.label()));
            }
        }
        Command::Library => app.open_library(),
        Command::Research => {
            if app.doc.is_some() {
                app.mode = Mode::Research;
            } else {
                app.notice = Some("Open an article first".to_string());
            }
        }
        Command::Toc => {
            if app.sections.is_empty() {
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
        Command::Quit => app.should_quit = true,
    }
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
}
