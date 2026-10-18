use crossterm::event;
#[cfg(not(windows))]
use {event::Event, std::io};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Reader;

#[cfg(not(windows))]
pub struct Reader;

#[cfg(not(windows))]
impl Reader {
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }

    pub fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

#[cfg(any(windows, test))]
fn shift_key_event(
    virtual_key: u16,
    scan_code: u16,
    pressed: bool,
    modifiers: event::KeyModifiers,
) -> Option<event::KeyEvent> {
    use event::{KeyCode, KeyEvent, KeyEventKind, ModifierKeyCode};
    // VK_SHIFT, VK_LSHIFT and VK_RSHIFT from the Windows virtual-key table.
    let side = match virtual_key {
        0x10 if scan_code == 0x36 => ModifierKeyCode::RightShift,
        0x10 | 0xa0 => ModifierKeyCode::LeftShift,
        0xa1 => ModifierKeyCode::RightShift,
        _ => return None,
    };
    Some(KeyEvent::new_with_kind(
        KeyCode::Modifier(side),
        modifiers,
        if pressed {
            KeyEventKind::Press
        } else {
            KeyEventKind::Release
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use event::{Event, KeyCode, KeyEventKind, KeyModifiers, ModifierKeyCode};

    #[test]
    fn windows_shift_records_drive_the_same_buttons_as_kitty_events() {
        let mut controls = crate::controls::Controls::default();
        for (key, scan, side) in [
            (0x10, 0x2a, ModifierKeyCode::LeftShift),
            (0x10, 0x36, ModifierKeyCode::RightShift),
            (0xa0, 0, ModifierKeyCode::LeftShift),
            (0xa1, 0, ModifierKeyCode::RightShift),
        ] {
            for pressed in [true, false] {
                let modifiers = if pressed {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                let event = shift_key_event(key, scan, pressed, modifiers).unwrap();
                assert_eq!(event.code, KeyCode::Modifier(side));
                assert_eq!(
                    event.kind,
                    if pressed {
                        KeyEventKind::Press
                    } else {
                        KeyEventKind::Release
                    }
                );
                assert!(controls.update_modifiers(&Event::Key(event)));
            }
        }
        assert!(shift_key_event(0x46, 0x21, true, KeyModifiers::SHIFT).is_none());
    }
}
