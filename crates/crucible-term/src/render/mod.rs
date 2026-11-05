//! The CPU renderer: a pure paint function from (grid snapshot, metrics, damage) to a
//! premultiplied ARGB8888 pixel buffer (the `wl_shm` format). No I/O and no Wayland types:
//! glyphs come from [`Fonts`], whose shaping and rasterizing work on bytes already loaded.
//!
//! Underline colour (SGR 58) is not carried by the core model, so underlines draw in the
//! cell's foreground colour.

use crate::font::{Fonts, Metrics, Style};
use crucible_term_core::{Cell, Color, CursorKind, CursorStyle, Damage, Grid, Underline, Width};

/// An sRGB colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Colours: the 256-colour palette, default fg/bg, cursor, and the default background's alpha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Indices 0-15 configurable, 16-255 the xterm cube and grey ramp.
    pub palette: [Rgb; 256],
    /// Default foreground.
    pub fg: Rgb,
    /// Default background.
    pub bg: Rgb,
    /// Cursor colour.
    pub cursor: Rgb,
    /// Alpha of the default background (and padding); explicit cell backgrounds are opaque.
    pub alpha: u8,
}

/// The built-in 16 colours (foot's defaults).
const BASE16: [Rgb; 16] = [
    Rgb(0x24, 0x24, 0x24),
    Rgb(0xf6, 0x2b, 0x5a),
    Rgb(0x47, 0xb4, 0x13),
    Rgb(0xe3, 0xc4, 0x01),
    Rgb(0x24, 0xac, 0xd4),
    Rgb(0xf2, 0xaf, 0xfd),
    Rgb(0x13, 0xc2, 0x99),
    Rgb(0xe6, 0xe6, 0xe6),
    Rgb(0x61, 0x61, 0x61),
    Rgb(0xff, 0x4d, 0x51),
    Rgb(0x35, 0xd4, 0x50),
    Rgb(0xe9, 0xe8, 0x36),
    Rgb(0x5d, 0xc5, 0xf8),
    Rgb(0xfe, 0xab, 0xf2),
    Rgb(0x24, 0xdf, 0xc4),
    Rgb(0xff, 0xff, 0xff),
];

impl Default for Theme {
    fn default() -> Theme {
        let mut palette = [Rgb(0, 0, 0); 256];
        palette[..16].copy_from_slice(&BASE16);
        let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
        for i in 0..216 {
            palette[16 + i] = Rgb(level(i / 36), level((i / 6) % 6), level(i % 6));
        }
        for i in 0..24 {
            let v = (8 + i * 10) as u8;
            palette[232 + i] = Rgb(v, v, v);
        }
        Theme {
            palette,
            fg: Rgb(0xdc, 0xdc, 0xcc),
            bg: Rgb(0x11, 0x11, 0x11),
            cursor: Rgb(0xdc, 0xdc, 0xcc),
            alpha: 0xff,
        }
    }
}

/// A premultiplied ARGB8888 pixel buffer.
pub struct Canvas<'a> {
    /// `width * height` pixels, row-major.
    pub px: &'a mut [u32],
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
}

/// How to draw the cursor this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorDraw {
    /// Shape and blink request from the core.
    pub style: CursorStyle,
    /// The window has keyboard focus (unfocused: hollow block).
    pub focused: bool,
    /// The blink phase is "on" (the window's blink timer owns this; always true without blink).
    pub blink_on: bool,
}

/// Columns and rows that fit a `width` x `height` buffer with `pad` pixels on every side.
pub fn grid_size(width: u32, height: u32, m: &Metrics, pad: u32) -> (usize, usize) {
    let w = width.saturating_sub(2 * pad) / m.cell_w;
    let h = height.saturating_sub(2 * pad) / m.cell_h;
    (w.max(1) as usize, h.max(1) as usize)
}

/// The rows to repaint for `damage`, plus the cursor's old and new rows; `None` = everything.
pub fn rows_to_paint(
    damage: &Damage,
    old_cursor: Option<usize>,
    cursor: usize,
) -> Option<Vec<usize>> {
    match damage {
        Damage::Full => None,
        Damage::Rows(r) => {
            let mut v = r.clone();
            v.extend(old_cursor);
            v.push(cursor);
            v.sort_unstable();
            v.dedup();
            Some(v)
        }
    }
}

fn premul(c: Rgb, a: u8) -> u32 {
    let m = |v: u8| (u32::from(v) * u32::from(a) + 127) / 255;
    (u32::from(a) << 24) | (m(c.0) << 16) | (m(c.1) << 8) | m(c.2)
}

