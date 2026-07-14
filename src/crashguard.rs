//! Crash safety (PRD §7's "Crash" row, the v0.5 milestone's "crash-safe
//! terminal restore", FR-PR-1): two complementary mechanisms, not one.
//!
//! - [`TerminalGuard`] is an RAII guard around raw mode / the alternate
//!   screen. Restoring on `Drop` — rather than only via an explicit
//!   "restore" call after the run loop returns — means *any* early exit
//!   from `main` between `TerminalGuard::enter` and the end of the process
//!   (an early `?`, a future refactor that adds one, a `return` added later
//!   in `run`) unwinds through the guard and restores the terminal. It
//!   cannot cover a panic in a *different* task, though: tokio spawns
//!   typeahead requests on their own tasks (`main::fire_typeahead`), and a
//!   panic there never unwinds `main`'s stack at all, so this guard's
//!   `Drop` would never run for it.
//! - [`install_panic_hook`] covers exactly that gap. `std::panic::set_hook`
//!   is process-global: it fires from whichever thread/task actually
//!   panics, main task or spawned. Its restore step is intentionally
//!   independent of `TerminalGuard` (no shared state, no `Mutex`) because a
//!   panic can happen with the guard in any state, including one the hook
//!   can't observe — so it just redoes the same best-effort, idempotent
//!   restore unconditionally. Redundant work in the common case; the only
//!   correct choice in the panicking one.
//!
//! Both restore paths funnel through [`restore_terminal_best_effort`] so
//! there is exactly one implementation of "what restoring means" — see its
//! own doc comment for why every step is independent for both correctness
//! reasons.
//!
//! The panic hook also writes a local crash report
//! (`$XDG_STATE_HOME/wikitui/crash-<unixtime>.txt`) and prints its path to
//! stderr. FR-PR-1 is explicit that this is never auto-submitted anywhere —
//! there is no code in this module, or anywhere else in the app, that sends
//! it over the network. It exists purely so a user who hits a bug has
//! something concrete to attach to an issue if *they* choose to.

use std::io;
use std::path::{Path, PathBuf};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// RAII guard owning the terminal for exactly as long as raw mode and the
/// alternate screen are active. See the module doc comment for why `Drop`
/// (rather than an explicit post-`run()` restore call) is what closes PRD
/// §7's "broken terminal after a crash" gap for ordinary control-flow exits.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    /// Enables raw mode and enters the alternate screen. If entering the
    /// alternate screen fails after raw mode was already enabled, raw mode
    /// is best-effort undone before the error is returned — so a failed
    /// `enter()` never leaves the terminal half-configured for however long
    /// it takes the caller to notice and exit.
    pub fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(e.into());
            }
        };
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_best_effort();
    }
}

/// PRD FR-NV-9: turns real terminal mouse capture on or off. A free function
/// rather than a `TerminalGuard` method because it needs only a raw write to
/// stdout (`EnableMouseCapture`/`DisableMouseCapture` are themselves plain
/// escape sequences, same as every other crossterm terminal command here) —
/// both the one-time startup toggle (`main::run`, from config) and the
/// runtime `:set mouse=on|off` toggle (`main::execute_command`, from
/// mid-session) call this directly rather than threading a `TerminalGuard`
/// reference through the whole event loop for one escape sequence.
pub fn set_mouse_capture(enabled: bool) -> io::Result<()> {
    if enabled {
        execute!(io::stdout(), EnableMouseCapture)
    } else {
        execute!(io::stdout(), DisableMouseCapture)
    }
}

/// Temporarily yields the terminal to an external, interactively-run
/// program — PRD FR-BM-2's `ba` annotate, the first (only, today) caller,
/// spawning `$EDITOR` (SEC-5's external-command boundary: the child gets a
/// normal-looking terminal to draw its own UI into, never the alternate
/// screen wikitui itself owns). `suspend` leaves the alternate screen and
/// disables raw mode; `Drop` restores both — the same RAII shape as
/// [`TerminalGuard`] and for the identical reason: whatever the caller does
/// between `suspend()` and the guard going out of scope (running the child
/// process, reading its output, an early `return` on error) restores the
/// terminal on every exit path, not just the one a call-symmetric
/// enter/leave pair would remember to cover.
pub struct SuspendedTerminal<'a> {
    terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>,
}

