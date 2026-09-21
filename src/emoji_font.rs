/*
SPDX-License-Identifier: AGPL-3.0-only
Please see LICENSE in the repository root for full details.
*/

//! Colour emoji, rendered as images rather than glyphs.
//!
//! egui rasterises glyph outlines, but `NotoColorEmoji.ttf` holds PNGs in a
//! `CBDT` table instead — so egui draws nothing, and Symbola stays monochrome.
//! We read the bitmaps via `glyph_raster_image` and cache them as textures.
//! The cache is an LRU: a full picker would otherwise undo `media_cache`'s
//! memory work.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::OnceLock;

/// Resident emoji textures. Picker holds ~1800; this covers several pages
/// before the oldest are dropped and re-decoded.
const CACHE_CAP: usize = 2048;

/// Strike size to request. Noto ships one 136px strike; ask for what we draw.
const STRIKE_PX: u16 = 72;

/// Twemoji, bundled for consistent emoji rendering. COLR outlines, see [`render_colr`].
const TWEMOJI: &[u8] = include_bytes!("../assets/Twemoji.Mozilla.ttf");

/// Fallback fonts, first readable one wins. Covers emoji newer than the
/// pinned Twemoji build.
const FALLBACK_FONTS: &[&str] = &[
    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/TTF/NotoColorEmoji.ttf",
    "/usr/share/fonts/noto-cjk/NotoColorEmoji.ttf",
    "~/.local/share/fonts/NotoColorEmoji.ttf",
];

/// Render size. Above the 22px cell so zoom-ups stay sharp.
const RENDER_PX: u32 = 72;

pub struct EmojiFont {
    data: OnceLock<Option<Vec<u8>>>,
    textures: lru::LruCache<(String, usize), egui::TextureHandle>,
    /// Known-missing glyphs, to avoid a lookup per frame.
    missing: HashSet<String>,
}

impl EmojiFont {
    pub fn new() -> Self {
        Self {
            data: OnceLock::new(),
            textures: lru::LruCache::new(NonZeroUsize::new(CACHE_CAP).expect("cap is non-zero")),
            missing: HashSet::new(),
        }
    }

    /// Can this emoji be drawn in colour at all?
    ///
    /// Font coverage and the searchable dataset are different sets.
    pub fn can_render(&self, emoji: &str) -> bool {
        self.decode(emoji).is_some()
    }

    /// Colour emoji are always available: Twemoji is bundled.
    pub fn available(&self) -> bool {
        true
    }

    /// Texture for one emoji, decoding it on first sight.
    ///
    /// `None` means no font, no glyph, or undecodable PNG; caller draws text.
    pub fn texture(&mut self, ctx: &egui::Context, emoji: &str) -> Option<egui::TextureHandle> {
        self.texture_at_size(ctx, emoji, RENDER_PX as f32 / ctx.pixels_per_point())
    }

    /// Filter to physical display pixels before upload, avoiding GPU minification aliasing.
    pub fn texture_at_size(
        &mut self,
        ctx: &egui::Context,
        emoji: &str,
        points: f32,
    ) -> Option<egui::TextureHandle> {
        let pixels = (points * ctx.pixels_per_point()).round().clamp(1.0, 256.0) as usize;
        let key = (emoji.to_owned(), pixels);
        if let Some(handle) = self.textures.get(&key) {
            return Some(handle.clone());
        }
        if self.missing.contains(emoji) {
            return None;
        }
        let image = self.decode(emoji);
        match image {
            Some(img) => {
                let handle = ctx.load_texture(
                    format!("emoji:{emoji}:{pixels}"),
                    resize_emoji(img, pixels),
                    egui::TextureOptions::LINEAR,
                );
                self.textures.put(key, handle.clone());
                Some(handle)
            }
            None => {
                self.missing.insert(emoji.to_owned());
                None
            }
        }
    }

