use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Widget, Wrap},
};
use std::io::{self, Write};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

struct Glyph {
    area: Rect,
    bytes: (usize, usize),
}

#[derive(Default)]
pub struct TextSelection {
    key: String,
    text: String,
    area: Rect,
    image: Option<Buffer>,
    glyphs: Vec<Glyph>,
    anchor: Option<usize>,
    selected: Option<(usize, usize)>,
}

impl TextSelection {
    pub fn clear(&mut self) {
        self.anchor = None;
        self.selected = None;
    }

    pub fn cancel_drag(&mut self) {
        self.anchor = None;
    }

    pub fn range(&self) -> Option<(usize, usize)> {
        self.selected
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selected.map(|(start, end)| &self.text[start..end])
    }

    pub fn height(&self, key: &str, text: &str, width: u16, limit: u16) -> u16 {
        if width == 0 || limit == 0 {
            return 0;
        }
        if self.key == key
            && self.text == text
            && self
                .image
                .as_ref()
                .is_some_and(|image| image.area.width == width)
            && self.complete()
        {
            return self.area.height.min(limit);
        }
        let mut image = Buffer::empty(Rect::new(0, 0, width, limit));
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .render(image.area, &mut image);
        rendered_height(&image)
    }

    pub fn render(&mut self, key: &str, text: &str, area: Rect, buffer: &mut Buffer) -> u16 {
        if area.is_empty() {
            *self = Self::default();
            return 0;
        }
        if self.key != key
            || self.text != text
            || self.area.x != area.x
            || self.area.y != area.y
            || self.image.as_ref().is_none_or(|image| {
                image.area.width != area.width
                    || image.area.height != area.height
                        && !(self.complete() && self.area.height <= area.height)
            })
        {
            self.clear();
            self.key = key.into();
            self.text = text.into();
            let mut image = Buffer::empty(Rect::new(0, 0, area.width, area.height));
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .render(image.area, &mut image);
            let height = rendered_height(&image);
            self.area = Rect::new(area.x, area.y, area.width, height);
            self.glyphs.clear();
            let mut source = text.grapheme_indices(true).peekable();
            for y in 0..height {
                let Some(last) = (0..area.width)
                    .rev()
                    .find(|&x| !image[(x, y)].symbol().trim().is_empty())
                else {
                    continue;
                };
                let mut x = 0;
                while x <= last {
                    let symbol = image[(x, y)].symbol();
                    while let Some(&(_, grapheme)) = source.peek() {
                        if grapheme == symbol {
                            break;
                        }
                        if grapheme.chars().all(char::is_whitespace)
                            || grapheme.width() == 0
                            || grapheme.width() > usize::from(area.width)
                        {
                            source.next();
                        } else {
                            break;
                        }
                    }
                    let width = symbol.width().max(1) as u16;
                    if let Some(&(start, grapheme)) = source.peek()
                        && grapheme == symbol
                    {
                        self.glyphs.push(Glyph {
                            area: Rect::new(x, y, width, 1),
                            bytes: (start, start + grapheme.len()),
                        });
                        source.next();
                    }
                    x += width;
                }
            }
            self.image = Some(image);
        }
        let image = self.image.as_ref().unwrap();
        let artist = text.find('\n').map_or(text.len(), |start| start + 1);
        for glyph in &self.glyphs {
            let style = if glyph.bytes.0 < artist {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Gray)
                    .remove_modifier(Modifier::BOLD)
            };
            for x in glyph.area.x..glyph.area.right() {
                buffer[(area.x + x, area.y + glyph.area.y)]
                    .set_symbol(image[(x, glyph.area.y)].symbol())
                    .set_style(style);
            }
        }
        if let Some((start, end)) = self.selected {
            for glyph in &self.glyphs {
                if glyph.bytes.0 >= start && glyph.bytes.1 <= end {
                    for x in glyph.area.x..glyph.area.right() {
                        buffer[(area.x + x, area.y + glyph.area.y)]
                            .set_fg(Color::Black)
                            .set_bg(Color::Cyan);
                    }
                }
            }
        }
        self.area.height
    }

    fn complete(&self) -> bool {
        self.glyphs
            .last()
            .is_some_and(|glyph| glyph.bytes.1 == self.text.trim_end().len())
    }

    fn nearest(&self, column: u16, row: u16) -> Option<usize> {
        if row >= self.area.bottom() {
            return self.glyphs.len().checked_sub(1);
        }
        let y = row.saturating_sub(self.area.y);
        let x = column
            .saturating_sub(self.area.x)
            .min(self.area.width.saturating_sub(1));
        self.glyphs
            .iter()
            .rposition(|glyph| glyph.area.y < y || glyph.area.y == y && glyph.area.x <= x)
            .or((!self.glyphs.is_empty()).then_some(0))
    }

    fn extend(&mut self, end: usize) {
        if let Some(start) = self.anchor {
            self.selected = Some((
                self.glyphs[start.min(end)].bytes.0,
                self.glyphs[start.max(end)].bytes.1,
            ));
        }
    }

    pub fn handle(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.clear();
                if self.area.contains((mouse.column, mouse.row).into()) {
                    self.anchor = self.nearest(mouse.column, mouse.row);
                    return self.anchor.is_some();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                if self.anchor.is_some() =>
            {
                if let Some(end) = self.nearest(mouse.column, mouse.row)
                    && (mouse.kind == MouseEventKind::Drag(MouseButton::Left)
                        || self.anchor != Some(end)
                        || self.selected.is_some())
                {
                    self.extend(end);
                }
                if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                    self.anchor = None;
                }
                return true;
            }
            MouseEventKind::Down(MouseButton::Right)
                if self.anchor.is_some() || self.selected.is_some() =>
            {
                self.clear();
                return true;
            }
            _ => {}
        }
        false
    }
}

