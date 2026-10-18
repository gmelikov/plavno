use crossterm::event::Event;
use crossterm_winapi::{Console, Handle, InputRecord};
use std::io;

mod parse;

pub struct Reader {
    console: Console,
    surrogate: Option<u16>,
    buttons: parse::MouseButtonsPressed,
}

impl Reader {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            console: Console::from(Handle::current_in_handle()?),
            surrogate: None,
            buttons: parse::MouseButtonsPressed::default(),
        })
    }

    pub fn read(&mut self) -> io::Result<Event> {
        loop {
            let event = match self.console.read_single_input_event()? {
                InputRecord::KeyEvent(record) => {
                    parse::handle_key_event(record, &mut self.surrogate)
                }
                InputRecord::MouseEvent(record) => {
                    let event = parse::handle_mouse_event(record, &self.buttons);
                    self.buttons = parse::MouseButtonsPressed {
                        left: record.button_state.left_button(),
                        right: record.button_state.right_button(),
                        middle: record.button_state.middle_button(),
                    };
                    event
                }
                InputRecord::WindowBufferSizeEvent(_) => {
                    let (width, height) = crossterm::terminal::size()?;
                    Some(Event::Resize(width, height))
                }
                InputRecord::FocusEvent(record) => Some(if record.set_focus {
                    Event::FocusGained
                } else {
                    Event::FocusLost
                }),
                _ => None,
            };
            if let Some(event) = event {
                return Ok(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};
    use crossterm_winapi::KeyEventRecord;
    use winapi::um::wincon::{INPUT_RECORD, KEY_EVENT};

    fn record(key: u16, scan: u16, character: u16, down: bool, modifiers: u32) -> KeyEventRecord {
        let mut input = INPUT_RECORD {
            EventType: KEY_EVENT,
            ..INPUT_RECORD::default()
        };
        unsafe {
            let record = input.Event.KeyEvent_mut();
            record.bKeyDown = i32::from(down);
            record.wRepeatCount = 1;
            record.wVirtualKeyCode = key;
            record.wVirtualScanCode = scan;
            *record.uChar.UnicodeChar_mut() = character;
            record.dwControlKeyState = modifiers;
        }
        let InputRecord::KeyEvent(record) = input.into() else {
            unreachable!()
        };
        record
    }

    #[test]
    fn native_shift_events_preserve_both_sides_and_release() {
        let mut surrogate = None;
        for (scan, side) in [
            (42, ModifierKeyCode::LeftShift),
            (54, ModifierKeyCode::RightShift),
        ] {
            for down in [true, false] {
                let modifiers = if down { 0x10 } else { 0 };
                let event =
                    parse::handle_key_event(record(0x10, scan, 0, down, modifiers), &mut surrogate);
                assert_eq!(
                    event,
                    Some(Event::Key(KeyEvent::new_with_kind(
                        KeyCode::Modifier(side),
                        if down {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                        if down {
                            KeyEventKind::Press
                        } else {
                            KeyEventKind::Release
                        },
                    )))
                );
            }
        }
    }

    #[test]
    fn native_text_surrogates_and_control_shortcuts_keep_upstream_semantics() {
        let mut surrogate = None;
        for character in ['Ж', 'f'] {
            assert_eq!(
                parse::handle_key_event(record(0, 0, character as u16, true, 0), &mut surrogate),
                Some(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE
                )))
            );
        }
        assert!(parse::handle_key_event(record(0, 0, 0xd83d, true, 0), &mut surrogate).is_none());
        // A modifier between the UTF-16 halves must not discard the buffered character.
        assert!(parse::handle_key_event(record(0x10, 42, 0, true, 0x10), &mut surrogate).is_some());
        assert_eq!(
            parse::handle_key_event(record(0, 0, 0xde00, true, 0), &mut surrogate),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('😀'),
                KeyModifiers::NONE
            )))
        );
        assert_eq!(
            parse::handle_key_event(record(0x46, 0x21, 'f' as u16, true, 0x08), &mut surrogate),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL
            )))
        );
    }
}
