#![cfg(target_os = "linux")]

use std::{
    fs::{self, File},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    child: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn read_until(master: &mut File, output: &mut Vec<u8>, done: impl Fn(&[u8]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(output) {
        let mut bytes = [0; 8192];
        let requests = output.windows(4).filter(|w| *w == b"\x1b[6n").count();
        match master.read(&mut bytes) {
            Ok(count) => output.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("TUI ended before the expected output: {error}"),
        }
        for _ in requests..output.windows(4).filter(|w| *w == b"\x1b[6n").count() {
            master.write_all(b"\x1b[1;1R").unwrap();
        }
        assert!(output.len() < 1024 * 1024, "TUI produced excessive output");
        assert!(
            Instant::now() < deadline,
            "TUI did not produce the expected output: {:?}",
            String::from_utf8_lossy(&output[output.len().saturating_sub(2000)..])
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn screen_text(bytes: &[u8]) -> String {
    let mut cells = vec![vec![' '; 80]; 24];
    let (mut x, mut y) = (0usize, 0usize);
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    for end in chars.by_ref() {
                        if ('@'..='~').contains(&end) {
                            let values: Vec<usize> =
                                params.split(';').map(|v| v.parse().unwrap_or(0)).collect();
                            let first = values[0];
                            match end {
                                'H' | 'f' => {
                                    y = first.max(1) - 1;
                                    x = values.get(1).copied().unwrap_or(1).max(1) - 1;
                                }
                                'G' => x = first.max(1) - 1,
                                'd' => y = first.max(1) - 1,
                                'A' => y = y.saturating_sub(first.max(1)),
                                'B' => y += first.max(1),
                                'C' => x += first.max(1),
                                'D' => x = x.saturating_sub(first.max(1)),
                                'J' if first == 2 => cells.iter_mut().for_each(|row| row.fill(' ')),
                                'K' if y < cells.len() => {
                                    let range = match first {
                                        1 => 0..(x + 1).min(80),
                                        2 => 0..80,
                                        _ => x.min(80)..80,
                                    };
                                    cells[y][range].fill(' ');
                                }
                                _ => {}
                            }
                            break;
                        }
                        params.push(end);
                    }
                }
                Some(']') => {
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' || ch == '\x1b' && chars.next() == Some('\\') {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => x = 0,
            '\n' => y += 1,
            ch if !ch.is_control() => {
                if y < cells.len() && x < cells[y].len() {
                    cells[y][x] = ch;
                }
                x += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
            _ => {}
        }
    }
    cells
        .into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_wave(screen: &str) -> bool {
    screen
        .chars()
        .any(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch))
}

fn pane_height(screen: &str) -> Option<usize> {
    let top = screen.lines().position(|line| line.contains('╭'))?;
    let bottom = screen.lines().position(|line| line.contains('╰'))?;
    bottom.checked_sub(top).map(|height| height + 1)
}

fn click_label(master: &mut File, output: &mut Vec<u8>, label: &str) {
    let locate = |bytes: &[u8]| {
        let screen = screen_text(bytes);
        screen.lines().enumerate().find_map(|(row, line)| {
            let index = line.find(label)?;
            Some((
                row,
                unicode_width::UnicodeWidthStr::width(&line[..index]) + 1,
            ))
        })
    };
    read_until(master, output, |bytes| locate(bytes).is_some());
    let (row, column) = locate(output).unwrap();
    master
        .write_all(format!("\x1b[<0;{column};{}M\x1b[<0;{column};{}m", row + 1, row + 1).as_bytes())
        .unwrap();
}

fn spawn_player(args: &[&str]) -> (Fixture, File) {
    spawn_player_with_state(args, None)
}

fn spawn_player_with_state(args: &[&str], state: Option<&Path>) -> (Fixture, File) {
    static NEXT_FIXTURE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "plavno-tui-test-{}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("player"),
        NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    fs::create_dir(&root).unwrap();
    let alsa = root.join("alsa.conf");
    fs::write(&alsa, "pcm.!default { type null }\n").unwrap();
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        0
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_plavno"));
    command
        .arg("--file")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"))
        .args(args)
        .env("XDG_CONFIG_HOME", &root)
        .env("XDG_STATE_HOME", state.unwrap_or(root.as_path()))
        .env("ALSA_CONFIG_PATH", &alsa)
        .env("TERM", "xterm-256color")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    (Fixture { root, child }, master)
}

fn check_title_selection(args: &[&str]) {
    let (mut fixture, mut master) = spawn_player(args);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(4).any(|w| w == b"tone")
    });
    master.write_all(b" ").unwrap();
    master
        .write_all(b"\x1b[<0;4;3M\x1b[<32;7;3M\x1b[<0;7;3m")
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(6).any(|w| w == b"Ctrl+C")
    });
    master.write_all(b"\x03").unwrap();
    let clipboard = b"\x1b]52;c;dG9uZQ==\x1b\\";
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(clipboard.len()).any(|w| w == clipboard)
    });
    assert!(
        fixture.child.try_wait().unwrap().is_none(),
        "Copying quit the player"
    );
    master.write_all(b"\x1b").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    master.write_all(b"\x03").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        let mut bytes = [0; 8192];
        let _ = master.read(&mut bytes);
        assert!(
            Instant::now() < deadline,
            "Ctrl+C did not quit after selection was cleared"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn tui_mouse_selection_copies_title_and_ctrl_c_quits_after_clearing_it() {
    check_title_selection(&[]);
}

#[test]
fn tui_no_wave_mouse_selection_copies_title_and_ctrl_c_quits_after_clearing_it() {
    check_title_selection(&["--no-wave"]);
}

#[test]
fn tui_artwork_center_hover_and_click_toggle_playback_and_clear_over_text() {
    let (_fixture, mut master) = spawn_player(&[]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("tone")
            && screen.contains("Playing")
            && screen.contains("q Quit")
            && pane_height(&screen).is_some()
    });
    master.write_all(b"\x1b[<35;40;10M").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Pause · Space")
    });
    master.write_all(b"\x1b[<0;40;10M\x1b[<0;40;10m").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Paused") && screen.contains("Play · Space")
    });
    master.write_all(b"\x1b[<35;4;3M").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        !screen_text(bytes).contains("Play · Space")
    });
    master
        .write_all(b"\x1b[<0;4;3M\x1b[<32;7;3M\x1b[<0;7;3m")
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Ctrl+C copy")
    });
    assert!(screen_text(&output).contains("Paused"));
    master
        .write_all(b"\x1b[<35;40;10M\x1b[<0;40;10M\x1b[<0;40;10m")
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Playing")
            && screen.contains("Pause · Space")
            && !screen.contains("Ctrl+C copy")
    });
}