/// `src` (premultiplied) at coverage `cov` over `dst`.
#[inline]
fn over(dst: u32, src: u32, cov: u32) -> u32 {
    if cov == 0 {
        return dst;
    }
    let sa = ((src >> 24) * cov + 127) / 255;
    let inv = 255 - sa;
    let mut out = 0;
    for sh in [0, 8, 16, 24] {
        let s = ((src >> sh) & 0xff) * cov / 255;
        let d = (dst >> sh) & 0xff;
        out |= ((s + (d * inv + 127) / 255).min(255)) << sh;
    }
    out
}

fn resolve(c: Color, default: Rgb, t: &Theme) -> Rgb {
    match c {
        Color::Default => default,
        Color::Indexed(i) => t.palette[i as usize],
        Color::Rgb(r, g, b) => Rgb(r, g, b),
    }
}

fn mix(a: Rgb, b: Rgb) -> Rgb {
    let m = |x: u8, y: u8| ((u16::from(x) + u16::from(y)) / 2) as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// A row's pixel band, clipped to the canvas.
struct Band<'a, 'b> {
    c: &'a mut Canvas<'b>,
    y0: usize,
    y1: usize,
}

impl Band<'_, '_> {
    fn fill(&mut self, x0: i64, y0: i64, w: i64, h: i64, color: u32) {
        let xs = x0.max(0) as usize;
        let xe = ((x0 + w).max(0) as usize).min(self.c.width);
        let ys = (y0.max(0) as usize).max(self.y0);
        let ye = ((y0 + h).max(0) as usize).min(self.y1);
        for y in ys..ye {
            let row = &mut self.c.px[y * self.c.width..(y + 1) * self.c.width];
            for p in &mut row[xs.min(xe)..xe] {
                *p = color;
            }
        }
    }

    fn blend(&mut self, x: i64, y: i64, src: u32, cov: u32) {
        if x < 0 || y < self.y0 as i64 || y >= self.y1 as i64 || x >= self.c.width as i64 {
            return;
        }
        let i = y as usize * self.c.width + x as usize;
        self.c.px[i] = over(self.c.px[i], src, cov);
    }
}

/// Paint `grid` into `canvas`: every row when `rows` is `None` (padding too), otherwise only the
/// listed rows (each repainted across the full canvas width, side padding included). Returns
/// the repainted pixel bands as `(y, height)` for the window's buffer damage.
pub fn paint(
    canvas: &mut Canvas<'_>,
    grid: &Grid,
    theme: &Theme,
    fonts: &mut Fonts,
    pad: u32,
    cursor: CursorDraw,
    rows: Option<&[usize]>,
) -> Vec<(usize, usize)> {
    let m = fonts.metrics();
    let bg = premul(theme.bg, theme.alpha);
    let all: Vec<usize>;
    let rows = match rows {
        Some(r) => r,
        None => {
            canvas.px.fill(bg);
            all = (0..grid.rows).collect();
            &all
        }
    };
    let mut bands = Vec::new();
    for &r in rows {
        if r >= grid.screen.len() {
            continue;
        }
        let y0 = pad as usize + r * m.cell_h as usize;
        let y1 = (y0 + m.cell_h as usize).min(canvas.height);
        if y0 >= y1 {
            continue;
        }
        let cur = (grid.cursor.1 == r).then_some(grid.cursor.0);
        let mut band = Band { c: canvas, y0, y1 };
        paint_row(
            &mut band,
            &grid.screen[r].cells,
            theme,
            fonts,
            &m,
            pad,
            cur,
            cursor,
        );
        bands.push((y0, y1 - y0));
    }
    if rows.len() == grid.rows {
        // A full repaint: report the whole buffer (padding included).
        bands = vec![(0, canvas.height)];
    }
    bands
}

struct CellColors {
    fg: Rgb,
    bg: Option<Rgb>,
}

fn colors(cell: &Cell, t: &Theme) -> CellColors {
    let a = &cell.attrs;
    let mut fg = resolve(a.fg, t.fg, t);
    let mut bg = match a.bg {
        Color::Default => None,
        c => Some(resolve(c, t.bg, t)),
    };
    if a.inverse {
        let b = bg.unwrap_or(t.bg);
        bg = Some(fg);
        fg = b;
    }
    if a.faint {
        fg = mix(fg, bg.unwrap_or(t.bg));
    }
    CellColors { fg, bg }
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    b: &mut Band<'_, '_>,
    cells: &[Cell],
    t: &Theme,
    fonts: &mut Fonts,
    m: &Metrics,
    pad: u32,
    cur: Option<usize>,
    cursor: CursorDraw,
) {
    let (cw, ch) = (i64::from(m.cell_w), i64::from(m.cell_h));
    let y = b.y0 as i64;
    b.fill(0, y, b.c.width as i64, ch, premul(t.bg, t.alpha));
    let show = cursor.blink_on && cursor.style.shape != CursorKind::Hidden;
    let block = show && cursor.focused && cursor.style.shape == CursorKind::Block;
    // Backgrounds first, so glyph overhang from one cell is not erased by the next.
    for (x, cell) in cells.iter().enumerate() {
        let cx = i64::from(pad) + x as i64 * cw;
        let mut col = colors(cell, t);
        if block && cur == Some(x) {
            col.bg = Some(t.cursor);
        }
        if let Some(bgc) = col.bg {
            b.fill(cx, y, cw, ch, premul(bgc, 255));
        }
    }
    for (x, cell) in cells.iter().enumerate() {
        if cell.width == Width::Spacer {
            continue;
        }
        let cx = i64::from(pad) + x as i64 * cw;
        let span = if cell.width == Width::Wide { 2 } else { 1 };
        let mut col = colors(cell, t);
        if block && cur == Some(x) {
            col.fg = col.bg.unwrap_or(t.bg);
        }
        let fg = premul(col.fg, 255);
        if !cell.text.is_empty() {
            let style = Style::of(cell.attrs.bold, cell.attrs.italic);
            let shaped = fonts.shape(&cell.text, style);
            for p in shaped.iter() {
                let Some(g) = fonts.glyph(p.face, p.id) else {
                    continue;
                };
                if g.color {
                    let gx = cx + (cw * span - i64::from(g.w)) / 2;
                    let gy = y + (ch - i64::from(g.h)) / 2;
                    for (i, px) in g.data.chunks_exact(4).enumerate() {
                        let (ix, iy) = ((i as u32 % g.w) as i64, (i as u32 / g.w) as i64);
                        let src = (u32::from(px[3]) << 24)
                            | (u32::from(px[0]) << 16)
                            | (u32::from(px[1]) << 8)
                            | u32::from(px[2]);
                        b.blend(gx + ix, gy + iy, src, 255);
                    }
                } else {
                    let gx = cx + p.x.round() as i64 + i64::from(g.left);
                    let gy = y + i64::from(m.baseline) - p.y.round() as i64 - i64::from(g.top);
                    for (i, &cov) in g.data.iter().enumerate() {
                        let (ix, iy) = ((i as u32 % g.w) as i64, (i as u32 / g.w) as i64);
                        b.blend(gx + ix, gy + iy, fg, u32::from(cov));
                    }
                }
            }
        }
        decorate(b, cell, cx, y, cw * span, m, fg);
    }
    if let (true, Some(x)) = (show && !block, cur) {
        let cx = i64::from(pad) + x as i64 * cw;
        let s = i64::from(m.stroke);
        let c = premul(t.cursor, 255);
        match (cursor.focused, cursor.style.shape) {
            (false, _) => {
                b.fill(cx, y, cw, s, c);
                b.fill(cx, y + ch - s, cw, s, c);
                b.fill(cx, y, s, ch, c);
                b.fill(cx + cw - s, y, s, ch, c);
            }
            (true, CursorKind::Underline) => b.fill(cx, y + i64::from(m.underline_y), cw, s, c),
            (true, CursorKind::Beam) => b.fill(cx, y, s, ch, c),
            (true, _) => {}
        }
    }
}

fn decorate(b: &mut Band<'_, '_>, cell: &Cell, cx: i64, y: i64, w: i64, m: &Metrics, fg: u32) {
    let s = i64::from(m.stroke);
    let uy = y + i64::from(m.underline_y);
    match cell.attrs.underline {
        Underline::None => {}
        Underline::Single => b.fill(cx, uy, w, s, fg),
        Underline::Double => {
            let top = (uy - s).max(y);
            b.fill(cx, top, w, s, fg);
            b.fill(cx, top + 2 * s, w, s, fg);
        }
        Underline::Curly => {
            let amp = (s as f32).max(1.5);
            let period = i64::from(m.cell_w).max(4) as f32;
            for dx in 0..w {
                let phase = ((cx + dx) as f32 / period) * std::f32::consts::TAU;
                let cy = uy as f32 + amp * (0.5 - 0.5 * phase.cos()) - amp / 2.0;
                b.fill(cx + dx, cy.round() as i64, 1, s, fg);
            }
        }
        Underline::Dotted => {
            let mut dx = 0;
            while dx < w {
                b.fill(cx + dx, uy, s.min(w - dx), s, fg);
                dx += 2 * s;
            }
        }
        Underline::Dashed => {
            let dash = (i64::from(m.cell_w) / 3).max(1);
            let mut dx = 0;
            while dx < w {
                b.fill(cx + dx, uy, dash.min(w - dx), s, fg);
                dx += 2 * dash;
            }
        }
    }
    if cell.attrs.strikethrough {
        b.fill(cx, y + i64::from(m.strike_y), w, s, fg);
    }
}

#[cfg(test)]
mod tests;
