use crate::{audio::Levels, engine::Engine};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};
use std::time::Instant;

pub const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
// All temporal harmonics repeat together after ten full turns.
const PHASE_PERIOD: f32 = std::f32::consts::TAU * 10.0;

pub struct Wave {
    phase: f32,
    rms: f32,
    bass: f32,
    pulse: f32,
    peak: f32,
    active: bool,
    playback: (u64, u64),
    updated: Instant,
}

impl Default for Wave {
    fn default() -> Self {
        Self {
            phase: 0.0,
            rms: 0.0,
            bass: 0.0,
            pulse: 0.0,
            peak: 0.0,
            active: false,
            playback: (0, 0),
            updated: Instant::now(),
        }
    }
}

fn smooth(value: &mut f32, target: f32, dt: f32, attack: f32, release: f32) {
    let time = if target > *value { attack } else { release };
    *value += (target - *value) * (1.0 - (-dt / time).exp());
}

impl Wave {
    pub fn update(&mut self, engine: &Engine) {
        let now = Instant::now();
        let dt = now.duration_since(self.updated).as_secs_f32().min(0.1);
        self.updated = now;
        if self.playback != engine.playback_id() {
            self.playback = engine.playback_id();
            self.rms = 0.0;
            self.bass = 0.0;
            self.pulse = 0.0;
            self.peak = 0.0;
        }
        self.active = engine.track().is_some() && (!engine.paused || engine.is_scrubbing());
        if engine.track().is_none() {
            self.rms = 0.0;
            self.bass = 0.0;
            self.pulse = 0.0;
            self.peak = 0.0;
        }
        if self.active {
            self.step(engine.levels(), dt);
        }
    }

    fn step(&mut self, levels: Levels, dt: f32) {
        smooth(&mut self.rms, levels.rms.sqrt(), dt, 0.018, 0.09);
        smooth(&mut self.bass, levels.bass.sqrt(), dt, 0.014, 0.11);
        smooth(&mut self.pulse, levels.pulse, dt, 0.006, 0.055);
        smooth(&mut self.peak, levels.peak.sqrt(), dt, 0.007, 0.065);
        self.phase = (self.phase
            + dt * (1.8 + self.rms * 1.2 + self.bass * 1.4 + self.pulse * 5.5))
            % PHASE_PERIOD;
    }
}

fn dot(layer: &mut [u8], width: usize, x: usize, y: usize) {
    const BITS: [[u8; 2]; 4] = [[1, 8], [2, 16], [4, 32], [64, 128]];
    layer[(y / 4) * width + x / 2] |= BITS[y % 4][x % 2];
}

impl Widget for &Wave {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let inner = area;
        let width = usize::from(inner.width);
        let height = usize::from(inner.height);
        let pixels = (width * 2, height * 4);
        let center = (pixels.1 - 1) as f32 / 2.0;
        let amplitude = (self.rms * 0.50 + self.bass * 0.20 + self.pulse * 0.35).min(0.94);
        let detail = ((self.peak - self.rms * 0.8).max(0.0) * 0.55 + self.pulse * 0.45).min(1.0);
        let mut layers: [Vec<u8>; 3] = std::array::from_fn(|_| vec![0; width * height]);
        for (index, layer) in layers.iter_mut().enumerate() {
            let mut previous = center as usize;
            for x in 0..pixels.0 {
                let position = x as f32 / (pixels.0 - 1) as f32;
                let envelope = (std::f32::consts::PI * position).sin().max(0.0).sqrt();
                let phase = self.phase - index as f32 * 0.5;
                let angle = position * std::f32::consts::TAU;
                let signal = (angle * 2.4 - phase).sin() * 0.56
                    + (angle * 4.8 + phase * 1.3).sin() * 0.22
                    + (angle * 1.2 - phase * 0.7).sin() * 0.12
                    + (angle * 9.6 - phase * 2.1).sin() * detail * 0.14
                    + (angle * 14.4 + phase * 2.7).sin() * self.pulse * 0.10;
                let scale = [1.0, 0.78, 0.55][index];
                let y = (center + signal * center * amplitude * envelope * scale)
                    .round()
                    .clamp(0.0, (pixels.1 - 1) as f32) as usize;
                for point in previous.min(y)..=previous.max(y) {
                    dot(layer, width, x, point);
                }
                previous = y;
            }
        }
        let pulse = self.pulse;
        let brightness = 0.55 + self.peak * 0.45;
        let colors = if self.active {
            [
                Color::Rgb(
                    ((65.0 + pulse * 180.0) * brightness) as u8,
                    ((230.0 - pulse * 65.0) * brightness) as u8,
                    (240.0 * brightness) as u8,
                ),
                Color::Rgb(130, 95, 210),
                Color::Rgb(35, 65, 90),
            ]
        } else {
            [Color::Cyan, Color::DarkGray, Color::DarkGray]
        };
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let mask = layers[0][index] | layers[1][index] | layers[2][index];
                if mask == 0 {
                    continue;
                }
                let color = (0..3).find(|&layer| layers[layer][index] != 0).unwrap();
                buffer[(inner.x + x as u16, inner.y + y as u16)]
                    .set_char(char::from_u32(0x2800 + u32::from(mask)).unwrap())
                    .set_fg(colors[color]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_reach_full_strength_in_one_frame_and_release_between_fast_beats() {
        let mut wave = Wave::default();
        wave.step(
            Levels {
                pulse: 1.0,
                ..Levels::default()
            },
            1.0 / 30.0,
        );
        assert!(wave.pulse > 0.9, "A hit was blurred across multiple frames");
        for _ in 0..4 {
            wave.step(Levels::default(), 1.0 / 30.0);
        }
        assert!(wave.pulse < 0.15, "Fast consecutive hits blend together");
    }

    #[test]
    fn phase_boundaries_keep_the_wave_shape_continuous() {
        let area = Rect::new(0, 0, 80, 12);
        for (turns, pulse) in [(1.0, 0.2), (10.0, 0.2), (10.0, 1.0)] {
            let mut wave = Wave {
                phase: std::f32::consts::TAU * turns - 0.001,
                rms: 0.6,
                bass: 0.4,
                pulse,
                peak: 0.5,
                active: true,
                ..Wave::default()
            };
            let mut before = Buffer::empty(area);
            (&wave).render(area, &mut before);
            wave.step(
                Levels {
                    rms: 0.36,
                    bass: 0.16,
                    pulse,
                    peak: 0.25,
                },
                0.0005,
            );
            let mut after = Buffer::empty(area);
            (&wave).render(area, &mut after);
            let changed = before
                .content
                .iter()
                .zip(&after.content)
                .filter(|(a, b)| a.symbol() != b.symbol())
                .count();
            assert!(
                changed <= 20,
                "Wave jumped across {turns} turns: {changed} cells changed"
            );
        }
    }
}
