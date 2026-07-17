//! Rule-based syntax highlighting for code blocks (PRD FR-RD-1).
//!
//! **Why not `syntect`.** The obvious library choice pulls ~48 transitive
//! crates (flate2, plist, quick-xml, yaml-rust, fancy-regex, bincode,
//! walkdir, …) and bundles a set of `.tmTheme` colors — colors this build
//! would then *discard*, because FR-RD-1's highlighting has to map onto the
//! active wikitui *theme*'s slots so it honors the FR-TH-3 capability
//! degradation pipeline and `NO_COLOR` (FR-TH-5). Using syntect purely as a
//! tokenizer means dragging in all that weight for the scope-stack machinery
//! and throwing its one real deliverable (the theme) away. The codebase
//! already rejects heavy/native deps on exactly these grounds (see
//! `Cargo.toml`'s libzim/libdbus notes). So this is a small, dependency-free
//! tokenizer covering the languages Wikipedia code samples most commonly use
//! (Rust, Python, C-family, JavaScript, shell). An unrecognized language hint
//! resolves to `None` and the block renders uniformly in `theme.code` — the
//! documented plain fallback, never an error.
//!
//! **Scope of the tokenizer.** It is deliberately line-oriented, carrying
//! only one piece of state across lines: whether a C-style block comment
//! (`/* … */`) is open. Single-line strings and line/block comments,
//! keywords, and numeric literals are recognized; multi-line strings
//! (Python triple-quotes, shell heredocs) are *not* tracked across lines —
//! an unterminated string simply colors to end of line. This is the
//! "adequate, documented" tier FR-RD-1 needs for a terminal reader, not a
//! full grammar.

/// The token classes the highlighter emits. `layout.rs` maps each onto a
/// `SpanKind`/theme slot at paint time (so degradation + `NO_COLOR` are
/// handled once, centrally). `Plain` covers everything uncategorized —
/// identifiers, operators, punctuation, whitespace — and renders in the base
/// `theme.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Keyword,
    Str,
    Comment,
    Number,
}

/// The languages the rule-based highlighter covers (PRD FR-RD-1). Anything
/// else is `None` from [`Lang::detect`] and renders plain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    C,
    JavaScript,
    Shell,
}

