//! Lines drawn as real pixels (for terminals with the kitty graphics protocol): the
//! chart's text is drawn as usual and its lines become an image under the text.

use std::cell::RefCell;

use ratatui::layout::Rect;
use ratatui::style::Color;

/// A line (or dots) to paint, in data coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub points: Vec<(f64, f64)>,
    pub color: Color,
    /// Dots at each point instead of a line through them.
    pub dots: bool,
}

/// One plot's image: where it goes (in cells), what it shows, and what's on it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Picture {
    pub rect: Rect,
    pub x: (f64, f64),
    pub y: (f64, f64),
    pub strokes: Vec<Stroke>,
    /// x ranges shaded (a measured stretch).
    pub shade: Vec<(f64, f64)>,
    /// Cells covered by text or a popup (the image is left clear there), row by row.
    pub covered: Vec<bool>,
}

/// Pixel drawing for one frame: the size of a cell and the pictures drawn.
#[derive(Debug, Default)]
pub struct Pixels {
    /// Pixels per cell (width, height).
    pub cell: (f64, f64),
    pub pictures: RefCell<Vec<Picture>>,
}

impl Picture {
    /// Image size in pixels for a cell size, capped so a big screen stays cheap to send.
    pub fn size(&self, cell: (f64, f64)) -> (usize, usize) {
        let (w, h) = (
            f64::from(self.rect.width) * cell.0,
            f64::from(self.rect.height) * cell.1,
        );
        let scale = (MAX_PIXELS / (w * h).max(1.0)).sqrt().min(1.0);
        (((w * scale) as usize).max(1), ((h * scale) as usize).max(1))
    }

    /// The image as RGBA bytes (transparent where nothing is drawn).
    pub fn rasterize(&self, cell: (f64, f64)) -> (usize, usize, Vec<u8>) {
        let (w, h) = self.size(cell);
        let mut canvas = Canvas::new(w, h);
        let (x0, x1) = self.x;
        let (y0, y1) = self.y;
        let sx = w as f64 / (x1 - x0).max(f64::MIN_POSITIVE);
        let sy = h as f64 / (y1 - y0).max(f64::MIN_POSITIVE);
        let at = |p: &(f64, f64)| ((p.0 - x0) * sx, (y1 - p.1) * sy);
        for &(a, b) in &self.shade {
            let (l, r) = (at(&(a.max(x0), 0.0)).0, at(&(b.min(x1), 0.0)).0);
            canvas.fill_columns(l, r, rgb(Color::Indexed(236)), 0.6);
        }
        // About a ninth of a row: thin, but never a hairline.
        let px_per_row = h as f64 / f64::from(self.rect.height.max(1));
        let width = (px_per_row / 9.0).clamp(1.2, 4.0);
        for s in &self.strokes {
            let pts: Vec<(f64, f64)> = s
                .points
                .iter()
                .filter(|p| p.0.is_finite() && p.1.is_finite())
                .map(at)
                .collect();
            if s.dots {
                canvas.dots(&pts, width * 0.9);
            } else {
                canvas.polyline(&pts, width);
            }
            canvas.composite(rgb(s.color));
        }
        // Keep text and popups readable: nothing under them.
        let (cw, ch) = (
            w as f64 / f64::from(self.rect.width.max(1)),
            h as f64 / f64::from(self.rect.height.max(1)),
        );
        for (i, _) in self.covered.iter().enumerate().filter(|c| *c.1) {
            let (col, row) = (
                i % usize::from(self.rect.width),
                i / usize::from(self.rect.width),
            );
            canvas.clear_rect(
                (col as f64 * cw) as usize,
                (row as f64 * ch) as usize,
                ((col + 1) as f64 * cw).ceil() as usize,
                ((row + 1) as f64 * ch).ceil() as usize,
            );
        }
        (w, h, canvas.rgba)
    }
}

/// Most pixels in one image (about 1920×1250).
const MAX_PIXELS: f64 = 2.4e6;

