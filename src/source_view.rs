use crate::backend::Service;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use std::time::{Duration, Instant};

pub enum Action {
    Close,
    Select(&'static str),
}

pub struct SourceView {
    services: Vec<Service>,
    state: ListState,
    area: Rect,
    back: Rect,
    play: Rect,
    pressed: Option<KeyCode>,
    clicked: Option<(usize, Instant)>,
}

impl SourceView {
    pub fn new(services: Vec<Service>, current: &str) -> Self {
        let selected = services.iter().position(|s| s.key == current).unwrap_or(0);
        Self {
            services,
            state: ListState::default().with_selected(Some(selected)),
            area: Rect::default(),
            back: Rect::default(),
            play: Rect::default(),
            pressed: None,
            clicked: None,
        }
    }
    fn select(&self) -> Option<Action> {
        self.services
            .get(self.state.selected()?)
            .map(|s| Action::Select(s.key))
    }
    fn move_by(&mut self, offset: isize) {
        self.clicked = None;
        if !self.services.is_empty() {
            self.state.select(Some(
                self.state
                    .selected()
                    .unwrap_or(0)
                    .saturating_add_signed(offset)
                    .min(self.services.len() - 1),
            ));
        }
    }
    pub fn handle(&mut self, event: Event) -> Option<Action> {
        if let Event::Mouse(mouse) = event {
            let point = (mouse.column, mouse.row).into();
            let button = if self.back.contains(point) {
                Some(KeyCode::Esc)
            } else if self.play.contains(point) {
                Some(KeyCode::Enter)
            } else {
                None
            };
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Right) => return Some(Action::Close),
                MouseEventKind::Down(MouseButton::Left) if button.is_some() => {
                    self.pressed = button;
                    return None;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    let pressed = self.pressed.take();
                    if pressed.is_some() && pressed == button {
                        return if pressed == Some(KeyCode::Esc) {
                            Some(Action::Close)
                        } else {
                            self.select()
                        };
                    }
                    return None;
                }
                _ => {}
            }
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 's') => return Some(Action::Close),
                    KeyCode::Enter => return self.select(),
                    KeyCode::Up => self.move_by(-1),
                    KeyCode::Down => self.move_by(1),
                    _ => {}
                }
            }
            Event::Mouse(mouse) if self.area.contains((mouse.column, mouse.row).into()) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.move_by(-3),
                    MouseEventKind::ScrollDown => self.move_by(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        let index = self.state.offset() + usize::from(mouse.row - self.area.y);
                        if index < self.services.len() {
                            self.state.select(Some(index));
                            if self.clicked.is_some_and(|(last, time)| {
                                last == index && time.elapsed() < Duration::from_millis(350)
                            }) {
                                self.clicked = None;
                                return self.select();
                            }
                            self.clicked = Some((index, Instant::now()));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        None
    }
    pub fn draw(&mut self, frame: &mut Frame) {
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(4),
        ])
        .margin(1)
        .split(frame.area());
        frame.render_widget(
            Paragraph::new("Choose a wave · Click select · Double-click / Enter play · Esc back")
                .block(Block::bordered().title(format!(" plavno {} · Sources ", crate::VERSION))),
            rows[0],
        );
        let block = Block::bordered();
        self.area = block.inner(rows[1]);
        let items: Vec<_> = self
            .services
            .iter()
            .map(|s| {
                ListItem::new(format!(
                    "{}  {}",
                    if s.available { "●" } else { "○" },
                    s.name
                ))
                .style(Style::default().fg(if s.available {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(Color::DarkGray)),
            rows[1],
            &mut self.state,
        );
        let detail = self
            .state
            .selected()
            .and_then(|i| self.services.get(i))
            .map_or("", |s| s.status.as_str());
        frame.render_widget(
            Paragraph::new(format!(
                "{detail}\nMixed mode chooses a random connected service for each track."
            ))
            .wrap(Wrap { trim: true }),
            rows[2],
        );
        self.play = Rect::new(
            rows[2].x,
            rows[2].bottom().saturating_sub(1),
            6.min(rows[2].width),
            1.min(rows[2].height),
        );
        self.back = Rect::new(
            self.play.right() + 1,
            self.play.y,
            6.min(rows[2].width.saturating_sub(7)),
            self.play.height,
        );
        frame.render_widget(
            Paragraph::new("[Play]").style(Style::default().fg(Color::Cyan)),
            self.play,
        );
        frame.render_widget(
            Paragraph::new("[Back]").style(Style::default().fg(Color::Cyan)),
            self.back,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEvent};

    #[test]
    fn mouse_selects_without_starting_and_double_click_or_play_activates() {
        let mut view = SourceView::new(
            ["first", "second"]
                .map(|key| Service {
                    key,
                    name: key,
                    status: "Ready".into(),
                    available: true,
                    favorites: false,
                    favorites_shuffle: false,
                    wave_styles: false,
                    allow_repeats: false,
                })
                .to_vec(),
            "first",
        );
        view.area = Rect::new(1, 1, 20, 5);
        view.play = Rect::new(1, 8, 6, 1);
        view.back = Rect::new(8, 8, 6, 1);
        let click = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(
            view.handle(click(MouseEventKind::Down(MouseButton::Left), 2, 2))
                .is_none()
        );
        assert_eq!(view.state.selected(), Some(1));
        assert!(matches!(
            view.handle(click(MouseEventKind::Down(MouseButton::Left), 2, 2)),
            Some(Action::Select("second"))
        ));
        assert!(
            view.handle(click(MouseEventKind::Down(MouseButton::Left), 2, 8))
                .is_none()
        );
        assert!(matches!(
            view.handle(click(MouseEventKind::Up(MouseButton::Left), 2, 8)),
            Some(Action::Select("second"))
        ));
        assert!(matches!(
            view.handle(click(MouseEventKind::Down(MouseButton::Right), 25, 10)),
            Some(Action::Close)
        ));
    }
}
