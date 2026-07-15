//! `wikitui config doctor` (PRD §6.7): a plain-stdout report that runs
//! before any TUI initialization — a broken terminal state must never be
//! the reason a config problem is hard to see. Five sections, in order:
//! resolved config (with per-value provenance), config problems, user theme
//! files (FR-TH-1/6), the FR-TH-6 contrast lint (built-ins and user themes
//! both), and a terminal-capability report (including FR-TH-3's resolved
//! color depth).
//!
//! Exit code is the only thing anything else should depend on: 0 when
//! nothing rose to `IssueLevel::Error` (unknown keys and rejected values
//! are warnings, not errors — the app still starts fine on those), 1
//! otherwise.

use std::io::IsTerminal;

use crate::config::{IssueLevel, ResolvedConfig};
use crate::theme;

/// Prints the full report to stdout and returns the process exit code.
pub fn run(resolved: &ResolvedConfig) -> i32 {
    // PRD FR-TH-1: the same `themes/` directory `main::run` loads from —
    // `doctor` is a standalone entry point (PRD §6.7), so it re-loads here
    // rather than depending on a prior load having happened in this process.
    let themes_dir = resolved
        .config_path
        .as_deref()
        .and_then(|p| p.parent())
        .map(|d| d.join("themes"));
    let (user_themes, theme_warnings) = theme::load_user_themes(themes_dir.as_deref());

    print_resolved_config(resolved);
    println!();
    print_problems(resolved);
    println!();
    print_user_themes(&user_themes, &theme_warnings);
    println!();
    print_contrast_lint(&user_themes);
    println!();
    print_capabilities(&resolved.terminal.color_depth.value);

    if resolved.has_errors() { 1 } else { 0 }
}

/// PRD FR-TH-1: lists every theme file `load_user_themes` found under
/// `themes_dir`, its declared `dark`/`allow_low_contrast` metadata and
/// source path, and any parse warning (a bad file is never fatal here
/// either — see that function's own doc comment).
fn print_user_themes(user_themes: &[theme::LoadedUserTheme], warnings: &[String]) {
    println!("User theme files (FR-TH-1):");
    if user_themes.is_empty() && warnings.is_empty() {
        println!("  none found");
        return;
    }
    for t in user_themes {
        println!(
            "  {:?}: {} ({}, allow_low_contrast={})",
            t.name,
            t.source.display(),
            if t.dark { "dark" } else { "light" },
            t.allow_low_contrast
        );
    }
    for warning in warnings {
        println!("  [warning] {warning}");
    }
}

