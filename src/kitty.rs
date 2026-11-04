//! The kitty graphics protocol: charts' lines sent as images (kitty, Ghostty, WezTerm…).
//!
//! Each plot's picture is sent compressed (`o=z`) and placed over its cells under
//! the text (`z=-1`), so labels, markers and the cursor stay text on top. A picture
//! is sent again only when it changes; a new one replaces the old under a fresh id,
//! so there's no blank frame in between.

use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::render::pixels::Picture;

/// Whether this terminal draws kitty images, from what it says about itself. Inside
/// tmux or screen the escapes don't get through, so not there.
pub fn supported() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    if !var("TMUX").is_empty() || var("TERM").starts_with("screen") {
        return false;
    }
    let term = var("TERM");
    let program = var("TERM_PROGRAM");
    term.contains("kitty")
        || term.contains("ghostty")
        || ["ghostty", "wezterm", "kitty"].contains(&program.as_str())
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
}

/// Pixels per cell, from the terminal (a guess when it doesn't say).
pub fn cell_size() -> (f64, f64) {
    match ratatui::crossterm::terminal::window_size() {
        Ok(s) if s.width > 0 && s.height > 0 && s.columns > 0 && s.rows > 0 => (
            f64::from(s.width) / f64::from(s.columns),
            f64::from(s.height) / f64::from(s.rows),
        ),
        _ => (10.0, 20.0),
    }
}

/// What's on screen, to send only what changed.
#[derive(Debug, Default)]
pub struct Kitty {
    /// Per picture slot, what's showing.
    shown: Vec<Shown>,
    next_id: u32,
}

#[derive(Debug)]
struct Shown {
    id: u32,
    /// Everything it showed, and just where and what range (not the data).
    all: u64,
    frame: u64,
    at: Instant,
}

/// New data alone redraws a picture at most this often: a live feed ticks many
/// times a second, and each picture is a few hundred KB for the terminal to take in.
/// A move of the view (a pan, a zoom, a popup) redraws at once.
const DATA_EVERY: Duration = Duration::from_millis(1000);

impl Kitty {
    /// Escapes that bring the screen up to date with these pictures (empty: nothing to do).
    pub fn update(&mut self, pictures: &[Picture], cell: (f64, f64)) -> String {
        self.update_at(pictures, cell, Instant::now())
    }

    fn update_at(&mut self, pictures: &[Picture], cell: (f64, f64), now: Instant) -> String {
        let mut out = String::new();
        for (slot, p) in pictures.iter().enumerate() {
            let (all, frame) = fingerprint(p, cell);
            if let Some(s) = self.shown.get(slot)
                && (s.all == all || (s.frame == frame && now - s.at < DATA_EVERY))
            {
                continue;
            }
            self.next_id = self.next_id % 1_000_000 + 1;
            let id = self.next_id;
            let (w, h, rgba) = p.rasterize(cell);
            place(&mut out, id, p.rect, w, h, &rgba);
            let shown = Shown {
                id,
                all,
                frame,
                at: now,
            };
            match self.shown.get_mut(slot) {
                Some(s) => {
                    delete(&mut out, s.id);
                    *s = shown;
                }
                None => self.shown.push(shown),
            }
        }
        for s in self.shown.drain(pictures.len()..) {
            delete(&mut out, s.id);
        }
        out
    }

    /// Escapes that take every picture away (leaving pixels, or the viewer).
    pub fn clear(&mut self) -> String {
        if self.shown.is_empty() {
            return String::new();
        }
        self.shown.clear();
        "\x1b_Ga=d,d=A,q=2\x1b\\".into()
    }

    /// After the screen was cleared (a resize): send everything again.
    pub fn forget(&mut self) {
        self.shown.clear();
    }
}

/// Hashes of everything a picture shows, and of just its place, size, x range and
/// what covers it.
fn fingerprint(p: &Picture, cell: (f64, f64)) -> (u64, u64) {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (p.rect.x, p.rect.y, p.rect.width, p.rect.height).hash(&mut h);
    p.size(cell).hash(&mut h);
    p.x.0.to_bits().hash(&mut h);
    p.x.1.to_bits().hash(&mut h);
    p.covered.hash(&mut h);
    let frame = h.finish();
    p.y.0.to_bits().hash(&mut h);
    p.y.1.to_bits().hash(&mut h);
    for s in &p.strokes {
        format!("{:?}{}", s.color, s.dots).hash(&mut h);
        s.points.len().hash(&mut h);
        for pt in &s.points {
            pt.0.to_bits().hash(&mut h);
            pt.1.to_bits().hash(&mut h);
        }
    }
    for m in &p.shapes {
        format!("{:?}{:?}", m.kind, m.color).hash(&mut h);
        for v in [m.x, m.y, m.side] {
            v.to_bits().hash(&mut h);
        }
    }
    (h.finish(), frame)
}

