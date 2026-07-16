//! PRD FR-OFF-8: a Kiwix ZIM read-backend — opening a `.zim` archive as an
//! offline source, bridging the online (MediaWiki API) and Kiwix (bundled-
//! archive) worlds §3.3 draws a line between ("Kiwix/ZIM solves 'all of
//! Wikipedia offline'; wikitui saves *pages you chose*"). This module is the
//! bridge: read-only, local, no network — a ZIM article's HTML feeds the
//! same `doc::parse_article_html` pipeline a Parsoid fetch or a saved page
//! does.
//!
//! ## Why this crate, not a `libzim` binding
//!
//! §6.1's accepted-cost list says "Rust ZIM bindings are immature" — true of
//! bindings to the real `libzim` C++ library specifically. This chunk
//! evaluated both crates.io options:
//!
//! - `zim-rs` (0.1.1, wrapping `zim-sys` 0.1.0's `cxx` binding to `libzim`):
//!   fails to build in any environment without `libzim-dev`/`libzim.pc`
//!   installed — confirmed here, `zim-sys`'s build script panics in
//!   `pkg-config --libs --cflags libzim` ("Package libzim was not found").
//!   Same shape of failure as the `keychain` feature's `libdbus-1`
//!   requirement (see `Cargo.toml`), except unconditional: `zim-sys` has no
//!   feature gate to hide behind, so taking this path would either break the
//!   default build here or require inventing one.
//! - `zim` (0.4.0, `dignifiedquire/zim`): a from-scratch, pure-Rust,
//!   mmap-based reader with no C dependency. Builds clean. Read-only (no
//!   write path exists — exactly what FR-OFF-8 needs). Targets ZIM major
//!   versions 5 and 6, decoding Zstd/LZMA2/uncompressed clusters — what
//!   real Kiwix downloads use. Its own compression decoder has a `todo!()`
//!   gap for the legacy Bzip2/Zlib tags (nobody's `zimwriterfs` has produced
//!   those in years); `unsupported_compression` below turns that from a
//!   would-be process-crashing panic into a clean [`ZimError`].
//!
//! Verdict: pure-Rust ZIM reading is viable today via the `zim` crate —
//! narrower than `libzim`'s (unreachable-here) feature set, but sufficient
//! for read-only article extraction, which is all this backend needs.
//!
//! ## Scope of this chunk
//!
//! - Open an archive, look up an article by title (spaces/underscores/case
//!   folded, redirects followed), extract its HTML.
//! - A lightweight title-substring search (`search_titles`) for the
//!   no-exact-match case — not full-text: that would mean decompressing
//!   every cluster up front, which this chunk does not attempt.
//! - Wired as an offline source: `--zim <file>` / `:zim open <file>` /
//!   `:zim <title>` (see `main.rs`), and as a fallback `open_title` tries
//!   after the network and pinned saved pages both come up empty.
//! - Not attempted: writing ZIM files (never needed — FR-OFF-8 is read-
//!   only), a picker/browse UI over an archive's full title list (`:zim
//!   <title>`'s no-match path lists candidates in the status line instead),
//!   and hardening every possible malformed-file panic in the underlying
//!   crate (see `checked_entry`/`checked_cluster`'s doc comments for the one
//!   gap — out-of-range blob indices — this chunk leaves unguarded).

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use zim::{DirectoryEntry, MimeType, Namespace, Target, Zim};

/// Redirect hops followed before giving up (PRD SEC-3's "pathological-input"
/// posture, applied to a corrupt or cyclic redirect chain rather than HTML
/// nesting depth). Real archives never chain more than one or two.
const MAX_REDIRECT_HOPS: u8 = 10;