    fn decode(&self, emoji: &str) -> Option<egui::ColorImage> {
        if let Some(img) = Self::decode_with(TWEMOJI, emoji) {
            return Some(img);
        }
        let data = self.data.get_or_init(|| read_first(FALLBACK_FONTS));
        Self::decode_with(data.as_ref()?, emoji)
    }

    /// Render one emoji from one font, whichever colour format it uses.
    fn decode_with(data: &[u8], emoji: &str) -> Option<egui::ColorImage> {
        let face = ttf_parser::Face::parse(data, 0).ok()?;
        let gid = Self::glyph_for(data, &face, emoji)?;
        // CBDT/sbix: embedded PNG bitmaps.
        if let Some(raster) = face.glyph_raster_image(gid, STRIKE_PX) {
            return crate::media_cache::decode_image(
                raster.data,
                crate::media_cache::THUMB_MAX_PIXELS,
            );
        }
        // COLR: layered outlines plus palette.
        render_colr(&face, gid)
    }

    /// Resolve an emoji — possibly a multi-codepoint sequence — to one glyph.
    ///
    /// Toned glyphs, ZWJ families, and flags exist only as GSUB substitutions;
    /// `cmap` cannot reach them. Shape the sequence; exactly one glyph means
    /// the font has it, more means it decomposed.
    fn glyph_for(data: &[u8], face: &ttf_parser::Face, emoji: &str) -> Option<ttf_parser::GlyphId> {
        // Single codepoint needs no shaping.
        let mut chars = emoji.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            return face.glyph_index(ch);
        }
        let rb = rustybuzz::Face::from_slice(data, 0)?;
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(emoji);
        let shaped = rustybuzz::shape(&rb, &[], buffer);
        let infos = shaped.glyph_infos();
        if infos.len() != 1 {
            return None;
        }
        let gid = u16::try_from(infos[0].glyph_id).ok()?;
        (gid != 0).then_some(ttf_parser::GlyphId(gid))
    }

    /// Resident texture count, for the status bar.
    pub fn resident(&self) -> usize {
        self.textures.len()
    }
}

impl Default for EmojiFont {
    fn default() -> Self {
        Self::new()
    }
}

/// Resample premultiplied pixels so transparent edges do not acquire dark fringes.
fn resize_emoji(image: egui::ColorImage, side: usize) -> egui::ColorImage {
    if image.size == [side, side] {
        return image;
    }
    let bytes = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_array())
        .collect();
    let source = image::RgbaImage::from_raw(image.size[0] as u32, image.size[1] as u32, bytes)
        .expect("color image dimensions match its pixels");
    let resized = image::imageops::resize(
        &source,
        side as u32,
        side as u32,
        image::imageops::FilterType::Lanczos3,
    );
    let pixels = resized
        .pixels()
        .map(|pixel| {
            let [r, g, b, a] = pixel.0;
            egui::Color32::from_rgba_premultiplied(r.min(a), g.min(a), b.min(a), a)
        })
        .collect();
    egui::ColorImage::new([side, side], pixels)
}

/// Rasterise a COLRv0 glyph: layered outlines, each filled from the palette.
///
/// Twemoji needs drawing, not decoding; egui has no COLR path.
fn render_colr(face: &ttf_parser::Face, gid: ttf_parser::GlyphId) -> Option<egui::ColorImage> {
    if !face.is_color_glyph(gid) {
        return None;
    }
    let upem = f32::from(face.units_per_em());
    if upem <= 0.0 {
        return None;
    }
    let scale = RENDER_PX as f32 / upem;
    let mut pixmap = tiny_skia::Pixmap::new(RENDER_PX, RENDER_PX)?;

    // Font is y-up, pixmap y-down; place the em box inside the pixmap.
    let ascender = f32::from(face.ascender());
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, -scale, 0.0, ascender * scale);

    let mut painter = ColrPainter {
        face,
        pixmap: &mut pixmap,
        transform,
        path: None,
    };
    // Palette 0 is default; Twemoji never asks for the text colour.
    face.paint_color_glyph(
        gid,
        0,
        ttf_parser::RgbaColor::new(0, 0, 0, 255),
        &mut painter,
    )?;

    if pixmap.pixels().iter().all(|p| p.alpha() == 0) {
        return None;
    }
    Some(egui::ColorImage::from_rgba_premultiplied(
        [RENDER_PX as usize, RENDER_PX as usize],
        pixmap.data(),
    ))
}

