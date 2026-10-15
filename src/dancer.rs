use crate::{
    audio::Levels,
    dance_motion::{Features, Genre, Motion, Point, Style},
    dance_space::{Capsule, Skeleton, Vec3},
    engine::Engine,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};
use std::{collections::VecDeque, time::Instant};

pub struct Dancer {
    motion: Motion,
    energy: f32,
    bass: f32,
    accent: f32,
    treble: f32,
    confidence: f32,
    previous_pulse: f32,
    since_beat: f32,
    beat_period: f32,
    heard_beat: bool,
    playback_speed: f32,
    playback: (u64, u64),
    updated: Instant,
    history: VecDeque<Skeleton>,
    history_clock: f32,
    was_playing: bool,
    genre: Genre,
    genre_playback: Option<(u64, u64)>,
}

fn follow(value: &mut f32, target: f32, dt: f32, attack: f32, release: f32) {
    let time = if target > *value { attack } else { release };
    *value += (target - *value) * (1.0 - (-dt / time).exp());
}

impl Dancer {
    pub fn new(style: Style) -> Self {
        Self {
            motion: Motion::new(style),
            energy: 0.0,
            bass: 0.0,
            accent: 0.0,
            treble: 0.0,
            confidence: 0.0,
            previous_pulse: 0.0,
            since_beat: 0.0,
            beat_period: 0.5,
            heard_beat: false,
            playback_speed: 1.0,
            playback: (0, 0),
            updated: Instant::now(),
            history: VecDeque::with_capacity(9),
            history_clock: 0.0,
            was_playing: false,
            genre: Genre::Unknown,
            genre_playback: None,
        }
    }

    pub fn update(&mut self, engine: &Engine, columns: u16) {
        let now = Instant::now();
        let dt = now.duration_since(self.updated).as_secs_f32().min(0.1);
        self.updated = now;
        let playback = engine.playback_id();
        self.sync_playback(playback, engine.speed);
        if let Some(track) = engine.track()
            && self.genre_playback != Some(playback)
        {
            self.genre = Genre::from_metadata(track.genre.as_deref());
            self.genre_playback = Some(playback);
        }
        self.motion.set_space(columns);
        let paused = engine.paused && !engine.is_scrubbing();
        let playing = engine.track().is_some() && !paused && !engine.buffering();
        self.tick(engine.levels(), dt, playing, paused);
    }

    fn sync_playback(&mut self, playback: (u64, u64), speed: f32) {
        if self.playback != playback {
            self.playback = playback;
            self.previous_pulse = 0.0;
            self.since_beat = 0.0;
            self.heard_beat = false;
            self.confidence = 0.0;
            self.motion.begin_transition();
        }
        if self.playback_speed != speed {
            self.beat_period *= self.playback_speed / speed;
            self.playback_speed = speed;
            self.since_beat = 0.0;
            self.heard_beat = false;
            self.confidence = 0.0;
        }
    }

    pub fn wants_frames(&self, engine: &Engine) -> bool {
        (!engine.paused || engine.is_scrubbing()) && self.motion.transitioning()
    }

    fn tick(&mut self, levels: Levels, dt: f32, playing: bool, paused: bool) {
        let dt = if playing && !self.was_playing {
            dt.min(1.0 / 30.0)
        } else {
            dt
        };
        if self.was_playing && !playing && !paused {
            self.motion.begin_transition();
        }
        if self.was_playing != playing {
            self.since_beat = 0.0;
            self.heard_beat = false;
            self.previous_pulse = 0.0;
        }
        self.was_playing = playing;
        let active = !paused && (playing || self.motion.transitioning());
        self.step(if playing { levels } else { Levels::default() }, dt, active);
    }

    fn step(&mut self, levels: Levels, dt: f32, active: bool) {
        if !active {
            self.previous_pulse = 0.0;
            return;
        }
        self.since_beat += dt;
        let hit = levels.pulse > 0.3
            && levels.pulse > self.previous_pulse + 0.08
            && self.since_beat >= 0.15;
        if hit {
            if self.heard_beat && (0.15..=8.0).contains(&self.since_beat) {
                let error = (self.since_beat / self.beat_period - 1.0).abs();
                if error < 0.4 {
                    self.confidence += (1.0 - self.confidence) * 0.25;
                } else {
                    self.confidence *= 0.6;
                }
                self.beat_period += (self.since_beat - self.beat_period) * 0.2;
            }
            self.since_beat = 0.0;
            self.heard_beat = true;
        }
        if self.since_beat > self.beat_period * 2.5 {
            self.confidence *= (-dt / 1.5).exp();
        }
        self.previous_pulse = levels.pulse;
        follow(
            &mut self.energy,
            (levels.rms.sqrt() * 1.8).min(1.0),
            dt,
            0.2,
            0.4,
        );
        follow(
            &mut self.bass,
            (levels.bass.sqrt() * 2.0).min(1.0),
            dt,
            0.1,
            0.25,
        );
        follow(&mut self.accent, levels.pulse, dt, 0.025, 0.13);
        follow(
            &mut self.treble,
            ((levels.peak - levels.rms).max(0.0).sqrt() * 2.0).min(1.0),
            dt,
            0.12,
            0.3,
        );
        self.motion.advance(
            Features {
                energy: self.energy,
                bass: self.bass,
                accent: self.accent,
                treble: self.treble,
                period: self.beat_period,
                confidence: self.confidence,
                hit,
                genre: self.genre,
            },
            dt,
        );
        self.history_clock += dt;
        if self.history_clock >= 1.0 / 30.0 {
            self.history_clock %= 1.0 / 30.0;
            if self.history.len() == 9 {
                self.history.pop_front();
            }
            self.history.push_back(self.motion.spatial.pose);
        }
    }

