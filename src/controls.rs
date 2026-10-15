use crate::{backend::PlaybackMode, engine::Engine, visualization::Mode};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const PRIMARY_BUTTONS: usize = 7;

struct ButtonSpec {
    label: &'static str,
    shortcut: &'static str,
    key: KeyCode,
    enabled: bool,
}

impl ButtonSpec {
    fn width(&self) -> u16 {
        (self.label.width() + self.shortcut.width() + 3) as u16
    }
}

fn button_specs(engine: &Engine) -> Vec<ButtonSpec> {
    let playing = engine.playback_status() == "Playing";
    [
        ("Prev", "p", KeyCode::Char('p'), engine.can_previous()),
        (
            if playing { "Pause" } else { "Play" },
            "Space",
            KeyCode::Char(' '),
            engine.can_play(),
        ),
        ("Next", "n", KeyCode::Char('n'), engine.can_next()),
        ("Stop", "x", KeyCode::Char('x'), engine.can_pause()),
        ("Seek · Shift ±30s", "←/→", KeyCode::Null, false),
        (
            if engine.is_liked() {
                "♥ Unlike"
            } else {
                "♡ Like"
            },
            "l",
            KeyCode::Char('l'),
            engine.can_like(),
        ),
        ("Dislike", "d", KeyCode::Char('d'), engine.can_dislike()),
        ("History", "h", KeyCode::Char('h'), true),
        ("Sources", "s", KeyCode::Char('s'), true),
        (
            "Mix",
            "m",
            KeyCode::Char('m'),
            engine
                .services
                .iter()
                .any(|s| s.key == "mixed" && s.available),
        ),
        (
            if engine.source == "vk" {
                "My Music"
            } else {
                "Favorites"
            },
            "f",
            KeyCode::Char('f'),
            engine.can_shuffle_favorites(),
        ),
        ("Wave", "w", KeyCode::Char('w'), true),
        ("Visual", "v", KeyCode::Char('v'), true),
        ("Dance", "b", KeyCode::Char('b'), true),
        ("Flow", "g", KeyCode::Char('g'), engine.can_tune_wave()),
        ("Quit", "q", KeyCode::Char('q'), true),
        ("Undo", "u", KeyCode::Char('u'), engine.can_undo_dislike()),
    ]
    .into_iter()
    .filter(|(_, _, key, enabled)| *key != KeyCode::Char('u') || *enabled)
    .map(|(label, shortcut, key, enabled)| ButtonSpec {
        label,
        shortcut,
        key,
        enabled,
    })
    .collect()
}

fn button_layout(width: u16, buttons: &[ButtonSpec]) -> impl Iterator<Item = Rect> + '_ {
    let (mut x, mut y) = (0u16, 0u16);
    buttons.iter().enumerate().map(move |(index, button)| {
        let size = if button.key == KeyCode::Null && width.saturating_sub(x) >= 10 {
            button.width().min(width - x)
        } else {
            button.width().min(width)
        };
        let required = if button.key == KeyCode::Char('x') && width >= size + 12 {
            size + 12
        } else {
            size
        };
        if index == PRIMARY_BUTTONS || x > 0 && x.saturating_add(required) > width {
            x = 0;
            y += 1;
        }
        if index == PRIMARY_BUTTONS - 1 {
            x = width - size;
        }
        if index == PRIMARY_BUTTONS - 2 {
            let pair_width = size
                .saturating_add(2)
                .saturating_add(buttons[index + 1].width().min(width));
            if x.saturating_add(pair_width) <= width {
                x = width - pair_width;
            }
        }
        let area = Rect::new(x, y, size, 1);
        let gap = if index < PRIMARY_BUTTONS { 2 } else { 1 };
        x = x.saturating_add(size).saturating_add(gap);
        area
    })
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Key(KeyCode),
    Seek(f64),
    Scrub { position: f64, speed: f32 },
    VolumeDelta(f32),
    Redraw,
}

struct Button {
    area: Rect,
    key: KeyCode,
    enabled: bool,
}

