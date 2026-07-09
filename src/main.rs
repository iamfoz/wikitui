mod api;
mod app;
mod cli;
mod doc;
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
use app::{App, Mode};
use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = WikiClient::new(cli.lang.clone())?;

    if cli.dump {
        let title = cli
            .title
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--dump requires an article title"))?;
        let (canonical, html) = client.fetch_article_html(&title).await?;
        let document = doc::parse_article_html(&canonical, &html);
        print!("{}", doc::render_plain(&document));
        return Ok(());
    }

    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, &client, cli).await;
    restore_terminal(&mut terminal)?;
    result
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
    cli: Cli,
) -> Result<()> {
    let mut app = App::new(cli.lang.clone());

    if let Some(query) = cli.search {
        app.search_input = query;
        run_search(client, &mut app).await;
    } else if let Some(title) = cli.title {
        open_title(client, &mut app, &title).await;
    }

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // Block until an event arrives instead of redrawing on a timer —
        // an idle reader shouldn't spin the CPU or spam hide-cursor codes.
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            handle_key(client, &mut app, key.code, key.modifiers).await;
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
async fn open_title(client: &WikiClient, app: &mut App, title: &str) {
    app.loading = true;
    match client.fetch_article_html(title).await {
        Ok((canonical, html)) => {
            let document = doc::parse_article_html(&canonical, &html);
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
async fn open_title_from_history(client: &WikiClient, app: &mut App, title: &str) {
    app.loading = true;
    match client.fetch_article_html(title).await {
        Ok((canonical, html)) => {
            let document = doc::parse_article_html(&canonical, &html);
            app.set_document(document);
        }
        Err(e) => {
            app.status = format!("Error: {e}");
        }
    }
    app.loading = false;
}

async fn handle_key(client: &WikiClient, app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
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
                    open_title(client, app, &result.title).await;
                }
            }
            KeyCode::Char('?') => {
                app.prior_mode = app.mode;
                app.mode = Mode::Help;
            }
            _ => {}
        },
        Mode::Reading => match code {
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
            KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll_by(10),
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll_by(-10),
            KeyCode::Char(' ') => app.scroll_by(15),
            KeyCode::Tab => app.cycle_link(true),
            KeyCode::BackTab => app.cycle_link(false),
            KeyCode::Enter => {
                if let Some(link) = app.focused_link.and_then(|i| app.links.get(i)).cloned() {
                    match link.internal_title {
                        Some(title) => open_title(client, app, &title).await,
                        None => app.status = format!("External link: {}", link.href),
                    }
                }
            }
            KeyCode::Char('H') => {
                if let Some(title) = app.navigate_back_target() {
                    open_title_from_history(client, app, &title).await;
                } else {
                    app.status = "No earlier page in history".to_string();
                }
            }
            KeyCode::Char('L') => {
                if let Some(title) = app.navigate_forward_target() {
                    open_title_from_history(client, app, &title).await;
                } else {
                    app.status = "No later page in history".to_string();
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
            KeyCode::Esc => app.pending_g = false,
            _ => {}
        },
    }

    if !matches!(code, KeyCode::Char('g')) {
        app.pending_g = false;
    }
}

async fn run_search(client: &WikiClient, app: &mut App) {
    app.loading = true;
    match client.search(&app.search_input, 20).await {
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
