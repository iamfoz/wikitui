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

    /// Incognito mode (FR-CS-6, FR-PR-3): no reading-history writes for
    /// this run — see `App.incognito`'s doc comment for the full gate
    /// (stats/interest-model/prefetch suppression arrive with the rest of
    /// FR-PR-3, not built yet).
    #[arg(long)]
    pub incognito: bool,

    /// Skip the first-run onboarding tour (FR-CS-8) even when no config file
    /// exists yet. Scripting and `--dump` never trigger onboarding anyway;
    /// this is for an interactive session that wants to opt out.
    #[arg(long)]
    pub no_onboarding: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Configuration-related subcommands.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// PRD FR-PR-4: delete local persistence stores by name. Runs before any
    /// terminal/network/cache initialization (like `config doctor`) — see
    /// `cleardata`'s module doc comment for exactly what `--all` covers (not
    /// bookmarks, saved pages, or the research bibliography — those are
    /// user-created libraries, not tracking data).
    ClearData {
        /// Deletes the local reading-history database (PRD FR-HS-1).
        #[arg(long)]
        history: bool,
        /// Deletes the page cache (PRD §5.7's L2 store).
        #[arg(long)]
        cache: bool,
        /// Deletes local reading stats (PRD FR-PC-3) — a seam today: no
        /// stats file exists yet, so this reports a no-op.
        #[arg(long)]
        stats: bool,
        /// Deletes locally stored auth tokens (PRD FR-ACC-9) — a seam
        /// today: login doesn't exist yet, so this reports a no-op.
        #[arg(long)]
        auth: bool,
        /// Shorthand for `--history --cache --stats --auth`. Does **not**
        /// include bookmarks, saved pages, read-later, or the research
        /// bibliography — see this command's own doc comment.
        #[arg(long)]
        all: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Validate the resolved config, lint theme contrast (FR-TH-6), and
    /// report terminal capabilities — plain stdout, no TUI (PRD §6.7).
    Doctor,
}