#[test]
fn tui_seek_drag_scrubs_before_release_and_restores_pause_on_release_and_focus_loss() {
    let (_fixture, mut master) = spawn_player(&[]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("tone") && screen.contains(" / 1:00") && screen.contains("q Quit")
    });
    master.write_all(b" ").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Paused")
    });
    let row = screen_text(&output)
        .lines()
        .position(|line| line.contains(" / 1:00"))
        .unwrap()
        + 1;
    master
        .write_all(format!("\x1b[<0;20;{row}M").as_bytes())
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Paused") && !screen.contains("Scrubbing") && screen.contains("0:13 / 1:00")
    });
    master
        .write_all(format!("\x1b[<32;60;{row}M").as_bytes())
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Scrubbing →")
            && !screen.contains("Paused")
            && screen.contains("0:46 / 1:00")
    });
    master
        .write_all(format!("\x1b[<0;60;{row}m").as_bytes())
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Paused") && !screen.contains("Scrubbing") && screen.contains("0:46 / 1:00")
    });
    master
        .write_all(format!("\x1b[<0;60;{row}M\x1b[<32;20;{row}M").as_bytes())
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Scrubbing ←") && screen.contains("0:13 / 1:00")
    });
    master.write_all(b"\x1b[O").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Paused") && !screen.contains("Scrubbing") && screen.contains("0:13 / 1:00")
    });
}