struct Canvas {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
    /// The stroke being drawn: how much of each pixel it covers.
    cover: Vec<f32>,
    /// Pixels the stroke touched (to composite and reset only those).
    dirty: (usize, usize, usize, usize),
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            rgba: vec![0; w * h * 4],
            cover: vec![0.0; w * h],
            dirty: (usize::MAX, usize::MAX, 0, 0),
        }
    }

    fn touch(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        let d = &mut self.dirty;
        *d = (d.0.min(x0), d.1.min(y0), d.2.max(x1), d.3.max(y1));
    }

    /// The pixel range (clipped) a box around `[lo, hi]` covers.
    fn span(lo: f64, hi: f64, pad: f64, n: usize) -> (usize, usize) {
        let a = (lo - pad).floor().max(0.0) as usize;
        let b = ((hi + pad).ceil().max(0.0) as usize).min(n);
        (a.min(n), b)
    }

    /// An anti-aliased line through the points.
    fn polyline(&mut self, pts: &[(f64, f64)], width: f64) {
        if let [p] = pts {
            return self.dots(&[*p], width * 0.8);
        }
        for seg in pts.windows(2) {
            self.segment(seg[0], seg[1], width);
        }
    }

    fn segment(&mut self, a: (f64, f64), b: (f64, f64), width: f64) {
        let half = width / 2.0;
        let pad = half + 1.0;
        let (xa, xb) = Self::span(a.0.min(b.0), a.0.max(b.0), pad, self.w);
        let (ya, yb) = Self::span(a.1.min(b.1), a.1.max(b.1), pad, self.h);
        if xa >= xb || ya >= yb {
            return;
        }
        self.touch(xa, ya, xb, yb);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        for y in ya..yb {
            let py = y as f64 + 0.5;
            for x in xa..xb {
                let px = x as f64 + 0.5;
                let t = if len2 > 0.0 {
                    (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (ex, ey) = (px - (a.0 + t * dx), py - (a.1 + t * dy));
                let d = (ex * ex + ey * ey).sqrt();
                let c = (half + 0.5 - d).clamp(0.0, 1.0) as f32;
                let cell = &mut self.cover[y * self.w + x];
                *cell = cell.max(c);
            }
        }
    }

    /// A filled, anti-aliased disc at each point.
    fn dots(&mut self, pts: &[(f64, f64)], radius: f64) {
        for &p in pts {
            self.segment(p, p, radius * 2.0);
        }
    }

    /// Lay the stroke's coverage onto the image in a colour.
    fn composite(&mut self, (r, g, b): (u8, u8, u8)) {
        let (x0, y0, x1, y1) = self.dirty;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                let i = y * self.w + x;
                let a = std::mem::take(&mut self.cover[i]);
                if a > 0.0 {
                    blend(&mut self.rgba[i * 4..i * 4 + 4], (r, g, b), a);
                }
            }
        }
        self.dirty = (usize::MAX, usize::MAX, 0, 0);
    }

    /// Shade whole columns from `l` to `r` (pixels).
    fn fill_columns(&mut self, l: f64, r: f64, color: (u8, u8, u8), alpha: f32) {
        let (a, b) = Self::span(l, r, 0.0, self.w);
        for y in 0..self.h {
            for x in a..b {
                let i = y * self.w + x;
                blend(&mut self.rgba[i * 4..i * 4 + 4], color, alpha);
            }
        }
    }

    fn clear_rect(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        for y in y0..y1.min(self.h) {
            let row = y * self.w;
            self.rgba[(row + x0.min(self.w)) * 4..(row + x1.min(self.w)) * 4].fill(0);
        }
    }
}

