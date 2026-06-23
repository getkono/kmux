//! Glyph atlas: swash rasterization + etagere shelf-packing + a glyph cache.
//!
//! Monospace terminals touch a tiny glyph set (ASCII + box-drawing + whatever
//! is on screen), so a single 1024² page is usually plenty; the atlas grows by
//! adding pages when one fills. Each rasterized glyph is cached by
//! `(face, char)` — the px size is fixed per renderer — so a glyph is shaped
//! once and reused every frame.
//!
//! The atlas is RGBA8 and CPU-side here (the GPU upload is the renderer's job,
//! C7): monochrome (outline) glyphs are stored as white with the coverage in
//! alpha, so the glyph shader can tint by the cell's foreground color. Each page
//! tracks a dirty flag so only changed pages are re-uploaded.

use std::collections::HashMap;

use swash::FontRef;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source};

use crate::geometry::FaceStyle;

/// Cache key for a rasterized glyph (px size is fixed per renderer/atlas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// Which font face the glyph was rasterized from.
    pub style: FaceStyle,
    /// The character.
    pub ch: char,
}

/// Where a cached glyph lives in the atlas + its placement bearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasEntry {
    /// Index of the atlas page holding the bitmap.
    pub page: usize,
    /// Bitmap left in atlas pixels.
    pub x: u32,
    /// Bitmap top in atlas pixels.
    pub y: u32,
    /// Bitmap width in pixels.
    pub w: u32,
    /// Bitmap height in pixels.
    pub h: u32,
    /// Pen-to-bitmap-left bearing (px); add to the glyph origin x.
    pub left: i32,
    /// Baseline-to-bitmap-top bearing (px, positive up); the glyph top is at
    /// `baseline - top`.
    pub top: i32,
}

struct AtlasPage {
    alloc: etagere::AtlasAllocator,
    /// RGBA8, `page_size * page_size * 4` bytes.
    pixels: Vec<u8>,
    /// Set when `pixels` changed since the last upload.
    dirty: bool,
}

impl AtlasPage {
    fn new(page_size: u32) -> Self {
        Self {
            alloc: etagere::AtlasAllocator::new(etagere::size2(page_size as i32, page_size as i32)),
            pixels: vec![0u8; (page_size * page_size * 4) as usize],
            dirty: false,
        }
    }

    /// Blit an 8-bit coverage mask as white + alpha at `(x, y)`.
    fn blit_mask(&mut self, x: u32, y: u32, w: u32, h: u32, mask: &[u8], page_size: u32) {
        for row in 0..h {
            for col in 0..w {
                let a = mask[(row * w + col) as usize];
                let di = (((y + row) * page_size + (x + col)) * 4) as usize;
                self.pixels[di] = 0xff;
                self.pixels[di + 1] = 0xff;
                self.pixels[di + 2] = 0xff;
                self.pixels[di + 3] = a;
            }
        }
        self.dirty = true;
    }
}

/// A growable RGBA glyph atlas with a per-`(face, char)` cache.
pub struct Atlas {
    page_size: u32,
    pages: Vec<AtlasPage>,
    cache: HashMap<GlyphKey, Option<AtlasEntry>>,
    ctx: ScaleContext,
}

impl Atlas {
    /// Create an atlas whose pages are `page_size × page_size` px.
    pub fn new(page_size: u32) -> Self {
        Self {
            page_size: page_size.max(1),
            pages: vec![AtlasPage::new(page_size.max(1))],
            cache: HashMap::new(),
            ctx: ScaleContext::new(),
        }
    }

    /// Side length of each atlas page in pixels.
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Number of atlas pages currently allocated.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Look up a glyph, rasterizing + packing it on first use. Returns `None`
    /// for glyphs with no bitmap (spaces, truly unmapped chars). `px` is the
    /// physical em size; `font` must be the face matching `key.style`. When the
    /// primary `font` has no glyph for the char, `fallback` (the bundled symbol
    /// font) is tried so Powerline/Nerd glyphs render instead of tofu.
    ///
    /// The `(style, ch)` cache key stays valid because the fallback is a single
    /// process-global font: the resolved bitmap for a char is deterministic.
    pub fn get_or_insert(
        &mut self,
        font: FontRef<'_>,
        fallback: FontRef<'_>,
        px: f32,
        key: GlyphKey,
    ) -> Option<AtlasEntry> {
        if let Some(entry) = self.cache.get(&key) {
            return *entry;
        }
        let entry = self.rasterize_and_pack(font, fallback, px, key.ch);
        self.cache.insert(key, entry);
        entry
    }