    fn layers(&self, width: usize, height: usize) -> [Dots; 7] {
        let scale = ((height as f32 - 2.0) / 1.3)
            .min(35.2)
            .min(width as f32 / 1.6);
        let origin = Point::new(width as f32 / 2.0, height as f32 - scale - 1.0);
        let pose = &self.motion.spatial.pose;
        let mut layers: [Dots; 7] = std::array::from_fn(|index| {
            let mut dots = Dots::new(width, height);
            if index < 2 {
                self.history
                    .get(self.history.len().saturating_sub([8, 4][index]))
                    .unwrap_or(pose)
                    .draw(&mut dots, origin, scale, false);
            }
            dots
        });
        let surface = Surface::render(pose, width, height, origin, scale);
        surface.masks(&mut layers);
        layers[3] = layers[2].contour();
        if scale >= 28.0 {
            let face = pose.face();
            for (index, point) in face.into_iter().enumerate() {
                let screen = origin.offset(point.x * scale, point.y * scale);
                let (x, y) = (screen.x.floor() as usize, screen.y.floor() as usize);
                if screen.x < 0.0 || screen.y < 0.0 || x >= width || y >= height {
                    continue;
                }
                let dot = surface.dots[y * width + x];
                if pose.head_yaw.cos() > 0.15
                    && dot.depth.is_finite()
                    && dot.tint < 0.0
                    && dot.depth <= point.z * scale + 0.5
                {
                    if index < 2 {
                        for layer in &mut layers {
                            layer.clear(x, y);
                        }
                    } else {
                        layers[4].dot(x, y);
                    }
                }
            }
        }
        layers
    }

    fn colors(&self) -> [Color; 7] {
        let brightness = 0.7 + self.accent * 0.3;
        [
            Color::Rgb(25, 45, 65),
            Color::Rgb(65, 55, 105),
            Color::Rgb(30, (105.0 * brightness) as u8, (135.0 * brightness) as u8),
            Color::Rgb((115.0 * brightness) as u8, (245.0 * brightness) as u8, 255),
            Color::Rgb(195, 210, 245),
            Color::Rgb(175, 125, 240),
            Color::Rgb(100, 85, 160),
        ]
    }

    pub fn sixel(&self, area: Rect, cell: crate::sixel::CellSize) -> Option<crate::sixel::Image> {
        if area.width < 4 || area.height < 2 {
            return None;
        }
        let stage = Rect::new(
            area.x,
            area.bottom() - area.height.min(13),
            area.width,
            area.height.min(13),
        );
        let dot = f32::from(cell.height) / 4.0;
        let stage_width = f32::from(stage.width) * f32::from(cell.width);
        let stage_height = f32::from(stage.height) * f32::from(cell.height);
        let scale = ((stage_height - dot * 2.0) / 1.3)
            .min(35.2 * dot)
            .min(stage_width / 1.6);
        let origin = Point::new(stage_width / 2.0, stage_height - scale - dot);
        let mut bounds = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for pose in self
            .history
            .iter()
            .chain(std::iter::once(&self.motion.spatial.pose))
        {
            for point in pose.points() {
                bounds[0] = bounds[0].min(point.x);
                bounds[1] = bounds[1].min(point.y);
                bounds[2] = bounds[2].max(point.x);
                bounds[3] = bounds[3].max(point.y);
            }
        }
        let x0 = ((origin.x + (bounds[0] - 0.11) * scale) / f32::from(cell.width))
            .floor()
            .clamp(0.0, f32::from(stage.width)) as u16;
        let y0 = ((origin.y + (bounds[1] - 0.11) * scale) / f32::from(cell.height))
            .floor()
            .clamp(0.0, f32::from(stage.height)) as u16;
        let x1 = ((origin.x + (bounds[2] + 0.11) * scale) / f32::from(cell.width))
            .ceil()
            .clamp(0.0, f32::from(stage.width)) as u16;
        let y1 = ((origin.y + (bounds[3] + 0.11) * scale) / f32::from(cell.height))
            .ceil()
            .clamp(0.0, f32::from(stage.height)) as u16;
        let area = Rect::new(stage.x + x0, stage.y + y0, x1 - x0, y1 - y0);
        if area.is_empty() {
            return None;
        }
        let (width, height) = (
            usize::from(area.width) * usize::from(cell.width),
            usize::from(area.height) * usize::from(cell.height),
        );
        let origin = origin.offset(
            -f32::from(x0) * f32::from(cell.width),
            -f32::from(y0) * f32::from(cell.height),
        );
        let surface = Surface::render(&self.motion.spatial.pose, width, height, origin, scale);
        let mut pixels = vec![0; width * height];
        let trails = [8, 4].map(|age| {
            let mut trail = Dots::new(width, height);
            if let Some(pose) = self.history.get(self.history.len().saturating_sub(age)) {
                pose.draw(&mut trail, origin, scale, false);
            }
            trail
        });
        let edge = (scale * 0.012).ceil().max(1.0) as usize;
        for y in 0..height {
            for x in 0..width {
                let sample = surface.dots[y * width + x];
                if !sample.depth.is_finite() {
                    pixels[y * width + x] = if trails[1].has(x, y) {
                        2
                    } else if trails[0].has(x, y) {
                        1
                    } else {
                        0
                    };
                    continue;
                }
                pixels[y * width + x] = if sample.tint >= 0.0 {
                    if sample.tint > 0.5 { 6 } else { 7 }
                } else if x < edge
                    || y < edge
                    || !surface.dots[y * width + x - edge].depth.is_finite()
                    || !surface.dots[(y - edge) * width + x].depth.is_finite()
                {
                    4
                } else {
                    3
                };
            }
        }
        for (index, point) in self.motion.spatial.pose.face().into_iter().enumerate() {
            let screen = origin.offset(point.x * scale, point.y * scale);
            let (x, y) = (screen.x.floor() as usize, screen.y.floor() as usize);
            if screen.x < 0.0 || screen.y < 0.0 || x >= width || y >= height {
                continue;
            }
            let sample = surface.dots[y * width + x];
            if self.motion.spatial.pose.head_yaw.cos() > 0.15
                && sample.depth.is_finite()
                && sample.tint < 0.0
                && sample.depth <= point.z * scale + 0.5
            {
                pixels[y * width + x] = if index < 2 { 0 } else { 5 };
            }
        }
        let mut palette = [[0; 3]; 8];
        for (entry, color) in palette.iter_mut().skip(1).zip(self.colors()) {
            if let Color::Rgb(r, g, b) = color {
                *entry = [r, g, b];
            }
        }
        Some(crate::sixel::Image {
            area,
            width,
            height,
            pixels,
            palette,
        })
    }

