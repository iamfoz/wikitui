use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// wikitui — a terminal Wikipedia reader.
#[derive(Parser, Debug)]
#[command(name = "wikitui", version, about = "A terminal Wikipedia reader")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Article title to open directly, e.g. `wikitui "Alan Turing"`.
    pub title: Option<String>,

    /// Wikipedia language edition (subdomain), e.g. `de` for German.
    /// Unset means "let config/env decide" (PRD §6.7 precedence) — the
    /// eventual default is `en`, but only once every lower-precedence
    /// layer has had its say.
    #[arg(long)]
    pub lang: Option<String>,

    /// Open straight into search results for this query instead of an
    /// article.
    #[arg(long)]
    pub search: Option<String>,

    /// Render the article as plain text to stdout and exit (FR-RD-12): no
    /// alternate screen, no cursor addressing. Requires a title.
    #[arg(long)]
    pub dump: bool,

    /// Color theme: terminal (default), full, homebrew, night, paper,
    /// contrast. Press `T` at runtime to cycle through them. Unset means
    /// "let config/env decide" (PRD §6.7).
    #[arg(long)]
    pub theme: Option<String>,

    /// Maximum line measure in cells (FR-RD-9). Unset means "let
    /// config/env decide"; the eventual default is 88, clamped to 40..=200.
    #[arg(long)]
    pub measure: Option<u16>,

    /// East-Asian-Ambiguous width: 1 (narrow, default) or 2 (wide),
    /// per FR-RD-10.
    #[arg(long, value_name = "1|2")]
    pub ambiguous_width: Option<u8>,

    /// Citation style for Research mode and `--export-bibliography`: apa
    /// (default), harvard, mla, chicago.
    #[arg(long)]
    pub cite_style: Option<String>,

    /// Print the saved research bibliography to stdout in the given
    /// citation style (apa, harvard, mla, chicago) and exit — pipe it
    /// wherever you like: `wikitui --export-bibliography apa > refs.md`.
    #[arg(long, value_name = "STYLE")]
    pub export_bibliography: Option<String>,

    /// Path to `config.toml`, overriding both the platform config
    /// directory and `WIKITUI_CONFIG` (PRD §6.7).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Configuration-related subcommands.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Validate the resolved config, lint theme contrast (FR-TH-6), and
    /// report terminal capabilities — plain stdout, no TUI (PRD §6.7).
    Doctor,
}
