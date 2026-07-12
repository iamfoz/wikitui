mod api;
mod app;
mod cache;
mod cite;
mod cli;
mod command;
mod doc;
mod research;
mod target;
mod theme;
mod ui;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};

use api::WikiClient;
use app::{App, Mode, PageSource};
use cache::PageCache;
use cli::Cli;
use theme::Theme;

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // The TITLE argument may be a full wikipedia.org URL or a
    // lang-prefixed title (FR-CS-6); either overrides --lang.
    if let Some(raw) = &cli.title {
        let target = target::parse(raw);
        if let Some(lang) = target.lang {
            cli.lang = lang;
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

    let client = WikiClient::new()?;
    let page_cache = PageCache::open();

    if cli.dump {
        let title = cli
            .title
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--dump requires an article title"))?;
        let (html, _) = fetch_page(&client, &page_cache, &cli.lang, &title).await?;
        let document = doc::parse_article_html(&title, &html);
        print!("{}", doc::render_plain(&document));
        return Ok(());
    }

    let theme = Theme::by_name(&cli.theme).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown theme {:?} — choose one of: {}",
            cli.theme,
            Theme::NAMES.join(", ")
        )
    })?;
    // PRD FR-TH-5: NO_COLOR, when present and non-empty, strips color from
    // every theme regardless of which one is selected.
    let no_color = std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty());

    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, &client, &page_cache, cli, theme, no_color).await;
    restore_terminal(&mut terminal)?;
    result
}

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
        && page.age_secs < cache::FRESH_TTL_SECS
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

/// Copies text to the system clipboard via OSC 52 (PRD FR-NV-10), which
/// works over SSH because the *terminal emulator* performs the copy.
/// Terminals without OSC 52 support silently ignore the sequence — the
/// status line still reports what was yanked so the user can tell.
fn yank_to_clipboard(text: &str) -> std::io::Result<()> {
    use base64::Engine as _;
    use std::io::Write as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Ensure the terminal is never left in raw/alternate-screen mode after a
/// panic (PRD §7: "Crash — panic handler restores terminal state" — a
/// recorded failure mode of the incumbent client).
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &WikiClient,
    cache: &PageCache,
    cli: Cli,
    theme: Theme,
    no_color: bool,
) -> Result<()> {
    let mut app = App::new(cli.lang.clone(), theme, no_color);

    if let Some(query) = cli.search {
        app.search_input = query;
        run_search(client, &mut app).await;
    } else if let Some(title) = cli.title {
        open_title(client, cache, &mut app, &title).await;
    }

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // Block until an event arrives instead of redrawing on a timer —
        // an idle reader shouldn't spin the CPU or spam hide-cursor codes.
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            handle_key(client, cache, &mut app, key.code, key.modifiers).await;
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
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
        Mode::Search => match code {
            KeyCode::Esc => {
                app.mode = Mode::Reading;
                app.search_input.clear();
            }
            KeyCode::Enter => {
                if !app.search_input.trim().is_empty() {
                    run_search(client, app).await;
                }
            }
            KeyCode::Backspace => {
                app.search_input.pop();
            }
            KeyCode::Char(c) => {
                app.search_input.push(c);
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
        Command::Quit => app.should_quit = true,
    }
}

async fn run_search(client: &WikiClient, app: &mut App) {
    app.loading = true;
    match client.search(&app.lang, &app.search_input, 20).await {
        Ok(results) => {
            app.results = results;
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