/// Everything that can go wrong opening or reading a ZIM archive — surfaced
/// verbatim in status-bar notices (`main.rs`'s `:zim`/`--zim` handlers), so
/// every variant's `Display` is a complete, user-facing sentence fragment.
#[derive(Debug)]
pub enum ZimError {
    /// Anything the underlying `zim` crate itself reported: bad magic
    /// number, unsupported major version, an I/O error opening the file, an
    /// out-of-bounds offset in the directory/cluster tables. Its own
    /// `Display` already says which.
    Archive(zim::Error),
    /// A cluster's compression tag is one this build's `zim` 0.4.0 never
    /// implements decoding for (see this module's doc comment).
    UnsupportedCompression(&'static str),
    /// A directory/cluster index read from the file pointed outside the
    /// archive's own bounds — see `checked_entry`/`checked_cluster`.
    Corrupt(String),
    NotFound(String),
    /// The title resolved to a real entry, but not an HTML one (an image,
    /// stylesheet, or other non-article content).
    NotArticle(String),
    TooManyRedirects(String),
    InvalidUtf8(String),
}

impl fmt::Display for ZimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archive(e) => write!(f, "{e}"),
            Self::UnsupportedCompression(name) => write!(
                f,
                "this article's cluster uses {name} compression, which this build can't decode (zstd, lzma2, and uncompressed only)"
            ),
            Self::Corrupt(msg) => write!(f, "malformed archive: {msg}"),
            Self::NotFound(t) => write!(f, "no article titled {t:?} in this archive"),
            Self::NotArticle(t) => {
                write!(f, "{t:?} exists in this archive but isn't an HTML article")
            }
            Self::TooManyRedirects(t) => {
                write!(f, "{t:?}'s redirect chain is too long (possible loop)")
            }
            Self::InvalidUtf8(t) => write!(f, "{t:?}'s content isn't valid UTF-8 text"),
        }
    }
}

impl std::error::Error for ZimError {}

/// An opened ZIM archive (PRD FR-OFF-8): read-only, offline, mmap-backed by
/// the pure-Rust `zim` crate (see module doc for why this crate). `App.zim`
/// holds at most one of these at a time — this chunk doesn't attempt
/// multi-archive sessions.
pub struct ZimArchive {
    inner: Zim,
    path: PathBuf,
    /// Normalized-title -> (display title, URL-list index), built once at
    /// open time by `build_title_index` — see its doc comment for exactly
    /// which entries qualify. Eager, not lazy: `zim`'s directory iterator is
    /// O(article_count), and re-scanning it on every `:zim <title>` would
    /// make lookup linear in archive size instead of a hash-map hit.
    titles: HashMap<String, (String, u32)>,
}