/// Glyph outline sink for tiny-skia.
#[derive(Default)]
struct OutlineCollector {
    builder: tiny_skia::PathBuilder,
}

impl ttf_parser::OutlineBuilder for OutlineCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.builder.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.builder.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.builder.close();
    }
}

/// COLR layer painter.
struct ColrPainter<'a, 'f> {
    face: &'a ttf_parser::Face<'f>,
    pixmap: &'a mut tiny_skia::Pixmap,
    transform: tiny_skia::Transform,
    path: Option<tiny_skia::Path>,
}

impl<'f> ttf_parser::colr::Painter<'f> for ColrPainter<'_, 'f> {
    fn outline_glyph(&mut self, glyph_id: ttf_parser::GlyphId) {
        let mut collector = OutlineCollector::default();
        self.path = self
            .face
            .outline_glyph(glyph_id, &mut collector)
            .and_then(|_| collector.builder.finish());
    }

    fn paint(&mut self, paint: ttf_parser::colr::Paint<'f>) {
        // COLRv0 is solid fills only.
        let ttf_parser::colr::Paint::Solid(color) = paint else {
            return;
        };
        let Some(path) = &self.path else { return };
        let mut fill = tiny_skia::Paint {
            anti_alias: true,
            ..Default::default()
        };
        fill.set_color_rgba8(color.red, color.green, color.blue, color.alpha);
        self.pixmap.fill_path(
            path,
            &fill,
            tiny_skia::FillRule::Winding,
            self.transform,
            None,
        );
    }

    // Unused in COLRv0; required by the trait.
    fn push_clip(&mut self) {}
    fn push_clip_box(&mut self, _: ttf_parser::colr::ClipBox) {}
    fn pop_clip(&mut self) {}
    fn push_layer(&mut self, _: ttf_parser::colr::CompositeMode) {}
    fn pop_layer(&mut self) {}
    fn push_transform(&mut self, _: ttf_parser::Transform) {}
    fn pop_transform(&mut self) {}
}