    fn compose(layers: &[Dots; 7], colors: &[Color; 7], index: usize) -> Option<(u8, Color)> {
        let body = layers[2].cells[index];
        if body != 0 {
            // Braille has one foreground color per cell. Sparse details tint the
            // body's color instead of recoloring every dot at full intensity.
            let coverage = |layer: usize| {
                (layers[layer].cells[index] & body).count_ones() as f32 / body.count_ones() as f32
            };
            let blend = |from, to, amount: f32| {
                let (Color::Rgb(r, g, b), Color::Rgb(rr, gg, bb)) = (from, to) else {
                    unreachable!()
                };
                let mix = |a, b| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount) as u8;
                Color::Rgb(mix(r, rr), mix(g, gg), mix(b, bb))
            };
            let rim = blend(colors[2], colors[3], coverage(3) * 0.8);
            let rim = blend(rim, colors[6], coverage(6) * 0.85);
            let color = blend(rim, colors[4], (coverage(4) * 0.6).min(0.2));
            let color = blend(color, colors[5], coverage(5) * 0.85);
            return Some((body, color));
        }
        let mask = layers[0].cells[index] | layers[1].cells[index];
        (mask != 0).then(|| {
            let front = usize::from(layers[1].cells[index] != 0);
            (mask, colors[front])
        })
    }
}

#[derive(Clone, Copy)]
struct SurfaceDot {
    depth: f32,
    tint: f32,
}

struct Surface {
    dots: Vec<SurfaceDot>,
    width: usize,
    height: usize,
    origin: Point,
    scale: f32,
    chest_depth: f32,
}

impl Surface {
    fn render(pose: &Skeleton, width: usize, height: usize, origin: Point, scale: f32) -> Self {
        let chest = pose.shoulders[0].lerp(pose.shoulders[1], 0.5);
        let mut surface = Self {
            dots: vec![
                SurfaceDot {
                    depth: f32::NEG_INFINITY,
                    tint: -1.0
                };
                width * height
            ],
            width,
            height,
            origin,
            scale,
            chest_depth: chest.z,
        };
        for part in pose.body_parts() {
            surface.capsule(part.start, part.end, part.radii, false);
        }
        surface.head(pose.head, pose.head_width(), pose.head_roll);
        let nose = pose.nose();
        surface.capsule(nose.start, nose.end, nose.radii, false);
        for index in 0..2 {
            for part in pose.arm_parts(index, scale) {
                surface.capsule(part.start, part.end, part.radii, true);
            }
        }
        surface
    }

    fn screen(&self, point: Vec3) -> Vec3 {
        Vec3::new(
            self.origin.x + point.x * self.scale,
            self.origin.y + point.y * self.scale,
            point.z * self.scale,
        )
    }

