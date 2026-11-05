//! Fonts: discovery (fontconfig), fallback faces, cell metrics, shaping (swash) and the glyph
//! cache. All file I/O happens in [`Fonts::discover`] and [`Fonts::prepare`]; shaping and
//! rasterizing ([`Fonts::shape`], [`Fonts::glyph`]) are pure computation over bytes already in
//! memory, so the paint function that calls them does no I/O.

pub mod fc;

use fc::{Fontconfig, Located};
use std::collections::HashMap;
use std::rc::Rc;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::zeno::Format;
use swash::{CacheKey, FontRef};

/// The four faces of a family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    /// Upright, normal weight.
    Regular = 0,
    /// Bold.
    Bold = 1,
    /// Italic or oblique.
    Italic = 2,
    /// Bold italic.
    BoldItalic = 3,
}

impl Style {
    /// The style for a cell's SGR bold/italic flags.
    pub fn of(bold: bool, italic: bool) -> Style {
        match (bold, italic) {
            (false, false) => Style::Regular,
            (true, false) => Style::Bold,
            (false, true) => Style::Italic,
            (true, true) => Style::BoldItalic,
        }
    }
}

/// What can fail while loading fonts.
#[derive(Debug)]
pub enum FontError {
    /// Fontconfig could not be initialised.
    Fontconfig,
    /// No font matched the family.
    NoMatch(String),
    /// A font file could not be read.
    Read(std::path::PathBuf, std::io::Error),
    /// A font file did not parse.
    Parse(std::path::PathBuf),
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FontError::Fontconfig => write!(f, "fontconfig failed to initialise"),
            FontError::NoMatch(fam) => write!(f, "no font matches {fam:?}"),
            FontError::Read(p, e) => write!(f, "{}: {e}", p.display()),
            FontError::Parse(p) => write!(f, "{}: not a usable font", p.display()),
        }
    }
}

impl std::error::Error for FontError {}

/// One loaded font face (the file's bytes plus the face's table offset).
pub struct Face {
    data: Vec<u8>,
    offset: u32,
    key: CacheKey,
}

impl Face {
    /// Parse face `index` of a font file's bytes.
    pub fn from_bytes(data: Vec<u8>, index: u32) -> Option<Face> {
        let f = FontRef::from_index(&data, index as usize)?;
        let (offset, key) = (f.offset, f.key);
        Some(Face { data, offset, key })
    }

    fn font(&self) -> FontRef<'_> {
        FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }

    fn covers(&self, ch: char) -> bool {
        self.font().charmap().map(ch) != 0
    }
}

/// Cell geometry in buffer pixels at the current size and scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Cell width (the primary face's advance, rounded).
    pub cell_w: u32,
    /// Cell height (ascent + descent + leading, rounded up).
    pub cell_h: u32,
    /// Baseline, from the cell top.
    pub baseline: u32,
    /// Top of the underline, from the cell top.
    pub underline_y: u32,
    /// Stroke thickness for underline, strikethrough and cursor bars (>= 1).
    pub stroke: u32,
    /// Top of the strikethrough, from the cell top.
    pub strike_y: u32,
    /// Font size in pixels (points x scale x 96/72).
    pub px: f32,
}

impl Metrics {
    /// Metrics from a face at `px` pixels.
    fn of(face: &Face, px: f32) -> Metrics {
        let font = face.font();
        let m = font.metrics(&[]).scale(px);
        let gm = font.glyph_metrics(&[]).scale(px);
        let adv = gm.advance_width(font.charmap().map('M'));
        let cell_w = (adv.round() as u32).max(1);
        let ascent = m.ascent.ceil();
        let cell_h = ((m.ascent + m.descent + m.leading).ceil() as u32).max(1);
        let baseline = (ascent as u32).min(cell_h);
        let stroke = (m.stroke_size.round() as u32).max(1);
        let under = (ascent - m.underline_offset).round().max(0.0) as u32;
        let underline_y = under.min(cell_h.saturating_sub(stroke));
        let strike_off = if m.strikeout_offset > 0.0 {
            m.strikeout_offset
        } else {
            m.x_height / 2.0
        };
        let strike_y = (ascent - strike_off).round().max(0.0) as u32;
        Metrics {
            cell_w,
            cell_h,
            baseline,
            underline_y,
            stroke,
            strike_y: strike_y.min(cell_h.saturating_sub(stroke)),
            px,
        }
    }
}