/// `src` over the pixel, with straight (not premultiplied) alpha.
fn blend(px: &mut [u8], (r, g, b): (u8, u8, u8), a: f32) {
    let da = f32::from(px[3]) / 255.0;
    let out = a + da * (1.0 - a);
    if out <= 0.0 {
        return;
    }
    let mix = |s: u8, d: u8| {
        ((f32::from(s) * a + f32::from(d) * da * (1.0 - a)) / out)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    px[0] = mix(r, px[0]);
    px[1] = mix(g, px[1]);
    px[2] = mix(b, px[2]);
    px[3] = (out * 255.0).round() as u8;
}

/// A terminal colour as RGB (xterm's palette for the named and indexed ones).
pub fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (0xcd, 0x31, 0x31),
        Color::Green => (0x0d, 0xbc, 0x79),
        Color::Yellow => (0xe5, 0xe5, 0x10),
        Color::Blue => (0x24, 0x72, 0xc8),
        Color::Magenta => (0xbc, 0x3f, 0xbc),
        Color::Cyan => (0x11, 0xa8, 0xcd),
        Color::Gray => (0xc0, 0xc0, 0xc0),
        Color::DarkGray => (0x76, 0x76, 0x76),
        Color::LightRed => (0xf1, 0x4c, 0x4c),
        Color::LightGreen => (0x23, 0xd1, 0x8b),
        Color::LightYellow => (0xf5, 0xf5, 0x43),
        Color::LightBlue => (0x3b, 0x8e, 0xea),
        Color::LightMagenta => (0xd6, 0x70, 0xd6),
        Color::LightCyan => (0x29, 0xb8, 0xdb),
        Color::White | Color::Reset => (0xe5, 0xe5, 0xe5),
        Color::Indexed(i) => indexed(i),
    }
}

fn indexed(i: u8) -> (u8, u8, u8) {
    const BASIC: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    match i {
        0..=15 => rgb(BASIC[usize::from(i)]),
        16..=231 => {
            let i = i - 16;
            let lvl = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (lvl(i / 36), lvl((i / 6) % 6), lvl(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(strokes: Vec<Stroke>) -> Picture {
        Picture {
            rect: Rect::new(0, 0, 10, 5),
            x: (0.0, 10.0),
            y: (0.0, 10.0),
            strokes,
            ..Picture::default()
        }
    }

    fn alpha(img: &[u8], w: usize, x: usize, y: usize) -> u8 {
        img[(y * w + x) * 4 + 3]
    }

    #[test]
    fn a_line_is_painted_in_its_colour_and_the_rest_is_clear() {
        // A flat line across the middle, 10×20-pixel cells.
        let p = picture(vec![Stroke {
            points: vec![(0.0, 5.0), (10.0, 5.0)],
            color: Color::Indexed(81),
            dots: false,
        }]);
        let (w, h, img) = p.rasterize((10.0, 20.0));
        assert_eq!((w, h), (100, 100));
        assert_eq!(img.len(), w * h * 4);
        let mid = (50 * w + 50) * 4;
        assert_eq!(&img[mid..mid + 4], &[0x5f, 0xd7, 0xff, 255], "on the line");
        assert_eq!(alpha(&img, w, 50, 10), 0, "far above it");
        assert_eq!(alpha(&img, w, 50, 90), 0, "far below it");
    }

    #[test]
    fn covered_cells_are_left_clear() {
        let mut p = picture(vec![Stroke {
            points: vec![(0.0, 5.0), (10.0, 5.0)],
            color: Color::White,
            dots: false,
        }]);
        p.covered = vec![false; 50];
        // Row 2 (the middle), column 4: a label sits there.
        p.covered[2 * 10 + 4] = true;
        let (w, _, img) = p.rasterize((10.0, 20.0));
        assert_eq!(alpha(&img, w, 45, 50), 0, "under the label");
        assert_eq!(alpha(&img, w, 25, 50), 255, "beside it");
    }

    #[test]
    fn dots_and_shading() {
        let mut p = picture(vec![Stroke {
            points: vec![(2.0, 8.0)],
            color: Color::Red,
            dots: true,
        }]);
        p.shade = vec![(6.0, 8.0)];
        let (w, _, img) = p.rasterize((10.0, 20.0));
        assert_eq!(alpha(&img, w, 20, 20), 255, "the dot");
        assert_eq!(alpha(&img, w, 40, 20), 0, "nothing beside it");
        assert!(alpha(&img, w, 70, 50) > 0, "the shaded stretch");
    }

    #[test]
    fn a_huge_screen_is_capped() {
        let mut p = picture(Vec::new());
        p.rect = Rect::new(0, 0, 400, 120);
        let (w, h) = p.size((20.0, 40.0));
        assert!(w * h <= MAX_PIXELS as usize + w + h);
        assert!((w as f64 / h as f64 - 400.0 * 20.0 / (120.0 * 40.0)).abs() < 0.01);
    }
}
