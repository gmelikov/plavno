use crate::{
    artwork::Artwork,
    backend::PlaybackMode,
    cached_frame::CachedFrame,
    controls::{self, Controls},
    engine::{Command, Engine, Event},
    flow_view::{self, FlowView},
    history_view::{Action, HistoryView},
    mpris::Mpris,
    source_view::{self, SourceView},
    text_selection::{self, TextSelection},
    visualization::{Mode, Options, Renderer, Visualization},
    wave::FRAME_INTERVAL,
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
    widgets::{Block, BorderType, Paragraph, Widget, Wrap},
};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            crossterm::terminal::EndSynchronizedUpdate,
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture
        );
        ratatui::restore();
    }
}

enum View {
    Player,
    History(HistoryView),
    Sources(SourceView),
    Flow(FlowView),
}

impl View {
    fn is_player(&self) -> bool {
        matches!(self, Self::Player)
    }

    fn wants_frames(&self, engine: &Engine, visual: Option<&Visualization>) -> bool {
        self.is_player()
            && (engine.track().is_some() && (!engine.paused || engine.is_scrubbing())
                || visual.is_some_and(|visual| visual.wants_frames(engine)))
    }

    fn draw(&mut self, frame: &mut Frame) -> bool {
        match self {
            Self::Player => return false,
            Self::History(history) => history.draw(frame),
            Self::Sources(sources) => sources.draw(frame),
            Self::Flow(flow) => flow.draw(frame),
        }
        true
    }

    fn handle(&mut self, event: event::Event, engine: &mut Engine) -> bool {
        let redraw = matches!(self, Self::Flow(_))
            || !matches!(&event, event::Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved);
        let close = match self {
            Self::Player => return false,
            Self::History(history) => match history.handle(event) {
                Some(Action::Close) => true,
                Some(Action::Play(entry)) => {
                    engine.play_history(&entry);
                    true
                }
                None => false,
            },
            Self::Sources(sources) => match sources.handle(event) {
                Some(source_view::Action::Close) => true,
                Some(source_view::Action::Select(key)) => {
                    engine.start_source(key);
                    engine
                        .services
                        .iter()
                        .any(|service| service.key == key && service.available)
                }
                None => false,
            },
            Self::Flow(flow) => match flow.handle(event) {
                Some(flow_view::Action::Close) => true,
                Some(flow_view::Action::Apply(style)) => {
                    engine.set_wave_style(style);
                    true
                }
                None => false,
            },
        };
        if close {
            *self = Self::Player;
        }
        redraw
    }
}

