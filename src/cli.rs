use clap::Parser;

/// wikitui — a terminal Wikipedia reader.
#[derive(Parser, Debug)]
#[command(name = "wikitui", version, about = "A terminal Wikipedia reader")]
pub struct Cli {
    /// Article title to open directly, e.g. `wikitui "Alan Turing"`.
    pub title: Option<String>,

    /// Wikipedia language edition (subdomain), e.g. `de` for German.
    #[arg(long, default_value = "en")]
    pub lang: String,

    /// Open straight into search results for this query instead of an
    /// article.
    #[arg(long)]
    pub search: Option<String>,

    /// Render the article as plain text to stdout and exit (FR-RD-12): no
    /// alternate screen, no cursor addressing. Requires a title.
    #[arg(long)]
    pub dump: bool,

    /// Color theme: terminal (default), full, homebrew, night, paper,
    /// contrast. Press `T` at runtime to cycle through them.
    #[arg(long, default_value = "terminal")]
    pub theme: String,

    /// Print the saved research bibliography to stdout in the given
    /// citation style (apa, harvard, mla, chicago) and exit — pipe it
    /// wherever you like: `wikitui --export-bibliography apa > refs.md`.
    #[arg(long, value_name = "STYLE")]
    pub export_bibliography: Option<String>,
}