    fn capsule(&mut self, start: Vec3, end: Vec3, radii: [f32; 2], arm: bool) {
        let (a, b) = (self.screen(start), self.screen(end));
        let radii = radii.map(|r| r * self.scale);
        let radius = radii[0].max(radii[1]);
        let x0 = (a.x.min(b.x) - radius).floor().max(0.0) as usize;
        let x1 = (a.x.max(b.x) + radius)
            .ceil()
            .max(0.0)
            .min(self.width as f32) as usize;
        let y0 = (a.y.min(b.y) - radius).floor().max(0.0) as usize;
        let y1 = (a.y.max(b.y) + radius)
            .ceil()
            .max(0.0)
            .min(self.height as f32) as usize;
        let length = (b.x - a.x).powi(2) + (b.y - a.y).powi(2);
        for y in y0..y1 {
            for x in x0..x1 {
                let t = if length > 0.0001 {
                    (((x as f32 + 0.5 - a.x) * (b.x - a.x) + (y as f32 + 0.5 - a.y) * (b.y - a.y))
                        / length)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let nearest = a.lerp(b, t);
                let perpendicular =
                    (x as f32 + 0.5 - nearest.x).powi(2) + (y as f32 + 0.5 - nearest.y).powi(2);
                let dz = b.z - a.z;
                let front = if length > 0.0001 {
                    (t + dz * (radius * radius - perpendicular).max(0.0).sqrt()
                        / (length.sqrt() * (length + dz * dz).sqrt()))
                    .clamp(0.0, 1.0)
                } else {
                    f32::from(b.z > a.z)
                };
                // The closest projected point is not necessarily the closest 3D surface.
                // Include the end caps, particularly for a limb aimed at the viewer.
                for t in [t, front, 0.0, 1.0] {
                    let center = a.lerp(b, t);
                    let distance =
                        (x as f32 + 0.5 - center.x).powi(2) + (y as f32 + 0.5 - center.y).powi(2);
                    let radius = radii[0] + (radii[1] - radii[0]) * t;
                    if distance <= radius * radius {
                        let depth = center.z + (radius * radius - distance).sqrt();
                        let tint = if arm {
                            crate::choreography::ease(
                                ((center.z / self.scale - self.chest_depth + 0.03) / 0.06)
                                    .clamp(0.0, 1.0),
                            )
                        } else {
                            -1.0
                        };
                        self.write(x, y, depth, tint);
                    }
                }
            }
        }
    }

    fn head(&mut self, center: Vec3, width: f32, roll: f32) {
        let point = self.screen(center);
        let rx = width * self.scale;
        let ry = 0.061 * self.scale;
        let extent = rx.max(ry);
        for y in (point.y - extent).floor().max(0.0) as usize
            ..(point.y + extent).ceil().max(0.0).min(self.height as f32) as usize
        {
            for x in (point.x - extent).floor().max(0.0) as usize
                ..(point.x + extent).ceil().max(0.0).min(self.width as f32) as usize
            {
                let (dx, dy) = (x as f32 + 0.5 - point.x, y as f32 + 0.5 - point.y);
                let distance = ((dx * roll.cos() + dy * roll.sin()) / rx).powi(2)
                    + ((-dx * roll.sin() + dy * roll.cos()) / ry).powi(2);
                if distance <= 1.0 {
                    self.write(
                        x,
                        y,
                        point.z + 0.047 * self.scale * (1.0 - distance).sqrt(),
                        -1.0,
                    );
                }
            }
        }
    }

    fn write(&mut self, x: usize, y: usize, depth: f32, tint: f32) {
        let dot = &mut self.dots[y * self.width + x];
        if depth > dot.depth {
            *dot = SurfaceDot { depth, tint };
        }
    }

    fn masks(&self, layers: &mut [Dots; 7]) {
        const ORDER: [[f32; 2]; 4] = [[0.5, 4.5], [6.5, 2.5], [3.5, 7.5], [5.5, 1.5]];
        let mut opaque = Dots::new(self.width, self.height);
        for y in 0..self.height {
            for x in 0..self.width {
                let dot = self.dots[y * self.width + x];
                if !dot.depth.is_finite() {
                    continue;
                }
                layers[2].dot(x, y);
                if dot.tint < 0.0 {
                    opaque.dot(x, y);
                } else if dot.tint > ORDER[y % 4][x % 2] / 8.0 {
                    layers[5].dot(x, y);
                } else {
                    layers[6].dot(x, y);
                }
            }
        }
        for (rear, body) in layers[6].cells.iter_mut().zip(&opaque.cells) {
            if *body != 0 {
                *rear = 0;
            }
        }
    }
}

impl Skeleton {
    fn draw(&self, dots: &mut Dots, origin: Point, scale: f32, filled: bool) {
        self.draw_body(dots, origin, scale, filled);
        dots.capsules(
            (0..2).flat_map(|index| self.arm_parts(index, scale)),
            origin,
            scale,
            filled,
        );
    }

