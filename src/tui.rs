use crate::{
    artwork::Artwork,
    backend::PlaybackMode,
    controls::{self, Controls},
    engine::{Command, Engine, Event},
    flow_view::{self, FlowView},
    history_view::{Action, HistoryView},
    mpris::Mpris,
    source_view::{self, SourceView},
    text_selection::{self, TextSelection},
    wave::{FRAME_INTERVAL, Wave},
};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, BorderType, Gauge, Paragraph, Widget, Wrap},
};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture
        );
        ratatui::restore();
    }
}

pub fn run(
    mut engine: Engine,
    events: std::sync::mpsc::Sender<Event>,
    receiver: std::sync::mpsc::Receiver<Event>,
    mpris: Option<Mpris>,
    wave_enabled: bool,
) -> Result<()> {
    let mut terminal = ratatui::try_init()?;
    let guard = TerminalGuard;
    execute!(
        std::io::stdout(),
        EnableMouseCapture,
        EnableFocusChange,
        EnableBracketedPaste
    )?;
    std::thread::Builder::new()
        .name("keys".into())
        .spawn(move || {
            loop {
                match event::read() {
                    Ok(event) => {
                        if events.send(Event::Terminal(event)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = events.send(Event::InputError);
                        break;
                    }
                }
            }
        })?;
    engine.start();
    let mut wave = wave_enabled.then(Wave::default);
    let mut artwork = Artwork::new()?;
    let mut dirty = true;
    let mut last_draw = Instant::now();
    let mut last_mpris = Instant::now();
    let mut controls = Controls::default();
    let mut selection = TextSelection::default();
    let mut history: Option<HistoryView> = None;
    let mut flow: Option<FlowView> = None;
    let mut sources = (!engine.services.iter().any(|s| s.available))
        .then(|| SourceView::new(engine.services.clone(), engine.source));
    loop {
        dirty |= artwork.update(
            wave.as_ref()
                .filter(|_| history.is_none() && sources.is_none() && flow.is_none())
                .and_then(|_| engine.media_track())
                .and_then(|track| track.cover_url.as_deref()),
        );
        let frame_interval = if wave.is_some() {
            FRAME_INTERVAL
        } else {
            Duration::from_millis(250)
        };
        if dirty {
            if history.is_none()
                && sources.is_none()
                && flow.is_none()
                && let Some(wave) = &mut wave
            {
                wave.update(&engine);
            }
            terminal.draw(|frame| {
                if let Some(flow) = &mut flow {
                    flow.draw(frame);
                    controls.reset();
                    selection = TextSelection::default();
                } else if let Some(sources) = &mut sources {
                    sources.draw(frame);
                    controls.reset();
                    selection = TextSelection::default();
                } else if let Some(history) = &mut history {
                    history.draw(frame);
                    controls.reset();
                    selection = TextSelection::default();
                } else {
                    draw(
                        frame,
                        &engine,
                        wave.as_ref(),
                        &mut artwork,
                        &mut controls,
                        &mut selection,
                    );
                }
            })?;
            last_draw = Instant::now();
            dirty = false;
        }
        let updating = history.is_none()
            && sources.is_none()
            && flow.is_none()
            && engine.track().is_some()
            && !engine.paused;
        let wait = if updating {
            frame_interval.saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_millis(250)
        };
        match receiver.recv_timeout(wait) {
            Ok(Event::Terminal(event::Event::FocusLost)) => {
                controls.cancel_drag();
                selection.cancel_drag();
                if let Some(flow) = &mut flow {
                    flow.cancel_press();
                }
            }
            Ok(Event::Terminal(event::Event::FocusGained)) => {
                dirty = true;
            }
            Ok(Event::Terminal(event)) if flow.is_some() => {
                if matches!(&event, event::Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                match flow.as_mut().unwrap().handle(event) {
                    Some(flow_view::Action::Close) => flow = None,
                    Some(flow_view::Action::Apply(style)) => {
                        engine.set_wave_style(style);
                        flow = None;
                    }
                    None => {}
                }
                dirty = true;
            }
            Ok(Event::Terminal(event)) if sources.is_some() => {
                let redraw = !matches!(&event,event::Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved);
                if matches!(&event, event::Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                match sources.as_mut().unwrap().handle(event) {
                    Some(source_view::Action::Close) => sources = None,
                    Some(source_view::Action::Select(key)) => {
                        engine.start_source(key);
                        if engine.services.iter().any(|s| s.key == key && s.available) {
                            sources = None;
                        }
                    }
                    None => {}
                }
                dirty |= redraw;
            }
            Ok(Event::Terminal(event)) if history.is_some() => {
                let redraw = !matches!(&event,event::Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved);
                if matches!(&event, event::Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                match history.as_mut().unwrap().handle(event) {
                    Some(Action::Close) => history = None,
                    Some(Action::Play(entry)) => {
                        engine.play_history(&entry);
                        history = None;
                    }
                    None => {}
                }
                dirty |= redraw;
            }
            Ok(Event::Terminal(event)) => {
                let mut redraw = true;
                let event = match event {
                    event::Event::Mouse(mouse) => {
                        let previous = selection.range();
                        if selection.handle(mouse) {
                            controls.cancel_drag();
                            None
                        } else {
                            match controls.handle(mouse, engine.track().and_then(|t| t.duration)) {
                                Some(controls::Action::Key(key)) => {
                                    Some(event::Event::Key(KeyEvent::new(key, KeyModifiers::NONE)))
                                }
                                Some(controls::Action::Seek(position)) => {
                                    engine.seek_to(position);
                                    None
                                }
                                Some(controls::Action::VolumeDelta(delta)) => {
                                    engine.change_volume(delta);
                                    None
                                }
                                Some(controls::Action::Redraw) => None,
                                None => {
                                    redraw = previous != selection.range();
                                    None
                                }
                            }
                        }
                    }
                    event::Event::Resize(_, _) => {
                        controls.reset();
                        selection = TextSelection::default();
                        Some(event)
                    }
                    other => Some(other),
                };
                if let Some(event) = event {
                    match event {
                        event::Event::Key(key)
                            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            if key.kind == KeyEventKind::Repeat
                                && !matches!(
                                    key.code,
                                    KeyCode::Left
                                        | KeyCode::Right
                                        | KeyCode::PageUp
                                        | KeyCode::PageDown
                                )
                            {
                                continue;
                            }
                            if key.code == KeyCode::Char('q')
                                || (key.modifiers.contains(KeyModifiers::CONTROL)
                                    && key.code == KeyCode::Char('c'))
                            {
                                if key.code == KeyCode::Char('c')
                                    && let Some(text) = selection.selected_text()
                                {
                                    text_selection::copy_to_clipboard(std::io::stdout(), text)?;
                                    engine.message = "Selection sent to terminal clipboard".into();
                                    dirty = true;
                                    continue;
                                }
                                break;
                            }
                            match key.code {
                                KeyCode::Char('s') => {
                                    sources = Some(SourceView::new(
                                        engine.services.clone(),
                                        engine.source,
                                    ))
                                }
                                KeyCode::Char('m') => engine.start_source("mixed"),
                                KeyCode::Char('g') => {
                                    if engine.can_tune_wave() {
                                        flow = Some(FlowView::new(
                                            engine.source,
                                            engine.name,
                                            engine.wave_style(),
                                        ));
                                    } else {
                                        engine.message =
                                            "Select Yandex Music or VK Music to tune its wave"
                                                .into();
                                    }
                                }
                                KeyCode::Char('h' | '/') => {
                                    history = Some(HistoryView::new(
                                        engine
                                            .history
                                            .iter()
                                            .rev()
                                            .filter(|entry| {
                                                engine
                                                    .services
                                                    .iter()
                                                    .any(|s| s.key == entry.source && s.available)
                                            })
                                            .cloned()
                                            .collect(),
                                    ));
                                }
                                KeyCode::Char(' ') => {
                                    engine.handle(Event::Control(Command::PlayPause))
                                }
                                KeyCode::Char('x') => engine.handle(Event::Control(Command::Stop)),
                                KeyCode::Esc => {
                                    controls.cancel_drag();
                                    selection.clear();
                                }
                                KeyCode::Char('n') => engine.next(),
                                KeyCode::Char('p') => engine.previous(),
                                KeyCode::Left | KeyCode::Right => {
                                    let seconds = if key
                                        .modifiers
                                        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
                                    {
                                        30.0
                                    } else {
                                        5.0
                                    };
                                    engine.handle(Event::Control(Command::Seek(
                                        if key.code == KeyCode::Left {
                                            -seconds
                                        } else {
                                            seconds
                                        },
                                    )));
                                }
                                KeyCode::PageUp => {
                                    engine.handle(Event::Control(Command::Seek(-30.0)))
                                }
                                KeyCode::PageDown => {
                                    engine.handle(Event::Control(Command::Seek(30.0)))
                                }
                                KeyCode::Home => engine.seek_to(0.0),
                                KeyCode::End => {
                                    if let Some(duration) = engine.track().and_then(|t| t.duration)
                                    {
                                        engine.seek_to(duration);
                                    }
                                }
                                KeyCode::Char('l') => engine.like(),
                                KeyCode::Char('d') => engine.dislike(),
                                KeyCode::Char('u') => engine.undo_dislike(),
                                KeyCode::Char('v') => {
                                    wave = if wave.is_some() {
                                        None
                                    } else {
                                        Some(Wave::default())
                                    };
                                    controls.reset();
                                    engine.message = if wave.is_some() {
                                        "Wave visualization enabled"
                                    } else {
                                        "Wave visualization disabled"
                                    }
                                    .into();
                                }
                                KeyCode::Char('w') => engine.start_mode(PlaybackMode::MyVibe),
                                KeyCode::Char('f') => {
                                    engine.start_mode(PlaybackMode::FavoritesShuffle)
                                }
                                KeyCode::Char('+') | KeyCode::Char('=') => {
                                    engine.change_volume(0.05)
                                }
                                KeyCode::Char('-') => engine.change_volume(-0.05),
                                _ => continue,
                            }
                        }
                        event::Event::Resize(_, _) => {}
                        _ => continue,
                    }
                }
                dirty |= redraw;
            }
            Ok(Event::InputError) => anyhow::bail!("Failed to read keyboard input"),
            Ok(Event::Control(Command::Quit)) => break,
            Ok(event) => {
                engine.handle(event);
                dirty = true;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        dirty |= engine.tick();
        if (dirty || last_mpris.elapsed() >= Duration::from_millis(250))
            && let Some(mpris) = &mpris
        {
            mpris.update(&engine);
            last_mpris = Instant::now();
        }
        if history.is_none()
            && sources.is_none()
            && flow.is_none()
            && engine.track().is_some()
            && !engine.paused
            && last_draw.elapsed() >= frame_interval
        {
            dirty = true;
        }
    }
    let done = engine.shutdown();
    drop(mpris);
    drop(guard);
    if done.recv_timeout(Duration::from_secs(2)).is_err() {
        eprintln!("plavno: final playback feedback was not confirmed; stopping the wait");
    }
    Ok(())
}

fn draw(
    frame: &mut Frame,
    engine: &Engine,
    wave: Option<&Wave>,
    artwork: &mut Artwork,
    controls: &mut Controls,
    selection: &mut TextSelection,
) {
    controls.sync_track(engine);
    controls.begin_draw();
    let compact = frame.area().height < 20;
    let width = frame.area().width.saturating_sub(6);
    let favorites = engine.favorites_supported || engine.source == "mixed";
    let quality = engine.quality().unwrap_or_default();
    let volume = format!("Volume: {:.0}%", engine.volume * 100.0);
    let queue = format!(" · Queued: {}", engine.queue_len());
    let state = if engine.loading {
        "Loading"
    } else if engine.playback_status() == "Stopped" {
        "Stopped"
    } else if engine.paused {
        "Paused"
    } else if engine.buffering() {
        "Buffering"
    } else if engine.track().is_some() {
        "Playing"
    } else {
        "Stopped"
    };
    let notice_width = width.saturating_sub(quality.width() as u16 + 2);
    let notice_rows = if compact
        || !engine.message.contains('\n') && engine.message.width() <= usize::from(notice_width)
    {
        1
    } else {
        2
    };
    let detail_rows = if favorites { 3u16 } else { 2 } + notice_rows - 1;
    let progress_rows = if compact { 1 } else { 3 };
    let control_rows = Controls::height(frame.area().width.saturating_sub(2), engine);
    let track_text = engine.media_track().map(|track| {
        let artist = if !track.source.is_empty() && track.source != engine.source {
            format!("{} · {}", track.artist, track.source)
        } else {
            track.artist.clone()
        };
        let text = if artist.is_empty() {
            track.title.clone()
        } else {
            format!("{}\n{artist}", track.title)
        };
        (track.cache_key(), text)
    });
    let identity_min = track_text
        .as_ref()
        .map_or(0, |(_, text)| if text.contains('\n') { 2 } else { 1 });
    let identity_limit = frame
        .area()
        .height
        .saturating_sub(4 + detail_rows + progress_rows + control_rows);
    let identity_rows = if wave.is_none() {
        track_text.as_ref().map_or(0, |(key, text)| {
            selection.height(key, text, width, identity_limit)
        })
    } else {
        0
    };
    let rows = Layout::vertical([
        if wave.is_some() {
            Constraint::Min(0)
        } else {
            Constraint::Length(2 + identity_rows + detail_rows)
        },
        Constraint::Length(progress_rows),
        Constraint::Length(control_rows),
    ])
    .flex(Flex::Start)
    .margin(1)
    .split(frame.area());
    let accent = Style::default().fg(Color::Cyan);
    let mut service = if engine.favorites_supported && engine.source != "mixed" {
        format!(
            "{} · {}",
            engine.name,
            if engine.mode == PlaybackMode::FavoritesShuffle && engine.source == "vk" {
                "My Music · Shuffle"
            } else if engine.mode == PlaybackMode::MyVibe && engine.source != "yandex" {
                "Wave"
            } else {
                engine.mode.label()
            }
        )
    } else {
        engine.name.to_owned()
    };
    if engine.can_tune_wave() && engine.mode == PlaybackMode::MyVibe {
        service.push_str(&format!(" · {}", engine.wave_style().label()));
    }
    let pane = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(format!(" plavno · {service} ")).style(accent));
    let inner = pane.inner(rows[0]);
    frame.render_widget(pane, rows[0]);
    if let Some(wave) = wave {
        artwork.render(inner, frame.buffer_mut());
        frame.render_widget(wave, inner);
    }
    let content = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let details = content;
    let footer_rows = detail_rows.min(details.height.saturating_sub(identity_min));
    let notice_rows = notice_rows.min(footer_rows.saturating_sub(1).max(1));
    let identity = Rect::new(
        content.x,
        content.y,
        content.width,
        details.height.saturating_sub(footer_rows),
    );
    if let Some((key, text)) = track_text {
        selection.render(&key, &text, identity, frame.buffer_mut());
    } else {
        *selection = TextSelection::default();
    }
    let style = Style::default().fg(Color::Gray);
    if footer_rows > 0 {
        let status = if selection.range().is_some() {
            format!("{state} · Ctrl+C copy · Esc clear")
        } else {
            state.to_owned()
        };
        let volume_width =
            (volume.width() + queue.width() + 6).min(usize::from(content.width)) as u16;
        let status_width = status
            .width()
            .min(usize::from(content.width.saturating_sub(volume_width + 2)))
            as u16;
        let row = details.bottom() - footer_rows;
        frame.render_widget(
            Line::from(status).style(accent),
            Rect::new(content.x, row, status_width, 1),
        );
        controls.draw_volume(
            frame,
            Rect::new(content.right() - volume_width, row, volume_width, 1),
            &volume,
            &queue,
        );
    }
    if favorites && footer_rows >= 2 + notice_rows {
        let line = if engine.can_like() {
            Line::from(if engine.is_liked() {
                "♥ In favorites · l to remove"
            } else if engine.favorite_state().is_some() {
                "♡ l to add to favorites"
            } else {
                "Favorites: ? · l to check and toggle"
            })
            .style(accent)
        } else {
            Line::default()
        };
        let width = line.width().min(usize::from(content.width)) as u16;
        frame.render_widget(
            line,
            Rect::new(content.x, details.bottom() - notice_rows - 1, width, 1),
        );
    }
    if footer_rows >= 2 {
        let quality_width = quality.width().min(usize::from(content.width)) as u16;
        let row = details.bottom() - notice_rows;
        frame.render_widget(
            Line::from(quality).style(style),
            Rect::new(content.x, row, quality_width, 1),
        );
        let width = content.width.saturating_sub(quality_width + 2);
        overlay_notice(
            engine.message.as_str(),
            Rect::new(content.right() - width, row, width, notice_rows),
            frame.buffer_mut(),
        );
    }
    let seconds = controls.position(engine.seconds());
    let duration = engine.track().and_then(|track| track.duration);
    let ratio = duration
        .filter(|d| *d > 0.0)
        .map_or(0.0, |d| (seconds / d).clamp(0.0, 1.0));
    let seek_area = if compact {
        rows[1]
    } else {
        Block::bordered().inner(rows[1])
    };
    let progress = Gauge::default()
        .gauge_style(accent)
        .ratio(ratio)
        .label(format!(
            "{} / {}",
            clock(seconds),
            duration.map_or("—:—".into(), clock)
        ));
    frame.render_widget(
        if compact {
            progress
        } else {
            progress.block(Block::bordered())
        },
        rows[1],
    );
    controls.seek = seek_area;
    controls.draw(frame, rows[2], engine, wave.is_some());
}

fn overlay_notice(text: &str, area: Rect, buffer: &mut Buffer) {
    if text.is_empty() || area.is_empty() {
        return;
    }
    let mut image = Buffer::empty(Rect::new(0, 0, area.width, area.height));
    Paragraph::new(text)
        .wrap(Wrap { trim: true })
        .alignment(ratatui::layout::Alignment::Right)
        .render(image.area, &mut image);
    for y in 0..area.height {
        if let Some(first) = (0..area.width).find(|&x| !image[(x, y)].symbol().trim().is_empty()) {
            for x in first..area.width {
                buffer[(area.x + x, area.y + y)]
                    .set_symbol(image[(x, y)].symbol())
                    .set_fg(Color::Yellow);
            }
        }
    }
}

fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_wrap_inside_the_panel_without_erasing_its_background() {
        let area = Rect::new(2, 3, 12, 2);
        let mut buffer = Buffer::empty(area);
        for cell in &mut buffer.content {
            cell.set_char('▀')
                .set_fg(Color::Magenta)
                .set_bg(Color::Rgb(12, 24, 36));
        }
        overlay_notice("Like failed: offline", area, &mut buffer);
        let first: String = (0..area.width)
            .map(|x| buffer[(area.x + x, area.y)].symbol())
            .collect();
        assert_eq!(first, "Like failed:");
        assert_eq!(buffer[(7, 4)].symbol(), "o");
        assert_eq!(buffer[(7, 4)].fg, Color::Yellow);
        assert_eq!(buffer[(2, 4)].symbol(), "▀");
        assert_eq!(buffer[(2, 4)].fg, Color::Magenta);
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| cell.bg == Color::Rgb(12, 24, 36))
        );
    }
}