impl<'a> SuspendedTerminal<'a> {
    /// Best-effort by construction (see [`restore_terminal_best_effort`]):
    /// a failed step here still returns a guard whose `Drop` will attempt
    /// the corresponding restore, never leaving the terminal in a worse
    /// state than pressing on with the external command anyway.
    pub fn suspend(terminal: &'a mut Terminal<CrosstermBackend<io::Stdout>>) -> Self {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = execute!(io::stdout(), crossterm::cursor::Show);
        Self { terminal }
    }
}

impl Drop for SuspendedTerminal<'_> {
    fn drop(&mut self) {
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = execute!(io::stdout(), EnterAlternateScreen);
        // The external program just drew its own content over the real
        // screen while wikitui was away; ratatui's diffing otherwise
        // assumes the terminal still shows whatever it last painted, so a
        // stale diff here would leave leftover fragments of the child
        // program's UI on screen. `clear()` forces the next `draw()` to
        // repaint every cell instead of diffing against that stale buffer.
        let _ = self.terminal.clear();
    }
}

/// What "restore the terminal" means, shared by `TerminalGuard::drop` and
/// the panic hook. Every step is attempted independently — never chained
/// with `?` — for two reasons at once: it must be **best-effort** (one
/// failing step, e.g. because stdout is already gone, must not skip the
/// others) and **idempotent** (calling it when the terminal was never
/// fully set up, or was already restored, must be harmless) — both
/// requirements PRD §7 states explicitly for the panic path, and both
/// equally necessary for `TerminalGuard`'s normal `Drop` path.
fn restore_terminal_best_effort() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = execute!(io::stdout(), crossterm::cursor::Show);
    // PRD FR-NV-9: unconditionally attempt to release mouse capture,
    // regardless of whether this session ever enabled it — idempotent and
    // harmless on a terminal that never received `EnableMouseCapture` (it is
    // simply an escape sequence the terminal either recognizes and undoes,
    // or silently ignores), and the panic hook has no way to know this
    // process's `mouse` setting at panic time anyway (see this function's
    // own "best-effort and idempotent" contract above).
    let _ = execute!(io::stdout(), crossterm::event::DisableMouseCapture);
    // PRD FR-TH-4: same reasoning for DEC 2031 color-scheme-change
    // notifications — best-effort disabled on every restore, whether or not
    // `auto_theme` ever enabled them.
    let _ = execute!(
        io::stdout(),
        crossterm::style::Print(crate::autotheme::DISABLE_COLOR_SCHEME_NOTIFICATIONS)
    );
}

/// Installs a panic hook that restores the terminal, writes a local crash
/// report, and prints its path — then still calls through to the previous
/// hook (whatever it was: the Rust default, or one installed by a test
/// harness) so no existing panic-reporting behavior is lost, only added to.
/// Must be called **before** `TerminalGuard::enter` — a panic during
/// startup, before the guard exists, still needs the terminal left alone
/// (nothing to restore yet), but a panic anytime after needs this hook
/// already in place.
pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort and infallible by construction (see
        // `restore_terminal_best_effort`) — this closure must never itself
        // panic, which is why nothing here uses `?` or `.unwrap()`.
        restore_terminal_best_effort();

        let message = panic_message(info);
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "<unknown location>".to_string());
        // FR-PR-1: a backtrace is only ever captured when the user already
        // opted into `RUST_BACKTRACE` themselves — never collected by
        // default, and never sent anywhere regardless (this whole report is
        // a local file, full stop).
        let backtrace = std::env::var("RUST_BACKTRACE")
            .ok()
            .filter(|v| v != "0")
            .map(|_| std::backtrace::Backtrace::force_capture().to_string());
        let contents = format_crash_report(&message, &location, backtrace.as_deref());

        match write_crash_report(&contents) {
            Some(path) => eprintln!(
                "wikitui hit a bug and had to stop — sorry about that. \
                 A local crash report was written to {} (never sent anywhere, see PRD FR-PR-1); \
                 attach it if you file an issue.",
                path.display()
            ),
            None => eprintln!(
                "wikitui hit a bug and had to stop — sorry about that. \
                 (Could not write a local crash report: no writable state directory found.)"
            ),
        }

        original(info);
    }));
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Builds the crash report's text. Pure (no I/O), so it's directly
/// testable without touching the filesystem or triggering a real panic.
fn format_crash_report(message: &str, location: &str, backtrace: Option<&str>) -> String {
    let mut out = format!(
        "wikitui {} crash report\npanic: {message}\nlocation: {location}\n",
        env!("CARGO_PKG_VERSION")
    );
    match backtrace {
        Some(bt) => {
            out.push_str("backtrace (RUST_BACKTRACE was set):\n");
            out.push_str(bt);
            out.push('\n');
        }
        None => out.push_str("backtrace: not captured (set RUST_BACKTRACE=1 to include one)\n"),
    }
    out
}