    fn draw_body(&self, dots: &mut Dots, origin: Point, scale: f32, filled: bool) {
        dots.capsules(
            self.body_parts().chain(std::iter::once(self.nose())),
            origin,
            scale,
            filled,
        );
        dots.ellipse(
            origin.offset(self.head.x * scale, self.head.y * scale),
            self.head_width() * scale,
            0.061 * scale,
            filled,
        );
    }
}

struct Dots {
    width: usize,
    height: usize,
    cells: Vec<u8>,
}

impl Dots {
    fn capsules(
        &mut self,
        parts: impl IntoIterator<Item = Capsule>,
        origin: Point,
        scale: f32,
        filled: bool,
    ) {
        let screen = |point: Vec3| {
            let point = point.project();
            origin.offset(point.x * scale, point.y * scale)
        };
        for part in parts {
            self.capsule(
                screen(part.start),
                screen(part.end),
                part.radii[0] * scale,
                part.radii[1] * scale,
                filled,
            );
        }
    }

    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![0; width.div_ceil(2) * height.div_ceil(4)],
        }
    }

    fn dot(&mut self, x: usize, y: usize) {
        const BITS: [[u8; 2]; 4] = [[1, 8], [2, 16], [4, 32], [64, 128]];
        self.cells[y / 4 * self.width.div_ceil(2) + x / 2] |= BITS[y % 4][x % 2];
    }

    fn clear(&mut self, x: usize, y: usize) {
        const BITS: [[u8; 2]; 4] = [[1, 8], [2, 16], [4, 32], [64, 128]];
        self.cells[y / 4 * self.width.div_ceil(2) + x / 2] &= !BITS[y % 4][x % 2];
    }

    fn has(&self, x: usize, y: usize) -> bool {
        const BITS: [[u8; 2]; 4] = [[1, 8], [2, 16], [4, 32], [64, 128]];
        self.cells[y / 4 * self.width.div_ceil(2) + x / 2] & BITS[y % 4][x % 2] != 0
    }

    fn contour(&self) -> Self {
        let mut outline = Self::new(self.width, self.height);
        for (index, &mask) in self.cells.iter().enumerate() {
            if mask == 0 {
                continue;
            }
            let (x0, y0) = (
                index % self.width.div_ceil(2) * 2,
                index / self.width.div_ceil(2) * 4,
            );
            for dy in 0..4 {
                for dx in 0..2 {
                    let (x, y) = (x0 + dx, y0 + dy);
                    if x >= self.width || y >= self.height || !self.has(x, y) {
                        continue;
                    }
                    // Light the upper/left rim inside the existing silhouette.
                    // Outward dilation adds a whole dot to very narrow limbs.
                    if x == 0 || y == 0 || !self.has(x - 1, y) || !self.has(x, y - 1) {
                        outline.dot(x, y);
                    }
                }
            }
        }
        outline
    }

    fn fill(&mut self, low: Point, high: Point, contains: impl Fn(Point) -> bool) {
        let x0 = low.x.floor().max(0.0) as usize;
        let y0 = low.y.floor().max(0.0) as usize;
        let x1 = (high.x.ceil().max(0.0) as usize).min(self.width);
        let y1 = (high.y.ceil().max(0.0) as usize).min(self.height);
        for y in y0..y1 {
            for x in x0..x1 {
                if contains(Point::new(x as f32 + 0.5, y as f32 + 0.5)) {
                    self.dot(x, y);
                }
            }
        }
    }

    fn capsule(&mut self, a: Point, b: Point, r1: f32, r2: f32, filled: bool) {
        let radius = r1.max(r2).max(0.55);
        let length_squared = (b.x - a.x).powi(2) + (b.y - a.y).powi(2);
        self.fill(
            Point::new(a.x.min(b.x) - radius, a.y.min(b.y) - radius),
            Point::new(a.x.max(b.x) + radius, a.y.max(b.y) + radius),
            |p| {
                let t = if length_squared > 0.0001 {
                    (((p.x - a.x) * (b.x - a.x) + (p.y - a.y) * (b.y - a.y)) / length_squared)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let center = a.lerp(b, t);
                let radius = (r1 + (r2 - r1) * t).max(0.55);
                let distance = (p.x - center.x).powi(2) + (p.y - center.y).powi(2);
                distance <= radius * radius
                    && (filled || distance >= (radius - 0.85).max(0.0).powi(2))
            },
        );
    }

    fn ellipse(&mut self, center: Point, rx: f32, ry: f32, filled: bool) {
        let (rx, ry) = (rx.max(0.6), ry.max(0.6));
        self.fill(center.offset(-rx, -ry), center.offset(rx, ry), |p| {
            let distance = ((p.x - center.x) / rx).powi(2) + ((p.y - center.y) / ry).powi(2);
            distance <= 1.0 && (filled || distance >= (1.0 - 0.85 / rx.min(ry)).max(0.0).powi(2))
        });
    }
}