/// Where to find a face for a character the primary faces lack.
pub trait Fallback {
    /// A face covering `ch`; `color` prefers a colour (emoji) font.
    fn locate(&mut self, ch: char, color: bool) -> Option<Located>;
}

/// Fontconfig-backed fallback for a family.
pub struct FcFallback {
    fc: Fontconfig,
    family: String,
}

impl Fallback for FcFallback {
    fn locate(&mut self, ch: char, color: bool) -> Option<Located> {
        self.fc.find(&self.family, Style::Regular, Some(ch), color)
    }
}

/// One positioned glyph of a shaped cell, relative to the cell's left edge and baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    /// Index into the face list.
    pub face: usize,
    /// Glyph id in that face.
    pub id: u16,
    /// Horizontal offset in pixels.
    pub x: f32,
    /// Vertical offset in pixels (up is positive).
    pub y: f32,
}

/// A rasterized glyph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glyph {
    /// Left bearing in pixels.
    pub left: i32,
    /// Top bearing above the baseline in pixels.
    pub top: i32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// Colour glyph (premultiplied RGBA, 4 bytes/px; to be centred in its cells) or an 8-bit
    /// coverage mask (1 byte/px).
    pub color: bool,
    /// Pixel data.
    pub data: Vec<u8>,
}

/// Glyph cache key: (face, glyph id, size, scale).
type GlyphKey = (usize, u16, u32, u32);

/// The loaded faces, metrics, and the shape and glyph caches.
pub struct Fonts {
    faces: Vec<Face>,
    primary: [usize; 4],
    /// Fallback faces resolved by [`Fonts::prepare`] (`None`: nothing covers it).
    coverage: HashMap<char, Option<usize>>,
    loaded: HashMap<Located, usize>,
    fallback: Option<Box<dyn Fallback>>,
    size_pt: f32,
    scale: f32,
    metrics: Metrics,
    shape_ctx: ShapeContext,
    scale_ctx: ScaleContext,
    shapes: HashMap<(String, Style), Rc<[Placed]>>,
    glyphs: HashMap<GlyphKey, Option<Rc<Glyph>>>,
}

fn read_face(loc: &Located) -> Result<Face, FontError> {
    let data = std::fs::read(&loc.path).map_err(|e| FontError::Read(loc.path.clone(), e))?;
    Face::from_bytes(data, loc.index).ok_or_else(|| FontError::Parse(loc.path.clone()))
}

/// Characters that want colour emoji presentation (a coarse range test; text-presentation
/// symbols outside these blocks keep the primary face unless it lacks them).
pub fn is_emoji(ch: char) -> bool {
    matches!(ch as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B50..=0x2B55)
}