/// `$XDG_STATE_HOME/wikitui` (PRD §6.4's storage table), or — on platforms
/// `directories` has no native state-dir concept for (macOS, Windows) — the
/// data dir's own `state` subdirectory, so a crash report always has
/// somewhere to land rather than silently failing to write on those
/// platforms.
fn crash_report_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "wikitui")?;
    Some(match dirs.state_dir() {
        Some(state) => state.to_path_buf(),
        None => dirs.data_dir().join("state"),
    })
}

/// Writes `contents` to `dir/crash-<unix_time>.txt`, creating `dir` if
/// needed. Split out from `write_crash_report` so a test can point it at a
/// temp directory instead of the real platform state dir — see the tests
/// module.
fn write_crash_report_at(dir: &Path, contents: &str, unix_time: u64) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("crash-{unix_time}.txt"));
    std::fs::write(&path, contents)?;
    Ok(path)
}

/// Resolves the real platform crash-report directory and writes to it.
/// `None` on any failure (no writable directory found, or the write itself
/// failed) — the panic hook already handles that case by printing a
/// fallback message instead of a path.
fn write_crash_report(contents: &str) -> Option<PathBuf> {
    let dir = crash_report_dir()?;
    let unix_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    write_crash_report_at(&dir, contents, unix_time).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_crash_report_includes_message_location_and_version() {
        let report = format_crash_report("boom", "src/main.rs:42:5", None);
        assert!(report.contains(env!("CARGO_PKG_VERSION")));
        assert!(report.contains("panic: boom"));
        assert!(report.contains("location: src/main.rs:42:5"));
        assert!(report.contains("not captured"));
    }

    #[test]
    fn format_crash_report_includes_backtrace_when_provided() {
        let report = format_crash_report("boom", "src/main.rs:1:1", Some("0: some::frame"));
        assert!(report.contains("RUST_BACKTRACE was set"));
        assert!(report.contains("some::frame"));
    }

    /// PRD FR-PR-1 / §7: the report-writing function, called directly (no
    /// real panic needed) against a temp directory instead of the real
    /// platform state dir.
    #[test]
    fn write_crash_report_at_creates_the_directory_and_file() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-crashguard-test-{}-{}",
            std::process::id(),
            "creates_dir"
        ));
        let _ = std::fs::remove_dir_all(&tmp);

        let path = write_crash_report_at(
            &tmp,
            "wikitui 0.1.0 crash report\npanic: boom\n",
            1_700_000_000,
        )
        .expect("write must succeed against a fresh temp dir");

        assert_eq!(path, tmp.join("crash-1700000000.txt"));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("panic: boom"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_crash_report_at_is_idempotent_when_the_directory_already_exists() {
        let tmp = std::env::temp_dir().join(format!(
            "wikitui-crashguard-test-{}-{}",
            std::process::id(),
            "idempotent"
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // Calling it twice (e.g. two crashes in one session) must not error
        // just because the directory is already there.
        assert!(write_crash_report_at(&tmp, "first", 1).is_ok());
        assert!(write_crash_report_at(&tmp, "second", 2).is_ok());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn restore_terminal_best_effort_does_not_panic_outside_a_real_terminal() {
        // No raw mode / alternate screen active in a `cargo test` process —
        // this must still be a harmless no-op-ish call, not a panic, since
        // the panic hook calls it unconditionally from within a panic.
        restore_terminal_best_effort();
    }
}
