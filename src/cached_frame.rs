use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
};

const SYMBOL: u8 = 1;
const FOREGROUND: u8 = 2;
const BACKGROUND: u8 = 4;
const MODIFIERS: u8 = 8;

struct Paint {
    index: usize,
    fields: u8,
    cell: Cell,
}

/// Reusable background and sparse foreground, with animation drawn between them.
pub struct CachedFrame {
    area: Rect,
    background: Vec<Paint>,
    foreground: Vec<Paint>,
}

pub struct Builder {
    background: Buffer,
}

impl Builder {
    pub fn finish(self, buffer: &Buffer, highlight: impl FnOnce(&mut Buffer)) -> CachedFrame {
        let mut reference = self.background.clone();
        highlight(&mut reference);
        let mut foreground = Vec::new();
        for (index, (cell, base)) in buffer.content.iter().zip(&reference.content).enumerate() {
            let mut fields = 0;
            if !cell.symbol().is_empty() {
                fields = SYMBOL;
            } else {
                if cell.fg != base.fg {
                    fields |= FOREGROUND;
                }
                if cell.bg != base.bg {
                    fields |= BACKGROUND;
                }
                if cell.modifier != base.modifier {
                    fields |= MODIFIERS;
                }
            }
            if fields != 0 {
                foreground.push(Paint {
                    index,
                    fields,
                    cell: cell.clone(),
                });
            }
        }
        let background = self
            .background
            .content
            .into_iter()
            .enumerate()
            .filter(|(_, cell)| *cell != Cell::EMPTY)
            .map(|(index, cell)| Paint {
                index,
                fields: SYMBOL,
                cell,
            })
            .collect();
        CachedFrame {
            area: self.background.area,
            background,
            foreground,
        }
    }
}

impl CachedFrame {
    pub fn begin(buffer: &mut Buffer) -> Builder {
        let background = buffer.clone();
        // Empty symbols mark untouched cells; explicit spaces (including wide
        // glyph continuations) must still cover the animation underneath.
        for cell in &mut buffer.content {
            cell.set_symbol("");
        }
        Builder { background }
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    // Ratatui supplies an empty frame, so blank background cells need no copying.
    pub fn background(&self, buffer: &mut Buffer) {
        for paint in &self.background {
            buffer.content[paint.index].clone_from(&paint.cell);
        }
    }

    pub fn foreground(&self, buffer: &mut Buffer) {
        for paint in &self.foreground {
            let target = &mut buffer.content[paint.index];
            if paint.fields & SYMBOL != 0 {
                target.clone_from(&paint.cell);
            } else {
                if paint.fields & FOREGROUND != 0 {
                    target.fg = paint.cell.fg;
                }
                if paint.fields & BACKGROUND != 0 {
                    target.bg = paint.cell.bg;
                }
                if paint.fields & MODIFIERS != 0 {
                    target.modifier = paint.cell.modifier;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        layout::Rect,
        style::{Color, Modifier, Style},
        widgets::{Paragraph, Widget},
    };

    fn highlight(buffer: &mut Buffer) {
        for cell in &mut buffer.content[20..40] {
            cell.bg = Color::Rgb(25, 40, 50);
            if let Color::Rgb(r, g, b) = cell.fg {
                cell.fg = Color::Rgb(r / 2, g / 2, b / 2);
            }
        }
    }

    fn foreground(buffer: &mut Buffer) {
        Paragraph::new("Track name")
            .style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .render(Rect::new(2, 0, 15, 1), buffer);
        Paragraph::new("曲目 👩‍🎤")
            .style(Style::default().fg(Color::Gray))
            .render(Rect::new(1, 1, 18, 1), buffer);
        Paragraph::new(" Settings ")
            .style(Style::default().fg(Color::Cyan).bg(Color::Blue))
            .render(Rect::new(5, 2, 12, 1), buffer);
    }

    #[test]
    fn cached_layers_match_live_text_spaces_wide_glyphs_and_style_only_overlays() {
        for cover in [false, true] {
            let mut base = Buffer::empty(Rect::new(0, 0, 20, 4));
            if cover {
                for cell in &mut base.content {
                    cell.set_char('▀')
                        .set_fg(Color::Rgb(50, 60, 70))
                        .set_bg(Color::Rgb(10, 20, 30));
                }
            }
            let mut work = base.clone();
            let cached = CachedFrame::begin(&mut work);
            highlight(&mut work);
            foreground(&mut work);
            let cached = cached.finish(&work, highlight);
            for phase in 0..8 {
                let animate = |buffer: &mut Buffer| {
                    for (i, cell) in buffer.content.iter_mut().enumerate() {
                        if (i + phase) % 3 != 0 {
                            cell.set_char('⠿').set_fg(Color::Rgb(90, 180, 240));
                        }
                    }
                };
                let mut expected = base.clone();
                animate(&mut expected);
                highlight(&mut expected);
                foreground(&mut expected);
                let mut actual = Buffer::empty(base.area);
                cached.background(&mut actual);
                animate(&mut actual);
                highlight(&mut actual);
                cached.foreground(&mut actual);
                assert_eq!(actual, expected, "cover={cover} phase={phase}");
            }
        }
    }
}