impl Widget for &Dancer {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width < 4 || area.height < 2 {
            return;
        }
        let stage = Rect::new(
            area.x,
            area.bottom() - area.height.min(13),
            area.width,
            area.height.min(13),
        );
        let layers = self.layers(usize::from(stage.width) * 2, usize::from(stage.height) * 4);
        let colors = self.colors();
        for y in 0..stage.height {
            for x in 0..stage.width {
                let index = usize::from(y) * usize::from(stage.width) + usize::from(x);
                if let Some((mask, color)) = Dancer::compose(&layers, &colors, index) {
                    buffer[(stage.x + x, stage.y + y)]
                        .set_char(char::from_u32(0x2800 + u32::from(mask)).unwrap())
                        .set_fg(color);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_pixels_crop_to_the_dancer_and_preserve_the_terminal_stage() {
        let mut dancer = Dancer::new(Style::Flow);
        for _ in 0..180 {
            dancer.step(
                Levels {
                    rms: 0.2,
                    bass: 0.15,
                    ..Levels::default()
                },
                1.0 / 30.0,
                true,
            );
        }
        let area = Rect::new(3, 16, 74, 18);
        let cell = crate::sixel::CellSize {
            width: 8,
            height: 16,
        };
        let image = dancer.sixel(area, cell).unwrap();
        assert_eq!(image.area.intersection(area), image.area);
        assert!(image.area.y >= area.bottom() - 13);
        assert!(image.area.width < area.width);
        assert_eq!(image.width, usize::from(image.area.width) * 8);
        assert_eq!(image.height, usize::from(image.area.height) * 16);
        assert!(image.height > 52);
        assert!(
            image.pixels.contains(&0) && image.pixels.contains(&3) && image.pixels.contains(&6)
        );
        assert!(image.pixels.iter().all(|color| *color < 8));
        assert!(dancer.sixel(Rect::new(0, 0, 1, 1), cell).is_none());
        if let Some(path) = std::env::var_os("PLAVNO_SIXEL_PREVIEW") {
            let path = std::path::Path::new(&path);
            let png = image::RgbaImage::from_fn(image.width as u32, image.height as u32, |x, y| {
                let color = image.pixels[y as usize * image.width + x as usize];
                let [r, g, b] = image.palette[usize::from(color)];
                image::Rgba([r, g, b, if color == 0 { 0 } else { 255 }])
            });
            png.save(path).unwrap();
            std::fs::write(path.with_extension("sixel"), image.encode()).unwrap();
        }
    }

    fn empty_surface() -> Surface {
        Surface {
            dots: vec![
                SurfaceDot {
                    depth: f32::NEG_INFINITY,
                    tint: -1.0
                };
                64
            ],
            width: 8,
            height: 8,
            origin: Point::new(4.5, 4.5),
            scale: 30.0,
            chest_depth: 0.0,
        }
    }

    #[test]
    fn a_limb_aimed_at_the_camera_uses_the_near_end_cap() {
        let mut surface = empty_surface();
        surface.capsule(
            Vec3::new(0.0, 0.0, -0.2),
            Vec3::new(0.0, 0.0, 0.2),
            [0.02; 2],
            true,
        );
        let dot = surface.dots[4 * 8 + 4];
        assert!((dot.depth - 6.6).abs() < 0.0001);
        assert!(dot.tint > 0.99);
    }

    #[test]
    fn depth_buffer_resolves_body_and_arm_surfaces_independent_of_draw_order() {
        for depth in [-0.15, 0.15] {
            for reverse in [false, true] {
                let mut surface = empty_surface();
                for arm in if reverse {
                    [true, false]
                } else {
                    [false, true]
                } {
                    let z = if arm { depth } else { 0.0 };
                    let radius = if arm { 0.02 } else { 0.06 };
                    surface.capsule(
                        Vec3::new(-0.1, 0.0, z),
                        Vec3::new(0.1, 0.0, z),
                        [radius; 2],
                        arm,
                    );
                }
                assert_eq!(surface.dots[4 * 8 + 4].tint >= 0.0, depth > 0.0);
            }
        }
    }

    #[test]
    fn the_body_occludes_rear_arms_but_front_arms_remain_visible() {
        let mut dancer = Dancer::new(Style::Flow);
        let chest = dancer.motion.spatial.pose.shoulders[0]
            .lerp(dancer.motion.spatial.pose.shoulders[1], 0.5);
        for (index, side) in [-1.0, 1.0].into_iter().enumerate() {
            dancer.motion.spatial.pose.elbows[index] =
                chest.add(Vec3::new(-side * 0.03, 0.09, 0.0));
            dancer.motion.spatial.pose.wrists[index] =
                chest.add(Vec3::new(-side * 0.22, 0.15, 0.0));
        }
        let mut opaque = Dots::new(120, 52);
        dancer.motion.spatial.pose.draw_body(
            &mut opaque,
            Point::new(60.0, 52.0 - 35.2 - 1.0),
            35.2,
            true,
        );
        for i in 0..2 {
            dancer.motion.spatial.pose.elbows[i].z = -0.2;
            dancer.motion.spatial.pose.wrists[i].z = -0.2;
        }
        let rear = dancer.layers(120, 52);
        for (&arm, &body) in rear[5].cells.iter().zip(&opaque.cells) {
            assert_eq!(arm & body, 0, "A rear arm is tinted over the body");
        }
        assert!(
            rear[6].cells.iter().any(|cell| *cell != 0),
            "Visible rear arms disappear outside the body"
        );
        for (&arm, &body) in rear[6].cells.iter().zip(&opaque.cells) {
            assert!(arm == 0 || body == 0, "A rear dot recolors a body cell");
        }
        for i in 0..2 {
            dancer.motion.spatial.pose.elbows[i].z = 0.2;
            dancer.motion.spatial.pose.wrists[i].z = 0.2;
        }
        let front = dancer.layers(120, 52);
        let overlap = front[5]
            .cells
            .iter()
            .zip(&opaque.cells)
            .map(|(&arm, &body)| (arm & body).count_ones())
            .sum::<u32>();
        assert!(overlap > 5, "Front arms cannot pass over the body");
        assert_eq!(
            front[2].cells, rear[2].cells,
            "Depth changes the projected silhouette"
        );
    }

    #[test]
    fn arm_depth_fades_tint_without_changing_geometry() {
        let mut dancer = Dancer::new(Style::Flow);
        let mut counts = Vec::new();
        for depth in [-0.2, -0.02, 0.0, 0.02, 0.2] {
            for i in 0..2 {
                dancer.motion.spatial.pose.elbows[i].z = depth;
                dancer.motion.spatial.pose.wrists[i].z = depth;
            }
            let layers = dancer.layers(120, 52);
            counts.push(
                layers[5]
                    .cells
                    .iter()
                    .map(|cell| cell.count_ones())
                    .sum::<u32>(),
            );
        }
        assert!(counts.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(
            counts[2] > counts[0] && counts[2] < counts[4],
            "Depth jumps between front and rear instead of fading"
        );
    }

    #[test]
    fn arms_use_a_distinct_tint_and_palms_extend_beyond_the_wrists() {
        let mut dancer = Dancer::new(Style::Flow);
        for (index, side) in [-1.0, 1.0].into_iter().enumerate() {
            dancer.motion.spatial.pose.elbows[index] = Vec3::new(side * 0.21, 0.36, 0.1);
            dancer.motion.spatial.pose.wrists[index] = Vec3::new(side * 0.36, 0.36, 0.1);
        }
        let layers = dancer.layers(120, 52);
        let colors = dancer.colors();
        let mut tinted = 0;
        for (index, &arms) in layers[5].cells.iter().enumerate() {
            if arms == 0 {
                continue;
            }
            assert_eq!(
                arms & !layers[2].cells[index],
                0,
                "Arm tint changes the silhouette"
            );
            let Some((_, Color::Rgb(r, g, b))) = Dancer::compose(&layers, &colors, index) else {
                panic!("Missing arm color")
            };
            tinted += usize::from(r > g.saturating_add(15) && b > r);
        }
        assert!(tinted >= 4, "Arms do not stand out from the cyan body");
        // Outstretched palms occupy pixels beyond the old forearm end caps.
        let origin = Point::new(60.0, 52.0 - 35.2 - 1.0);
        for side in [-1.0, 1.0] {
            let fingertip = origin.offset(side * 0.4 * 35.2, 0.36 * 35.2);
            assert!(layers[2].has(fingertip.x.floor() as usize, fingertip.y.floor() as usize));
        }
        let small = dancer.layers(24, 8);
        assert!(small[5].cells.iter().any(|cell| *cell != 0));
    }

    #[test]
    fn foreground_cells_preserve_the_silhouette_and_keep_face_highlights_cool() {
        let dancer = Dancer::new(Style::Flow);
        let area = Rect::new(0, 0, 60, 13);
        let layers = dancer.layers(120, 52);
        let mut buffer = Buffer::empty(area);
        (&dancer).render(area, &mut buffer);
        let mut foreground_cells = 0;
        for (index, &body) in layers[2].cells.iter().enumerate() {
            if body == 0 {
                continue;
            }
            foreground_cells += 1;
            let cell = &buffer.content[index];
            let mask = cell.symbol().chars().next().unwrap() as u32 - 0x2800;
            assert_eq!(
                mask,
                u32::from(body),
                "Contour or trails thicken the silhouette"
            );
            let Color::Rgb(r, g, b) = cell.fg else {
                panic!("Missing silhouette color")
            };
            assert!(
                b >= r && b >= g,
                "A face detail recolors the whole cell yellow"
            );
        }
        assert!(foreground_cells > 10);
    }

    #[test]
    fn rim_and_details_stay_inside_the_body_and_preserve_small_terminal_cells() {
        let dancer = Dancer::new(Style::Flow);
        let layers = dancer.layers(120, 52);
        assert!(layers[3].cells.iter().any(|cell| *cell != 0));
        assert!(layers[4].cells.iter().any(|cell| *cell != 0));
        for (&body, &edge) in layers[2].cells.iter().zip(&layers[3].cells) {
            assert_eq!(edge & !body, 0, "Rim adds dots outside the body");
        }
        let colors = dancer.colors();
        let Color::Rgb(_, body_green, _) = colors[2] else {
            panic!()
        };
        let Color::Rgb(_, edge_green, _) = colors[3] else {
            panic!()
        };
        assert!(edge_green > body_green * 2);
        let small = dancer.layers(24, 8);
        assert!(
            small[4].cells.iter().all(|cell| *cell == 0),
            "Face details crowd a tiny figure"
        );
        let mut filled_head = Dots::new(120, 52);
        let scale = 35.2;
        let origin = Point::new(60.0, 52.0 - scale - 1.0);
        dancer
            .motion
            .spatial
            .pose
            .draw(&mut filled_head, origin, scale, true);
        let removed = filled_head
            .cells
            .iter()
            .zip(&layers[2].cells)
            .map(|(&before, &after)| (before & !after).count_ones())
            .sum::<u32>();
        assert!(removed >= 2, "Eyes are not readable as negative space");
    }

    #[test]
    fn track_change_keeps_pose_and_trail_instead_of_recreating_the_dancer() {
        let mut dancer = Dancer::new(Style::Groove);
        let levels = Levels {
            rms: 0.2,
            bass: 0.15,
            ..Levels::default()
        };
        for _ in 0..240 {
            dancer.step(levels, 1.0 / 30.0, true);
        }
        let pose = dancer.motion.pose;
        let history = dancer.history.clone();
        let spatial = dancer.motion.spatial.pose;
        dancer.sync_playback((4, 2), 1.0);
        assert_eq!(dancer.motion.pose, pose, "Track change reset the body rig");
        assert_eq!(dancer.motion.spatial.pose, spatial);
        assert_eq!(
            dancer.history, history,
            "Track change cleared the motion trail"
        );
    }

    #[test]
    fn loading_gap_settles_without_teleporting_and_pause_freezes_the_transition() {
        let mut dancer = Dancer::new(Style::Groove);
        let levels = Levels {
            rms: 0.2,
            bass: 0.15,
            ..Levels::default()
        };
        for _ in 0..120 {
            dancer.tick(levels, 1.0 / 30.0, true, false);
        }
        dancer.sync_playback((2, 1), 1.0);
        let pose = dancer.motion.pose;
        let history = dancer.history.clone();
        let spatial = dancer.motion.spatial.pose;
        for _ in 0..60 {
            dancer.tick(Levels::default(), 1.0 / 30.0, false, true);
        }
        assert_eq!(dancer.motion.pose, pose);
        assert_eq!(dancer.motion.spatial.pose, spatial);
        assert_eq!(dancer.history, history);
        assert!(dancer.motion.transitioning());
        let mut previous = pose;
        for _ in 0..180 {
            dancer.tick(Levels::default(), 1.0 / 30.0, false, false);
            assert!(previous.head.distance(dancer.motion.pose.head) < 0.1);
            previous = dancer.motion.pose;
        }
        assert!(!dancer.motion.transitioning());
        assert!(dancer.motion.pose.ankles.iter().all(|foot| foot.y == 0.97));
        let settled = dancer.motion.pose;
        let spatial_settled = dancer.motion.spatial.pose;
        dancer.tick(Levels::default(), 1.0 / 30.0, false, false);
        assert_eq!(dancer.motion.pose, settled);
        assert_eq!(dancer.motion.spatial.pose, spatial_settled);
        dancer.sync_playback((2, 2), 1.0);
        assert_eq!(dancer.motion.pose, settled);
        assert_eq!(dancer.motion.spatial.pose, spatial_settled);
        assert_eq!(dancer.history.len(), 9);
        dancer.tick(levels, 1.0 / 30.0, true, false);
        assert!(settled.head.distance(dancer.motion.pose.head) < 0.1);
    }

    #[test]
    fn regular_beats_build_confidence_while_ambient_and_pause_keep_free_movement() {
        let levels = Levels {
            rms: 0.2,
            bass: 0.15,
            ..Levels::default()
        };
        let mut rhythmic = Dancer::new(Style::Auto);
        let mut ambient = Dancer::new(Style::Auto);
        for step in 0..900 {
            rhythmic.step(
                Levels {
                    pulse: if step % 10 == 0 { 1.0 } else { 0.0 },
                    ..levels
                },
                1.0 / 30.0,
                true,
            );
            ambient.step(levels, 1.0 / 30.0, true);
        }
        assert!(rhythmic.confidence > 0.9);
        assert!(rhythmic.beat_period < 0.35);
        assert_eq!(ambient.confidence, 0.0);
        let paused = rhythmic.motion.pose;
        for _ in 0..60 {
            rhythmic.step(levels, 1.0 / 30.0, false);
        }
        assert_eq!(rhythmic.motion.pose, paused);
        for _ in 0..300 {
            rhythmic.step(levels, 1.0 / 30.0, true);
        }
        assert!(
            rhythmic.confidence < 0.05,
            "Missing beats do not return to flow"
        );
    }

    #[test]
    fn rendering_clips_and_keeps_the_wave_and_cover_under_the_silhouette() {
        let mut dancer = Dancer::new(Style::Groove);
        for _ in 0..120 {
            dancer.step(
                Levels {
                    rms: 0.2,
                    bass: 0.15,
                    ..Levels::default()
                },
                1.0 / 30.0,
                true,
            );
        }
        if std::env::var_os("PLAVNO_DANCE_BENCH").is_some() {
            for width in [120, 320] {
                let started = Instant::now();
                for _ in 0..1200 {
                    std::hint::black_box(dancer.layers(width, 52));
                }
                eprintln!(
                    "3D raster: {} columns, {:.3} ms/frame; surface buffer {} bytes",
                    width / 2,
                    started.elapsed().as_secs_f64() * 1000.0 / 1200.0,
                    width * 52 * std::mem::size_of::<SurfaceDot>()
                );
            }
        }
        let bounds = Rect::new(0, 0, 80, 40);
        for area in [
            Rect::new(3, 21, 74, 16),
            Rect::new(4, 20, 12, 2),
            Rect::new(2, 3, 1, 1),
        ] {
            let mut buffer = Buffer::empty(bounds);
            for cell in &mut buffer.content {
                cell.set_char('⠁').set_bg(Color::Rgb(20, 30, 40));
            }
            let before = buffer.clone();
            (&dancer).render(area, &mut buffer);
            let mut changed = 0;
            for y in 0..bounds.height {
                for x in 0..bounds.width {
                    assert_eq!(buffer[(x, y)].bg, before[(x, y)].bg);
                    if !area.contains((x, y).into()) {
                        assert_eq!(buffer[(x, y)], before[(x, y)]);
                    }
                    changed += usize::from(buffer[(x, y)] != before[(x, y)]);
                }
            }
            if area.width >= 4 {
                assert!(
                    changed > 0 && changed < usize::from(area.width) * usize::from(area.height) / 2
                );
            }
        }
        if let Some(path) = std::env::var_os("PLAVNO_DANCE_PREVIEW") {
            let mut image = image::RgbImage::new(960, 960);
            for (row, style) in [
                Style::Flow,
                Style::Groove,
                Style::Pulse,
                Style::House,
                Style::Hiphop,
                Style::Rave,
            ]
            .into_iter()
            .enumerate()
            {
                let mut dancer = Dancer::new(style);
                for frame in 0..4 {
                    for step in 0..60 {
                        dancer.step(
                            Levels {
                                rms: 0.2,
                                bass: 0.12,
                                pulse: if step % 15 == 0 { 1.0 } else { 0.0 },
                                ..Levels::default()
                            },
                            1.0 / 30.0,
                            true,
                        );
                    }
                    let layers = dancer.layers(120, 80);
                    let colors = dancer.colors();
                    for y in 0..80 {
                        for x in 0..120 {
                            let cell = y / 4 * 60 + x / 2;
                            let bit = [[1, 8], [2, 16], [4, 32], [64, 128]][y % 4][x % 2];
                            // Braille foreground is shared by every dot in a terminal cell.
                            let (mask, color) = Dancer::compose(&layers, &colors, cell)
                                .unwrap_or((0, Color::Rgb(8, 12, 20)));
                            let Color::Rgb(r, g, b) = color else {
                                unreachable!()
                            };
                            let color = if mask & bit != 0 {
                                [r, g, b]
                            } else {
                                [8, 12, 20]
                            };
                            for dy in 0..2 {
                                for dx in 0..2 {
                                    image.put_pixel(
                                        (frame * 240 + x * 2 + dx) as u32,
                                        (row * 160 + y * 2 + dy) as u32,
                                        image::Rgb(color),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            image.save(std::path::Path::new(&path)).unwrap();
        }
    }
}