impl Fonts {
    /// Discover `family` (regular, bold, italic, bold italic) through fontconfig at `size_pt`
    /// points and output `scale`, with fontconfig fallback for missing characters.
    pub fn discover(family: &str, size_pt: f32, scale: f32) -> Result<Fonts, FontError> {
        let fc = Fontconfig::new().ok_or(FontError::Fontconfig)?;
        let mut locs = Vec::new();
        for st in [
            Style::Regular,
            Style::Bold,
            Style::Italic,
            Style::BoldItalic,
        ] {
            let loc = fc.find(family, st, None, false);
            if st == Style::Regular && loc.is_none() {
                return Err(FontError::NoMatch(family.to_string()));
            }
            locs.push(loc);
        }
        let mut faces = Vec::new();
        let mut loaded = HashMap::new();
        let mut primary = [0; 4];
        for (i, loc) in locs.into_iter().enumerate() {
            let Some(loc) = loc else { continue };
            if let Some(&id) = loaded.get(&loc) {
                primary[i] = id;
                continue;
            }
            match read_face(&loc) {
                Ok(face) => {
                    faces.push(face);
                    loaded.insert(loc, faces.len() - 1);
                    primary[i] = faces.len() - 1;
                }
                Err(e) if i == 0 => return Err(e),
                Err(e) => eprintln!("crucible-term: font: {e}; using the regular face"),
            }
        }
        let fallback = Box::new(FcFallback {
            fc,
            family: family.to_string(),
        });
        Ok(Fonts::build(
            faces,
            primary,
            loaded,
            Some(fallback),
            size_pt,
            scale,
        ))
    }

    #[cfg(test)]
    /// Fonts from faces already in memory (`faces[0]` is regular; bold/italic/bold-italic
    /// default to it), with an optional fallback source. Used by tests and benchmarks.
    pub fn from_faces(
        faces: Vec<Face>,
        primary: [usize; 4],
        fallback: Option<Box<dyn Fallback>>,
        size_pt: f32,
        scale: f32,
    ) -> Fonts {
        Fonts::build(faces, primary, HashMap::new(), fallback, size_pt, scale)
    }

    fn build(
        faces: Vec<Face>,
        primary: [usize; 4],
        loaded: HashMap<Located, usize>,
        fallback: Option<Box<dyn Fallback>>,
        size_pt: f32,
        scale: f32,
    ) -> Fonts {
        let px = size_pt * scale * 96.0 / 72.0;
        let metrics = Metrics::of(&faces[primary[0]], px);
        Fonts {
            faces,
            primary,
            coverage: HashMap::new(),
            loaded,
            fallback,
            size_pt,
            scale,
            metrics,
            shape_ctx: ShapeContext::new(),
            scale_ctx: ScaleContext::new(),
            shapes: HashMap::new(),
            glyphs: HashMap::new(),
        }
    }

    /// Debug-only memory attribution (`CTERM_DEBUG_MEM`): `(face_total, emoji_face, face_count,
    /// shape_entries, glyph_entries, glyph_bitmap_bytes)`. The emoji face is the loaded face whose
    /// path names "emoji" (a heuristic); its bytes are included in `face_total`.
    pub fn mem_stats(&self) -> (usize, usize, usize, usize, usize, usize) {
        let total = self.faces.iter().map(|f| f.data.len()).sum();
        let emoji = self
            .loaded
            .iter()
            .filter(|(l, _)| l.path.to_string_lossy().to_lowercase().contains("emoji"))
            .map(|(_, &i)| self.faces[i].data.len())
            .sum();
        let bitmaps = self.glyphs.values().flatten().map(|g| g.data.len()).sum();
        (
            total,
            emoji,
            self.faces.len(),
            self.shapes.len(),
            self.glyphs.len(),
            bitmaps,
        )
    }

