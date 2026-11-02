//! Small UI building blocks shared by the viewer and the builder: tappable
//! buttons that wrap on narrow (phone) screens, a hit-test registry, and a text field.

use std::cell::RefCell;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// Clickable regions recorded while drawing, looked up on mouse events.
pub struct Hits<T>(RefCell<Vec<(Rect, T)>>);

impl<T: Clone> Default for Hits<T> {
    fn default() -> Self {
        Self(RefCell::new(Vec::new()))
    }
}

impl<T: Clone> Hits<T> {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    pub fn push(&self, rect: Rect, t: T) {
        self.0.borrow_mut().push((rect, t));
    }

    /// The topmost region containing the point (later pushes win).
    pub fn at(&self, col: u16, row: u16) -> Option<(Rect, T)> {
        self.0
            .borrow()
            .iter()
            .rev()
            .find(|(r, _)| r.contains(Position { x: col, y: row }))
            .cloned()
    }
}

#[derive(Debug, Clone)]
pub struct Button<T> {
    pub label: String,
    pub action: T,
    /// Highlighted (e.g. a toggle that's on).
    pub active: bool,
}

impl<T> Button<T> {
    pub fn new(label: impl Into<String>, action: T) -> Self {
        Self {
            label: label.into(),
            action,
            active: false,
        }
    }

    pub fn active(mut self, on: bool) -> Self {
        self.active = on;
        self
    }

    fn width(&self) -> u16 {
        self.label.width() as u16 + 2
    }
}

/// Lay buttons out left to right, wrapping to new rows. Returns each button's
/// rect relative to (0, 0) and the number of rows used.
pub fn flow<T>(buttons: &[Button<T>], width: u16) -> (Vec<Rect>, u16) {
    let (mut x, mut y) = (0u16, 0u16);
    let mut rects = Vec::with_capacity(buttons.len());
    for b in buttons {
        let w = b.width().min(width.max(1));
        if x > 0 && x + w > width {
            x = 0;
            y += 1;
        }
        rects.push(Rect {
            x,
            y,
            width: w,
            height: 1,
        });
        x += w + 1;
    }
    (rects, if buttons.is_empty() { 0 } else { y + 1 })
}

/// Rows needed to show these buttons at this width.
pub fn flow_height<T>(buttons: &[Button<T>], width: u16) -> u16 {
    flow(buttons, width).1
}

/// Draw buttons into `area` (wrapping) and register them as hit targets.
pub fn render_buttons<T: Clone>(
    buttons: &[Button<T>],
    area: Rect,
    buf: &mut Buffer,
    hits: &Hits<T>,
) {
    let (rects, _) = flow(buttons, area.width);
    for (b, r) in buttons.iter().zip(rects) {
        if r.y >= area.height {
            break;
        }
        let rect = Rect {
            x: area.x + r.x,
            y: area.y + r.y,
            ..r
        };
        // Reverse video uses the terminal's own colours, so it's readable in any theme.
        let style = if b.active {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        };
        Line::from(Span::styled(format!(" {} ", b.label), style)).render(rect, buf);
        hits.push(rect, b.action.clone());
    }
}

/// A single-line text field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextInput {
    pub value: String,
    /// Cursor position in chars.
    pub cursor: usize,
}

impl TextInput {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn set(&mut self, value: impl Into<String>) {
        *self = Self::new(value);
    }

    fn byte(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map_or(self.value.len(), |(i, _)| i)
    }

    /// Handle a key; returns true if the value changed.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let len = self.value.chars().count();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.value.drain(..self.byte(self.cursor));
                self.cursor = 0;
                return true;
            }
            KeyCode::Char('k') if ctrl => {
                let at = self.byte(self.cursor);
                self.value.truncate(at);
                return true;
            }
            KeyCode::Char('w') if ctrl => {
                let before: String = self.value.chars().take(self.cursor).collect();
                let trimmed = before.trim_end();
                let start = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                let keep = trimmed[..start].chars().count();
                let (a, b) = (self.byte(keep), self.byte(self.cursor));
                self.value.drain(a..b);
                self.cursor = keep;
                return true;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char(c) if !ctrl => {
                let at = self.byte(self.cursor);
                self.value.insert(at, c);
                self.cursor += 1;
                return true;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let at = self.byte(self.cursor - 1);
                self.value.remove(at);
                self.cursor -= 1;
                return true;
            }
            KeyCode::Delete if self.cursor < len => {
                let at = self.byte(self.cursor);
                self.value.remove(at);
                return true;
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => {}
        }
        false
    }

    /// First visible char so the cursor stays on screen.
    fn scroll(&self, width: usize) -> usize {
        self.cursor.saturating_sub(width.saturating_sub(1))
    }

    /// Place the cursor from a click at `col` within a field drawn at `area`.
    pub fn click(&mut self, area: Rect, col: u16) {
        let offset = self.scroll(area.width as usize);
        let len = self.value.chars().count();
        self.cursor = (offset + col.saturating_sub(area.x) as usize).min(len);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, focused: bool, placeholder: &str) {
        let width = area.width as usize;
        let bg = if focused {
            Color::Rgb(40, 44, 60)
        } else {
            Color::Rgb(28, 30, 38)
        };
        let base = Style::default().bg(bg).fg(Color::White);
        buf.set_style(area, base);
        if self.value.is_empty() && !focused {
            Line::styled(placeholder.to_string(), base.fg(Color::DarkGray)).render(area, buf);
            return;
        }
        let offset = self.scroll(width);
        let shown: String = self.value.chars().skip(offset).take(width).collect();
        Line::styled(shown, base).render(area, buf);
        if focused {
            let x = area.x + (self.cursor - offset) as u16;
            if x < area.right() {
                let cell = &mut buf[(x, area.y)];
                cell.set_style(Style::default().bg(Color::Yellow).fg(Color::Black));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn flow_wraps() {
        let b: Vec<Button<u8>> = ["aaaa", "bbbb", "cccc"]
            .iter()
            .map(|l| Button::new(*l, 0))
            .collect();
        let (rects, rows) = flow(&b, 14);
        assert_eq!(rows, 2);
        assert_eq!((rects[1].x, rects[1].y), (7, 0));
        assert_eq!((rects[2].x, rects[2].y), (0, 1));
    }

    #[test]
    fn text_editing() {
        let mut t = TextInput::new("héllo");
        assert!(t.key(k(KeyCode::Backspace)));
        assert_eq!(t.value, "héll");
        t.key(k(KeyCode::Home));
        t.key(k(KeyCode::Right));
        t.key(k(KeyCode::Char('X')));
        assert_eq!(t.value, "hXéll");
        t.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.value, "éll");
        t.click(Rect::new(10, 0, 20, 1), 12);
        assert_eq!(t.cursor, 2);
    }

    #[test]
    fn hits_prefer_latest() {
        let h: Hits<u8> = Hits::default();
        h.push(Rect::new(0, 0, 10, 10), 1);
        h.push(Rect::new(2, 2, 2, 2), 2);
        assert_eq!(h.at(3, 3).map(|x| x.1), Some(2));
        assert_eq!(h.at(0, 0).map(|x| x.1), Some(1));
        assert!(h.at(20, 0).is_none());
    }
}