pub fn run(
    mut engine: Engine,
    events: std::sync::mpsc::Sender<Event>,
    receiver: std::sync::mpsc::Receiver<Event>,
    mpris: Option<Mpris>,
    wave_enabled: bool,
    options: Options,
) -> Result<()> {
    let mut cell_size = if options.renderer == Renderer::Sixel {
        Some(crate::sixel::CellSize::read()?)
    } else {
        None
    };
    let mut sixel = crate::sixel::Presenter::new(options.sixel_update);
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
    let mut visual = Visualization::new(options);
    let mut visual_enabled = wave_enabled;
    let mut artwork = Artwork::new()?;
    let mut scene: Option<Scene> = None;
    let mut dirty = true;
    let mut last_draw = Instant::now();
    let mut last_mpris = Instant::now();
    let mut controls = Controls::default();
    let mut selection = TextSelection::default();
    let mut view = if engine.services.iter().any(|service| service.available) {
        View::Player
    } else {
        View::Sources(SourceView::new(engine.services.clone(), engine.source))
    };
    loop {
        let artwork_changed = artwork.update(if visual_enabled && view.is_player() {
            engine
                .media_track()
                .and_then(|track| track.cover_url.as_deref())
        } else {
            None
        });
        dirty |= artwork_changed;
        if artwork_changed {
            scene = None;
        }
        let frame_interval = if visual_enabled {
            FRAME_INTERVAL
        } else {
            Duration::from_millis(250)
        };
        if dirty {
            if view.is_player() && visual_enabled {
                let columns = terminal.size()?.width;
                visual.update(
                    &engine,
                    cell_size.map_or(columns, |cell| cell.dance_columns(columns)),
                );
            }
            let visible_visual = visual_enabled.then_some(&visual);
            let graphics = cell_size.is_some()
                && view.is_player()
                && visual_enabled
                && visual.mode == Mode::Dance;
            let full_redraw = sixel.full_redraw();
            let mut sync = if graphics || sixel.active() {
                Some(crate::sixel::FrameGuard::begin(full_redraw)?)
            } else {
                None
            };
            if sync.is_some() && full_redraw {
                // The graphics clear invalidated the screen. Discard the previous
                // text buffer so Ratatui paints it once, together with normal updates.
                terminal.swap_buffers();
            }
            let mut dance_area = None;
            let completed = terminal.draw(|frame| {
                if view.draw(frame) {
                    controls.reset();
                    selection = TextSelection::default();
                } else {
                    dance_area = draw(
                        frame,
                        &engine,
                        visible_visual,
                        &mut artwork,
                        &mut controls,
                        &mut selection,
                        &mut scene,
                    );
                }
            })?;
            let image = dance_area.and_then(|area| {
                visible_visual
                    .zip(cell_size)
                    .and_then(|(visual, cell)| visual.sixel(area, cell, completed.buffer))
            });
            if let Some(sync) = &mut sync {
                sync.present(&mut sixel, image, completed.buffer, cell_size.unwrap())?;
            }
            drop(sync);
            last_draw = Instant::now();
            dirty = false;
        }
        let updating = view.wants_frames(&engine, visual_enabled.then_some(&visual));
        let wait = if updating {
            frame_interval.saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_millis(250)
        };
        let wait = if engine.is_scrubbing() {
            wait.min(Duration::from_millis(10))
        } else {
            wait
        };
        match receiver.recv_timeout(wait) {
            Ok(Event::Terminal(event::Event::FocusLost)) => {
                controls.cancel_drag();
                selection.cancel_drag();
                dirty = true;
                if let View::Flow(flow) = &mut view {
                    flow.cancel_press();
                }
            }
            Ok(Event::Terminal(event::Event::FocusGained)) => {
                dirty = true;
            }
            Ok(Event::Terminal(event::Event::Resize(_, _))) => {
                if cell_size.is_some() {
                    cell_size = Some(crate::sixel::CellSize::read()?);
                    sixel.invalidate();
                }
                controls.reset();
                selection.cancel_drag();
                dirty = true;
                if let View::Flow(flow) = &mut view {
                    flow.cancel_press();
                }
            }
            Ok(Event::Terminal(event)) if !view.is_player() => {
                if matches!(&event, event::Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                dirty |= view.handle(event, &mut engine);
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
                                Some(controls::Action::Scrub { position, speed }) => {
                                    engine.scrub_to(position, speed);
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
                    other => Some(other),
                };
                if let Some(event) = event {
                    match event {
                        event::Event::Key(key)
                            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            if controls.is_seeking() {
                                controls.cancel_drag();
                                engine.end_scrub();
                            }
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
                                    scene = None;
                                    continue;
                                }
                                break;
                            }
                            match key.code {
                                KeyCode::Char('s') => {
                                    view = View::Sources(SourceView::new(
                                        engine.services.clone(),
                                        engine.source,
                                    ))
                                }
                                KeyCode::Char('m') => engine.start_source("mixed"),
                                KeyCode::Char('g') => {
                                    if engine.can_tune_wave() {
                                        view = View::Flow(FlowView::new(
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
                                    view = View::History(HistoryView::new(
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
                                    visual_enabled = !visual_enabled;
                                    controls.reset();
                                    engine.message = format!(
                                        "{} visualization {}",
                                        visual.mode.label(),
                                        if visual_enabled {
                                            "enabled"
                                        } else {
                                            "disabled"
                                        }
                                    );
                                }
                                KeyCode::Char('b') => {
                                    visual.mode = visual.mode.toggle();
                                    visual_enabled = true;
                                    controls.reset();
                                    engine.message =
                                        format!("{} visualization enabled", visual.mode.label());
                                }
                                KeyCode::Char('w') => engine.start_mode(PlaybackMode::MyVibe),
                                KeyCode::Char('f') => {
                                    engine.start_mode(PlaybackMode::FavoritesShuffle)
                                }
                                KeyCode::Char('+') | KeyCode::Char('=') => {
                                    engine.change_volume(0.05)
                                }
                                KeyCode::Char('-') => engine.change_volume(-0.05),
                                KeyCode::Char('[') => engine.change_speed(-0.05),
                                KeyCode::Char(']') => engine.change_speed(0.05),
                                _ => continue,
                            }
                        }
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
        if !controls.is_seeking() {
            engine.end_scrub();
        }
        dirty |= engine.tick();
        // UI/backend changes rebuild static content; the animation timer below
        // only schedules a new composite of the cached layers.
        if dirty {
            scene = None;
        }
        if (dirty || last_mpris.elapsed() >= Duration::from_millis(250))
            && let Some(mpris) = &mpris
        {
            mpris.update(&engine);
            last_mpris = Instant::now();
        }
        if view.wants_frames(&engine, visual_enabled.then_some(&visual))
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

struct Scene {
    frame: CachedFrame,
    state: &'static str,
    wave: Rect,
    dance: Rect,
    progress: Rect,
    duration: Option<f64>,
    seconds: Option<u64>,
    progress_label: String,
}

fn playback_state(engine: &Engine) -> &'static str {
    if engine.is_scrubbing() {
        "Scrubbing"
    } else if engine.loading {
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
    }
}

fn draw(
    frame: &mut Frame,
    engine: &Engine,
    visual: Option<&Visualization>,
    artwork: &mut Artwork,
    controls: &mut Controls,
    selection: &mut TextSelection,
    scene: &mut Option<Scene>,
) -> Option<Rect> {
    let state = playback_state(engine);
    if scene
        .as_ref()
        .is_none_or(|scene| scene.frame.area() != frame.area() || scene.state != state)
    {
        *scene = Some(build_scene(
            frame, engine, visual, artwork, controls, selection, state,
        ));
        frame.buffer_mut().reset();
    }
    let scene = scene.as_mut().unwrap();
    scene.frame.background(frame.buffer_mut());
    if let Some(visual) = visual {
        visual.render(scene.wave, scene.dance, frame.buffer_mut());
    }
    controls.highlight_transport(frame.buffer_mut());
    scene.frame.foreground(frame.buffer_mut());
    let seconds = controls.position(engine.seconds());
    let whole_seconds = seconds.max(0.0) as u64;
    if scene.seconds != Some(whole_seconds) {
        scene.seconds = Some(whole_seconds);
        scene.progress_label = format!(
            "{} / {}",
            clock(seconds),
            scene.duration.map_or("—:—".into(), clock)
        );
    }
    let ratio = scene
        .duration
        .filter(|d| *d > 0.0)
        .map_or(0.0, |d| (seconds / d).clamp(0.0, 1.0));
    overlay_progress(
        ratio,
        &scene.progress_label,
        scene.progress,
        frame.buffer_mut(),
    );
    visual
        .filter(|visual| visual.mode == Mode::Dance && visual.renderer == Renderer::Sixel)
        .map(|_| scene.dance)
}

fn build_scene(
    frame: &mut Frame,
    engine: &Engine,
    visual: Option<&Visualization>,
    artwork: &mut Artwork,
    controls: &mut Controls,
    selection: &mut TextSelection,
    state: &'static str,
) -> Scene {
    controls.sync_track(engine);
    controls.begin_draw();
    let compact = frame.area().height < 20;
    let width = frame.area().width.saturating_sub(6);
    let favorites = engine.favorites_supported || engine.source == "mixed";
    let quality = engine.quality().unwrap_or_default();
    let volume = format!("Volume: {:.0}%", engine.volume * 100.0);
    let speed = format!("Speed: {:.2}x", engine.speed);
    let queue = format!(" · Queued: {}", engine.queue_len());
    let status = if selection.range().is_some() {
        format!("{state} · Ctrl+C copy · Esc clear")
    } else {
        state.to_owned()
    };
    let favorite = if favorites && engine.can_like() {
        if engine.is_liked() {
            "♥ In favorites · l to remove"
        } else if engine.favorite_state().is_some() {
            "♡ l to add to favorites"
        } else {
            "Favorites: ? · l to check and toggle"
        }
    } else {
        ""
    };
    let notice_width = width.saturating_sub(status.width().max(favorite.width()) as u16 + 2);
    let notice_rows = if compact
        || !engine.message.contains('\n') && engine.message.width() <= usize::from(notice_width)
    {
        1
    } else {
        2
    };
    let detail_rows = if favorites { 3u16 } else { 2 } + notice_rows - 1;
    let progress_rows = 1;
    let control_rows = Controls::height(width, engine);
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
    let identity_rows = if visual.is_none() {
        track_text.as_ref().map_or(0, |(key, text)| {
            selection.height(key, text, width, identity_limit)
        })
    } else {
        0
    };
    let rows = Layout::vertical([if visual.is_some() {
        Constraint::Min(0)
    } else {
        Constraint::Length(2 + identity_rows + detail_rows + progress_rows + control_rows)
    }])
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
    let content = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(progress_rows),
        Constraint::Length(control_rows),
    ])
    .split(content);
    let details = rows[0];
    let footer_rows = detail_rows.min(details.height.saturating_sub(identity_min));
    let notice_rows = notice_rows.min(footer_rows.saturating_sub(1).max(1));
    let identity = Rect::new(
        content.x,
        content.y,
        content.width,
        details.height.saturating_sub(footer_rows),
    );
    let text_height = track_text.as_ref().map_or(0, |(key, text)| {
        selection.height(key, text, identity.width, identity.height)
    });
    let dance_top = (identity.y + text_height)
        .max(frame.area().y + frame.area().height / 2 + 1)
        .min(identity.bottom());
    let dance_area = Rect::new(
        identity.x,
        dance_top,
        identity.width,
        identity.bottom().saturating_sub(dance_top),
    );
    if visual.is_some() {
        artwork.render(inner, frame.buffer_mut());
    }
    let cached = CachedFrame::begin(frame.buffer_mut());
    controls.draw_transport(
        frame,
        inner,
        Rect::new(identity.x, identity.y, identity.width, text_height),
        engine,
    );
    if let Some((key, text)) = track_text {
        selection.render(&key, &text, identity, frame.buffer_mut());
    } else {
        *selection = TextSelection::default();
    }
    let style = Style::default().fg(Color::Gray);
    let settings_width = (volume.width() + speed.width() + queue.width() + 15)
        .min(usize::from(content.width)) as u16;
    if footer_rows > 0 {
        let status_width = status
            .width()
            .min(usize::from(content.width.saturating_sub(notice_width + 2)))
            as u16;
        let row = details.bottom() - footer_rows;
        frame.render_widget(
            Line::from(status).style(accent),
            Rect::new(content.x, row, status_width, 1),
        );
        controls.draw_settings(
            frame,
            Rect::new(
                content.right() - settings_width,
                details.bottom() - 1,
                settings_width,
                1,
            ),
            &volume,
            &speed,
            &queue,
        );
    }
    if favorites && footer_rows >= 2 + notice_rows {
        let line = Line::from(favorite).style(accent);
        let width = line.width().min(usize::from(content.width)) as u16;
        frame.render_widget(
            line,
            Rect::new(content.x, details.bottom() - notice_rows - 1, width, 1),
        );
    }
    if footer_rows >= 2 {
        let quality_width = quality.width().min(usize::from(
            content.width.saturating_sub(settings_width + 2),
        )) as u16;
        frame.render_widget(
            Line::from(quality).style(style),
            Rect::new(content.x, details.bottom() - 1, quality_width, 1),
        );
        let width = notice_width.min(content.width);
        overlay_notice(
            engine.message.as_str(),
            Rect::new(
                content.right() - width,
                details.bottom() - footer_rows,
                width,
                notice_rows,
            ),
            frame.buffer_mut(),
        );
    }
    controls.seek = rows[1];
    controls.draw(frame, rows[2], engine, visual.map(|visual| visual.mode));
    let cached = cached.finish(frame.buffer_mut(), |buffer| {
        controls.highlight_transport(buffer)
    });
    Scene {
        frame: cached,
        state,
        wave: details,
        dance: dance_area,
        progress: rows[1],
        duration: engine.track().and_then(|track| track.duration),
        seconds: None,
        progress_label: String::new(),
    }
}

fn overlay_progress(ratio: f64, label: &str, area: Rect, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let position = (ratio * f64::from(area.width.saturating_sub(1))).round() as u16;
    for x in 0..area.width {
        buffer[(area.x + x, area.y)]
            .set_symbol(if x == position { "●" } else { "━" })
            .set_fg(if x <= position {
                Color::Cyan
            } else {
                Color::Rgb(70, 85, 100)
            });
    }
    let width = label.width().min(usize::from(area.width)) as u16;
    Line::from(label)
        .style(Style::default().fg(Color::White))
        .render(
            Rect::new(area.x + (area.width - width) / 2, area.y, width, 1),
            buffer,
        );
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
    fn progress_overlays_the_cover_without_erasing_its_background() {
        let area = Rect::new(2, 3, 20, 1);
        let mut buffer = Buffer::empty(area);
        for ratio in [0.0, 0.5, 1.0] {
            for cell in &mut buffer.content {
                cell.set_char('▀').set_bg(Color::Rgb(12, 24, 36));
            }
            overlay_progress(ratio, "0:30 / 1:00", area, &mut buffer);
            assert!(
                buffer
                    .content
                    .iter()
                    .all(|cell| cell.bg == Color::Rgb(12, 24, 36))
            );
            let line: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(line.contains("0:30 / 1:00"));
            assert_eq!(buffer[(area.x, area.y)].fg, Color::Cyan);
        }
    }

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
