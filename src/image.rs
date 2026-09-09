//! Inline-image decoding and the Unicode half-block renderer (PRD FR-RD-8,
//! §6.3). This is wikitui's always-works image path: it turns decoded RGBA
//! pixels into a grid of `▀` (upper-half-block) cells whose foreground is the
//! top pixel and background the bottom pixel of each cell — two vertical
//! pixels per text row, so an `H`-row box carries `2H` rows of vertical
//! resolution. No terminal cooperation is required (unlike kitty/iTerm2/sixel
//! in `graphics.rs`); it is just colored cells, which is why it is the path
//! this environment verifies end to end.
//!
//! The geometry ([`image_box_cells`]) is shared by the layout engine (which
//! reserves the box's rows) and the paint step (which fills them), so the two
//! can never disagree on a box's size. Decoded pixels live in an
//! [`ImageStore`] on `App`, keyed by source URL, populated asynchronously
//! (placeholder until ready, never blocking the UI — the same tokio-spawn +
//! mpsc precedent as article/typeahead loading). Alpha is composited over the
//! theme background at paint time, so the store stays theme-independent.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Hard cap on a decoded image's dimensions (PRD SEC-3 spirit: bound the work
/// a hostile/oversized asset can cause). Thumbnails are far smaller; anything
/// past this is rejected to alt text rather than decoded.
const MAX_IMAGE_DIM: u32 = 4096;

/// quality-M2: caps how many inline/POTD image fetches run at once. Before
/// this bound, `main::request_visible_images` spawned one uncoordinated
/// `tokio::spawn` per not-yet-loaded image with nothing gating how many ran
/// concurrently — a media-heavy article could burst dozens of simultaneous
/// requests at once, competing with whatever else the client was doing. 3 is
/// generous enough that a screenful of images still appears quickly while
/// never approaching a fan-out that saturates the connection.
pub const IMAGE_FETCH_CONCURRENCY: usize = 3;

/// One shared limiter for every inline/POTD image fetch a session spawns
/// (`main::request_visible_images` and `main::request_start_page_image` both
/// acquire a permit before calling `WikiClient::fetch_image` — see the
/// latter's doc comment for why they share one pipeline, and so one budget).
/// A permit is only acquired *inside* the spawned task, after
/// `ImageStore::mark_loading` has already run synchronously in the caller —
/// waiting for a free slot delays the network request itself, never the
/// "loading" placeholder the reader sees this frame.
pub fn new_fetch_limiter() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(IMAGE_FETCH_CONCURRENCY))
}

/// A decoded raster image: tightly-packed RGBA8, `width * height * 4` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl DecodedImage {
    /// Nearest-neighbor sample at source pixel `(x, y)`, clamped in-bounds.
    /// Returns straight (non-premultiplied) RGBA.
    fn sample(&self, x: u32, y: u32) -> [u8; 4] {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let idx = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[idx],
            self.rgba[idx + 1],
            self.rgba[idx + 2],
            self.rgba[idx + 3],
        ]
    }
}

