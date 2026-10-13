use crate::backend::WaveStyle;
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};

pub enum Action {
    Close,
    Apply(WaveStyle),
}

pub struct FlowView {
    source: String,
    name: &'static str,
    state: ListState,
    area: Rect,
    pressed: Option<usize>,
}

impl FlowView {
    pub fn new(source: &str, name: &'static str, style: WaveStyle) -> Self {
        Self {
            source: source.into(),
            name,
            state: ListState::default()
                .with_selected(WaveStyle::ALL.iter().position(|s| *s == style)),
            area: Rect::default(),
            pressed: None,
        }
    }

    pub fn cancel_press(&mut self) {
        self.pressed = None;
    }

    fn move_by(&mut self, delta: isize) {
        self.state.select(Some(
            self.state
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(delta)
                .min(3),
        ));
    }

    pub fn handle(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | 'g') => return Some(Action::Close),
                    KeyCode::Char(c @ '1'..='4') => {
                        return Some(Action::Apply(WaveStyle::ALL[c as usize - '1' as usize]));
                    }
                    KeyCode::Enter => {
                        return Some(Action::Apply(
                            WaveStyle::ALL[self.state.selected().unwrap_or(0)],
                        ));
                    }
                    KeyCode::Up => self.move_by(-1),
                    KeyCode::Down => self.move_by(1),
                    _ => {}
                }
            }
            Event::Mouse(mouse) => {
                let index = self
                    .area
                    .contains((mouse.column, mouse.row).into())
                    .then(|| self.state.offset() + usize::from(mouse.row - self.area.y))
                    .filter(|index| *index < WaveStyle::ALL.len());
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Right) => return Some(Action::Close),
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.pressed = index;
                        if let Some(index) = index {
                            self.state.select(Some(index));
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(pressed) = self.pressed.take()
                            && index == Some(pressed)
                        {
                            return Some(Action::Apply(WaveStyle::ALL[pressed]));
                        }
                    }
                    MouseEventKind::ScrollUp => self.move_by(-1),
                    MouseEventKind::ScrollDown => self.move_by(1),
                    _ => {}
                }
            }
            Event::Resize(_, _) | Event::FocusLost => self.cancel_press(),
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Min(0),
        ])
        .margin(1)
        .split(frame.area());
        frame.render_widget(
            Paragraph::new("1–4 choose · ↑/↓ select · Enter apply · Esc back")
                .block(Block::bordered().title(format!(" plavno · Flow · {} ", self.name))),
            rows[0],
        );
        let block = Block::bordered();
        self.area = block.inner(rows[1]);
        let items: Vec<_> = WaveStyle::ALL
            .iter()
            .enumerate()
            .map(|(index, style)| {
                ListItem::new(format!(
                    " {}  {:<10} {}",
                    index + 1,
                    style.label(),
                    style.description(&self.source)
                ))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan)),
            rows[1],
            &mut self.state,
        );
        frame.render_widget(Paragraph::new("Click a preset to apply it. Applying starts a new wave; settings are saved per service.")
            .wrap(Wrap { trim: true }).style(Style::default().fg(Color::Gray)), rows[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent};

    #[test]
    fn flow_presets_support_keys_and_matching_mouse_press_release() {
        let mut view = FlowView::new("vk", "VK Music", WaveStyle::Balanced);
        view.area = Rect::new(2, 4, 60, 4);
        let mouse = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(
            view.handle(mouse(MouseEventKind::Down(MouseButton::Left), 5, 7))
                .is_none()
        );
        assert!(matches!(
            view.handle(mouse(MouseEventKind::Up(MouseButton::Left), 5, 7)),
            Some(Action::Apply(WaveStyle::Work))
        ));
        view.handle(mouse(MouseEventKind::Down(MouseButton::Left), 5, 4));
        view.cancel_press();
        assert!(
            view.handle(mouse(MouseEventKind::Up(MouseButton::Left), 5, 4))
                .is_none()
        );
        assert!(matches!(
            view.handle(Event::Key(KeyEvent::new(
                KeyCode::Char('3'),
                KeyModifiers::NONE
            ))),
            Some(Action::Apply(WaveStyle::Energetic))
        ));
    }
}