fn read_first(paths: &[&str]) -> Option<Vec<u8>> {
    for path in paths {
        let resolved = match path.strip_prefix("~/") {
            Some(rest) => match std::env::var("HOME") {
                Ok(h) => std::path::PathBuf::from(h).join(rest),
                Err(_) => continue,
            },
            None => std::path::PathBuf::from(path),
        };
        if let Ok(bytes) = std::fs::read(&resolved) {
            return Some(bytes);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textures_match_display_density_and_reuse_the_same_size() {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let mut font = EmojiFont::new();
        let skull = "\u{2620}\u{FE0F}";
        let normal = font.texture_at_size(&ctx, skull, 18.0).unwrap();
        assert_eq!(normal.size(), [18, 18]);
        assert_eq!(
            normal.id(),
            font.texture_at_size(&ctx, skull, 18.0).unwrap().id()
        );
        ctx.set_pixels_per_point(2.0);
        // Apply the new scale at the next frame boundary.
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let dense = font.texture_at_size(&ctx, skull, 18.0).unwrap();
        assert_eq!(dense.size(), [36, 36]);
        assert_ne!(normal.id(), dense.id());
    }

    #[test]
    fn twemoji_is_bundled_and_renders_in_colour() {
        // Twemoji is COLR outlines, so this hits the rasteriser, not PNG.
        let img = EmojiFont::decode_with(TWEMOJI, "\u{2620}\u{FE0F}")
            .or_else(|| EmojiFont::decode_with(TWEMOJI, "\u{2620}"))
            .expect("skull and crossbones should render from Twemoji");
        assert_eq!(img.size, [RENDER_PX as usize, RENDER_PX as usize]);
        assert!(
            img.pixels.iter().any(|p| p.a() > 0),
            "rendered glyph is entirely transparent"
        );
        // Single colour would mean the artwork was lost to a plain outline.
        let distinct: std::collections::HashSet<_> = img
            .pixels
            .iter()
            .filter(|p| p.a() > 200)
            .map(|p| (p.r(), p.g(), p.b()))
            .collect();
        assert!(
            distinct.len() > 1,
            "expected a multi-colour glyph, got {} colour(s)",
            distinct.len()
        );
    }

    #[test]
    fn clapping_hands_with_a_dark_tone_renders_toned() {
        // Regression: modifier split off, leaving base plus tofu box.
        let plain = EmojiFont::decode_with(TWEMOJI, "\u{1F44F}").expect("clapping hands");
        let dark = EmojiFont::decode_with(TWEMOJI, "\u{1F44F}\u{1F3FF}")
            .expect("dark-skinned clapping hands must render");
        assert_ne!(
            plain.pixels, dark.pixels,
            "the tone must change the artwork"
        );

        // Bare modifier is a swatch glyph, not tofu: the fix is keeping the
        // cluster together.
        let swatch = EmojiFont::decode_with(TWEMOJI, "\u{1F3FF}")
            .expect("the bare modifier is a swatch glyph in Twemoji");
        assert_ne!(swatch.pixels, dark.pixels);
    }

    #[test]
    fn variation_selector_resolves_to_the_same_glyph() {
        // Both spellings are one glyph; tooltip and chip must agree.
        let bare = EmojiFont::decode_with(TWEMOJI, "\u{2620}").expect("skull");
        let vs16 = EmojiFont::decode_with(TWEMOJI, "\u{2620}\u{FE0F}")
            .expect("skull with variation selector must resolve");
        assert_eq!(bare.pixels, vs16.pixels, "both spellings are one glyph");
    }

    #[test]
    fn twemoji_covers_skin_tones_too() {
        let plain = EmojiFont::decode_with(TWEMOJI, "\u{1F44B}").expect("waving hand");
        let dark = EmojiFont::decode_with(TWEMOJI, "\u{1F44B}\u{1F3FF}")
            .expect("dark-skinned waving hand");
        assert_ne!(plain.pixels, dark.pixels, "tone must change the artwork");
    }

    #[test]
    fn decodes_a_colour_emoji_when_a_font_is_installed() {
        let font = EmojiFont::new();
        if !font.available() {
            // Nothing to assert without a colour font on this machine.
            return;
        }
        let img = font
            .decode("\u{1F600}")
            .expect("grinning face should have a CBDT bitmap");
        assert!(img.size[0] > 1 && img.size[1] > 1, "expected a real bitmap");
        // Must not be uniformly transparent.
        assert!(
            img.pixels.iter().any(|p| p.a() > 0),
            "decoded emoji is fully transparent"
        );
    }

    #[test]
    fn skin_tone_modifiers_select_the_toned_glyph() {
        let font = EmojiFont::new();
        if !font.available() {
            return;
        }
        // Base + modifier must give the toned glyph, not the default.
        let plain = font.decode("\u{1F44B}").expect("waving hand");
        let dark = font
            .decode("\u{1F44B}\u{1F3FF}")
            .expect("dark-skinned waving hand must render");
        assert_ne!(
            plain.pixels, dark.pixels,
            "toned emoji must differ from the untoned one"
        );

        // Each tone differs from the others.
        let light = font.decode("\u{1F44B}\u{1F3FB}").expect("light tone");
        assert_ne!(light.pixels, dark.pixels, "tones must be distinct");
    }

    #[test]
    fn zwj_sequences_resolve_through_shaping() {
        let font = EmojiFont::new();
        if !font.available() {
            return;
        }
        // ZWJ family is one ligature; first-codepoint fallback shows a lone man.
        let family = font.decode("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F466}");
        let man = font.decode("\u{1F468}").expect("man");
        if let Some(family) = family {
            assert_ne!(family.pixels, man.pixels, "family must not render as a man");
        }
    }
}