fn print_resolved_config(resolved: &ResolvedConfig) {
    println!("Resolved configuration:");
    match &resolved.config_path {
        Some(path) => println!("  config file: {}", path.display()),
        None => println!("  config file: (none — using built-in defaults only)"),
    }
    println!(
        "  config_version = {} ({})",
        resolved.config_version.value, resolved.config_version.source
    );
    if let Some(summary) = &resolved.migration_summary {
        println!("  {summary}");
    }
    println!(
        "  lang = {:?} ({})",
        resolved.lang.value, resolved.lang.source
    );
    println!(
        "  languages = {:?} ({})",
        resolved.languages.value, resolved.languages.source
    );
    println!(
        "  theme = {:?} ({})",
        resolved.theme.value, resolved.theme.source
    );
    println!(
        "  measure = {} ({})",
        resolved.measure.value, resolved.measure.source
    );
    println!(
        "  ambiguous_width = {} ({}) -> ambiguous_wide = {}",
        if resolved.ambiguous_wide.value { 2 } else { 1 },
        resolved.ambiguous_wide.source,
        resolved.ambiguous_wide.value
    );
    println!(
        "  cite_style = {:?} ({})",
        resolved.cite_style.value, resolved.cite_style.source
    );
    println!(
        "  cache.max_mb = {} ({})",
        resolved.cache_max_mb.value, resolved.cache_max_mb.source
    );
    println!(
        "  cache.fresh_ttl_hours = {} ({})",
        resolved.cache_fresh_ttl_hours.value, resolved.cache_fresh_ttl_hours.source
    );
    println!(
        "  cache.force_refetch_days = {} ({})",
        resolved.cache_force_refetch_days.value, resolved.cache_force_refetch_days.source
    );
    println!(
        "  cache.dir = {} ({})",
        match &resolved.cache_dir.value {
            Some(p) => format!("{:?} (override)", p.display()),
            None => "(none — using the platform cache directory)".to_string(),
        },
        resolved.cache_dir.source
    );
    // PRD FR-PR-5: the *effective* directory a real run would write to —
    // resolves the override (if any) the same way `PageCache::open` does, so
    // `doctor` never shows a value that could disagree with reality.
    println!(
        "  cache: effective directory = {}",
        crate::cache::resolve_pages_dir(resolved.cache_dir.value.as_deref())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none — cache disabled, no platform directory found)".to_string())
    );
    println!(
        "  active_wiki = {:?} ({})",
        resolved.active_wiki.value, resolved.active_wiki.source
    );
    println!(
        "  base_url = {:?} ({})",
        resolved.base_url_template.value, resolved.base_url_template.source
    );
    println!(
        "  readlater_auto_dequeue = {} ({})",
        resolved.readlater_auto_dequeue.value, resolved.readlater_auto_dequeue.source
    );
    println!(
        "  history.retention_days = {} ({}){}",
        resolved.history_retention_days.value,
        resolved.history_retention_days.source,
        if resolved.history_retention_days.value == 0 {
            " -- keep forever"
        } else {
            ""
        }
    );
    let pf = &resolved.prefetch;
    println!(
        "  prefetch.enabled = {} ({})",
        pf.enabled.value, pf.enabled.source
    );
    println!(
        "  prefetch.daily_mb = {} ({}), hourly_requests = {} ({}), metered = {:?} ({})",
        pf.daily_mb.value,
        pf.daily_mb.source,
        pf.hourly_requests.value,
        pf.hourly_requests.source,
        pf.metered.value,
        pf.metered.source
    );
    println!(
        "  prefetch.top_n = {} ({}), weights = lead {} / pageviews {} / affinity {}",
        pf.top_n.value,
        pf.top_n.source,
        pf.weight_lead.value,
        pf.weight_pageviews.value,
        pf.weight_affinity.value
    );
    println!(
        "  network.contact = {:?} ({})",
        resolved.network_contact.value, resolved.network_contact.source
    );
    println!(
        "  startpage = {:?} ({})",
        resolved.startpage.value, resolved.startpage.source
    );
    let t = &resolved.terminal;
    println!(
        "  mouse = {} ({}) -- FR-NV-9, off means native terminal selection/copy is untouched",
        t.mouse.value, t.mouse.source
    );
    println!(
        "  animations = {:?} ({})",
        t.animations.value, t.animations.source
    );
    println!(
        "  auto_theme = {} ({}); theme_light = {:?} ({}); theme_dark = {:?} ({})",
        t.auto_theme.value,
        t.auto_theme.source,
        t.theme_light.value,
        t.theme_light.source,
        t.theme_dark.value,
        t.theme_dark.source
    );
    println!(
        "  hyperlinks = {:?} ({})",
        t.hyperlinks.value, t.hyperlinks.source
    );
}

fn print_problems(resolved: &ResolvedConfig) {
    if resolved.issues.is_empty() {
        println!("Config problems: none");
        return;
    }
    println!("Config problems:");
    for issue in &resolved.issues {
        let tag = match issue.level {
            IssueLevel::Warning => "warning",
            IssueLevel::Error => "error",
        };
        for (i, line) in issue.message.lines().enumerate() {
            if i == 0 {
                println!("  [{tag}] {line}");
            } else {
                println!("          {line}");
            }
        }
    }
}