enum Drag {
    Seek {
        position: f64,
        moved: Instant,
        scrubbing: bool,
    },
    Button(Target),
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Button(KeyCode),
    Transport(KeyCode),
}

impl Target {
    fn key(self) -> KeyCode {
        match self {
            Self::Button(key) | Self::Transport(key) => key,
        }
    }
}

#[derive(Default)]
pub struct Controls {
    pub seek: Rect,
    buttons: Vec<Button>,
    transport: Vec<Button>,
    text: Rect,
    speed: Rect,
    drag: Option<Drag>,
    hover: Option<Target>,
    track: Option<(String, String)>,
}

fn fraction(area: Rect, column: u16) -> Option<f64> {
    (area.width >= 2).then(|| {
        f64::from(column.saturating_sub(area.x).min(area.width - 1)) / f64::from(area.width - 1)
    })
}

fn scrub_speed(delta: f64, duration: f64, elapsed: Duration) -> f32 {
    let bars_per_second = delta.abs() / duration / elapsed.as_secs_f64().max(0.001);
    let speed = 2.0 + (bars_per_second - 1.0).clamp(0.0, 2.0);
    (speed * delta.signum()) as f32
}

impl Controls {
    pub fn height(width: u16, engine: &Engine) -> u16 {
        button_layout(width, &button_specs(engine))
            .last()
            .map_or(1, |area| area.y + 1)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn cancel_drag(&mut self) {
        self.drag = None;
        self.hover = None;
    }

    pub fn sync_track(&mut self, engine: &Engine) {
        let track = engine.media_track();
        if track.map(|t| (t.source.as_str(), t.id.as_str()))
            != self
                .track
                .as_ref()
                .map(|(source, id)| (source.as_str(), id.as_str()))
        {
            self.cancel_drag();
            self.track = track.map(|t| (t.source.clone(), t.id.clone()));
        }
    }

    pub fn position(&self, actual: f64) -> f64 {
        match self.drag {
            Some(Drag::Seek { position, .. }) => position,
            _ => actual,
        }
    }

    pub fn is_seeking(&self) -> bool {
        matches!(self.drag, Some(Drag::Seek { .. }))
    }

    fn target_at(&self, column: u16, row: u16) -> Option<Target> {
        let point = (column, row).into();
        if self.seek.contains(point) || self.text.contains(point) {
            return None;
        }
        self.buttons
            .iter()
            .find(|button| button.area.contains(point))
            .map(|button| button.enabled.then_some(Target::Button(button.key)))
            .unwrap_or_else(|| {
                self.transport
                    .iter()
                    .find(|button| button.enabled && button.area.contains(point))
                    .map(|button| Target::Transport(button.key))
            })
    }

    fn button_style(&self, key: KeyCode, enabled: bool) -> Style {
        let base = Style::default();
        let target = Target::Button(key);
        if !enabled {
            base.fg(Color::DarkGray)
        } else if matches!(self.drag, Some(Drag::Button(t)) if t == target)
            && self.hover == Some(target)
        {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(30, 120, 135))
        } else if self.hover == Some(target) || key == KeyCode::Char(' ') {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            base.fg(Color::Gray)
        }
    }

