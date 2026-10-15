use crate::{dance_motion::Style, dancer::Dancer, engine::Engine, wave::Wave};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Mode {
    #[default]
    Wave,
    Dance,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Renderer {
    #[default]
    Braille,
    Sixel,
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub mode: Mode,
    pub style: Style,
    pub renderer: Renderer,
    pub sixel_update: crate::sixel::UpdateMode,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wave => "Wave",
            Self::Dance => "Dance",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Wave => Self::Dance,
            Self::Dance => Self::Wave,
        }
    }
}

pub struct Visualization {
    pub mode: Mode,
    pub renderer: Renderer,
    wave: Wave,
    dancer: Dancer,
}

impl Visualization {
    pub fn new(options: Options) -> Self {
        Self {
            mode: options.mode,
            renderer: options.renderer,
            wave: Wave::default(),
            dancer: Dancer::new(options.style),
        }
    }

    pub fn update(&mut self, engine: &Engine, columns: u16) {
        self.wave.update(engine);
        if self.mode == Mode::Dance {
            self.dancer.update(engine, columns);
        }
    }

    pub fn wants_frames(&self, engine: &Engine) -> bool {
        self.mode == Mode::Dance && self.dancer.wants_frames(engine)
    }

    pub fn render(&self, wave_area: Rect, dance_area: Rect, buffer: &mut Buffer) {
        (&self.wave).render(wave_area, buffer);
        if self.mode == Mode::Dance && self.renderer == Renderer::Braille {
            (&self.dancer).render(dance_area, buffer);
        }
    }

    pub fn sixel(
        &self,
        area: Rect,
        cell: crate::sixel::CellSize,
        buffer: &Buffer,
    ) -> Option<crate::sixel::Image> {
        let mut image = self.dancer.sixel(area, cell)?;
        image.protect_text(buffer, cell);
        Some(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dance_overlays_the_wave_and_preserves_every_cell_outside_its_stage() {
        let wave_area = Rect::new(0, 0, 80, 36);
        let dance_area = Rect::new(0, 21, 80, 15);
        let wave = Visualization::new(Options {
            mode: Mode::Wave,
            style: Style::Flow,
            ..Options::default()
        });
        let dance = Visualization::new(Options {
            mode: Mode::Dance,
            style: Style::Flow,
            ..Options::default()
        });
        let mut background = Buffer::empty(wave_area);
        let mut combined = Buffer::empty(wave_area);
        wave.render(wave_area, dance_area, &mut background);
        dance.render(wave_area, dance_area, &mut combined);
        let mut changed = 0;
        for y in 0..wave_area.height {
            for x in 0..wave_area.width {
                if !dance_area.contains((x, y).into()) {
                    assert_eq!(background[(x, y)], combined[(x, y)]);
                }
                changed += usize::from(background[(x, y)] != combined[(x, y)]);
            }
        }
        assert!(changed > 10);
        assert!(combined.content.iter().any(|cell| cell.symbol() != " "));
    }
}
