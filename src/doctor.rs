//! `wikitui config doctor` (PRD §6.7): a plain-stdout report that runs
//! before any TUI initialization — a broken terminal state must never be
//! the reason a config problem is hard to see. Four sections, in order:
//! resolved config (with per-value provenance), config problems, the
//! FR-TH-6 theme-contrast lint, and a terminal-capability report.
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
    print_resolved_config(resolved);
    println!();
    print_problems(resolved);
    println!();
    print_contrast_lint();
    println!();
    print_capabilities();

    if resolved.has_errors() { 1 } else { 0 }
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

fn print_contrast_lint() {
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
}

fn print_capabilities() {
    println!("Terminal capabilities:");
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    let truecolor = colorterm == "truecolor" || colorterm == "24bit";
    println!("  truecolor ($COLORTERM={colorterm:?}): {truecolor}");
    println!("  $TERM: {:?}", std::env::var("TERM").unwrap_or_default());
    println!("  NO_COLOR active: {}", crate::no_color_active());
    let stdout_is_tty = std::io::stdout().is_terminal();
    println!("  stdout is a tty: {stdout_is_tty}");
    match crossterm::terminal::size() {
        Ok((cols, rows)) => println!("  detected size: {cols}x{rows}"),
        Err(_) => println!("  detected size: unknown (not a tty)"),
    }
}