    pub fn handle(&mut self, mouse: MouseEvent, duration: Option<f64>) -> Option<Action> {
        let point = (mouse.column, mouse.row).into();
        let duration = duration.filter(|d| d.is_finite() && *d > 0.0);
        match mouse.kind {
            MouseEventKind::Moved => {
                let hover = self.target_at(mouse.column, mouse.row);
                if self.hover != hover {
                    self.hover = hover;
                    return Some(Action::Redraw);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None;
                let hover = self.target_at(mouse.column, mouse.row);
                let changed = self.hover != hover;
                self.hover = hover;
                if self.seek.contains(point) {
                    let position = duration? * fraction(self.seek, mouse.column)?;
                    self.drag = Some(Drag::Seek {
                        position,
                        moved: Instant::now(),
                        scrubbing: false,
                    });
                    return Some(Action::Seek(position));
                }
                if let Some(target) = hover {
                    self.drag = Some(Drag::Button(target));
                    return Some(Action::Redraw);
                }
                return changed.then_some(Action::Redraw);
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                self.hover = self.target_at(mouse.column, mouse.row);
                let released = matches!(mouse.kind, MouseEventKind::Up(_));
                let action = match self.drag {
                    Some(Drag::Seek {
                        position: previous,
                        moved,
                        scrubbing,
                    }) => {
                        let Some(duration) = duration else {
                            self.cancel_drag();
                            return Some(Action::Redraw);
                        };
                        let position = duration * fraction(self.seek, mouse.column)?;
                        if released {
                            if scrubbing || (position - previous).abs() >= 0.001 {
                                Some(Action::Seek(position))
                            } else {
                                Some(Action::Redraw)
                            }
                        } else if (position - previous).abs() < 0.001 {
                            Some(Action::Redraw)
                        } else {
                            let now = Instant::now();
                            let speed = scrub_speed(
                                position - previous,
                                duration,
                                now.duration_since(moved),
                            );
                            self.drag = Some(Drag::Seek {
                                position,
                                moved: now,
                                scrubbing: true,
                            });
                            Some(Action::Scrub { position, speed })
                        }
                    }
                    Some(Drag::Button(target)) if released => {
                        (self.hover == Some(target)).then_some(Action::Key(target.key()))
                    }
                    Some(Drag::Button(_)) => Some(Action::Redraw),
                    None => None,
                };
                if released {
                    self.drag = None;
                }
                return action.or(released.then_some(Action::Redraw));
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let direction = if mouse.kind == MouseEventKind::ScrollUp {
                    1.0
                } else {
                    -1.0
                };
                if self.speed.contains(point) {
                    return Some(Action::Key(KeyCode::Char(if direction > 0.0 {
                        ']'
                    } else {
                        '['
                    })));
                }
                if self.seek.contains(point) {
                    let seconds = if mouse
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
                    {
                        30.0
                    } else {
                        5.0
                    };
                    return Some(Action::Key(if direction > 0.0 {
                        if seconds == 30.0 {
                            KeyCode::PageDown
                        } else {
                            KeyCode::Right
                        }
                    } else if seconds == 30.0 {
                        KeyCode::PageUp
                    } else {
                        KeyCode::Left
                    }));
                }
                let step = if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    0.01
                } else {
                    0.05
                };
                return Some(Action::VolumeDelta(direction as f32 * step));
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.cancel_drag();
                return Some(Action::Redraw);
            }
            _ => {}
        }
        None
    }

    pub fn begin_draw(&mut self) {
        self.buttons.clear();
        self.transport.clear();
        self.text = Rect::default();
        self.speed = Rect::default();
    }

    fn hovered_transport(&self) -> Option<&Button> {
        let Some(Target::Transport(key)) = self.hover else {
            return None;
        };
        self.transport
            .iter()
            .find(|zone| zone.key == key && zone.enabled)
    }

    pub fn highlight_transport(&self, buffer: &mut Buffer) {
        if let Some(zone) = self.hovered_transport() {
            highlight_zone(zone.area, buffer);
        }
    }

