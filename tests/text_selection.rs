#![cfg(any(target_os = "linux", all(target_os = "macos", feature = "test-audio")))]

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
    // A PTY read may end inside a synchronized repaint, after the old screen
    // was cleared. Evaluate the screen only after that frame has finished.
    let unfinished_frame = |bytes: &[u8]| {
        let begin = bytes.windows(8).rposition(|chunk| chunk == b"\x1b[?2026h");
        let end = bytes.windows(8).rposition(|chunk| chunk == b"\x1b[?2026l");
        begin.is_some_and(|begin| end.is_none_or(|end| end < begin))
    };
    while unfinished_frame(output) || !done(output) {
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

#[path = "support/terminal.rs"]
mod terminal;

fn screen_text(bytes: &[u8]) -> String {
    terminal::screen_text(bytes, 80, 48)
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

fn label_position(bytes: &[u8], label: &str) -> Option<(usize, usize)> {
    let screen = screen_text(bytes);
    // A notification can mention Undo before its button has been repainted.
    let first_row = if label == "u Undo" {
        screen.lines().position(|line| line.contains("q Quit"))?
    } else {
        0
    };
    screen
        .lines()
        .enumerate()
        .filter(|(row, _)| *row >= first_row)
        .filter_map(|(row, line)| {
            let index = line.find(label)?;
            Some((
                row,
                unicode_width::UnicodeWidthStr::width(&line[..index]) + 1,
            ))
        })
        .last()
}

#[test]
fn mouse_target_prefers_the_control_below_a_notification_with_the_same_label() {
    let output = b"\x1b[16;1HDisliked: tone.flac - u Undo\x1b[22;1H q Quit   u Undo ";
    assert_eq!(label_position(output, "u Undo"), Some((21, 11)));
}

#[test]
fn mouse_target_waits_for_the_control_when_a_notification_is_repainted_first() {
    let output = b"\x1b[16;1HDisliked: tone.flac - u Undo\x1b[22;1H q Quit ";
    assert_eq!(label_position(output, "u Undo"), None);
}

fn click_label(master: &mut File, output: &mut Vec<u8>, label: &str) {
    read_until(master, output, |bytes| {
        label_position(bytes, label).is_some()
    });
    let (row, column) = label_position(output, label).unwrap();
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
    #[cfg(target_os = "linux")]
    let alsa = root.join("alsa.conf");
    #[cfg(target_os = "linux")]
    fs::write(&alsa, "pcm.!default { type null }\n").unwrap();
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let rows = if args.contains(&"--dance") || args.contains(&"--dance-style") {
        40
    } else {
        24
    };
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: 80,
        ws_xpixel: if args.contains(&"sixel") { 640 } else { 0 },
        ws_ypixel: if args.contains(&"sixel") {
            rows * 16
        } else {
            0
        },
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
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
        .env("TERM", "xterm-256color")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    #[cfg(target_os = "linux")]
    command.env("ALSA_CONFIG_PATH", &alsa);
    #[cfg(feature = "test-audio")]
    command.env("PLAVNO_TEST_AUDIO", "1");
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
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
fn tui_dancer_switches_with_mouse_remembers_its_mode_and_preserves_track_controls() {
    let (_fixture, mut master) = spawn_player(&["--dance-style", "flow", "--renderer", "braille"]);
    let mut output = Vec::new();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        screen.contains("tone") && has_wave(&screen) && screen.contains("b Dance")
    });
    let screen = screen_text(&output);
    assert!(has_wave(&screen));
    assert!(
        screen.lines().take(21).any(has_wave),
        "Wave disappeared in dance mode"
    );
    // Capture a developed waveform, rather than the flat startup frame.
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes)
            .lines()
            .take(21)
            .filter(|line| has_wave(line))
            .count()
            >= 3
    });
    master.write_all(b" ").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Paused")
    });
    let dots = |bytes: &[u8]| {
        screen_text(bytes)
            .lines()
            .enumerate()
            .flat_map(|(y, line)| {
                line.chars().enumerate().filter_map(move |(x, ch)| {
                    ('\u{2800}'..='\u{28ff}')
                        .contains(&ch)
                        .then_some((x, y, ch))
                })
            })
            .collect::<Vec<_>>()
    };
    let frozen = dots(&output);
    click_label(&mut master, &mut output, "b Dance");
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        has_wave(&screen) && screen.contains("Wave visualization enabled")
    });
    master.write_all(b"b").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        has_wave(&screen) && screen.contains("Dance visualization enabled")
    });
    assert_eq!(
        dots(&output),
        frozen,
        "Switching modes reset the paused animation"
    );
    click_label(&mut master, &mut output, "v Visual");
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        !has_wave(&screen) && screen.contains("Dance visualization disabled")
    });
    master.write_all(b"v").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        let screen = screen_text(bytes);
        has_wave(&screen)
            && screen.contains("Dance visualization enabled")
            && screen.contains("Paused")
    });
    assert_eq!(
        dots(&output),
        frozen,
        "Hiding visualization reset the paused animation"
    );
    master.write_all(b"\x1b[O-").unwrap();
    read_until(&mut master, &mut output, |bytes| {
        screen_text(bytes).contains("Volume: 95%")
    });
    let screen = screen_text(&output);
    for label in ["tone", "x Stop", "←/→ Seek", "Speed: 1.00x", "q Quit"] {
        assert!(screen.contains(label), "Dancer covered {label}");
    }
}