/// FR-TH-6, extended (deliverable 3) to user theme files: built-ins first
/// (unchanged from before this chunk), then every loaded user theme's own
/// fg/bg, link/bg, dim/bg ratios. A user theme with `[meta]
/// allow_low_contrast = true` still shows its ratio (so the number is never
/// hidden) but never the `<-- WARN` flag — the same suppression
/// `load_user_themes` itself already applies to the startup warning this
/// report is the on-demand equivalent of.
fn print_contrast_lint(user_themes: &[theme::LoadedUserTheme]) {
    println!(
        "Theme contrast lint (FR-TH-6, warns below {:.1}:1):",
        theme::ContrastCheck::AA_THRESHOLD
    );
    let report = theme::builtin_contrast_report();
    let mut by_theme: Vec<&str> = report.iter().map(|c| c.theme).collect();
    by_theme.sort_unstable();
    by_theme.dedup();
    for theme_name in by_theme {
        for check in report.iter().filter(|c| c.theme == theme_name) {
            let flag = if check.passes() { "" } else { " <-- WARN" };
            println!(
                "  {}: {} = {:.2}:1 [{}]{flag}",
                check.theme,
                check.pair,
                check.ratio,
                check.level()
            );
        }
    }
    // Documented per Appendix C, not a lint finding: `night`'s fg/bg is a
    // deliberate AA-only tradeoff (red held near full brightness so it
    // stays visible dark-adapted), not something to brighten toward AAA.
    if let Some(night_fg) = report
        .iter()
        .find(|c| c.theme == "night" && c.pair == "fg/bg")
        && night_fg.level() == "AA"
    {
        println!("  note: night's fg/bg clears AA but not AAA by design (Appendix C) — not a bug");
    }

    if !user_themes.is_empty() {
        println!("  -- user themes --");
        let user_report = theme::user_contrast_report(user_themes);
        for t in user_themes {
            for check in user_report.iter().filter(|c| c.theme == t.name.as_str()) {
                let flag = if check.passes() || t.allow_low_contrast {
                    ""
                } else {
                    " <-- WARN"
                };
                println!(
                    "  {}: {} = {:.2}:1 [{}]{flag}",
                    check.theme,
                    check.pair,
                    check.ratio,
                    check.level()
                );
            }
        }
    }
}

fn print_capabilities(configured_color_depth: &str) {
    println!("Terminal capabilities:");
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    let truecolor = colorterm == "truecolor" || colorterm == "24bit";
    println!("  truecolor ($COLORTERM={colorterm:?}): {truecolor}");
    println!("  $TERM: {:?}", std::env::var("TERM").unwrap_or_default());
    println!("  NO_COLOR active: {}", crate::no_color_active());
    // PRD FR-TH-3: the depth every theme's colors actually get mapped to —
    // NO_COLOR forces mono the same way it does everywhere else (FR-TH-5's
    // policy layer), otherwise `color_depth` resolves `auto` against the
    // env pair printed just above.
    let depth = if crate::no_color_active() {
        theme::ColorDepth::Mono
    } else {
        theme::resolve_color_depth(configured_color_depth)
    };
    println!(
        "  color_depth = {configured_color_depth:?} -> resolved: {}",
        depth.label()
    );
    let stdout_is_tty = std::io::stdout().is_terminal();
    println!("  stdout is a tty: {stdout_is_tty}");
    match crossterm::terminal::size() {
        Ok((cols, rows)) => println!("  detected size: {cols}x{rows}"),
        Err(_) => println!("  detected size: unknown (not a tty)"),
    }
    let accessible = crate::accessible_active();
    println!("  ACCESSIBLE active: {accessible}");
    // PRD FR-RD-2 / §6.7: `auto`'s own heuristic (a real tty, not
    // ACCESSIBLE) — not a true capability probe (no terminal in the wild
    // reliably self-reports OSC 8 support), but the same rule the reading
    // view itself uses at `hyperlinks = auto`.
    let hyperlink_env = crate::hyperlink::HyperlinkEnv {
        is_tty: stdout_is_tty,
        accessible,
    };
    println!(
        "  OSC 8 hyperlinks (auto heuristic): {}",
        crate::hyperlink::active(crate::hyperlink::HyperlinkMode::Auto, hyperlink_env)
    );
    println!(
        "  clipboard (OSC 52 on yank): attempted unconditionally; silently ignored by a terminal that doesn't support it"
    );
}