impl ZimArchive {
    /// Opens and indexes a `.zim` file. Cheap relative to archive size: the
    /// crate mmaps rather than reading the whole file, and indexing walks
    /// the directory once without touching any cluster/blob data.
    pub fn open(path: &Path) -> Result<Self, ZimError> {
        let inner = Zim::new(path).map_err(ZimError::Archive)?;
        let titles = build_title_index(
            inner
                .iterate_by_urls()
                .enumerate()
                .map(|(i, e)| (i as u32, e)),
        );
        Ok(Self {
            inner,
            path: path.to_path_buf(),
            titles,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Total directory entries (articles, images, redirects, metadata —
    /// everything the archive's header counts).
    pub fn article_count(&self) -> usize {
        self.inner.article_count()
    }

    /// How many entries `:zim <title>` can actually resolve — the subset of
    /// `article_count` this module indexed as lookup-able (see
    /// `build_title_index`).
    pub fn indexed_title_count(&self) -> usize {
        self.titles.len()
    }

    /// The archive's designated main/welcome page, if it declares one — the
    /// default `:zim`/`--zim` target when no title is given. Does not
    /// follow redirects itself (unlike `article_html`); `main.rs` feeds
    /// whatever title this returns straight back into `article_html`, which
    /// does.
    pub fn main_page_title(&self) -> Option<String> {
        let idx = self.inner.header.main_page?;
        let entry = checked_entry(&self.inner, idx).ok()?;
        Some(display_title(&entry))
    }

    /// Looks up `title` (folded via `normalize_title`) and returns its
    /// resolved display title and HTML, following redirects up to
    /// [`MAX_REDIRECT_HOPS`].
    pub fn article_html(&self, title: &str) -> Result<(String, String), ZimError> {
        let key = normalize_title(title);
        let Some((_, start_idx)) = self.titles.get(&key) else {
            return Err(ZimError::NotFound(title.to_string()));
        };
        let mut idx = *start_idx;
        let mut hops = 0u8;
        loop {
            let entry = checked_entry(&self.inner, idx)?;
            match entry.target {
                Some(Target::Redirect(next)) => {
                    hops += 1;
                    if hops > MAX_REDIRECT_HOPS {
                        return Err(ZimError::TooManyRedirects(title.to_string()));
                    }
                    idx = next;
                }
                Some(Target::Cluster(cluster_idx, blob_idx)) => {
                    if !is_html_mime(&entry.mime_type) {
                        return Err(ZimError::NotArticle(title.to_string()));
                    }
                    let cluster = checked_cluster(&self.inner, cluster_idx)?;
                    if let Some(name) = unsupported_compression(&cluster) {
                        return Err(ZimError::UnsupportedCompression(name));
                    }
                    let blob = cluster.get_blob(blob_idx).map_err(ZimError::Archive)?;
                    let html = String::from_utf8(blob.to_vec())
                        .map_err(|_| ZimError::InvalidUtf8(title.to_string()))?;
                    return Ok((display_title(&entry), html));
                }
                None => return Err(ZimError::NotArticle(title.to_string())),
            }
        }
    }

    /// Up to `limit` indexed titles containing `query` (case-insensitive
    /// substring, sorted, deduplicated) — the `:zim <title>` no-exact-match
    /// fallback's candidate list. Title-only, not ranked or fuzzy (see
    /// module doc's scope note).
    pub fn search_titles(&self, query: &str, limit: usize) -> Vec<String> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<&str> = self
            .titles
            .values()
            .filter(|(display, _)| display.to_lowercase().contains(&q))
            .map(|(display, _)| display.as_str())
            .collect();
        hits.sort_unstable();
        hits.dedup();
        hits.into_iter().take(limit).map(str::to_string).collect()
    }
}

/// MediaWiki/ZIM titles conventionally appear with spaces (the human-
/// readable `title` field) or underscores (the `url` field); folding both to
/// lowercase-with-underscores means `:zim Alan Turing`, `:zim Alan_Turing`,
/// and `:zim alan turing` all resolve to the same entry — the same
/// normalization `target::parse` already applies to online titles.
fn normalize_title(title: &str) -> String {
    title.trim().replace(' ', "_").to_lowercase()
}

/// A directory entry's human-readable title, falling back to its URL when
/// the title field is empty (the ZIM spec's own documented convention — see
/// `DirectoryEntry::title`'s doc comment in the `zim` crate).
fn display_title(entry: &DirectoryEntry) -> String {
    if entry.title.is_empty() {
        entry.url.clone()
    } else {
        entry.title.clone()
    }
}

fn is_html_mime(mime: &MimeType) -> bool {
    matches!(mime, MimeType::Type(m) if m.starts_with("text/html"))
}

/// Scans every directory entry once and records the ones this module treats
/// as lookup-able "articles": HTML content entries and the redirects that
/// might point at them. ZIM has two directory-namespace schemes in the
/// wild — the classic per-type namespaces (`A` articles, `I`/`J` images, `M`
/// metadata, …) most Kiwix downloads before ~2021 use, and the newer
/// "no-namespace" layout (openzim.org's 2020 restructuring) that files all
/// real content under `C` and disambiguates by MIME type instead. Indexing
/// `Namespace::Articles` unconditionally and `Namespace::UserContent` only
/// when it's HTML or a redirect (`UserContent` also holds images, CSS, JS)
/// covers archives from either era without needing to sniff the ZIM minor
/// version.
fn build_title_index(
    entries: impl Iterator<Item = (u32, DirectoryEntry)>,
) -> HashMap<String, (String, u32)> {
    let mut titles = HashMap::new();
    for (idx, entry) in entries {
        let indexable = match entry.namespace {
            Namespace::Articles => true,
            Namespace::UserContent => {
                matches!(entry.target, Some(Target::Redirect(_))) || is_html_mime(&entry.mime_type)
            }
            _ => false,
        };
        if !indexable {
            continue;
        }
        let display = display_title(&entry);
        titles.insert(normalize_title(&display), (display, idx));
    }
    titles
}

/// Bounds-checks before calling the crate's own `get_by_url_index`, which
/// indexes its internal `Vec` directly and panics out of range. A
/// well-formed archive never has an out-of-range redirect target, but this
/// module opens arbitrary user-supplied files (`:zim open <path>`), and an
/// indexing panic there would take the whole TUI down with it.
fn checked_entry(zim: &Zim, idx: u32) -> Result<DirectoryEntry, ZimError> {
    if idx as usize >= zim.article_count() {
        return Err(ZimError::Corrupt(format!(
            "directory index {idx} is out of range (archive has {})",
            zim.article_count()
        )));
    }
    zim.get_by_url_index(idx).map_err(ZimError::Archive)
}

/// Same bounds-checking rationale as `checked_entry`, for `get_cluster`'s
/// direct indexing into the cluster-offset table. This does not (and, given
/// the crate's API, cannot cheaply) guard the deeper case of an out-of-range
/// *blob* index within an otherwise-valid cluster — `zim` exposes no
/// blob-count query before decompression, so a genuinely malformed cluster
/// can still panic inside `Cluster::get_blob`. Documented, not fixed, in
/// this chunk.
fn checked_cluster(zim: &Zim, idx: u32) -> Result<zim::Cluster<'_>, ZimError> {
    if idx as usize >= zim.header.cluster_count as usize {
        return Err(ZimError::Corrupt(format!(
            "cluster index {idx} is out of range (archive has {})",
            zim.header.cluster_count
        )));
    }
    zim.get_cluster(idx).map_err(ZimError::Archive)
}

/// `zim` 0.4.0's `Cluster::decompress` has an unimplemented `todo!()` panic
/// for Bzip2/Zlib-compressed clusters (real Kiwix archives are Zstd, with
/// Lzma2 in pre-2022 dumps; Bzip2/Zlib are a legacy `zimwriterfs` path
/// nobody produces anymore, but a very old or hand-crafted file could still
/// carry one). `Cluster::compression()` itself never panics — it just
/// returns the tag already parsed at `get_cluster` time — so checking it
/// before ever calling `get_blob` (which is what triggers the decompress
/// path) turns a would-be process crash into a clean [`ZimError`]. The
/// crate's `Compression` type lives in a private module and isn't
/// nameable from outside it (only `Cluster` itself is re-exported), so this
/// reads it back through its plain, un-overridden `#[derive(Debug)]` rather
/// than a type-level match — exact and stable for the pinned `zim = "0.4"`
/// dependency.
fn unsupported_compression(cluster: &zim::Cluster<'_>) -> Option<&'static str> {
    match format!("{:?}", cluster.compression()).as_str() {
        "Bzip2" => Some("bzip2"),
        "Zlib" => Some("zlib"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Pure unit tests: `build_title_index` over hand-built entries, no
    // file I/O (`DirectoryEntry`'s fields are all public, so a `zim` crate
    // consumer can construct one directly without going through the byte
    // format at all). ----------------------------------------------------

    fn html_entry(namespace: Namespace, title: &str, url: &str) -> DirectoryEntry {
        DirectoryEntry {
            mime_type: MimeType::Type("text/html".to_string()),
            namespace,
            revision: None,
            url: url.to_string(),
            title: title.to_string(),
            target: Some(Target::Cluster(0, 0)),
        }
    }

    #[test]
    fn indexes_classic_articles_namespace() {
        let entries = vec![(
            0u32,
            html_entry(Namespace::Articles, "Alan Turing", "Alan_Turing"),
        )];
        let idx = build_title_index(entries.into_iter());
        assert_eq!(
            idx.get("alan_turing"),
            Some(&("Alan Turing".to_string(), 0))
        );
    }

    #[test]
    fn indexes_html_under_modern_usercontent_namespace() {
        let entries = vec![(
            0u32,
            html_entry(Namespace::UserContent, "Alan Turing", "A/Alan_Turing"),
        )];
        let idx = build_title_index(entries.into_iter());
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn excludes_non_html_usercontent_entries() {
        let image = DirectoryEntry {
            mime_type: MimeType::Type("image/png".to_string()),
            namespace: Namespace::UserContent,
            revision: None,
            url: "I/turing.png".to_string(),
            title: String::new(),
            target: Some(Target::Cluster(0, 1)),
        };
        let idx = build_title_index(vec![(0u32, image)].into_iter());
        assert!(
            idx.is_empty(),
            "an image entry must not become a lookup-able title"
        );
    }

    #[test]
    fn excludes_metadata_and_index_namespaces() {
        let meta = DirectoryEntry {
            mime_type: MimeType::Type("text/plain".to_string()),
            namespace: Namespace::Metadata,
            revision: None,
            url: "Title".to_string(),
            title: String::new(),
            target: Some(Target::Cluster(0, 2)),
        };
        let idx = build_title_index(vec![(0u32, meta)].into_iter());
        assert!(idx.is_empty());
    }

    #[test]
    fn indexes_a_redirect_by_its_own_title() {
        let redirect = DirectoryEntry {
            mime_type: MimeType::Redirect,
            namespace: Namespace::Articles,
            revision: None,
            url: "Turing_Alan".to_string(),
            title: "Turing, Alan".to_string(),
            target: Some(Target::Redirect(0)),
        };
        let idx = build_title_index(vec![(1u32, redirect)].into_iter());
        assert_eq!(
            idx.get("turing,_alan"),
            Some(&("Turing, Alan".to_string(), 1))
        );
    }

    #[test]
    fn falls_back_to_url_when_title_is_empty() {
        let entry = html_entry(Namespace::Articles, "", "Alan_Turing");
        let idx = build_title_index(vec![(0u32, entry)].into_iter());
        assert_eq!(
            idx.get("alan_turing"),
            Some(&("Alan_Turing".to_string(), 0))
        );
    }

    #[test]
    fn normalize_title_folds_case_and_spacing() {
        assert_eq!(normalize_title("Alan Turing"), "alan_turing");
        assert_eq!(normalize_title("Alan_Turing"), "alan_turing");
        assert_eq!(normalize_title("  alan turing  "), "alan_turing");
    }

    #[test]
    fn is_html_mime_matches_only_text_html() {
        assert!(is_html_mime(&MimeType::Type("text/html".to_string())));
        assert!(is_html_mime(&MimeType::Type(
            "text/html;charset=utf-8".to_string()
        )));
        assert!(!is_html_mime(&MimeType::Type("image/png".to_string())));
        assert!(!is_html_mime(&MimeType::Redirect));
        assert!(!is_html_mime(&MimeType::LinkTarget));
    }

    // ---- Byte-level fixture: a hand-rolled minimal ZIM v5 file, exercised
    // through the real `Zim::new` mmap-and-parse path. ---------------------

    /// One directory entry for [`build_fixture`].
    enum Entry {
        /// A real HTML article: gets its own blob in the fixture's one
        /// shared cluster.
        Html {
            title: &'static str,
            url: &'static str,
            html: &'static [u8],
        },
        /// A redirect to another entry, addressed by its position in the
        /// `entries` slice passed to `build_fixture`.
        Redirect {
            title: &'static str,
            url: &'static str,
            target: usize,
        },
    }

    /// The fixture's one cluster's compression, per test.
    #[derive(Clone, Copy)]
    enum FixtureCompression {
        None,
        Zstd,
        /// Not a real compressed payload — just the raw compression *tag*,
        /// to exercise `unsupported_compression`'s guard without needing a
        /// real bzip2 encoder (this module never calls `get_blob` on it, so
        /// the bytes after the tag are never actually decoded).
        UnsupportedTag(u8),
    }

    fn encode_dirent(
        mime_id: u16,
        namespace: u8,
        target_bytes: [u8; 4],
        extra: Option<[u8; 4]>,
        url: &str,
        title: &str,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&mime_id.to_le_bytes());
        buf.push(0); // unused parameter-length byte, ignored by the reader
        buf.push(namespace);
        buf.extend_from_slice(&0u32.to_le_bytes()); // revision
        buf.extend_from_slice(&target_bytes);
        if let Some(extra) = extra {
            buf.extend_from_slice(&extra);
        }
        buf.extend_from_slice(url.as_bytes());
        buf.push(0);
        buf.extend_from_slice(title.as_bytes());
        buf.push(0);
        buf
    }

    /// Hand-rolls a minimal, valid ZIM v5 file (openzim.org's header +
    /// directory + cluster format) covering exactly what `ZimArchive`
    /// reads. No external `.zim` binary is checked into the repo — the
    /// format is small enough to encode directly, and doing so here doubles
    /// as an executable spec of exactly which bytes this module depends on.
    /// Deliberately minimal against the real spec: no geo index, one
    /// mimetype ("text/html"), one cluster holding every [`Entry::Html`]'s
    /// blob in order.
    fn build_fixture(
        entries: &[Entry],
        compression: FixtureCompression,
        main_page: Option<u32>,
    ) -> Vec<u8> {
        const HEADER_LEN: u64 = 80;
        const MIME_TABLE: &[u8] = b"text/html\0\0";

        // Directory entries + the parallel blob list (one blob per
        // `Entry::Html`, in the shared cluster).
        let mut dirents = Vec::with_capacity(entries.len());
        let mut blobs: Vec<&[u8]> = Vec::new();
        for entry in entries {
            match *entry {
                Entry::Html { title, url, html } => {
                    let blob_idx = blobs.len() as u32;
                    blobs.push(html);
                    dirents.push(encode_dirent(
                        0,
                        b'A',
                        0u32.to_le_bytes(),
                        Some(blob_idx.to_le_bytes()),
                        url,
                        title,
                    ));
                }
                Entry::Redirect { title, url, target } => {
                    dirents.push(encode_dirent(
                        0xffff,
                        b'A',
                        (target as u32).to_le_bytes(),
                        None,
                        url,
                        title,
                    ));
                }
            }
        }

        // The cluster payload: a blob-offset table followed by the
        // concatenated blobs (see `parse_blob_list`/`get_blob` in the `zim`
        // crate for the exact layout this reproduces).
        let mut payload = Vec::new();
        let table_len = (blobs.len() as u32 + 1) * 4;
        let mut running = table_len;
        payload.extend_from_slice(&running.to_le_bytes());
        for b in &blobs {
            running += b.len() as u32;
            payload.extend_from_slice(&running.to_le_bytes());
        }
        for b in &blobs {
            payload.extend_from_slice(b);
        }

        let (details_byte, cluster_body): (u8, Vec<u8>) = match compression {
            FixtureCompression::None => (0x00, payload),
            FixtureCompression::Zstd => (
                0x05,
                zstd::stream::encode_all(&payload[..], 0).expect("zstd-encode the fixture payload"),
            ),
            FixtureCompression::UnsupportedTag(tag) => (tag, vec![0u8; 4]),
        };
        let mut cluster_bytes = Vec::with_capacity(1 + cluster_body.len());
        cluster_bytes.push(details_byte);
        cluster_bytes.extend_from_slice(&cluster_body);

        // Lay out everything after the fixed 80-byte header: the mime
        // table, then dirents, then the three pointer lists, then the
        // cluster, then a dummy 16-byte checksum (never verified by this
        // module — `verify_checksum` is not called).
        let mut body = Vec::new();
        body.extend_from_slice(MIME_TABLE);

        let mut dirent_offsets = Vec::with_capacity(dirents.len());
        for d in &dirents {
            dirent_offsets.push(HEADER_LEN + body.len() as u64);
            body.extend_from_slice(d);
        }

        let url_ptr_pos = HEADER_LEN + body.len() as u64;
        for off in &dirent_offsets {
            body.extend_from_slice(&off.to_le_bytes());
        }

        let title_ptr_pos = HEADER_LEN + body.len() as u64;
        // Title order is unused by `ZimArchive` (it builds its own index by
        // scanning `iterate_by_urls`), so identity order is fine here.
        for i in 0..dirents.len() as u32 {
            body.extend_from_slice(&i.to_le_bytes());
        }

        let cluster_ptr_pos = HEADER_LEN + body.len() as u64;
        let cluster_start = HEADER_LEN + body.len() as u64 + 8;
        body.extend_from_slice(&cluster_start.to_le_bytes());
        body.extend_from_slice(&cluster_bytes);

        let checksum_pos = HEADER_LEN + body.len() as u64;
        body.extend_from_slice(&[0u8; 16]);

        let mut header = Vec::with_capacity(80);
        header.extend_from_slice(&72_173_914u32.to_le_bytes()); // ZIM magic number (openzim.org)
        header.extend_from_slice(&5u16.to_le_bytes()); // version_major
        header.extend_from_slice(&0u16.to_le_bytes()); // version_minor
        header.extend_from_slice(&[0u8; 16]); // uuid, unused by this reader
        header.extend_from_slice(&(dirents.len() as u32).to_le_bytes()); // article_count
        header.extend_from_slice(&1u32.to_le_bytes()); // cluster_count
        header.extend_from_slice(&url_ptr_pos.to_le_bytes());
        header.extend_from_slice(&title_ptr_pos.to_le_bytes());
        header.extend_from_slice(&cluster_ptr_pos.to_le_bytes());
        header.extend_from_slice(&HEADER_LEN.to_le_bytes()); // mime_list_pos == 80: no geo index
        header.extend_from_slice(&main_page.unwrap_or(0xffff_ffff).to_le_bytes());
        header.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // layout_page: none
        header.extend_from_slice(&checksum_pos.to_le_bytes());
        assert_eq!(header.len(), 80);

        header.extend_from_slice(&body);
        header
    }

    static FIXTURE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A uniquely named path under the OS temp dir, same convention
    /// `cache::tests::temp_cache` uses — process id plus a per-test counter
    /// so parallel test threads never collide.
    fn fixture_path() -> std::path::PathBuf {
        let n = FIXTURE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("wikitui-zim-test-{}-{n}.zim", std::process::id()))
    }

    fn write_fixture(
        entries: &[Entry],
        compression: FixtureCompression,
        main_page: Option<u32>,
    ) -> std::path::PathBuf {
        let bytes = build_fixture(entries, compression, main_page);
        let path = fixture_path();
        std::fs::write(&path, &bytes).expect("write fixture bytes");
        path
    }

    #[test]
    fn opens_and_reads_a_single_uncompressed_article() {
        let path = write_fixture(
            &[Entry::Html {
                title: "Test Article",
                url: "Test_Article",
                html: b"<html><body><p>Hello ZIM.</p></body></html>",
            }],
            FixtureCompression::None,
            Some(0),
        );
        let archive = ZimArchive::open(&path).expect("open fixture archive");
        assert_eq!(archive.article_count(), 1);
        assert_eq!(archive.indexed_title_count(), 1);
        assert_eq!(archive.main_page_title().as_deref(), Some("Test Article"));

        let (title, html) = archive
            .article_html("Test Article")
            .expect("look up article");
        assert_eq!(title, "Test Article");
        assert!(html.contains("Hello ZIM."));
    }

    #[test]
    fn lookup_is_case_and_underscore_insensitive() {
        let path = write_fixture(
            &[Entry::Html {
                title: "Alan Turing",
                url: "Alan_Turing",
                html: b"<html><body>bio</body></html>",
            }],
            FixtureCompression::None,
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        assert!(archive.article_html("alan turing").is_ok());
        assert!(archive.article_html("Alan_Turing").is_ok());
        assert!(archive.article_html("ALAN TURING").is_ok());
    }

    #[test]
    fn unknown_title_is_a_clean_not_found_error() {
        let path = write_fixture(
            &[Entry::Html {
                title: "Test Article",
                url: "Test_Article",
                html: b"<html></html>",
            }],
            FixtureCompression::None,
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        let err = archive.article_html("Nonexistent").unwrap_err();
        assert!(matches!(err, ZimError::NotFound(_)));
        assert!(err.to_string().contains("Nonexistent"));
    }

    #[test]
    fn redirect_resolves_to_the_target_articles_html() {
        let path = write_fixture(
            &[
                Entry::Html {
                    title: "Test Article",
                    url: "Test_Article",
                    html: b"<html><body>real content</body></html>",
                },
                Entry::Redirect {
                    title: "Old Name",
                    url: "Old_Name",
                    target: 0,
                },
            ],
            FixtureCompression::None,
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        let (title, html) = archive.article_html("Old Name").expect("follow redirect");
        assert_eq!(title, "Test Article");
        assert!(html.contains("real content"));
    }

    #[test]
    fn zstd_compressed_cluster_decompresses_correctly() {
        let path = write_fixture(
            &[Entry::Html {
                title: "Zstd Article",
                url: "Zstd_Article",
                html: b"<html><body>compressed content here</body></html>",
            }],
            FixtureCompression::Zstd,
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        let (_, html) = archive.article_html("Zstd Article").unwrap();
        assert!(html.contains("compressed content here"));
    }

    #[test]
    fn unsupported_compression_is_a_clean_error_not_a_panic() {
        let path = write_fixture(
            &[Entry::Html {
                title: "Bzip2 Article",
                url: "Bzip2_Article",
                html: b"<html></html>",
            }],
            FixtureCompression::UnsupportedTag(3), // 3 == Bzip2
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        let err = archive.article_html("Bzip2 Article").unwrap_err();
        assert!(matches!(err, ZimError::UnsupportedCompression("bzip2")));
    }

    #[test]
    fn multiple_articles_share_one_cluster_correctly() {
        let path = write_fixture(
            &[
                Entry::Html {
                    title: "First",
                    url: "First",
                    html: b"<html><body>first body</body></html>",
                },
                Entry::Html {
                    title: "Second",
                    url: "Second",
                    html: b"<html><body>second body</body></html>",
                },
            ],
            FixtureCompression::None,
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        let (_, first) = archive.article_html("First").unwrap();
        let (_, second) = archive.article_html("Second").unwrap();
        assert!(first.contains("first body"));
        assert!(second.contains("second body"));
    }

    #[test]
    fn search_titles_finds_case_insensitive_substrings() {
        let path = write_fixture(
            &[
                Entry::Html {
                    title: "Alan Turing",
                    url: "Alan_Turing",
                    html: b"<html></html>",
                },
                Entry::Html {
                    title: "Alan Partridge",
                    url: "Alan_Partridge",
                    html: b"<html></html>",
                },
                Entry::Html {
                    title: "Computer Science",
                    url: "Computer_Science",
                    html: b"<html></html>",
                },
            ],
            FixtureCompression::None,
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        let hits = archive.search_titles("alan", 10);
        assert_eq!(
            hits,
            vec!["Alan Partridge".to_string(), "Alan Turing".to_string()]
        );
        assert!(archive.search_titles("nonexistent", 10).is_empty());
    }

    #[test]
    fn corrupt_magic_number_is_a_clean_open_error() {
        // `ZimArchive` isn't `Debug` (its inner `zim::Zim` mmap wrapper
        // isn't either), so this checks the `Result` by hand rather than
        // via `unwrap_err`/`assert_eq!`, which would need to format the Ok
        // side too.
        let path = fixture_path();
        std::fs::write(&path, b"not a zim file at all, just garbage bytes").unwrap();
        match ZimArchive::open(&path) {
            Err(ZimError::Archive(_)) => {}
            Err(other) => panic!("expected ZimError::Archive, got a different ZimError: {other}"),
            Ok(_) => panic!("expected an error opening a garbage file"),
        }
    }

    #[test]
    fn parse_article_html_renders_zim_content_through_the_shared_pipeline() {
        // PRD FR-OFF-8: a ZIM article's HTML feeds the same document/render
        // pipeline a network or saved-page article does — no separate
        // renderer.
        let path = write_fixture(
            &[Entry::Html {
                title: "Rendered Article",
                url: "Rendered_Article",
                html: b"<html><body><h2>Section</h2><p>Body text.</p></body></html>",
            }],
            FixtureCompression::None,
            None,
        );
        let archive = ZimArchive::open(&path).unwrap();
        let (title, html) = archive.article_html("Rendered Article").unwrap();
        let document = crate::doc::parse_article_html(&title, &html);
        let plain = crate::doc::render_plain(&document, "en");
        assert!(plain.contains("Section"));
        assert!(plain.contains("Body text."));
    }
}