fn rendered_height(image: &Buffer) -> u16 {
    (0..image.area.height)
        .rev()
        .find(|&y| (0..image.area.width).any(|x| !image[(x, y)].symbol().trim().is_empty()))
        .map_or(1, |y| y + 1)
}

pub fn copy_to_clipboard(mut writer: impl Write, text: &str) -> io::Result<()> {
    write!(writer, "\x1b]52;c;{}\x1b\\", STANDARD.encode(text))?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn wrapped_unicode_selection_keeps_exact_source_text_and_survives_redraw() {
        let area = Rect::new(3, 4, 9, 4);
        let title = "Cafe\u{301} 世界 longwordwrapped";
        let mut buffer = Buffer::empty(area);
        let mut selection = TextSelection::default();
        selection.render("track", title, area, &mut buffer);
        assert!(selection.handle(mouse(MouseEventKind::Down(MouseButton::Left), 3, 4)));
        assert!(selection.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 100, 100)));
        assert!(selection.handle(mouse(MouseEventKind::Up(MouseButton::Left), 100, 100)));
        assert_eq!(selection.selected_text(), Some(title));
        selection.render("track", title, area, &mut buffer);
        assert_eq!(selection.selected_text(), Some(title));
        assert_eq!(buffer[(3, 4)].bg, Color::Cyan);
        assert!(selection.handle(mouse(MouseEventKind::Down(MouseButton::Left), 11, 4)));
        assert!(selection.handle(mouse(MouseEventKind::Up(MouseButton::Left), 8, 4)));
        assert_eq!(selection.selected_text(), Some("世界"));
        selection.render("next", title, area, &mut buffer);
        assert_eq!(selection.selected_text(), None);
    }

    #[test]
    fn simple_click_does_not_select_and_other_controls_receive_their_gestures() {
        let area = Rect::new(1, 1, 20, 3);
        let mut selection = TextSelection::default();
        selection.render("track", "A track name", area, &mut Buffer::empty(area));
        assert!(selection.handle(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1)));
        assert!(selection.handle(mouse(MouseEventKind::Up(MouseButton::Left), 3, 1)));
        assert_eq!(selection.selected_text(), None);
        assert!(!selection.handle(mouse(MouseEventKind::Down(MouseButton::Left), 3, 5)));
        assert!(!selection.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 3, 1)));
        assert!(!selection.handle(mouse(MouseEventKind::Up(MouseButton::Left), 3, 1)));
        assert!(!selection.handle(mouse(MouseEventKind::ScrollUp, 3, 1)));
    }

    #[test]
    fn artist_selection_and_dragging_across_track_fields_preserve_unicode_and_newlines() {
        let area = Rect::new(2, 3, 12, 4);
        let artist = "Cafe\u{301} 世界";
        let text = format!("Track\n{artist}");
        let mut buffer = Buffer::empty(area);
        let mut selection = TextSelection::default();
        selection.render("track", &text, area, &mut buffer);
        assert!(selection.handle(mouse(MouseEventKind::Down(MouseButton::Left), 2, 4)));
        assert!(selection.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 30, 4)));
        assert!(selection.handle(mouse(MouseEventKind::Up(MouseButton::Left), 30, 4)));
        assert_eq!(selection.selected_text(), Some(artist));
        selection.render("track", &text, area, &mut buffer);
        assert_eq!(selection.selected_text(), Some(artist));
        assert_eq!(buffer[(2, 4)].bg, Color::Cyan);
        assert_ne!(buffer[(2, 3)].bg, Color::Cyan);
        assert!(selection.handle(mouse(MouseEventKind::Down(MouseButton::Left), 2, 3)));
        assert!(selection.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 30, 4)));
        assert!(selection.handle(mouse(MouseEventKind::Up(MouseButton::Left), 30, 4)));
        assert_eq!(selection.selected_text(), Some(text.as_str()));
        let mut copied = Vec::new();
        copy_to_clipboard(&mut copied, selection.selected_text().unwrap()).unwrap();
        let payload = &copied[b"\x1b]52;c;".len()..copied.len() - 2];
        assert_eq!(STANDARD.decode(payload).unwrap(), text.as_bytes());
    }

    #[test]
    fn clipboard_payload_is_exact_utf8_and_cannot_inject_terminal_commands() {
        let text = "Track 世界\u{1b}]52;c;injected\u{7}";
        let mut output = Vec::new();
        copy_to_clipboard(&mut output, text).unwrap();
        let output = String::from_utf8(output).unwrap();
        let payload = output
            .strip_prefix("\u{1b}]52;c;")
            .unwrap()
            .strip_suffix("\u{1b}\\")
            .unwrap();
        assert_eq!(STANDARD.decode(payload).unwrap(), text.as_bytes());
        assert!(!payload.contains(['\u{1b}', '\u{7}']));
    }

    #[test]
    fn title_and_artist_overlay_preserves_artwork_and_wave_outside_the_text() {
        let area = Rect::new(2, 3, 16, 4);
        let mut buffer = Buffer::empty(area);
        for cell in &mut buffer.content {
            cell.set_char('⠿')
                .set_fg(Color::Magenta)
                .set_bg(Color::Rgb(12, 24, 36));
        }
        let mut selection = TextSelection::default();
        selection.render("track", "Track\nArtist", area, &mut buffer);
        assert_eq!(buffer[(2, 3)].symbol(), "T");
        assert!(buffer[(2, 3)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(2, 4)].symbol(), "A");
        assert_eq!(buffer[(2, 4)].fg, Color::Gray);
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| cell.bg == Color::Rgb(12, 24, 36))
        );
        assert_eq!(buffer[(12, 3)].symbol(), "⠿");
        assert_eq!(buffer[(12, 3)].fg, Color::Magenta);
        assert_eq!(buffer[(2, 5)].symbol(), "⠿");
    }

    #[test]
    fn compact_height_respects_word_wrapping_and_keeps_visible_artist_selection() {
        let text = "aaaa aaaa aaaa\nArtist";
        let mut selection = TextSelection::default();
        assert_eq!(selection.height("track", text, 7, 10), 4);
        let area = Rect::new(2, 3, 7, 10);
        selection.render("track", text, area, &mut Buffer::empty(area));
        selection.handle(mouse(MouseEventKind::Down(MouseButton::Left), 2, 6));
        selection.handle(mouse(MouseEventKind::Up(MouseButton::Left), 8, 6));
        assert_eq!(selection.selected_text(), Some("Artist"));
        let compact = Rect::new(2, 3, 7, 4);
        selection.render("track", text, compact, &mut Buffer::empty(compact));
        assert_eq!(selection.selected_text(), Some("Artist"));
        assert_eq!(selection.height("track", text, 7, 10), 4);
        let clipped = Rect::new(2, 3, 7, 2);
        selection.render("track", text, clipped, &mut Buffer::empty(clipped));
        assert_eq!(selection.height("track", text, 7, 10), 4);
    }
}
