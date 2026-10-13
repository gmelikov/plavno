use crate::{backend::PlaybackMode, engine::Engine};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
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
    VolumeDelta(f32),
    Redraw,
}

struct Button {
    area: Rect,
    key: KeyCode,
    enabled: bool,
}

enum Drag {
    Seek { position: f64 },
    Button(KeyCode),
}

#[derive(Default)]
pub struct Controls {
    pub seek: Rect,
    buttons: Vec<Button>,
    drag: Option<Drag>,
    hover: Option<KeyCode>,
    track: Option<(String, String)>,
}

fn fraction(area: Rect, column: u16) -> Option<f64> {
    (area.width >= 2).then(|| {
        f64::from(column.saturating_sub(area.x).min(area.width - 1)) / f64::from(area.width - 1)
    })
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

    fn button_at(&self, column: u16, row: u16) -> Option<KeyCode> {
        self.buttons
            .iter()
            .find(|button| button.enabled && button.area.contains((column, row).into()))
            .map(|button| button.key)
    }

    fn button_style(&self, key: KeyCode, enabled: bool) -> Style {
        let base = Style::default().bg(Color::Rgb(28, 32, 40));
        if !enabled {
            base.fg(Color::DarkGray)
        } else if matches!(self.drag, Some(Drag::Button(k)) if k == key) {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(30, 120, 135))
        } else if self.hover == Some(key) || key == KeyCode::Char(' ') {
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
                let hover = self.button_at(mouse.column, mouse.row);
                if self.hover != hover {
                    self.hover = hover;
                    return Some(Action::Redraw);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None;
                if self.seek.contains(point) {
                    let position = duration? * fraction(self.seek, mouse.column)?;
                    self.drag = Some(Drag::Seek { position });
                    return Some(Action::Redraw);
                }
                if let Some(key) = self.button_at(mouse.column, mouse.row) {
                    self.drag = Some(Drag::Button(key));
                    return Some(Action::Redraw);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                let released = matches!(mouse.kind, MouseEventKind::Up(_));
                let action = match self.drag {
                    Some(Drag::Seek { .. }) => {
                        let Some(duration) = duration else {
                            self.cancel_drag();
                            return Some(Action::Redraw);
                        };
                        let position = duration * fraction(self.seek, mouse.column)?;
                        self.drag = Some(Drag::Seek { position });
                        if released {
                            Some(Action::Seek(position))
                        } else {
                            Some(Action::Redraw)
                        }
                    }
                    Some(Drag::Button(key)) if released => {
                        (self.button_at(mouse.column, mouse.row) == Some(key))
                            .then_some(Action::Key(key))
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
    }

    pub fn draw_volume(&mut self, frame: &mut Frame, area: Rect, volume: &str, queue: &str) {
        let style = Style::default().fg(Color::Gray);
        if area.width < 6 {
            frame.render_widget(Line::from(volume).style(style), area);
            return;
        }
        let volume_width = volume.width().min(usize::from(area.width - 6)) as u16;
        frame.render_widget(
            Line::from(volume).style(style),
            Rect::new(area.x + 3, area.y, volume_width, 1),
        );
        let plus_x = area.x + 3 + volume_width;
        for (x, key, label) in [
            (area.x, KeyCode::Char('-'), " - "),
            (plus_x, KeyCode::Char('+'), " + "),
        ] {
            let rect = Rect::new(x, area.y, 3, 1);
            frame.render_widget(
                Paragraph::new(label).style(self.button_style(key, true)),
                rect,
            );
            self.buttons.push(Button {
                area: rect,
                key,
                enabled: true,
            });
        }
        let width = area.right().saturating_sub(plus_x + 3);
        frame.render_widget(
            Line::from(queue).style(style),
            Rect::new(plus_x + 3, area.y, width, 1),
        );
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, engine: &Engine, wave_enabled: bool) {
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
                KeyCode::Char('v') => wave_enabled,
                KeyCode::Char('g') => {
                    engine.can_tune_wave()
                        && engine.wave_style() != crate::backend::WaveStyle::Balanced
                }
                _ => false,
            };
            if active && button.enabled && self.hover != Some(button.key) && self.drag.is_none() {
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
    fn seek_drag_previews_until_release_and_clamps_outside_the_bar() {
        let mut controls = Controls {
            seek: Rect::new(10, 3, 101, 1),
            ..Controls::default()
        };
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Down(MouseButton::Left), 30, 3),
                Some(100.0)
            ),
            Some(Action::Redraw)
        );
        assert_eq!(
            controls.handle(
                mouse(MouseEventKind::Drag(MouseButton::Left), 80, 8),
                Some(100.0)
            ),
            Some(Action::Redraw)
        );
        assert_eq!(controls.position(20.0), 70.0);
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