impl Lang {
    /// Resolve a raw language hint (`doc::code_lang_from_class`'s output — a
    /// lower-cased `lang-X`/`language-X`/`mw-highlight-lang-X` token) to a
    /// covered language, or `None` for the plain fallback. The alias sets
    /// mirror what MediaWiki's SyntaxHighlight (Pygments) and Parsoid actually
    /// emit.
    pub fn detect(hint: &str) -> Option<Lang> {
        match hint.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(Lang::Rust),
            "python" | "py" | "python3" | "py3" => Some(Lang::Python),
            "c" | "h" | "cpp" | "c++" | "cc" | "cxx" | "hpp" | "objc" => Some(Lang::C),
            "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" | "node" => {
                Some(Lang::JavaScript)
            }
            "shell" | "sh" | "bash" | "zsh" | "console" | "shell-session" | "shellsession" => {
                Some(Lang::Shell)
            }
            _ => None,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while", "Box",
                "Option", "Result", "Some", "None", "Ok", "Err", "Vec", "String",
            ],
            Lang::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield", "self",
            ],
            Lang::C => &[
                "auto",
                "break",
                "case",
                "char",
                "const",
                "continue",
                "default",
                "do",
                "double",
                "else",
                "enum",
                "extern",
                "float",
                "for",
                "goto",
                "if",
                "inline",
                "int",
                "long",
                "register",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "struct",
                "switch",
                "typedef",
                "union",
                "unsigned",
                "void",
                "volatile",
                "while",
                "bool",
                "class",
                "namespace",
                "new",
                "delete",
                "public",
                "private",
                "protected",
                "template",
                "true",
                "false",
                "nullptr",
                "NULL",
            ],
            Lang::JavaScript => &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "debugger",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "let",
                "new",
                "null",
                "of",
                "return",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "with",
                "yield",
            ],
            Lang::Shell => &[
                "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case",
                "esac", "in", "function", "select", "return", "break", "continue", "echo",
                "export", "local", "readonly", "declare", "unset", "set", "shift", "source",
                "alias", "trap", "exit",
            ],
        }
    }

    /// The line-comment introducers (`//`, `#`). Longest-match-first isn't
    /// needed here since none is a prefix of another within one language.
    fn line_comments(self) -> &'static [&'static str] {
        match self {
            Lang::Rust | Lang::C | Lang::JavaScript => &["//"],
            Lang::Python | Lang::Shell => &["#"],
        }
    }

    /// The C-style block-comment delimiters, for the languages that have them.
    fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Lang::Rust | Lang::C | Lang::JavaScript => Some(("/*", "*/")),
            Lang::Python | Lang::Shell => None,
        }
    }

    /// The string-quote characters. JS adds the backtick template literal;
    /// shell keeps `'`/`"` (the highlighter doesn't try to model `$(…)`).
    fn string_quotes(self) -> &'static [char] {
        match self {
            Lang::JavaScript => &['"', '\'', '`'],
            _ => &['"', '\''],
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A line-oriented tokenizer that carries C-style block-comment state across
/// lines (see the module doc comment for the deliberate scope limits).
pub struct Highlighter {
    lang: Lang,
    in_block_comment: bool,
}

impl Highlighter {
    pub fn new(lang: Lang) -> Self {
        Highlighter {
            lang,
            in_block_comment: false,
        }
    }

    /// Tokenize one source line into `(text, kind)` runs. The concatenation
    /// of every run's text is exactly the input line (nothing is dropped or
    /// reordered), so the caller can lay the runs out in place. Adjacent
    /// `Plain` runs are merged; the width/wrapping layer coalesces the rest.
    pub fn highlight_line(&mut self, line: &str) -> Vec<(String, TokenKind)> {
        let chars: Vec<char> = line.chars().collect();
        let mut out: Vec<(String, TokenKind)> = Vec::new();
        let mut plain = String::new();
        let mut i = 0;

        let flush_plain = |plain: &mut String, out: &mut Vec<(String, TokenKind)>| {
            if !plain.is_empty() {
                out.push((std::mem::take(plain), TokenKind::Plain));
            }
        };

        // Continuation of a block comment opened on an earlier line.
        if self.in_block_comment {
            let (_open, close) = self.lang.block_comment().expect("in_block_comment set");
            if let Some(end) = find_sub(&chars, 0, close) {
                let upto = end + close.chars().count();
                out.push((chars[..upto].iter().collect(), TokenKind::Comment));
                self.in_block_comment = false;
                i = upto;
            } else {
                out.push((chars.iter().collect(), TokenKind::Comment));
                return out;
            }
        }

        while i < chars.len() {
            let c = chars[i];

            // Line comment: the rest of the line.
            if let Some(cmt) = self
                .lang
                .line_comments()
                .iter()
                .find(|p| starts_with(&chars, i, p))
            {
                flush_plain(&mut plain, &mut out);
                out.push((chars[i..].iter().collect(), TokenKind::Comment));
                let _ = cmt;
                return out;
            }

            // Block comment start.
            if let Some((open, close)) = self.lang.block_comment()
                && starts_with(&chars, i, open)
            {
                flush_plain(&mut plain, &mut out);
                let start = i;
                if let Some(end) = find_sub(&chars, i + open.chars().count(), close) {
                    let upto = end + close.chars().count();
                    out.push((chars[start..upto].iter().collect(), TokenKind::Comment));
                    i = upto;
                } else {
                    out.push((chars[start..].iter().collect(), TokenKind::Comment));
                    self.in_block_comment = true;
                    return out;
                }
                continue;
            }

            // String literal (single-line; unterminated colors to EOL).
            if self.lang.string_quotes().contains(&c) {
                flush_plain(&mut plain, &mut out);
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && self.lang != Lang::Shell {
                        i = (i + 2).min(chars.len());
                        continue;
                    }
                    if chars[i] == c {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push((chars[start..i].iter().collect(), TokenKind::Str));
                continue;
            }

            // Numeric literal at a token boundary.
            if c.is_ascii_digit() {
                flush_plain(&mut plain, &mut out);
                let start = i;
                i += 1;
                while i < chars.len() {
                    let d = chars[i];
                    if d.is_ascii_alphanumeric() || d == '.' || d == '_' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push((chars[start..i].iter().collect(), TokenKind::Number));
                continue;
            }

            // Identifier / keyword.
            if is_ident_start(c) {
                let start = i;
                i += 1;
                while i < chars.len() && is_ident_continue(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if self.lang.keywords().contains(&word.as_str()) {
                    flush_plain(&mut plain, &mut out);
                    out.push((word, TokenKind::Keyword));
                } else {
                    plain.push_str(&word);
                }
                continue;
            }

            // Anything else: whitespace, operators, punctuation.
            plain.push(c);
            i += 1;
        }

        flush_plain(&mut plain, &mut out);
        out
    }
}

/// Does `chars[i..]` start with `needle`?
fn starts_with(chars: &[char], i: usize, needle: &str) -> bool {
    let n: Vec<char> = needle.chars().collect();
    i + n.len() <= chars.len() && chars[i..i + n.len()] == n[..]
}

/// The index of the first occurrence of `needle` in `chars[from..]`, in
/// `chars`-space, or `None`.
fn find_sub(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || from > chars.len() {
        return None;
    }
    (from..=chars.len().saturating_sub(n.len())).find(|&i| chars[i..i + n.len()] == n[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_language_aliases() {
        assert_eq!(Lang::detect("rust"), Some(Lang::Rust));
        assert_eq!(Lang::detect("py"), Some(Lang::Python));
        assert_eq!(Lang::detect("BASH"), Some(Lang::Shell));
        assert_eq!(Lang::detect("cpp"), Some(Lang::C));
        assert_eq!(Lang::detect("ts"), Some(Lang::JavaScript));
        assert_eq!(Lang::detect("brainfuck"), None);
        assert_eq!(Lang::detect(""), None);
    }

    /// The FR-RD-1 headline: distinct token classes get distinct kinds, so a
    /// keyword, a string, and a comment can render in three different colors.
    #[test]
    fn rust_line_splits_keyword_string_and_comment() {
        let mut h = Highlighter::new(Lang::Rust);
        let toks = h.highlight_line(r#"let x = "hi"; // note"#);
        let kinds: Vec<TokenKind> = toks.iter().map(|(_, k)| *k).collect();
        assert!(kinds.contains(&TokenKind::Keyword), "let is a keyword");
        assert!(kinds.contains(&TokenKind::Str), "\"hi\" is a string");
        assert!(kinds.contains(&TokenKind::Comment), "// note is a comment");
        // Round-trip: the runs concatenate back to the exact input.
        let joined: String = toks.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, r#"let x = "hi"; // note"#);
    }

    #[test]
    fn keyword_only_matches_whole_words() {
        let mut h = Highlighter::new(Lang::Rust);
        // "reference" contains "ref" but must not be flagged a keyword.
        let toks = h.highlight_line("let reference = 1;");
        let ref_tok = toks.iter().find(|(t, _)| t.contains("reference")).unwrap();
        assert_eq!(ref_tok.1, TokenKind::Plain);
        let one = toks.iter().find(|(t, _)| t == "1").unwrap();
        assert_eq!(one.1, TokenKind::Number);
    }

    #[test]
    fn number_literals_are_flagged() {
        let mut h = Highlighter::new(Lang::C);
        let toks = h.highlight_line("int n = 0xFF + 42;");
        let hex = toks.iter().find(|(t, _)| t == "0xFF").unwrap();
        assert_eq!(hex.1, TokenKind::Number);
        assert!(
            toks.iter()
                .any(|(t, k)| t == "int" && *k == TokenKind::Keyword)
        );
    }

    /// A C-style block comment that opens on one line and closes on a later
    /// one keeps its Comment class across the gap — the one piece of
    /// cross-line state the tokenizer carries.
    #[test]
    fn block_comment_spans_multiple_lines() {
        let mut h = Highlighter::new(Lang::C);
        let l1 = h.highlight_line("code; /* open");
        assert_eq!(l1.last().unwrap().1, TokenKind::Comment);
        let l2 = h.highlight_line("still comment");
        assert_eq!(l2.len(), 1);
        assert_eq!(l2[0].1, TokenKind::Comment);
        let l3 = h.highlight_line("end */ code");
        assert_eq!(l3[0].1, TokenKind::Comment);
        // After the close, code is no longer comment.
        assert!(
            l3.iter()
                .any(|(t, k)| t.contains("code") && *k == TokenKind::Plain)
        );
    }

    #[test]
    fn python_uses_hash_comments_not_slashes() {
        let mut h = Highlighter::new(Lang::Python);
        let toks = h.highlight_line("x = 1  # a comment");
        assert!(
            toks.iter()
                .any(|(t, k)| t.starts_with("# a comment") && *k == TokenKind::Comment)
        );
        // A lone `/` is just plain text in Python.
        let mut h2 = Highlighter::new(Lang::Python);
        let toks2 = h2.highlight_line("y = a / b");
        assert!(toks2.iter().all(|(_, k)| *k != TokenKind::Comment));
    }

    /// Every run concatenates back to the input for arbitrary text — the
    /// invariant the layout relies on to place runs without losing bytes.
    #[test]
    fn runs_always_reconstruct_the_input_line() {
        for lang in [
            Lang::Rust,
            Lang::Python,
            Lang::C,
            Lang::JavaScript,
            Lang::Shell,
        ] {
            let mut h = Highlighter::new(lang);
            for line in [
                "",
                "   ",
                "fn main() { let s = \"x\"; }",
                "def f(): return 3.14  # pi",
                "echo \"$HOME\" # home",
                "a /* b */ c 0x1f 'q'",
            ] {
                let toks = h.highlight_line(line);
                let joined: String = toks.iter().map(|(t, _)| t.as_str()).collect();
                assert_eq!(joined, line, "lang {lang:?} line {line:?}");
            }
        }
    }
}
