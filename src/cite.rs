//! Bibliographic styles for exporting the research bibliography.
//!
//! Only `CitationKind::Article` entries have known structure (subject,
//! wiki, URL, access date), so only those are genuinely formatted per
//! style. `Reference` entries are raw text scraped from an article's
//! References section: per both style-guide rules (the work actually
//! consulted was Wikipedia, not the sources it cites) and publishing
//! practice for unparsed citation strings (JATS `<mixed-citation>`,
//! Crossref `unstructured`), they are exported verbatim in a separate,
//! clearly-labeled section with each style's secondary-source wording
//! ("as cited in" / "qtd. in" / "cited in" / "quoted in") pointing back to
//! the article they were found in — never silently interleaved as if they
//! had been parsed.
//!
//! Formats verified against style-guide sources (APA Style, Purdue OWL,
//! university library guides, Cite Them Right, CMOS 14.233); since output
//! is plain text, italics conventions become plain words. Wikipedia has no
//! individual author and no stable publish date, so the no-date ("n.d.")
//! conventions apply, with an access date — the same approach as
//! Wikipedia's own Special:CiteThisPage.

use chrono::{Datelike, NaiveDate};

use crate::research::{CitationKind, SavedCitation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiteStyle {
    Apa,
    Harvard,
    Mla,
    Chicago,
}

impl CiteStyle {
    pub const NAMES: [&'static str; 4] = ["apa", "harvard", "mla", "chicago"];

    pub fn by_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "apa" => Some(Self::Apa),
            "harvard" => Some(Self::Harvard),
            "mla" => Some(Self::Mla),
            "chicago" => Some(Self::Chicago),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Apa => "apa",
            Self::Harvard => "harvard",
            Self::Mla => "mla",
            Self::Chicago => "chicago",
        }
    }

    /// Display name for UI chrome.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Apa => "APA 7",
            Self::Harvard => "Harvard (Cite Them Right)",
            Self::Mla => "MLA 9",
            Self::Chicago => "Chicago 17",
        }
    }

    /// The next style in `NAMES` order, wrapping — the library view's `s`
    /// key cycles through these for live preview.
    pub fn next(&self) -> Self {
        match self {
            Self::Apa => Self::Harvard,
            Self::Harvard => Self::Mla,
            Self::Mla => Self::Chicago,
            Self::Chicago => Self::Apa,
        }
    }
}