    pub fn draw_transport(&mut self, frame: &mut Frame, area: Rect, text: Rect, engine: &Engine) {
        self.text = text;
        self.transport = transport_areas(area)
            .into_iter()
            .zip([
                (KeyCode::Char('p'), engine.can_previous()),
                (KeyCode::Char(' '), engine.can_play()),
                (KeyCode::Char('n'), engine.can_next()),
            ])
            .map(|(area, (key, enabled))| Button { area, key, enabled })
            .collect();
        let Some(zone) = self.hovered_transport() else {
            return;
        };
        highlight_zone(zone.area, frame.buffer_mut());
        let label = match zone.key {
            KeyCode::Char('p') => "‹ Prev · p",
            KeyCode::Char(' ') if engine.playback_status() == "Playing" => "Pause · Space",
            KeyCode::Char(' ') => "Play · Space",
            _ => "Next › · n",
        };
        let width = (label.width() as u16 + 2).min(zone.area.width);
        let label_area = Rect::new(
            zone.area.x + (zone.area.width - width) / 2,
            zone.area.y + zone.area.height / 2,
            width,
            zone.area.height.min(1),
        );
        frame.render_widget(
            Paragraph::new(format!(" {label} "))
                .alignment(ratatui::layout::Alignment::Center)
                .style(
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(25, 75, 85))
                        .add_modifier(Modifier::BOLD),
                ),
            label_area,
        );
    }

    pub fn draw_settings(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        volume: &str,
        speed: &str,
        queue: &str,
    ) {
        let style = Style::default().fg(Color::Gray);
        let mut x = area.x;
        for (index, (text, minus, plus)) in [(volume, '-', '+'), (speed, '[', ']')]
            .into_iter()
            .enumerate()
        {
            if index > 0 {
                let width = area.right().saturating_sub(x).min(3);
                frame.render_widget(
                    Line::from(" · ").style(style),
                    Rect::new(x, area.y, width, 1),
                );
                x += width;
            }
            let remaining = area.right().saturating_sub(x);
            if remaining < 6 {
                frame.render_widget(
                    Line::from(text).style(style),
                    Rect::new(x, area.y, remaining, 1),
                );
                return;
            }
            let width = text.width().min(usize::from(remaining - 6)) as u16;
            if index > 0 {
                self.speed = Rect::new(x, area.y, width + 6, 1);
            }
            let label_area = Rect::new(x + 3, area.y, width, 1);
            frame.render_widget(Line::from(text).style(style), label_area);
            self.buttons.push(Button {
                area: label_area,
                key: KeyCode::Null,
                enabled: false,
            });
            for (button_x, label) in [(x, minus), (x + 3 + width, plus)] {
                let key = KeyCode::Char(label);
                let rect = Rect::new(button_x, area.y, 3, 1);
                frame.render_widget(
                    Paragraph::new(format!(" {label} ")).style(self.button_style(key, true)),
                    rect,
                );
                self.buttons.push(Button {
                    area: rect,
                    key,
                    enabled: true,
                });
            }
            x += width + 6;
        }
        let width = area.right().saturating_sub(x);
        frame.render_widget(
            Line::from(queue).style(style),
            Rect::new(x, area.y, width, 1),
        );
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, engine: &Engine, visual: Option<Mode>) {
        let buttons = button_specs(engine);
        for (button, position) in buttons.iter().zip(button_layout(area.width, &buttons)) {
            if position.y >= area.height || position.is_empty() {
                break;
            }
            let rect = Rect::new(area.x + position.x, area.y + position.y, position.width, 1);
            if button.key == KeyCode::Null {
                frame.render_widget(
                    Paragraph::new(if position.width >= button.width() {
                        " ←/→ Seek · Shift ±30s "
                    } else {
                        " ←/→ Seek "
                    })
                    .style(Style::default().fg(Color::DarkGray)),
                    rect,
                );
                self.buttons.push(Button {
                    area: rect,
                    key: button.key,
                    enabled: false,
                });
                continue;
            }
            let mut style = self.button_style(button.key, button.enabled);
            let active = match button.key {
                KeyCode::Char('l') => engine.is_liked(),
                KeyCode::Char('m') => engine.source == "mixed",
                KeyCode::Char('f') => engine.mode == PlaybackMode::FavoritesShuffle,
                KeyCode::Char('w') => {
                    engine.mode == PlaybackMode::MyVibe && engine.source != "mixed"
                }
                KeyCode::Char('v') => visual.is_some(),
                KeyCode::Char('b') => visual == Some(Mode::Dance),
                KeyCode::Char('g') => {
                    engine.can_tune_wave()
                        && engine.wave_style() != crate::backend::WaveStyle::Balanced
                }
                _ => false,
            };
            if active
                && button.enabled
                && self.hover != Some(Target::Button(button.key))
                && self.drag.is_none()
            {
                style = style.fg(Color::Cyan);
            }
            let mut shortcut = style.add_modifier(Modifier::BOLD);
            if button.enabled && style.bg != Some(Color::Cyan) && self.drag.is_none() {
                shortcut = shortcut.fg(Color::Cyan);
            }
            let label = Line::from(vec![
                Span::raw(" "),
                Span::styled(button.shortcut, shortcut),
                Span::raw(format!(" {} ", button.label)),
            ]);
            frame.render_widget(Paragraph::new(label).style(style), rect);
            self.buttons.push(Button {
                area: rect,
                key: button.key,
                enabled: button.enabled,
            });
        }
    }
}