/// Decode PNG/JPEG bytes into RGBA. `None` on any decode failure or an image
/// exceeding [`MAX_IMAGE_DIM`] — the caller then keeps the alt-text
/// placeholder, so a broken/oversized asset degrades, never crashes. Only
/// PNG and JPEG are compiled in (`image` crate features), matching the
/// thumbnail formats Wikimedia serves.
pub fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    let decoded = image::load_from_memory(bytes).ok()?;
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 || width > MAX_IMAGE_DIM || height > MAX_IMAGE_DIM {
        return None;
    }
    Some(DecodedImage {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

/// One painted half-block cell: `▀` drawn with `fg` on `bg`. `fg` is the top
/// pixel of the cell's two-pixel column, `bg` the bottom — the standard
/// half-block encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HalfBlockCell {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
}

/// The half-block glyph every image cell uses.
pub const HALF_BLOCK: &str = "▀";

/// Compute the cell box `(cols, rows)` an image occupies, preserving its
/// aspect ratio and clamped to `max_cols`/`max_rows`. Terminal cells are
/// roughly twice as tall as wide, and each half-block cell stacks **two**
/// vertical pixels, so a `cols × rows` box displays the image across `cols`
/// horizontal and `2 * rows` vertical pixel-samples with an on-screen aspect
/// of `cols : 2 * rows` — matched to the source's `width : height`. Shared by
/// layout (box reservation) and paint (box filling) so they never diverge.
pub fn image_box_cells(px_w: u32, px_h: u32, max_cols: u16, max_rows: u16) -> (u16, u16) {
    let max_cols = max_cols.max(1);
    let max_rows = max_rows.max(1);
    if px_w == 0 || px_h == 0 {
        return (max_cols, 1);
    }
    let (pw, ph) = (f64::from(px_w), f64::from(px_h));
    // Start at the width cap and derive height from aspect: on-screen height
    // in cells = cols * (px_h / px_w) / 2 (the /2 accounts for two pixels per
    // cell row).
    let mut cols = max_cols;
    let mut rows = ((f64::from(cols) * ph / pw) / 2.0).round().max(1.0) as u16;
    if rows > max_rows {
        rows = max_rows;
        // Re-derive width from the clamped height, same aspect relation.
        cols = ((f64::from(rows) * 2.0 * pw / ph).round().max(1.0) as u16).min(max_cols);
    }
    (cols, rows)
}

/// Render a decoded image into `rows` rows of `cols` half-block cells,
/// compositing alpha over `bg` (the theme background). The image is sampled
/// nearest-neighbor into a `cols × 2*rows` grid; each cell takes the top
/// sample as its foreground and the bottom sample as its background — the
/// headline unit-verified path (PRD FR-RD-8). Paint calls [`half_block_row`]
/// one row at a time (a reserved [`crate::layout::SpanKind::ImageRow`] line);
/// this whole-box form is the tested reference.
#[cfg(test)]
pub fn render_half_blocks(
    img: &DecodedImage,
    cols: u16,
    rows: u16,
    bg: (u8, u8, u8),
) -> Vec<Vec<HalfBlockCell>> {
    (0..rows.max(1))
        .map(|r| half_block_row(img, cols, rows, r, bg))
        .collect()
}

/// Render a single cell row `row` of the `cols × rows` half-block box. Paint
/// calls this once per reserved image line (`SpanKind::ImageRow`), so the
/// per-frame cost is O(cols) per line rather than re-rendering the whole box.
pub fn half_block_row(
    img: &DecodedImage,
    cols: u16,
    rows: u16,
    row: u16,
    bg: (u8, u8, u8),
) -> Vec<HalfBlockCell> {
    let cols = cols.max(1);
    let rows = rows.max(1);
    let grid_h = u32::from(rows) * 2;
    let top_y = (u32::from(row) * 2 * img.height) / grid_h;
    let bottom_y = ((u32::from(row) * 2 + 1) * img.height) / grid_h;
    (0..cols)
        .map(|c| {
            let sx = (u32::from(c) * img.width) / u32::from(cols);
            HalfBlockCell {
                fg: composite(img.sample(sx, top_y), bg),
                bg: composite(img.sample(sx, bottom_y), bg),
            }
        })
        .collect()
}

/// Composite a straight-alpha RGBA pixel over an opaque `bg`, returning
/// opaque RGB. Fully transparent → `bg`; fully opaque → the pixel's RGB.
fn composite(px: [u8; 4], bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let a = f64::from(px[3]) / 255.0;
    let blend = |fg: u8, bg: u8| (f64::from(fg) * a + f64::from(bg) * (1.0 - a)).round() as u8;
    (blend(px[0], bg.0), blend(px[1], bg.1), blend(px[2], bg.2))
}

/// The load state of one inline image, keyed by source URL in [`ImageStore`].
#[derive(Debug, Clone)]
pub enum ImageState {
    /// Fetch/decode in flight (a spawned task will deliver the result).
    Loading,
    /// Fetch or decode failed — stays on the alt-text placeholder, never
    /// retried within a session (a bad thumbnail URL shouldn't spin).
    Failed,
    /// Decoded and ready to render as half-blocks.
    Ready(DecodedImage),
}

/// Decoded inline images for the session, keyed by source URL (PRD FR-RD-8).
/// Lookups are by the same URL string the document model and the async loader
/// use, so layout's box reservation and paint's box fill agree with what the
/// loader delivered. Deliberately not persisted — thumbnails are cheap to
/// re-fetch and the licensing rules (PRD §10) are simpler when nothing is
/// written to disk here.
#[derive(Debug, Clone, Default)]
pub struct ImageStore {
    entries: HashMap<String, ImageState>,
}

impl ImageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this src has any entry yet (loading, failed, or ready) — the
    /// idempotency guard the async loader uses so it spawns exactly one fetch
    /// per URL.
    pub fn contains(&self, src: &str) -> bool {
        self.entries.contains_key(src)
    }

    pub fn mark_loading(&mut self, src: String) {
        self.entries.insert(src, ImageState::Loading);
    }

    pub fn set_ready(&mut self, src: String, img: DecodedImage) {
        self.entries.insert(src, ImageState::Ready(img));
    }

    pub fn set_failed(&mut self, src: String) {
        self.entries.insert(src, ImageState::Failed);
    }

    /// The decoded image for `src`, or `None` unless it is `Ready`.
    pub fn ready(&self, src: &str) -> Option<&DecodedImage> {
        match self.entries.get(src) {
            Some(ImageState::Ready(img)) => Some(img),
            _ => None,
        }
    }

    /// Whether any image is still loading — drives the event loop's scoped
    /// poll so a decode result lands without a keypress (PRD FR-TB-3 pattern).
    pub fn any_loading(&self) -> bool {
        self.entries
            .values()
            .any(|s| matches!(s, ImageState::Loading))
    }

    /// The reserved cell box for `src` at the given caps, or `None` when it is
    /// not ready (loading/failed/absent → the layout emits the placeholder).
    pub fn box_for(&self, src: &str, max_cols: u16, max_rows: u16) -> Option<(u16, u16)> {
        let img = self.ready(src)?;
        Some(image_box_cells(img.width, img.height, max_cols, max_rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 image with four distinct solid quadrants: TL red, TR green,
    /// BL blue, BR yellow. Row-major RGBA.
    fn quad_2x2() -> DecodedImage {
        let px = |r, g, b| [r, g, b, 255u8];
        let mut rgba = Vec::new();
        rgba.extend_from_slice(&px(255, 0, 0)); // (0,0) TL red
        rgba.extend_from_slice(&px(0, 255, 0)); // (1,0) TR green
        rgba.extend_from_slice(&px(0, 0, 255)); // (0,1) BL blue
        rgba.extend_from_slice(&px(255, 255, 0)); // (1,1) BR yellow
        DecodedImage {
            width: 2,
            height: 2,
            rgba,
        }
    }

    #[test]
    fn half_blocks_map_top_pixel_to_fg_and_bottom_to_bg() {
        // A 2x2 image into a 2-col x 1-row box: exactly one cell per source
        // column, top row -> fg, bottom row -> bg.
        let cells = render_half_blocks(&quad_2x2(), 2, 1, (0, 0, 0));
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].len(), 2);
        assert_eq!(
            cells[0][0],
            HalfBlockCell {
                fg: (255, 0, 0), // TL red on top
                bg: (0, 0, 255), // BL blue on bottom
            }
        );
        assert_eq!(
            cells[0][1],
            HalfBlockCell {
                fg: (0, 255, 0),   // TR green on top
                bg: (255, 255, 0), // BR yellow on bottom
            }
        );
    }

    #[test]
    fn alpha_composites_over_the_theme_background() {
        // A single fully-transparent pixel resolves to the background color.
        let img = DecodedImage {
            width: 1,
            height: 2,
            rgba: vec![255, 255, 255, 0, 0, 0, 0, 0],
        };
        let cells = render_half_blocks(&img, 1, 1, (16, 20, 24));
        assert_eq!(cells[0][0].fg, (16, 20, 24));
        assert_eq!(cells[0][0].bg, (16, 20, 24));

        // Half-alpha white over black -> ~mid grey.
        let img = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![255, 255, 255, 128],
        };
        let cells = render_half_blocks(&img, 1, 1, (0, 0, 0));
        assert_eq!(cells[0][0].fg, (128, 128, 128));
    }

    #[test]
    fn box_geometry_preserves_aspect_and_clamps() {
        // A square image: cols == 2*rows on screen (cell height counts double).
        let (cols, rows) = image_box_cells(100, 100, 40, 100);
        assert_eq!(cols, 40);
        assert_eq!(rows, 20);

        // A very tall image is clamped by max_rows, and width re-derived.
        let (cols, rows) = image_box_cells(100, 1000, 40, 10);
        assert_eq!(rows, 10);
        assert!((1..=40).contains(&cols));

        // A wide image stays within max_cols.
        let (cols, rows) = image_box_cells(1000, 100, 40, 100);
        assert_eq!(cols, 40);
        assert_eq!(rows, 2);
    }

    #[test]
    fn store_tracks_state_and_reserves_boxes_only_when_ready() {
        let mut store = ImageStore::new();
        assert!(!store.contains("u"));
        store.mark_loading("u".to_string());
        assert!(store.contains("u"));
        assert!(store.any_loading());
        assert!(store.box_for("u", 40, 40).is_none());

        store.set_ready("u".to_string(), quad_2x2());
        assert!(!store.any_loading());
        assert_eq!(store.box_for("u", 40, 40), Some((40, 20)));
        assert!(store.ready("u").is_some());

        store.set_failed("v".to_string());
        assert!(store.box_for("v", 40, 40).is_none());
        assert!(store.ready("v").is_none());
    }
}