    /// Current cell metrics.
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// Change the output scale; recomputes metrics and drops the caches when it differs.
    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < f32::EPSILON {
            return;
        }
        self.scale = scale;
        let px = self.size_pt * scale * 96.0 / 72.0;
        self.metrics = Metrics::of(&self.faces[self.primary[0]], px);
        self.clear_caches();
    }

    /// Drop the shape and glyph caches (the next frame is "cold").
    pub fn clear_caches(&mut self) {
        self.shapes.clear();
        self.glyphs.clear();
    }

    #[cfg(test)]
    /// Drop only the shape cache (benchmarks: shape every cell every frame).
    pub fn clear_shape_cache(&mut self) {
        self.shapes.clear();
    }

    /// Resolve fallback faces for every character of `text` the primary faces lack (and colour
    /// faces for emoji). This is the I/O step (fontconfig query, file read), once per
    /// character; call it on new cell text before painting.
    pub fn prepare(&mut self, text: &str) {
        for ch in text.chars() {
            if (ch as u32) < 0x80 || self.coverage.contains_key(&ch) {
                continue;
            }
            let emoji = is_emoji(ch);
            if !emoji && self.faces[self.primary[0]].covers(ch) {
                continue;
            }
            if matches!(ch, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}') {
                continue;
            }
            let found = self.load_fallback(ch, emoji);
            self.coverage.insert(ch, found);
        }
    }

    fn load_fallback(&mut self, ch: char, emoji: bool) -> Option<usize> {
        let fb = self.fallback.as_mut()?;
        let loc = fb
            .locate(ch, emoji)
            .or_else(|| if emoji { fb.locate(ch, false) } else { None })?;
        if let Some(&id) = self.loaded.get(&loc) {
            return self.faces[id].covers(ch).then_some(id);
        }
        match read_face(&loc) {
            Ok(face) => {
                let covers = face.covers(ch);
                self.faces.push(face);
                let id = self.faces.len() - 1;
                self.loaded.insert(loc, id);
                covers.then_some(id)
            }
            Err(e) => {
                eprintln!("crucible-term: font fallback: {e}");
                None
            }
        }
    }

    /// The face that draws `ch` in `style`: a resolved emoji face first, then the style's
    /// primary face, the regular face, a resolved fallback, else the regular face (.notdef).
    pub fn face_for(&self, ch: char, style: Style) -> usize {
        let fb = self.coverage.get(&ch).copied().flatten();
        if is_emoji(ch)
            && let Some(id) = fb
        {
            return id;
        }
        let p = self.primary[style as usize];
        if self.faces[p].covers(ch) {
            return p;
        }
        let r = self.primary[0];
        if self.faces[r].covers(ch) {
            return r;
        }
        fb.unwrap_or(r)
    }

    /// The glyphs of one cell's text (a grapheme), shaped with swash and cached.
    pub fn shape(&mut self, text: &str, style: Style) -> Rc<[Placed]> {
        if let Some(s) = self.shapes.get(&(text.to_string(), style)) {
            return s.clone();
        }
        let mut chars = text.chars();
        let first = chars.next().unwrap_or(' ');
        let face = self.face_for(first, style);
        let placed: Rc<[Placed]> = if chars.next().is_none() {
            // One code point: a charmap lookup is the whole shaping job.
            let id = self.faces[face].font().charmap().map(first);
            Rc::from([Placed {
                face,
                id,
                x: 0.0,
                y: 0.0,
            }])
        } else {
            let font = self.faces[face].font();
            let mut shaper = self.shape_ctx.builder(font).size(self.metrics.px).build();
            shaper.add_str(text);
            let mut out = Vec::new();
            let mut pen = 0.0;
            shaper.shape_with(|cluster| {
                for g in cluster.glyphs {
                    out.push(Placed {
                        face,
                        id: g.id,
                        x: pen + g.x,
                        y: g.y,
                    });
                    pen += g.advance;
                }
            });
            Rc::from(out)
        };
        self.shapes
            .insert((text.to_string(), style), placed.clone());
        placed
    }

    /// Rasterize (or fetch from the cache) one glyph. `None` for empty glyphs (spaces).
    pub fn glyph(&mut self, face: usize, id: u16) -> Option<Rc<Glyph>> {
        let key = (face, id, self.metrics.px.to_bits(), self.scale.to_bits());
        if let Some(g) = self.glyphs.get(&key) {
            return g.clone();
        }
        let g = self.raster(face, id).map(Rc::new);
        self.glyphs.insert(key, g.clone());
        g
    }

    fn raster(&mut self, face: usize, id: u16) -> Option<Glyph> {
        let font = self.faces[face].font();
        let mut scaler = self
            .scale_ctx
            .builder(font)
            .size(self.metrics.px)
            .hint(true)
            .build();
        let img = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .format(Format::Alpha)
        .render(&mut scaler, id)?;
        let p = img.placement;
        if p.width == 0 || p.height == 0 {
            return None;
        }
        match img.content {
            Content::Mask => Some(Glyph {
                left: p.left,
                top: p.top,
                w: p.width,
                h: p.height,
                color: false,
                data: img.data,
            }),
            Content::Color | Content::SubpixelMask => {
                let mut data = img.data;
                for px in data.chunks_exact_mut(4) {
                    let a = u16::from(px[3]);
                    for c in &mut px[..3] {
                        *c = ((u16::from(*c) * a + 127) / 255) as u8;
                    }
                }
                let m = self.metrics;
                let (w, h, data) = fit(p.width, p.height, data, m.cell_w * 2, m.cell_h);
                Some(Glyph {
                    left: 0,
                    top: 0,
                    w,
                    h,
                    color: true,
                    data,
                })
            }
        }
    }
}