fn transport_areas(area: Rect) -> [Rect; 3] {
    std::array::from_fn(|index| {
        let start = u32::from(area.width) * index as u32 / 3;
        let end = u32::from(area.width) * (index as u32 + 1) / 3;
        Rect::new(
            area.x + start as u16,
            area.y,
            (end - start) as u16,
            area.height,
        )
    })
}

fn highlight_zone(area: Rect, buffer: &mut Buffer) {
    fn tint(color: Color) -> Color {
        let [r, g, b] = match color {
            Color::Rgb(r, g, b) => [r, g, b],
            _ => [0, 0, 0],
        };
        Color::Rgb(
            ((u16::from(r) * 7 + 25) / 8) as u8,
            ((u16::from(g) * 7 + 130) / 8) as u8,
            ((u16::from(b) * 7 + 150) / 8) as u8,
        )
    }
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &mut buffer[(x, y)];
            cell.set_bg(tint(cell.bg));
            if matches!(cell.fg, Color::Rgb(..)) {
                cell.set_fg(tint(cell.fg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn artwork_thirds_tile_the_panel_and_dispatch_matching_clicks() {
        let keys = [KeyCode::Char('p'), KeyCode::Char(' '), KeyCode::Char('n')];
        for width in [1, 2, 3, 10, 74, 120] {
            let area = Rect::new(4, 5, width, 8);
            let zones = transport_areas(area);
            assert_eq!(zones[0].x, area.x);
            assert_eq!(zones[2].right(), area.right());
            assert_eq!(zones[0].right(), zones[1].x);
            assert_eq!(zones[1].right(), zones[2].x);
            let mut controls = Controls {
                transport: zones
                    .into_iter()
                    .zip(keys)
                    .map(|(area, key)| Button {
                        area,
                        key,
                        enabled: true,
                    })
                    .collect(),
                ..Controls::default()
            };
            for (zone, key) in zones
                .into_iter()
                .zip(keys)
                .filter(|(zone, _)| !zone.is_empty())
            {
                let x = zone.x + zone.width / 2;
                assert_eq!(
                    controls.handle(mouse(MouseEventKind::Moved, x, 8), None),
                    Some(Action::Redraw)
                );
                assert_eq!(
                    controls.handle(mouse(MouseEventKind::Moved, x, 9), None),
                    None
                );
                controls.handle(mouse(MouseEventKind::Down(MouseButton::Left), x, 8), None);
                assert_eq!(
                    controls.handle(mouse(MouseEventKind::Up(MouseButton::Left), x, 8), None),
                    Some(Action::Key(key))
                );
            }
            assert_eq!(
                controls.handle(mouse(MouseEventKind::Moved, 0, 0), None),
                Some(Action::Redraw)
            );
        }
    }

    #[test]
    fn artwork_clicks_yield_to_text_seek_and_buttons_and_cancel_across_zones() {
        let zones = transport_areas(Rect::new(3, 4, 90, 12));
        let mut controls = Controls {
            transport: zones
                .into_iter()
                .zip([KeyCode::Char('p'), KeyCode::Char(' '), KeyCode::Char('n')])
                .map(|(area, key)| Button {
                    area,
                    key,
                    enabled: true,
                })
                .collect(),
            text: Rect::new(4, 4, 88, 2),
            seek: Rect::new(4, 12, 88, 1),
            ..Controls::default()
        };
        controls.buttons.push(Button {
            area: Rect::new(6, 14, 8, 1),
            key: KeyCode::Char('p'),
            enabled: true,
        });
        controls.buttons.push(Button {
            area: Rect::new(70, 14, 8, 1),
            key: KeyCode::Char('l'),
            enabled: false,
        });
        assert!(controls.target_at(10, 4).is_none());
        assert!(controls.target_at(10, 12).is_none());
        assert!(controls.target_at(71, 14).is_none());
        assert!(matches!(
            controls.target_at(7, 14),
            Some(Target::Button(KeyCode::Char('p')))
        ));
        controls.handle(mouse(MouseEventKind::Down(MouseButton::Left), 10, 8), None);
        assert_eq!(
            controls.handle(mouse(MouseEventKind::Up(MouseButton::Left), 50, 8), None),
            Some(Action::Redraw)
        );
        controls.handle(mouse(MouseEventKind::Down(MouseButton::Left), 7, 14), None);
        assert_eq!(
            controls.handle(mouse(MouseEventKind::Up(MouseButton::Left), 10, 8), None),
            Some(Action::Redraw)
        );
        controls.handle(mouse(MouseEventKind::Down(MouseButton::Left), 10, 8), None);
        controls.cancel_drag();
        assert_eq!(
            controls.handle(mouse(MouseEventKind::Up(MouseButton::Left), 10, 8), None),
            Some(Action::Redraw)
        );
        controls.transport[0].enabled = false;
        assert!(controls.target_at(10, 8).is_none());
        assert!(matches!(
            controls.handle(
                mouse(MouseEventKind::Down(MouseButton::Left), 48, 12),
                Some(100.0)
            ),
            Some(Action::Seek(_))
        ));
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Up(MouseButton::Left), 48, 12),
                Some(100.0)
            ),
            Some(Action::Redraw)
        );
    }

    #[test]
    fn artwork_hover_tints_only_its_zone_without_erasing_pixels_or_wave() {
        let area = Rect::new(0, 0, 15, 4);
        let mut buffer = Buffer::empty(area);
        for cell in &mut buffer.content {
            cell.set_char('▀')
                .set_fg(Color::Rgb(44, 22, 11))
                .set_bg(Color::Rgb(20, 30, 40));
        }
        buffer[(7, 1)].set_char('⠿').set_fg(Color::Cyan);
        let before = buffer.clone();
        let zone = transport_areas(area)[1];
        highlight_zone(zone, &mut buffer);
        for y in 0..area.height {
            for x in 0..area.width {
                assert_eq!(buffer[(x, y)].symbol(), before[(x, y)].symbol());
                if zone.contains((x, y).into()) {
                    assert_ne!(buffer[(x, y)].bg, before[(x, y)].bg);
                } else {
                    assert_eq!(buffer[(x, y)], before[(x, y)]);
                }
            }
        }
        assert_eq!(buffer[(7, 1)].fg, Color::Cyan);
    }

    #[test]
    fn wrapped_groups_keep_distinct_mouse_targets_at_different_terminal_widths() {
        let buttons: [ButtonSpec; 13] = std::array::from_fn(|index| ButtonSpec {
            label: if index == PRIMARY_BUTTONS - 1 {
                "♥ Unlike"
            } else {
                "Favorites"
            },
            shortcut: "Space",
            key: KeyCode::Char(char::from(b'a' + index as u8)),
            enabled: true,
        });
        for width in [12, 30, 78, 120] {
            let positions: Vec<_> = button_layout(width, &buttons).collect();
            assert!(positions[PRIMARY_BUTTONS].y > positions[PRIMARY_BUTTONS - 1].y);
            let mut controls = Controls::default();
            for (button, area) in buttons.iter().zip(&positions) {
                assert!(area.width > 0 && area.right() <= width);
                assert!(
                    positions
                        .iter()
                        .filter(|other| !area.intersection(**other).is_empty())
                        .count()
                        == 1
                );
                controls.buttons.push(Button {
                    area: *area,
                    key: button.key,
                    enabled: button.enabled,
                });
            }
            for (button, area) in buttons.iter().zip(&positions) {
                let x = area.x + area.width / 2;
                controls.handle(
                    mouse(MouseEventKind::Down(MouseButton::Left), x, area.y),
                    None,
                );
                assert_eq!(
                    controls.handle(
                        mouse(MouseEventKind::Up(MouseButton::Left), x, area.y),
                        None
                    ),
                    Some(Action::Key(button.key))
                );
            }
        }
    }

    #[test]
    fn scrub_velocity_stays_at_two_for_normal_drags_and_caps_fast_flicks_at_four() {
        let elapsed = Duration::from_millis(250);
        assert_eq!(scrub_speed(30.0, 120.0, elapsed), 2.0);
        assert_eq!(scrub_speed(60.0, 240.0, elapsed), 2.0);
        assert_eq!(scrub_speed(-30.0, 120.0, elapsed), -2.0);
        assert_eq!(scrub_speed(0.1, 120.0, elapsed), 2.0);
        assert!(scrub_speed(75.0, 120.0, elapsed) > 2.0);
        assert_eq!(scrub_speed(90.0, 120.0, Duration::from_millis(200)), 4.0);
        assert_eq!(scrub_speed(-90.0, 120.0, Duration::from_millis(200)), -4.0);
    }

    #[test]
    fn seek_click_jumps_without_scrubbing_and_drag_scrubs_until_release() {
        let mut controls = Controls {
            seek: Rect::new(10, 3, 101, 1),
            ..Controls::default()
        };
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Down(MouseButton::Left), 30, 3),
                Some(100.0)
            ),
            Some(Action::Seek(20.0))
        );
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Drag(MouseButton::Left), 30, 3),
                Some(100.0)
            ),
            Some(Action::Redraw)
        );
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Up(MouseButton::Left), 30, 3),
                Some(100.0)
            ),
            Some(Action::Redraw)
        );
        assert!(!controls.is_seeking());
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Down(MouseButton::Left), 30, 3),
                Some(100.0)
            ),
            Some(Action::Seek(20.0))
        );
        assert!(matches!(
            controls.handle(
                mouse(MouseEventKind::Drag(MouseButton::Left), 80, 8),
                Some(100.0)
            ),
            Some(Action::Scrub { position: 70.0, speed }) if (2.0..=4.0).contains(&speed)
        ));
        assert_eq!(controls.position(20.0), 70.0);
        assert!(
            matches!(controls.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 15, 8), Some(100.0)), Some(Action::Scrub { position: 5.0, speed }) if (-4.0..=-2.0).contains(&speed))
        );
        assert_eq!(controls.position(20.0), 5.0);
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Up(MouseButton::Left), 200, 8),
                Some(100.0)
            ),
            Some(Action::Seek(100.0))
        );
        assert_eq!(controls.position(35.0), 35.0);
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Up(MouseButton::Left), 50, 3),
                Some(100.0)
            ),
            Some(Action::Redraw)
        );
    }

    #[test]
    fn buttons_require_matching_press_and_release_and_respect_disabled_state() {
        let mut controls = Controls::default();
        controls.buttons.push(Button {
            area: Rect::new(1, 1, 6, 1),
            key: KeyCode::Char('n'),
            enabled: true,
        });
        controls.buttons.push(Button {
            area: Rect::new(8, 1, 6, 1),
            key: KeyCode::Char('l'),
            enabled: false,
        });
        assert_eq!(
            controls.handle(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1), None),
            Some(Action::Redraw)
        );
        assert_eq!(
            controls.handle(mouse(MouseEventKind::Up(MouseButton::Left), 20, 1), None),
            Some(Action::Redraw)
        );
        assert_eq!(
            controls.handle(mouse(MouseEventKind::Down(MouseButton::Left), 9, 1), None),
            None
        );
        controls.handle(mouse(MouseEventKind::Down(MouseButton::Left), 2, 1), None);
        assert_eq!(
            controls.handle(mouse(MouseEventKind::Up(MouseButton::Left), 3, 1), None),
            Some(Action::Key(KeyCode::Char('n')))
        );
    }
}