#[test]
fn tui_dislike_and_undo_update_private_persistent_state() {
    let (fixture, mut master) = spawn_player(&[]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(4).any(|word| word == b"tone")
    });
    let path = fixture.root.join("plavno/dislikes.json");
    master.write_all(b" d").unwrap();
    read_until(&mut master, &mut output, |_| path.exists());
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), 1);
    assert_eq!(saved[0]["source"], "local");
    let (_blocked_fixture, mut blocked_terminal) =
        spawn_player_with_state(&[], Some(&fixture.root));
    let mut blocked_output = Vec::new();
    read_until(&mut blocked_terminal, &mut blocked_output, |bytes| {
        bytes.windows(5).any(|word| word == b"queue")
    });
    assert!(!blocked_output.windows(4).any(|word| word == b"tone"));
    master.write_all(b"u").unwrap();
    read_until(&mut master, &mut output, |_| {
        fs::read(&path).is_ok_and(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .is_ok_and(|value| value.as_array().is_some_and(|entries| entries.is_empty()))
        })
    });
    let (_restored_fixture, mut restored_terminal) =
        spawn_player_with_state(&[], Some(&fixture.root));
    let mut restored_output = Vec::new();
    read_until(&mut restored_terminal, &mut restored_output, |bytes| {
        bytes.windows(4).any(|word| word == b"tone")
    });
    restored_terminal.write_all(b" ").unwrap();
    click_label(&mut restored_terminal, &mut restored_output, "d Dislike");
    read_until(&mut restored_terminal, &mut restored_output, |_| {
        fs::read(&path).is_ok_and(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .is_ok_and(|value| value.as_array().is_some_and(|entries| entries.len() == 1))
        })
    });
    click_label(&mut restored_terminal, &mut restored_output, "u Undo");
    read_until(&mut restored_terminal, &mut restored_output, |_| {
        fs::read(&path).is_ok_and(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .is_ok_and(|value| value.as_array().is_some_and(|entries| entries.is_empty()))
        })
    });
}

#[test]
fn tui_wave_toggle_restores_visualization_after_no_wave_startup() {
    let (mut fixture, mut master) = spawn_player(&["--no-wave"]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(4).any(|w| w == b"tone")
    });
    master.write_all(b" ").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Local FLAC") && screen.contains("Paused") && pane_height(&screen).is_some()
    });
    let screen = screen_text(&output);
    assert!(!has_wave(&screen));
    assert_eq!(screen.matches("plavno").count(), 1);
    let collapsed = pane_height(&screen).unwrap();
    assert!(output.windows(6).any(|w| w == b"Visual"));
    master.write_all(b"v").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        has_wave(&screen) && pane_height(&screen).is_some_and(|height| height > collapsed)
    });
    let expanded = pane_height(&screen_text(&output)).unwrap();
    assert!(expanded > collapsed);
    click_label(&mut master, &mut output, "v Visual");
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        !has_wave(&screen)
            && pane_height(&screen).is_some_and(|height| height < expanded)
            && screen.contains("Wave visualization disabled")
    });
    assert_eq!(screen_text(&output).matches("plavno").count(), 1);
    let screen = screen_text(&output);
    let notice = screen
        .lines()
        .position(|line| line.contains("Wave visualization disabled"))
        .unwrap();
    let bottom = screen.lines().position(|line| line.contains('╰')).unwrap();
    let progress = screen
        .lines()
        .position(|line| line.contains(" / "))
        .unwrap();
    assert!(notice < progress);
    for label in ["x Stop", "v Visual", "q Quit"] {
        let row = screen
            .lines()
            .position(|line| line.contains(label))
            .unwrap();
        assert!(
            progress < row && row < bottom,
            "Control outside the artwork panel: {label}"
        );
    }
    let line = screen.lines().nth(notice).unwrap();
    assert!(line.contains("Paused"));
    assert!(line.trim_end().ends_with("Wave visualization disabled │"));
    let line = screen.lines().nth(notice + 1).unwrap();
    assert!(line.contains("Local FLAC"));
    assert!(line.contains("Volume: 100% +"));
    assert!(line.trim_end().ends_with("Speed: 1.00x ]  · Queued: 0 │"));
    assert!(fixture.child.try_wait().unwrap().is_none());
    assert!(!screen_text(&output).contains("Starting"));
}