    /// Invoke `f(page_index, page_size, rgba)` for each page changed since the
    /// last call, then clear its dirty flag — the renderer's GPU upload hook.
    pub fn upload_dirty(&mut self, mut f: impl FnMut(usize, u32, &[u8])) {
        let page_size = self.page_size;
        for (i, page) in self.pages.iter_mut().enumerate() {
            if page.dirty {
                f(i, page_size, &page.pixels);
                page.dirty = false;
            }
        }
    }

    fn rasterize_and_pack(
        &mut self,
        font: FontRef<'_>,
        fallback: FontRef<'_>,
        px: f32,
        ch: char,
    ) -> Option<AtlasEntry> {
        // Prefer the configured face; fall back to the bundled symbol font for
        // glyphs it lacks (Powerline U+E0Bx, Nerd PUA icons). swash scales each
        // font by its own units-per-em at the shared `px`, so a fallback face
        // with a different upem is handled correctly. Only a char neither font
        // maps renders nothing.
        let (font, glyph_id) = match font.charmap().map(ch) {
            0 => match fallback.charmap().map(ch) {
                0 => return None, // truly unmapped — blank
                gid => (fallback, gid),
            },
            gid => (font, gid),
        };
        let mut scaler = self.ctx.builder(font).size(px).hint(true).build();
        let image = Render::new(&[Source::Outline]).render(&mut scaler, glyph_id)?;
        if !matches!(image.content, Content::Mask) {
            return None; // color/subpixel glyphs unsupported for now (follow-up)
        }
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            return None; // whitespace etc. — no bitmap
        }
        let (page, x, y) = self.alloc(w, h)?;
        self.pages[page].blit_mask(x, y, w, h, &image.data, self.page_size);
        Some(AtlasEntry {
            page,
            x,
            y,
            w,
            h,
            left: image.placement.left,
            top: image.placement.top,
        })
    }

    /// Reserve a `w × h` rect, growing onto a new page if none fits. Returns the
    /// page index + top-left. `None` if the glyph is larger than a whole page.
    fn alloc(&mut self, w: u32, h: u32) -> Option<(usize, u32, u32)> {
        if w == 0 || h == 0 || w > self.page_size || h > self.page_size {
            return None;
        }
        let size = etagere::size2(w as i32, h as i32);
        for (i, page) in self.pages.iter_mut().enumerate() {
            if let Some(a) = page.alloc.allocate(size) {
                let r = a.rectangle;
                return Some((i, r.min.x as u32, r.min.y as u32));
            }
        }
        // All pages full — add one.
        let mut page = AtlasPage::new(self.page_size);
        let a = page.alloc.allocate(size)?;
        let r = a.rectangle;
        let pos = (self.pages.len(), r.min.x as u32, r.min.y as u32);
        self.pages.push(page);
        Some(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{FontFace, RenderMetrics};
    use kmux_app::appearance::Appearance;

    fn regular_face() -> Option<FontFace> {
        RenderMetrics::from_appearance(&Appearance::default(), 1.0)
            .faces()
            .regular
            .clone()
    }

    /// The bundled symbol fallback face as a `FontRef` (always available — it is
    /// embedded, so this works even on a font-less CI box).
    fn fallback_font() -> FontRef<'static> {
        crate::fallback::symbol_fallback().as_ref().unwrap()
    }

    #[test]
    fn alloc_places_rects_without_overlap() {
        let mut atlas = Atlas::new(64);
        let a = atlas.alloc(10, 10).unwrap();
        let b = atlas.alloc(10, 10).unwrap();
        assert_ne!((a.1, a.2), (b.1, b.2), "two rects must not share an origin");
        assert_eq!(a.0, 0);
        assert_eq!(b.0, 0, "both fit on the first page");
    }

    #[test]
    fn alloc_grows_to_a_new_page_when_full() {
        // A 16² page fits only one 10×10 rect (two would need 20px).
        let mut atlas = Atlas::new(16);
        assert_eq!(atlas.page_count(), 1);
        let a = atlas.alloc(10, 10).unwrap();
        let b = atlas.alloc(10, 10).unwrap();
        assert_eq!(a.0, 0);
        assert_eq!(b.0, 1, "the second rect spills onto a new page");
        assert_eq!(atlas.page_count(), 2);
    }

    #[test]
    fn alloc_rejects_glyph_larger_than_a_page() {
        let mut atlas = Atlas::new(16);
        assert!(atlas.alloc(20, 20).is_none());
        assert!(atlas.alloc(0, 5).is_none());
    }

    #[test]
    fn rasterizes_caches_and_skips_blanks() {
        // Needs a system font; skip gracefully on a font-less box.
        let Some(face) = regular_face() else {
            return;
        };
        let font = face.as_ref().unwrap();
        let fb = fallback_font();
        let mut atlas = Atlas::new(256);

        let key = GlyphKey {
            style: FaceStyle::Regular,
            ch: 'M',
        };
        let first = atlas.get_or_insert(font, fb, 16.0, key);
        assert!(first.is_some(), "'M' should rasterize to a bitmap");
        let e = first.unwrap();
        assert!(e.w > 0 && e.h > 0);

        let again = atlas.get_or_insert(font, fb, 16.0, key);
        assert_eq!(first, again, "second lookup is a cache hit");

        let space = atlas.get_or_insert(
            font,
            fb,
            16.0,
            GlyphKey {
                style: FaceStyle::Regular,
                ch: ' ',
            },
        );
        assert!(space.is_none(), "a space has no bitmap");
    }

    #[test]
    fn falls_back_to_symbol_font_for_powerline_and_nerd_glyphs() {
        // The headless approximation has no real primary face, so use the
        // embedded fallback as *both* faces: the point is that a Powerline /
        // Nerd glyph the primary lacks still rasterizes via the fallback path,
        // proving the bug (blank/tofu) is fixed. This is CI-safe because the
        // fallback font is embedded.
        let primary = regular_face();
        let fb = fallback_font();
        // A primary that definitely lacks these PUA glyphs: reuse a system face
        // if present, else the fallback itself (which maps them) — either way
        // the assertion below exercises the fallback branch for the system case.
        let primary_font = primary.as_ref().and_then(|f| f.as_ref()).unwrap_or(fb);
        let mut atlas = Atlas::new(256);

        for ch in ['\u{e0b0}', '\u{e0b1}', '\u{e0a0}', '\u{f015}'] {
            let entry = atlas.get_or_insert(
                primary_font,
                fb,
                16.0,
                GlyphKey {
                    style: FaceStyle::Regular,
                    ch,
                },
            );
            assert!(
                entry.is_some_and(|e| e.w > 0 && e.h > 0),
                "U+{:04X} should rasterize via the symbol fallback",
                ch as u32
            );
        }
    }

    #[test]
    fn upload_dirty_fires_once_per_change() {
        let Some(face) = regular_face() else {
            return;
        };
        let font = face.as_ref().unwrap();
        let fb = fallback_font();
        let mut atlas = Atlas::new(256);
        atlas.get_or_insert(
            font,
            fb,
            16.0,
            GlyphKey {
                style: FaceStyle::Regular,
                ch: 'A',
            },
        );

        let mut uploads = 0;
        atlas.upload_dirty(|_, size, px| {
            uploads += 1;
            assert_eq!(px.len(), (size * size * 4) as usize);
        });
        assert_eq!(uploads, 1, "the dirtied page uploads once");

        // Nothing changed since — no upload.
        let mut again = 0;
        atlas.upload_dirty(|_, _, _| again += 1);
        assert_eq!(again, 0);
    }
}