#[test]
fn tui_sixel_update_modes_preserve_menus_controls_and_resize() {
    for (update, skin) in [
        ("full", "detailed"),
        ("diff", "detailed"),
        ("diff", "silhouette"),
    ] {
        let (_fixture, mut master) = spawn_player(&[
            "--dance-style",
            "flow",
            "--renderer",
            "sixel",
            "--sixel-update",
            update,
            "--dance-skin",
            skin,
        ]);
        let mut output = Vec::new();
        let sixel = b"\x1bP7;1;0q";
        let last_frame = |bytes: &[u8]| {
            bytes
                .windows(8)
                .rposition(|chunk| chunk == b"\x1b[?2026h")
                .unwrap_or(0)
        };
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("tone")
                && bytes.windows(sixel.len()).any(|chunk| chunk == sixel)
                && bytes.windows(8).any(|chunk| chunk == b"\x1b[?2026l")
        });
        assert!(output.windows(8).any(|chunk| chunk == b"\x1b[?2026h"));
        let mut starts: Vec<_> = output
            .windows(8)
            .enumerate()
            .filter_map(|(index, chunk)| (chunk == b"\x1b[?2026h").then_some(index))
            .collect();
        starts.push(output.len());
        for range in starts.windows(2) {
            let frame = &output[range[0]..range[1]];
            assert!(
                frame.windows(6).filter(|chunk| *chunk == b"plavno").count() <= 1,
                "Sixel frame prints the title more than once"
            );
        }
        let screen = screen_text(&output);
        for label in ["tone", "x Stop", "←/→ Seek", "Speed: 1.00x", "q Quit"] {
            assert!(screen.contains(label), "Missing {label}");
        }
        assert!(has_wave(&screen));
        master.write_all(b" ").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("Paused")
        });
        assert_eq!(
            output[last_frame(&output)..]
                .windows(4)
                .any(|chunk| chunk == b"\x1b[2J"),
            update == "full",
            "Unexpected screen clear in {update} mode"
        );

        master.write_all(b"b").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("Wave visualization enabled")
        });
        assert!(
            !output[last_frame(&output)..]
                .windows(sixel.len())
                .any(|chunk| chunk == sixel)
        );
        master.write_all(b"b").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("Dance visualization enabled")
                && bytes[last_frame(bytes)..]
                    .windows(sixel.len())
                    .any(|chunk| chunk == sixel)
        });
        master.write_all(b"s").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains(concat!("plavno ", env!("CARGO_PKG_VERSION"), " · Sources"))
        });
        assert!(
            !output[last_frame(&output)..]
                .windows(sixel.len())
                .any(|chunk| chunk == sixel)
        );
        master.write_all(b"\x1b").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("tone")
                && bytes[last_frame(bytes)..]
                    .windows(sixel.len())
                    .any(|chunk| chunk == sixel)
        });
        master.write_all(b"h").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("History ·")
        });
        assert!(
            !output[last_frame(&output)..]
                .windows(sixel.len())
                .any(|chunk| chunk == sixel)
        );
        master.write_all(b"\x1b").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("tone")
                && bytes[last_frame(bytes)..]
                    .windows(sixel.len())
                    .any(|chunk| chunk == sixel)
        });

        let size = libc::winsize {
            ws_row: 32,
            ws_col: 72,
            ws_xpixel: 576,
            ws_ypixel: 512,
        };
        assert_eq!(
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
        read_until(&mut master, &mut output, |bytes| {
            let frame = &bytes[last_frame(bytes)..];
            let screen = screen_text(bytes);
            screen
                .lines()
                .position(|line| line.contains('╰'))
                .is_some_and(|line| line < 32)
                && frame.windows(sixel.len()).any(|chunk| chunk == sixel)
        });
        master.write_all(b"v").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            screen_text(bytes).contains("Dance visualization disabled")
        });
        assert!(
            !output[last_frame(&output)..]
                .windows(sixel.len())
                .any(|chunk| chunk == sixel)
        );
        master.write_all(b"q").unwrap();
        read_until(&mut master, &mut output, |bytes| {
            bytes.windows(8).any(|chunk| chunk == b"\x1b[?1049l")
        });
    }
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
    assert!(screen_text(&output).contains(concat!(
        "plavno ",
        env!("CARGO_PKG_VERSION"),
        " · Local file"
    )));
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