#[test]
fn tui_inline_volume_buttons_work_without_a_slider_and_seek_hint_follows_stop() {
    let (_fixture, mut master) = spawn_player(&["--no-wave"]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("tone") && screen.contains("Volume: 100%")
    });
    master.write_all(b" ").unwrap();
    click_label(&mut master, &mut output, "- Volume");
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Volume: 95%")
    });
    click_label(&mut master, &mut output, "+  ·");
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Volume: 100%")
    });
    let screen = screen_text(&output);
    assert_eq!(screen.matches("Volume").count(), 1);
    let line = screen.lines().find(|line| line.contains("x Stop")).unwrap();
    assert!(line.find("←/→ Seek").unwrap() > line.find("x Stop").unwrap());
}

#[test]
fn tui_inline_speed_buttons_hotkeys_and_wheel_keep_the_rate_within_bounds() {
    let (_fixture, mut master) = spawn_player(&["--no-wave"]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("tone") && screen.contains("Speed: 1.00x")
    });
    master.write_all(b" ").unwrap();
    click_label(&mut master, &mut output, "[ Speed");
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Speed: 0.95x")
    });
    click_label(&mut master, &mut output, "]  · Queued");
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Speed: 1.00x")
    });
    master.write_all(b"[[[[[[[[[[[[[[[[[[[[").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Speed: 0.10x")
    });
    master
        .write_all(b"]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]")
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Speed: 2.00x")
    });
    click_label(&mut master, &mut output, "Speed:");
    let screen = screen_text(&output);
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("Speed:"))
        .unwrap();
    let column = unicode_width::UnicodeWidthStr::width(&line[..line.find("Speed:").unwrap()]) + 1;
    master
        .write_all(format!("\x1b[<65;{column};{}M", row + 1).as_bytes())
        .unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("Speed: 1.95x")
            && screen.contains("Volume: 100%")
            && screen.contains("Paused")
    });
}

#[test]
fn tui_keeps_rendering_after_focus_loss() {
    let (_fixture, mut master) = spawn_player(&[]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(4).any(|w| w == b"tone")
    });
    assert!(has_wave(&screen_text(&output)));
    master.write_all(b"\x1b[O-").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Volume: 95%")
    });
}

#[test]
fn tui_no_wave_keeps_controls_visible_and_renders_without_focus() {
    let (_fixture, mut master) = spawn_player(&["--no-wave"]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(4).any(|w| w == b"tone")
    });
    assert!(!has_wave(&screen_text(&output)));
    assert!(output.windows(6).any(|w| w == b"Volume"));
    master.write_all(b"\x1b[O-").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Volume: 95%")
    });
    assert!(!has_wave(&screen_text(&output)));
}

#[test]
fn tui_compact_layout_keeps_track_text_and_controls_after_resizing() {
    let (mut fixture, mut master) = spawn_player(&[]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(4).any(|w| w == b"tone")
    });
    let size = libc::winsize {
        ws_row: 16,
        ws_col: 48,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    output.clear();
    read_until(&mut master, &mut output, |bytes| {
        bytes.windows(4).any(|w| w == b"tone") && bytes.windows(6).any(|w| w == b"Volume")
    });
    assert!(
        output
            .windows("plavno · Local file".len())
            .any(|w| w == "plavno · Local file".as_bytes())
    );
    assert!(fixture.child.try_wait().unwrap().is_none());
    master.write_all(b"q").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        let mut bytes = [0; 8192];
        let _ = master.read(&mut bytes);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