/// "July 11, 2026" — APA/Chicago's month-day-year order.
fn date_mdy(iso: &str) -> String {
    NaiveDate::parse_from_str(iso, "%Y-%m-%d")
        .map(|d| d.format("%B %-d, %Y").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

/// "11 July 2026" — Harvard's day-month-year order, month in full.
fn date_dmy(iso: &str) -> String {
    NaiveDate::parse_from_str(iso, "%Y-%m-%d")
        .map(|d| d.format("%-d %B %Y").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

/// "11 July 2026" / "11 Sept. 2026" — MLA's day-month-year order with its
/// month abbreviations: months longer than four letters are abbreviated
/// (May, June, July stay in full).
fn date_mla(iso: &str) -> String {
    let Ok(d) = NaiveDate::parse_from_str(iso, "%Y-%m-%d") else {
        return iso.to_string();
    };
    const MLA_MONTHS: [&str; 12] = [
        "Jan.", "Feb.", "Mar.", "Apr.", "May", "June", "July", "Aug.", "Sept.", "Oct.", "Nov.",
        "Dec.",
    ];
    format!(
        "{} {} {}",
        d.day(),
        MLA_MONTHS[d.month0() as usize],
        d.year()
    )
}

/// MLA renders URLs without the scheme.
fn url_without_scheme(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
}

/// Formats one saved entry in the given style, as a single plain-text line.
pub fn format_citation(citation: &SavedCitation, style: CiteStyle) -> String {
    match citation.kind {
        CitationKind::Article => format_article(citation, style),
        CitationKind::Reference => format_reference(citation, style),
    }
}

fn format_article(citation: &SavedCitation, style: CiteStyle) -> String {
    let title = &citation.source_article;
    // In APA and MLA the template puts a period right after the title, so
    // a title with its own terminal period ("Washington, D.C.") would
    // double it — both styles collapse that to a single period. Harvard
    // and Chicago put a quote/comma there instead, so they keep the full
    // title.
    let title_dotless = title.strip_suffix('.').unwrap_or(title);
    let url = citation.url.as_deref().unwrap_or("");
    match style {
        // Title. (n.d.). In Wikipedia. Retrieved Month D, YYYY, from URL
        // (APA's preferred oldid-permalink variant needs revision metadata
        // the client doesn't fetch yet — the retrieval-date form is the
        // documented alternative for unarchived pages.)
        CiteStyle::Apa => format!(
            "{title_dotless}. (n.d.). In Wikipedia. Retrieved {}, from {url}",
            date_mdy(&citation.saved_at)
        ),
        // 'Title' (n.d.) Wikipedia. Available at: URL (Accessed: D Month YYYY).
        CiteStyle::Harvard => format!(
            "'{title}' (n.d.) Wikipedia. Available at: {url} (Accessed: {}).",
            date_dmy(&citation.saved_at)
        ),
        // "Title." Wikipedia, The Free Encyclopedia, Wikimedia Foundation,
        // URL. Accessed D Mon. YYYY.  (last-modified date omitted: needs
        // revision metadata the client doesn't fetch yet)
        CiteStyle::Mla => format!(
            "\"{title_dotless}.\" Wikipedia, The Free Encyclopedia, Wikimedia Foundation, {}. Accessed {}.",
            url_without_scheme(url),
            date_mla(&citation.saved_at)
        ),
        // Wikipedia, s.v. "Title," accessed Month D, YYYY, URL.
        // (CMOS 14.233's s.v. note form, access-date variant.)
        CiteStyle::Chicago => format!(
            "Wikipedia, s.v. \"{title},\" accessed {}, {url}.",
            date_mdy(&citation.saved_at)
        ),
    }
}

/// A verbatim reference from some article's References section, reproduced
/// as-is with the style's secondary-source wording pointing back to the
/// Wikipedia article it was found in. Genuinely verbatim: nothing is ever
/// stripped from the text (an ellipsis or "?" ending stays exactly as
/// printed) — a period is only *added* when the text has no terminal
/// punctuation of its own.
fn format_reference(citation: &SavedCitation, style: CiteStyle) -> String {
    let text = citation.text.trim_end();
    let sep = if text.ends_with(['.', '?', '!']) {
        ""
    } else {
        "."
    };
    let via = &citation.source_article;
    match style {
        CiteStyle::Apa => format!("{text}{sep} (As cited in \"{via},\" Wikipedia.)"),
        CiteStyle::Harvard => format!("{text}{sep} (Cited in '{via}', Wikipedia.)"),
        CiteStyle::Mla => format!("{text}{sep} Qtd. in \"{via}.\" Wikipedia."),
        CiteStyle::Chicago => format!("{text}{sep} Quoted in \"{via},\" Wikipedia."),
    }
}

/// Escapes the Markdown emphasis/code/link metacharacters that real
/// article titles actually contain ("M*A*S*H (TV series)" would otherwise
/// render as italicized "MASH"). Underscores are deliberately NOT escaped:
/// CommonMark doesn't emphasize intraword `_`, and escaping it would
/// mangle every wiki URL (`Alan\_Turing`).
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '*' | '`' | '[') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The whole bibliography as an export-ready Markdown document: the
/// style-formatted article entries first, then — separately and labeled as
/// such — the verbatim references scraped from those articles. Entries are
/// sorted alphabetically (case-insensitively) within each section, as
/// bibliographies conventionally are.
pub fn format_bibliography(citations: &[SavedCitation], style: CiteStyle) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Bibliography ({} style, exported {})\n",
        style.label(),
        crate::research::today()
    ));
    if citations.is_empty() {
        out.push_str("\n(no saved citations)\n");
        return out;
    }

    let mut section = |heading: &str, kind: CitationKind| {
        let mut lines: Vec<String> = citations
            .iter()
            .filter(|c| c.kind == kind)
            .map(|c| escape_markdown(&format_citation(c, style)))
            .collect();
        if lines.is_empty() {
            return;
        }
        lines.sort_by_key(|line| line.to_lowercase());
        out.push_str(&format!("\n## {heading}\n\n"));
        for line in lines {
            out.push_str("- ");
            out.push_str(&line);
            out.push('\n');
        }
    };

    section("Sources consulted", CitationKind::Article);
    section(
        "References cited within the above articles (verbatim, unparsed)",
        CitationKind::Reference,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn article() -> SavedCitation {
        SavedCitation {
            source_article: "Alan Turing".to_string(),
            source_lang: "en".to_string(),
            text: "\"Alan Turing.\" Wikipedia, The Free Encyclopedia. Wikimedia Foundation."
                .to_string(),
            url: Some("https://en.wikipedia.org/wiki/Alan_Turing".to_string()),
            saved_at: "2026-07-11".to_string(),
            kind: CitationKind::Article,
        }
    }

    fn reference(text: &str) -> SavedCitation {
        SavedCitation {
            source_article: "Alan Turing".to_string(),
            source_lang: "en".to_string(),
            text: text.to_string(),
            url: None,
            saved_at: "2026-07-11".to_string(),
            kind: CitationKind::Reference,
        }
    }

    /// The four article formats, locked verbatim to the worked examples
    /// verified against the style guides (APA Style, Cite Them Right,
    /// MLA library guides, CMOS 14.233's s.v. note form).
    #[test]
    fn article_formats_match_the_style_guide_worked_examples() {
        let c = article();
        assert_eq!(
            format_citation(&c, CiteStyle::Apa),
            "Alan Turing. (n.d.). In Wikipedia. Retrieved July 11, 2026, from https://en.wikipedia.org/wiki/Alan_Turing"
        );
        assert_eq!(
            format_citation(&c, CiteStyle::Harvard),
            "'Alan Turing' (n.d.) Wikipedia. Available at: https://en.wikipedia.org/wiki/Alan_Turing (Accessed: 11 July 2026)."
        );
        assert_eq!(
            format_citation(&c, CiteStyle::Mla),
            "\"Alan Turing.\" Wikipedia, The Free Encyclopedia, Wikimedia Foundation, en.wikipedia.org/wiki/Alan_Turing. Accessed 11 July 2026."
        );
        assert_eq!(
            format_citation(&c, CiteStyle::Chicago),
            "Wikipedia, s.v. \"Alan Turing,\" accessed July 11, 2026, https://en.wikipedia.org/wiki/Alan_Turing."
        );
    }

    /// MLA abbreviates months longer than four letters; May/June/July stay
    /// full. Also proves the day-first ordering.
    #[test]
    fn mla_month_abbreviations() {
        let mut c = article();
        c.saved_at = "2026-09-05".to_string();
        assert!(format_citation(&c, CiteStyle::Mla).ends_with("Accessed 5 Sept. 2026."));
        c.saved_at = "2026-05-01".to_string();
        assert!(format_citation(&c, CiteStyle::Mla).ends_with("Accessed 1 May 2026."));
        c.saved_at = "2026-01-31".to_string();
        assert!(format_citation(&c, CiteStyle::Mla).ends_with("Accessed 31 Jan. 2026."));
    }

    /// Verbatim references get each style's secondary-source wording,
    /// never a fake re-formatting.
    #[test]
    fn reference_formats_use_secondary_source_wording() {
        let c =
            reference("Hodges, Andrew. Alan Turing: The Enigma. Princeton University Press, 2012.");
        let apa = format_citation(&c, CiteStyle::Apa);
        assert!(
            apa.contains("As cited in \"Alan Turing,\" Wikipedia"),
            "{apa}"
        );
        let harvard = format_citation(&c, CiteStyle::Harvard);
        assert!(
            harvard.contains("Cited in 'Alan Turing', Wikipedia"),
            "{harvard}"
        );
        let mla = format_citation(&c, CiteStyle::Mla);
        assert!(mla.contains("Qtd. in \"Alan Turing.\" Wikipedia"), "{mla}");
        let chicago = format_citation(&c, CiteStyle::Chicago);
        assert!(
            chicago.contains("Quoted in \"Alan Turing,\" Wikipedia"),
            "{chicago}"
        );
        // The original text survives verbatim in all four.
        for style in [
            CiteStyle::Apa,
            CiteStyle::Harvard,
            CiteStyle::Mla,
            CiteStyle::Chicago,
        ] {
            assert!(
                format_citation(&c, style).starts_with("Hodges, Andrew. Alan Turing: The Enigma")
            );
        }
    }

    /// A malformed stored date must never panic or vanish — it falls back
    /// to the raw ISO string.
    #[test]
    fn malformed_saved_date_falls_back_to_the_raw_string() {
        let mut c = article();
        c.saved_at = "not-a-date".to_string();
        assert!(format_citation(&c, CiteStyle::Apa).contains("Retrieved not-a-date, from"));
    }

    /// Titles with their own terminal period ("Washington, D.C.") must not
    /// double it where the style's template adds one (APA, MLA); Harvard
    /// and Chicago put a quote/comma there and keep the title intact.
    #[test]
    fn title_ending_in_a_period_is_not_doubled() {
        let mut c = article();
        c.source_article = "Washington, D.C.".to_string();
        let apa = format_citation(&c, CiteStyle::Apa);
        assert!(apa.starts_with("Washington, D.C. (n.d.)."), "{apa}");
        assert!(!apa.contains(".."), "{apa}");
        let mla = format_citation(&c, CiteStyle::Mla);
        assert!(mla.starts_with("\"Washington, D.C.\""), "{mla}");
        assert!(!mla.contains(".."), "{mla}");
        let chicago = format_citation(&c, CiteStyle::Chicago);
        assert!(
            chicago.contains("\"Washington, D.C.,\""),
            "Chicago keeps the full title: {chicago}"
        );
        let harvard = format_citation(&c, CiteStyle::Harvard);
        assert!(harvard.starts_with("'Washington, D.C.'"), "{harvard}");
    }

    /// "Verbatim" means verbatim: an ellipsis or question-mark ending
    /// survives untouched, trailing whitespace doesn't produce ". .", and
    /// a period is only added when the text has no terminal punctuation.
    #[test]
    fn reference_text_is_truly_verbatim() {
        let ellipsis = reference("The Long Title...");
        assert!(
            format_citation(&ellipsis, CiteStyle::Apa)
                .starts_with("The Long Title... (As cited in"),
            "{}",
            format_citation(&ellipsis, CiteStyle::Apa)
        );

        let question = reference("Is God Real?");
        assert!(
            format_citation(&question, CiteStyle::Apa).starts_with("Is God Real? (As cited in")
        );

        let trailing_space = reference("Trailing space. ");
        assert!(
            format_citation(&trailing_space, CiteStyle::Apa)
                .starts_with("Trailing space. (As cited in")
        );

        let no_punctuation = reference("No punctuation at all");
        assert!(
            format_citation(&no_punctuation, CiteStyle::Apa)
                .starts_with("No punctuation at all. (As cited in")
        );
    }

    /// "M*A*S*H (TV series)" must survive a Markdown renderer — emphasis
    /// metacharacters are escaped in the export document (but underscores
    /// aren't, so wiki URLs stay copy-pasteable).
    #[test]
    fn bibliography_escapes_markdown_emphasis_in_titles() {
        let mut c = article();
        c.source_article = "M*A*S*H (TV series)".to_string();
        let bib = format_bibliography(&[c], CiteStyle::Apa);
        assert!(bib.contains("M\\*A\\*S\\*H"), "{bib}");
        assert!(
            bib.contains("en.wikipedia.org/wiki/Alan_Turing"),
            "URL underscores must stay unescaped: {bib}"
        );
    }

    #[test]
    fn bibliography_separates_articles_from_verbatim_references() {
        let citations = vec![
            reference("Zeta source"),
            article(),
            reference("Alpha source"),
        ];
        let bib = format_bibliography(&citations, CiteStyle::Apa);

        let sources_pos = bib
            .find("## Sources consulted")
            .expect("articles section present");
        let refs_pos = bib
            .find("## References cited within")
            .expect("verbatim section present");
        assert!(
            sources_pos < refs_pos,
            "style-formatted sources must come before verbatim refs"
        );

        // Alphabetical within the verbatim section.
        let alpha = bib.find("Alpha source").unwrap();
        let zeta = bib.find("Zeta source").unwrap();
        assert!(alpha < zeta);
    }

    #[test]
    fn bibliography_omits_empty_sections() {
        let only_articles = vec![article()];
        let bib = format_bibliography(&only_articles, CiteStyle::Harvard);
        assert!(bib.contains("## Sources consulted"));
        assert!(
            !bib.contains("## References cited within"),
            "no verbatim section when there are no references"
        );
    }

    #[test]
    fn empty_bibliography_says_so_instead_of_emitting_headers() {
        let bib = format_bibliography(&[], CiteStyle::Mla);
        assert!(bib.contains("(no saved citations)"));
        assert!(!bib.contains("## "));
    }

    #[test]
    fn style_round_trips_by_name_and_cycles_through_all() {
        for name in CiteStyle::NAMES {
            assert_eq!(CiteStyle::by_name(name).unwrap().name(), name);
        }
        assert_eq!(
            CiteStyle::by_name("APA").unwrap(),
            CiteStyle::Apa,
            "case-insensitive"
        );
        assert!(CiteStyle::by_name("vancouver").is_none());

        let mut style = CiteStyle::Apa;
        let mut seen = vec![style.name()];
        for _ in 0..CiteStyle::NAMES.len() - 1 {
            style = style.next();
            seen.push(style.name());
        }
        assert_eq!(seen, CiteStyle::NAMES.to_vec());
        assert_eq!(style.next(), CiteStyle::Apa, "cycle wraps");
    }
}
