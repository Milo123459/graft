//! Convert a ratatui buffer to ANSI-escaped text for `--once` output.

use std::fmt::Write;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

fn sgr_color(c: Color, bg: bool) -> String {
    let base = if bg { 40 } else { 30 };
    let ext = if bg { 48 } else { 38 };
    match c {
        Color::Reset => (base + 9).to_string(),
        Color::Black => base.to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray => (base + 7).to_string(),
        Color::DarkGray => (base + 60).to_string(),
        Color::LightRed => (base + 61).to_string(),
        Color::LightGreen => (base + 62).to_string(),
        Color::LightYellow => (base + 63).to_string(),
        Color::LightBlue => (base + 64).to_string(),
        Color::LightMagenta => (base + 65).to_string(),
        Color::LightCyan => (base + 66).to_string(),
        Color::White => (base + 67).to_string(),
        Color::Indexed(i) => format!("{ext};5;{i}"),
        Color::Rgb(r, g, b) => format!("{ext};2;{r};{g};{b}"),
    }
}

fn sgr(fg: Color, bg: Color, m: Modifier) -> String {
    let mut codes = vec!["0".to_string()];
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
    ] {
        if m.contains(flag) {
            codes.push(code.into());
        }
    }
    if fg != Color::Reset {
        codes.push(sgr_color(fg, false));
    }
    if bg != Color::Reset {
        codes.push(sgr_color(bg, true));
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// Render the buffer as text, one line per row, trailing blanks trimmed.
pub fn to_ansi(buf: &Buffer, color: bool) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        // Find the last non-blank cell so lines don't carry trailing spaces.
        let end = (area.left()..area.right())
            .rev()
            .find(|&x| {
                let c = &buf[(x, y)];
                c.symbol() != " " || c.bg != Color::Reset
            })
            .map_or(area.left(), |x| x + 1);
        let mut current = (Color::Reset, Color::Reset, Modifier::empty());
        let mut covered = 0;
        for x in area.left()..end {
            // Cells hidden behind a wide character.
            if covered > 0 {
                covered -= 1;
                continue;
            }
            let cell = &buf[(x, y)];
            covered = cell.symbol().width().saturating_sub(1);
            if color {
                let style = (cell.fg, cell.bg, cell.modifier);
                if style != current {
                    out.push_str(&sgr(style.0, style.1, style.2));
                    current = style;
                }
            }
            out.push_str(cell.symbol());
        }
        if color && current != (Color::Reset, Color::Reset, Modifier::empty()) {
            out.push_str("\x1b[0m");
        }
        let _ = writeln!(out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn plain_and_coloured() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 5, 2));
        buf.set_string(0, 0, "hi", Style::default().fg(Color::Red));
        assert_eq!(to_ansi(&buf, false), "hi\n\n");
        assert_eq!(to_ansi(&buf, true), "\x1b[0;31mhi\x1b[0m\n\n");
    }
}