/// Send an image and place it over `rect`'s cells, under the text, leaving the cursor
/// where it was.
fn place(out: &mut String, id: u32, rect: Rect, w: usize, h: usize, rgba: &[u8]) {
    let data = base64(&miniz_oxide::deflate::compress_to_vec_zlib(rgba, 1));
    // Save the cursor, go to the plot's top left (1-based).
    let _ = write!(out, "\x1b7\x1b[{};{}H", rect.y + 1, rect.x + 1);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (k, chunk) in chunks.iter().enumerate() {
        let more = u8::from(k + 1 < chunks.len());
        if k == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=T,f=32,o=z,s={w},v={h},i={id},p=1,c={},r={},C=1,z=-1,q=2,m={more};",
                rect.width, rect.height
            );
        } else {
            let _ = write!(out, "\x1b_Gm={more};");
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\x1b\\");
    }
    out.push_str("\x1b8");
}

fn delete(out: &mut String, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = u32::from(c[0]) << 16
            | u32::from(*c.get(1).unwrap_or(&0)) << 8
            | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..4 {
            if k <= c.len() {
                out.push(char::from(ABC[(n >> (18 - 6 * k)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::pixels::Stroke;
    use ratatui::style::Color;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    fn picture(y: f64) -> Picture {
        Picture {
            rect: Rect::new(3, 2, 40, 10),
            x: (0.0, 1.0),
            y: (0.0, 1.0),
            strokes: vec![Stroke {
                points: vec![(0.0, y), (1.0, 1.0 - y)],
                color: Color::Cyan,
                dots: false,
            }],
            ..Picture::default()
        }
    }

    #[test]
    fn sends_a_picture_once_and_again_only_when_it_changes() {
        let mut k = Kitty::default();
        let t = Instant::now();
        let first = k.update_at(&[picture(0.2)], (10.0, 20.0), t);
        // Placed at row 3, column 4 (1-based), 40×10 cells, under the text.
        assert!(first.starts_with(
            "\x1b7\x1b[3;4H\x1b_Ga=T,f=32,o=z,s=400,v=200,i=1,p=1,c=40,r=10,C=1,z=-1"
        ));
        assert!(first.ends_with("\x1b\\\x1b8"));
        // Every chunk is at most 4096 bytes of payload.
        for part in first.split("\x1b_G").skip(1) {
            let payload = part.split_once(';').unwrap().1;
            assert!(
                payload
                    .trim_end_matches("\x1b\\\x1b8")
                    .trim_end_matches("\x1b\\")
                    .len()
                    <= 4096
            );
        }
        assert_eq!(
            k.update_at(&[picture(0.2)], (10.0, 20.0), t),
            "",
            "unchanged"
        );
        // New data alone waits a second…
        let soon = t + Duration::from_millis(300);
        assert_eq!(k.update_at(&[picture(0.3)], (10.0, 20.0), soon), "");
        let later = t + Duration::from_millis(1100);
        let moved = k.update_at(&[picture(0.3)], (10.0, 20.0), later);
        assert!(moved.contains("i=2,"), "a fresh id");
        assert!(
            moved.ends_with("\x1b_Ga=d,d=I,i=1,q=2\x1b\\"),
            "then the old one goes"
        );
        // …but a move of the view goes at once.
        let mut panned = picture(0.3);
        panned.x = (0.5, 1.5);
        let soon = later + Duration::from_millis(50);
        assert!(k.update_at(&[panned], (10.0, 20.0), soon).contains("i=3,"));
        assert_eq!(k.update(&[], (10.0, 20.0)), "\x1b_Ga=d,d=I,i=3,q=2\x1b\\");
        assert_eq!(k.clear(), "", "nothing left to clear");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::render::pixels::Stroke;
    use ratatui::style::Color;

    #[test]
    #[ignore]
    fn time_a_frame() {
        let pts: Vec<(f64, f64)> = (0..3200)
            .map(|i| {
                (
                    i as f64,
                    (i as f64 / 50.0).sin() * 40.0 + (i as f64 * 7.3).sin() * 5.0,
                )
            })
            .collect();
        let p = Picture {
            rect: Rect::new(12, 1, 170, 45),
            x: (0.0, 3200.0),
            y: (-50.0, 50.0),
            strokes: vec![Stroke {
                points: pts,
                color: Color::Indexed(81),
                dots: false,
            }],
            ..Picture::default()
        };
        let cell = (18.0, 38.0);
        let t = std::time::Instant::now();
        let (w, h, rgba) = p.rasterize(cell);
        let r = t.elapsed();
        let t = std::time::Instant::now();
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&rgba, 1);
        let c = t.elapsed();
        let t = std::time::Instant::now();
        let b = base64(&z);
        let e = t.elapsed();
        eprintln!(
            "{w}x{h}: raster {r:?}, zlib {c:?} ({} KB), base64 {e:?} ({} KB)",
            z.len() / 1024,
            b.len() / 1024
        );
    }
}
