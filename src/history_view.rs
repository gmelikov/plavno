use crate::history::Entry;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph},
};
use std::time::{Duration, Instant};

pub enum Action {
    Close,
    Play(Box<Entry>),
}

pub struct HistoryView {
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    query: String,
    state: ListState,
    area: Rect,
    clicked: Option<(usize, Instant)>,
    buttons: [Rect; 3],
    pressed: Option<usize>,
}

impl HistoryView {
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut view = Self {
            entries,
            filtered: Vec::new(),
            query: String::new(),
            state: ListState::default(),
            area: Rect::default(),
            clicked: None,
            buttons: [Rect::default(); 3],
            pressed: None,
        };
        view.filter();
        view
    }

    fn filter(&mut self) {
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.matches(&self.query))
            .map(|(index, _)| index)
            .collect();
        self.state = ListState::default().with_selected((!self.filtered.is_empty()).then_some(0));
        self.clicked = None;
    }

    fn move_selection(&mut self, offset: isize) {
        if !self.filtered.is_empty() {
            let index = self
                .state
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(offset)
                .min(self.filtered.len() - 1);
            self.state.select(Some(index));
        }
        self.clicked = None;
    }

    fn play(&self) -> Option<Action> {
        let index = self.state.selected()?;
        Some(Action::Play(Box::new(
            self.entries[*self.filtered.get(index)?].clone(),
        )))
    }

    pub fn handle(&mut self, event: Event) -> Option<Action> {
        if let Event::Mouse(mouse) = event {
            let point = (mouse.column, mouse.row).into();
            let button = self.buttons.iter().position(|area| area.contains(point));
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Right) => return Some(Action::Close),
                MouseEventKind::Down(MouseButton::Left) if button.is_some() => {
                    self.pressed = button;
                    return None;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    let pressed = self.pressed.take();
                    if let Some(pressed) = pressed
                        && Some(pressed) == button
                    {
                        match pressed {
                            0 => return self.play(),
                            1 => {
                                self.query.clear();
                                self.filter();
                            }
                            2 => return Some(Action::Close),
                            _ => {}
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }
        match event {
            Event::Paste(text) => {
                self.query.extend(
                    text.chars()
                        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
                        .filter(|ch| !ch.is_control()),
                );
                self.filter();
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Esc => return Some(Action::Close),
                    KeyCode::Enter => return self.play(),
                    KeyCode::Up => self.move_selection(-1),
                    KeyCode::Down => self.move_selection(1),
                    KeyCode::PageUp => self.move_selection(-10),
                    KeyCode::PageDown => self.move_selection(10),
                    KeyCode::Home => self.move_selection(isize::MIN),
                    KeyCode::End => self.move_selection(isize::MAX),
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.filter();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.query.clear();
                        self.filter();
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.query.push(c);
                        self.filter();
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) if self.area.contains((mouse.column, mouse.row).into()) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.move_selection(-3),
                    MouseEventKind::ScrollDown => self.move_selection(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        let index = self.state.offset() + usize::from(mouse.row - self.area.y);
                        if index < self.filtered.len() {
                            self.state.select(Some(index));
                            if self.clicked.is_some_and(|(last, time)| {
                                last == index && time.elapsed() < Duration::from_millis(350)
                            }) {
                                return self.play();
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
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .margin(1)
        .split(frame.area());
        let accent = Style::default().fg(Color::Cyan);
        frame.render_widget(
            Paragraph::new(format!(
                "History · {} matches out of {}",
                self.filtered.len(),
                self.entries.len()
            ))
            .style(accent),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(format!("{}▏", self.query))
                .block(Block::bordered().title(" Search: title / artist "))
                .style(accent),
            rows[1],
        );
        let block = Block::bordered().title(" Recent plays first · UTC ");
        self.area = block.inner(rows[2]);
        if self.filtered.is_empty() {
            frame.render_widget(
                Paragraph::new(if self.entries.is_empty() {
                    "History is empty"
                } else {
                    "No matches"
                })
                .block(block),
                rows[2],
            );
        } else {
            let items: Vec<_> = self
                .filtered
                .iter()
                .map(|&index| {
                    let entry = &self.entries[index];
                    let date = time::OffsetDateTime::from_unix_timestamp(entry.played_at as i64)
                        .map(|date| {
                            format!(
                                "{}-{:02}-{:02} {:02}:{:02}",
                                date.year(),
                                u8::from(date.month()),
                                date.day(),
                                date.hour(),
                                date.minute()
                            )
                        })
                        .unwrap_or_default();
                    let artist = if entry.track.artist.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", entry.track.artist)
                    };
                    ListItem::new(format!(
                        "{date}  {}{artist} · {}",
                        entry.track.title, entry.source
                    ))
                })
                .collect();
            frame.render_stateful_widget(
                List::new(items)
                    .block(block)
                    .highlight_symbol("› ")
                    .highlight_style(accent.add_modifier(Modifier::BOLD)),
                rows[2],
                &mut self.state,
            );
        }
        for (index, label) in ["[Play]", "[Clear]", "[Back]"].into_iter().enumerate() {
            let offset = [0, 7, 15][index];
            self.buttons[index] = Rect::new(
                rows[3].x + offset,
                rows[3].y,
                (label.len() as u16).min(rows[3].width.saturating_sub(offset)),
                1.min(rows[3].height),
            );
            frame.render_widget(Paragraph::new(label).style(accent), self.buttons[index]);
        }
        if rows[3].height > 1 {
            frame.render_widget(Paragraph::new("Type to search · ↑/↓ select · Enter / double-click play · Esc / right-click back · Ctrl+U clear"), Rect::new(rows[3].x, rows[3].y + 1, rows[3].width, 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Track;
    use crossterm::event::MouseEvent;

    #[test]
    fn pasted_search_and_clear_back_buttons_work_without_playing_on_selection() {
        let entries = ["Calm sea", "Ocean wind"]
            .map(|title| Entry {
                source: "local".into(),
                played_at: 1,
                track: Track {
                    source: "local".into(),
                    id: title.into(),
                    title: title.into(),
                    artist: "Artist".into(),
                    duration: None,
                    cover_url: None,
                    genre: None,
                    batch: String::new(),
                },
            })
            .to_vec();
        let mut view = HistoryView::new(entries);
        view.area = Rect::new(1, 1, 30, 5);
        view.buttons = [
            Rect::new(1, 8, 6, 1),
            Rect::new(8, 8, 7, 1),
            Rect::new(16, 8, 6, 1),
        ];
        view.handle(Event::Paste("Ocean\nwind".into()));
        assert_eq!(view.filtered.len(), 1);
        let click = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        view.handle(click(MouseEventKind::Down(MouseButton::Left), 9, 8));
        view.handle(click(MouseEventKind::Up(MouseButton::Left), 9, 8));
        assert!(view.query.is_empty());
        assert_eq!(view.filtered.len(), 2);
        assert!(
            view.handle(click(MouseEventKind::Down(MouseButton::Left), 2, 2))
                .is_none()
        );
        assert_eq!(view.state.selected(), Some(1));
        assert!(matches!(
            view.handle(click(MouseEventKind::Down(MouseButton::Left), 2, 2)),
            Some(Action::Play(_))
        ));
        view.handle(click(MouseEventKind::Down(MouseButton::Left), 17, 8));
        assert!(matches!(
            view.handle(click(MouseEventKind::Up(MouseButton::Left), 17, 8)),
            Some(Action::Close)
        ));
    }
}