/// Downscale a premultiplied RGBA image (box filter) to fit `max_w` x `max_h`, keeping aspect.
fn fit(w: u32, h: u32, data: Vec<u8>, max_w: u32, max_h: u32) -> (u32, u32, Vec<u8>) {
    if w <= max_w && h <= max_h {
        return (w, h, data);
    }
    let f = (f64::from(max_w) / f64::from(w)).min(f64::from(max_h) / f64::from(h));
    let nw = ((f64::from(w) * f).round() as u32).max(1);
    let nh = ((f64::from(h) * f).round() as u32).max(1);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let y0 = y * h / nh;
        let y1 = ((y + 1) * h / nh).max(y0 + 1);
        for x in 0..nw {
            let x0 = x * w / nw;
            let x1 = ((x + 1) * w / nw).max(x0 + 1);
            let mut acc = [0u32; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * w + sx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += u32::from(data[i + c]);
                    }
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            let o = ((y * nw + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    (nw, nh, out)
}

/// A font pool key: family, size in points and output scale (the float bits, so exact).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FontKey {
    family: String,
    size_bits: u32,
    scale_bits: u32,
}

impl FontKey {
    /// The key for `family` at `size_pt` points and output `scale`.
    pub fn new(family: &str, size_pt: f32, scale: f32) -> FontKey {
        FontKey {
            family: family.to_string(),
            size_bits: size_pt.to_bits(),
            scale_bits: scale.to_bits(),
        }
    }

    fn scale(&self) -> f32 {
        f32::from_bits(self.scale_bits)
    }
}

/// One window's handle on a pool entry: the key it is filed under and the shared fonts.
pub struct PooledFonts {
    /// The pool key these fonts were built for.
    pub key: FontKey,
    /// The shared fonts (single-threaded host, so `Rc<RefCell>`).
    pub fonts: Rc<std::cell::RefCell<Fonts>>,
}

/// Fonts shared by the windows of one host, one [`Fonts`] per `(family, size_pt, scale)`.
/// The pool holds weak references: an entry dies with its last window.
#[derive(Default)]
pub struct FontPool {
    entries: HashMap<FontKey, std::rc::Weak<std::cell::RefCell<Fonts>>>,
}

impl FontPool {
    /// The shared fonts for `key`, built with `make` when no live window uses that key.
    pub fn get_with(
        &mut self,
        key: FontKey,
        make: impl FnOnce(&FontKey) -> Result<Fonts, FontError>,
    ) -> Result<PooledFonts, FontError> {
        self.prune();
        if let Some(fonts) = self.entries.get(&key).and_then(std::rc::Weak::upgrade) {
            return Ok(PooledFonts { key, fonts });
        }
        let fonts = Rc::new(std::cell::RefCell::new(make(&key)?));
        self.entries.insert(key.clone(), Rc::downgrade(&fonts));
        Ok(PooledFonts { key, fonts })
    }

    /// The shared fonts for `family` at `size_pt` and `scale`, discovered through fontconfig.
    pub fn get(
        &mut self,
        family: &str,
        size_pt: f32,
        scale: f32,
    ) -> Result<PooledFonts, FontError> {
        self.get_with(FontKey::new(family, size_pt, scale), |_| {
            Fonts::discover(family, size_pt, scale)
        })
    }

    /// Move `held` to the entry for its family and size at `scale`: share a live entry if one
    /// exists; if `held` is the only user, rescale it in place and re-file it; otherwise build a
    /// new entry with `make`. A shared instance is never rescaled. On a build error `held` stays.
    pub fn rescale_with(
        &mut self,
        held: &mut PooledFonts,
        scale: f32,
        make: impl FnOnce(&FontKey) -> Result<Fonts, FontError>,
    ) -> Result<(), FontError> {
        let key = FontKey {
            scale_bits: scale.to_bits(),
            ..held.key.clone()
        };
        if key == held.key {
            return Ok(());
        }
        self.prune();
        if let Some(fonts) = self.entries.get(&key).and_then(std::rc::Weak::upgrade) {
            *held = PooledFonts { key, fonts };
            return Ok(());
        }
        if Rc::strong_count(&held.fonts) == 1 {
            self.entries.remove(&held.key);
            held.fonts.borrow_mut().set_scale(key.scale());
            self.entries.insert(key.clone(), Rc::downgrade(&held.fonts));
            held.key = key;
            return Ok(());
        }
        let next = self.get_with(key, make)?;
        *held = next;
        Ok(())
    }

    /// [`FontPool::rescale_with`] discovering through fontconfig.
    pub fn rescale(&mut self, held: &mut PooledFonts, scale: f32) -> Result<(), FontError> {
        self.rescale_with(held, scale, |k| {
            Fonts::discover(
                &k.family,
                f32::from_bits(k.size_bits),
                f32::from_bits(k.scale_bits),
            )
        })
    }

    /// Forget entries whose last window has gone.
    pub fn prune(&mut self) {
        self.entries.retain(|_, w| w.strong_count() > 0);
    }

    /// Live entries (tests and debugging).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_face(_: &FontKey) -> Result<Fonts, FontError> {
        let out = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", "DejaVu Sans Mono"])
            .output()
            .expect("fc-match");
        let path = String::from_utf8(out.stdout).expect("utf8");
        let face = Face::from_bytes(std::fs::read(path).expect("read"), 0).expect("parse");
        Ok(Fonts::from_faces(vec![face], [0; 4], None, 11.0, 1.0))
    }

    /// Two windows at one key share one instance; a scale change on a shared instance moves the
    /// window to a new entry (the other keeps its metrics), an unshared one rescales in place,
    /// and the entry dies with its last window.
    #[test]
    fn pool_shares_by_key_and_moves_on_rescale() {
        let mut pool = FontPool::default();
        let key = FontKey::new("x", 11.0, 1.0);
        let mut a = pool.get_with(key.clone(), one_face).expect("a");
        let b = pool
            .get_with(key.clone(), |_| panic!("must share"))
            .expect("b");
        assert!(Rc::ptr_eq(&a.fonts, &b.fonts));
        let w1 = b.fonts.borrow().metrics().cell_w;
        pool.rescale_with(&mut a, 2.0, one_face).expect("rescale");
        assert!(!Rc::ptr_eq(&a.fonts, &b.fonts));
        assert_eq!(b.fonts.borrow().metrics().cell_w, w1);
        assert_eq!(pool.len(), 2);
        drop(b);
        let before = Rc::as_ptr(&a.fonts);
        pool.rescale_with(&mut a, 1.0, |_| panic!("unshared rescales in place"))
            .expect("in place");
        assert_eq!(Rc::as_ptr(&a.fonts), before);
        assert_eq!(a.key, key);
        drop(a);
        pool.prune();
        assert_eq!(pool.len(), 0);
    }
}
