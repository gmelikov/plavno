use crate::{
    audio::{Audio, CachedAudio, Levels, Prepared, Progress},
    backend::{Backend, Feedback, PlaybackMode, Service, Track, WaveStyle},
    dislikes::Dislikes,
    history::{self, Entry},
};
use anyhow::Result;
use crossterm::event::Event as TerminalEvent;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

pub const MIN_SPEED: f64 = 0.1;
pub const MAX_SPEED: f64 = 2.0;

pub enum Event {
    Terminal(TerminalEvent),
    Control(Command),
    InputError,
    Batch(u64, Result<Vec<Track>>),
    Prepared(u64, u64, Track, Result<Box<Prepared>>),
    Cached(u64, u64, Track, Arc<CachedAudio>),
    Warmed(u64, Track, Arc<AtomicBool>, Result<Arc<CachedAudio>>),
    Warning(u64, String),
    Favorite(u64, Track, bool, Result<Option<bool>>),
    Dislike(Track, bool, Result<bool>),
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    Volume(f64),
    Rate(f64),
    Seek(f64),
    SetPosition((u64, u64), f64),
    Quit,
}

enum Request {
    Start(u64, String, PlaybackMode, WaveStyle),
    More(u64, Vec<String>),
    Resolve(u64, u64, Track, Arc<AtomicBool>),
    Warm(u64, Track, Arc<AtomicBool>),
    Feedback(u64, Track, Feedback, f64),
    FavoriteState(u64, Track),
    ToggleFavorite(u64, Track),
    SetDislike(Track, bool),
    History(u64, Entry),
    Stop(Sender<()>),
}

struct Playing {
    track: Track,
    progress: Arc<Progress>,
    samples_per_second: u64,
    quality: String,
    buffer: Arc<CachedAudio>,
    position: f64,
    listened: f64,
}

struct Seeking {
    position: f64,
    listened: f64,
}

struct Scrubbing {
    position: f64,
    speed: f32,
    active_speed: f32,
    listened: f64,
    dispatched: Option<(f64, Instant)>,
}

enum Preparation {
    Playback(f64),
    Scrub { position: f64, reverse: bool },
}

struct Prefetch {
    serial: u64,
    track: Track,
    cancel: Arc<AtomicBool>,
    result: Option<Result<Box<Prepared>>>,
}

impl Prefetch {
    fn matches(&self, track: &Track) -> bool {
        self.track.cache_key() == track.cache_key() && self.track.batch == track.batch
    }

    fn cancel(self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Playing {
    fn seconds(&self) -> f64 {
        self.position + self.elapsed()
    }
    fn elapsed(&self) -> f64 {
        self.progress.samples.load(Ordering::Relaxed) as f64 / self.samples_per_second as f64
    }
    fn listened(&self) -> f64 {
        self.listened + self.elapsed()
    }
}

const HISTORY_LIMIT: usize = 100;

#[derive(Default)]
struct TrackQueue {
    upcoming: VecDeque<Track>,
    past: VecDeque<Track>,
}

fn prepare(
    events: &Sender<Event>,
    epoch: u64,
    serial: u64,
    track: Track,
    load: impl FnOnce() -> Result<Arc<CachedAudio>> + Send + 'static,
    cancel: Arc<AtomicBool>,
    at: Preparation,
) {
    let tx = events.clone();
    let error_track = track.clone();
    let spawn = std::thread::Builder::new()
        .name("prepare".into())
        .spawn(move || {
            let result = (|| {
                anyhow::ensure!(!cancel.load(Ordering::Relaxed), "Loading cancelled");
                load()
            })()
            .and_then(|buffer| {
                anyhow::ensure!(!cancel.load(Ordering::Relaxed), "Loading cancelled");
                let _ = tx.send(Event::Cached(epoch, serial, track.clone(), buffer.clone()));
                match at {
                    Preparation::Playback(0.0) => crate::audio::prepare(buffer, cancel),
                    Preparation::Playback(position) => {
                        crate::audio::prepare_at(buffer, cancel, position)
                    }
                    Preparation::Scrub { position, reverse } => {
                        crate::audio::prepare_scrub_at(buffer, cancel, position, reverse)
                    }
                }
            })
            .map(Box::new);
            let _ = tx.send(Event::Prepared(epoch, serial, track, result));
        });
    if let Err(error) = spawn {
        let _ = events.send(Event::Prepared(
            epoch,
            serial,
            error_track,
            Err(error.into()),
        ));
    }
}

impl TrackQueue {
    fn remember(&mut self, track: Track) {
        self.past.push_back(track);
        if self.past.len() > HISTORY_LIMIT {
            self.past.pop_front();
        }
    }

    fn next(&mut self, finished: Option<Track>) -> Option<Track> {
        if let Some(track) = finished {
            self.remember(track);
        }
        self.upcoming.pop_front()
    }

    fn previous(&mut self, current: Option<&Track>) -> Option<Track> {
        let previous = self.past.pop_back()?;
        if let Some(track) = current {
            self.upcoming.push_front(track.clone());
        }
        Some(previous)
    }
}

pub struct Engine {
    pub name: &'static str,
    pub source: &'static str,
    pub services: Vec<Service>,
    pub mode: PlaybackMode,
    wave_styles: HashMap<String, WaveStyle>,
    pub history: VecDeque<Entry>,
    pub favorites_supported: bool,
    pub paused: bool,
    pub volume: f32,
    pub speed: f32,
    pub message: String,
    pub loading: bool,
    queue: TrackQueue,
    cache: HashMap<String, Arc<CachedAudio>>,
    warming: HashMap<String, Arc<AtomicBool>>,
    liked: VecDeque<(String, bool)>,
    like_pending: bool,
    dislikes: Dislikes,
    audio: Audio,
    requests: Sender<Request>,
    events: Sender<Event>,
    current: Option<Playing>,
    selected: Option<Track>,
    stopped: bool,
    cancel: Arc<AtomicBool>,
    seen: VecDeque<String>,
    epoch: u64,
    serial: u64,
    active_serial: u64,
    playback_serial: u64,
    seeking: Option<Seeking>,
    scrubbing: Option<Scrubbing>,
    pub seek_revision: u64,
    prefetch: Option<Prefetch>,
    batch_pending: bool,
    exhausted: bool,
    filtered_batches: u8,
    preparing: bool,
}

impl Engine {
    pub fn new(mut backend: Box<dyn Backend>) -> Result<(Self, Sender<Event>, Receiver<Event>)> {
        let name = backend.name();
        let source = backend.key();
        let services = backend.services();
        let favorites_supported = backend.supports_favorites();
        let wave_styles = if cfg!(test) {
            HashMap::new()
        } else {
            crate::config::wave_styles().unwrap_or_else(|error| {
                eprintln!("plavno: wave settings unavailable: {error:#}");
                HashMap::new()
            })
        };
        let (history, history_path) = if cfg!(test) {
            (VecDeque::new(), None)
        } else {
            match history::path().and_then(|path| Ok((history::load(&path)?, path))) {
                Ok((entries, path)) => (entries, Some(path)),
                Err(error) => {
                    eprintln!("plavno: history unavailable: {error:#}");
                    (VecDeque::new(), None)
                }
            }
        };
        let dislikes = Dislikes::open()?;
        let audio = Audio::new()?;
        let (requests, receiver) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        let tx = events.clone();
        std::thread::Builder::new()
            .name("service".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let event = match request {
                        Request::Start(epoch, source, mode, style) => {
                            Event::Batch(epoch, backend.start_with_style(&source, mode, style))
                        }
                        Request::More(epoch, history) => {
                            Event::Batch(epoch, backend.more(&history))
                        }
                        Request::Feedback(epoch, track, kind, seconds) => {
                            if let Err(error) = backend.feedback(&track, kind, seconds) {
                                let _ = tx.send(Event::Warning(
                                    epoch,
                                    format!("Playback feedback: {error}"),
                                ));
                            }
                            continue;
                        }
                        Request::FavoriteState(epoch, track) => {
                            let result = backend.favorite_state(&track);
                            Event::Favorite(epoch, track, false, result)
                        }
                        Request::ToggleFavorite(epoch, track) => {
                            let result = backend.toggle_favorite(&track).map(Some);
                            Event::Favorite(epoch, track, true, result)
                        }
                        Request::SetDislike(track, disliked) => {
                            let result = backend.set_dislike(&track, disliked);
                            Event::Dislike(track, disliked, result)
                        }
                        Request::History(epoch, entry) => {
                            if let Some(path) = &history_path
                                && let Err(error) = history::save_entry(path, entry)
                            {
                                let _ =
                                    tx.send(Event::Warning(epoch, format!("History: {error:#}")));
                            }
                            continue;
                        }
                        Request::Stop(done) => {
                            let _ = done.send(());
                            break;
                        }
                        Request::Warm(epoch, track, cancel) => {
                            if cancel.load(Ordering::Relaxed) {
                                continue;
                            }
                            let resolved = backend.resolve(&track);
                            let tx = tx.clone();
                            let error_tx = tx.clone();
                            let error_track = track.clone();
                            let error_cancel = cancel.clone();
                            let spawn =
                                std::thread::Builder::new()
                                    .name("warm".into())
                                    .spawn(move || {
                                        let result = resolved.and_then(|input| {
                                            anyhow::ensure!(
                                                !cancel.load(Ordering::Relaxed),
                                                "Loading cancelled"
                                            );
                                            let buffer = crate::audio::cache_input(input)?;
                                            anyhow::ensure!(
                                                !cancel.load(Ordering::Relaxed),
                                                "Loading cancelled"
                                            );
                                            buffer.start_download()?;
                                            Ok(buffer)
                                        });
                                        let _ =
                                            tx.send(Event::Warmed(epoch, track, cancel, result));
                                    });
                            if let Err(error) = spawn {
                                let _ = error_tx.send(Event::Warmed(
                                    epoch,
                                    error_track,
                                    error_cancel,
                                    Err(error.into()),
                                ));
                            }
                            continue;
                        }
                        Request::Resolve(epoch, serial, track, cancel) => {
                            if cancel.load(Ordering::Relaxed) {
                                continue;
                            }
                            match backend.resolve(&track) {
                                Ok(input) => {
                                    if cancel.load(Ordering::Relaxed) {
                                        continue;
                                    }
                                    prepare(
                                        &tx,
                                        epoch,
                                        serial,
                                        track,
                                        move || crate::audio::cache_input(input),
                                        cancel,
                                        Preparation::Playback(0.0),
                                    );
                                    continue;
                                }
                                Err(error) => Event::Prepared(epoch, serial, track, Err(error)),
                            }
                        }
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            })?;
        Ok((
            Self {
                name,
                source,
                services,
                mode: PlaybackMode::MyVibe,
                history,
                favorites_supported,
                wave_styles,
                audio,
                requests,
                events: events.clone(),
                current: None,
                selected: None,
                stopped: false,
                paused: false,
                volume: 1.0,
                speed: 1.0,
                message: String::new(),
                loading: false,
                queue: TrackQueue::default(),
                cache: HashMap::new(),
                warming: HashMap::new(),
                liked: VecDeque::new(),
                like_pending: false,
                dislikes,
                cancel: Arc::new(AtomicBool::new(false)),
                seen: VecDeque::new(),
                epoch: 0,
                serial: 0,
                active_serial: 0,
                playback_serial: 0,
                seeking: None,
                scrubbing: None,
                seek_revision: 0,
                prefetch: None,
                batch_pending: false,
                exhausted: false,
                filtered_batches: 0,
                preparing: false,
            },
            events,
            rx,
        ))
    }

    fn request(&mut self, request: Request) {
        if self.requests.send(request).is_err() {
            self.loading = false;
            self.like_pending = false;
            self.message = "The backend has stopped".into();
        }
    }

    fn prepare_track(&mut self, serial: u64, track: Track, cancel: Arc<AtomicBool>) {
        if let Some(buffer) = self.cache.get(&track.cache_key()).cloned() {
            prepare(
                &self.events,
                self.epoch,
                serial,
                track,
                move || Ok(buffer),
                cancel,
                Preparation::Playback(0.0),
            );
        } else {
            self.request(Request::Resolve(self.epoch, serial, track, cancel));
        }
    }

    pub fn start(&mut self) {
        self.finish(Feedback::Skipped);
        self.cancel_prefetch();
        self.selected = None;
        self.stopped = false;
        self.epoch += 1;
        self.queue = TrackQueue::default();
        self.trim_cache();
        self.like_pending = false;
        self.seen.clear();
        self.exhausted = false;
        self.filtered_batches = 0;
        self.preparing = false;
        self.loading = true;
        self.batch_pending = true;
        self.message = "Starting…".into();
        self.request(Request::Start(
            self.epoch,
            self.source.into(),
            self.mode,
            self.wave_style(),
        ));
    }

    pub fn wave_style(&self) -> WaveStyle {
        self.wave_styles
            .get(self.source)
            .copied()
            .unwrap_or_default()
    }

    pub fn can_tune_wave(&self) -> bool {
        self.services
            .iter()
            .any(|service| service.key == self.source && service.available && service.wave_styles)
    }

    pub fn set_wave_style(&mut self, style: WaveStyle) {
        if !self.can_tune_wave() {
            self.message = "This source does not support Flow settings".into();
            return;
        }
        self.wave_styles.insert(self.source.into(), style);
        let saved = if cfg!(test) {
            Ok(())
        } else {
            crate::config::save_wave_styles(&self.wave_styles)
        };
        self.start_mode(PlaybackMode::MyVibe);
        self.message = match saved {
            Ok(()) => format!("Starting {} wave…", style.label()),
            Err(error) => format!("Wave style applied; settings were not saved: {error:#}"),
        };
    }

    pub fn start_mode(&mut self, mode: PlaybackMode) {
        if mode == PlaybackMode::FavoritesShuffle && !self.can_shuffle_favorites() {
            self.message = "This source does not support Favorites Shuffle".into();
            return;
        }
        self.mode = mode;
        self.paused = false;
        self.start();
    }

    pub fn start_source(&mut self, key: &str) {
        let Some(service) = self.services.iter().find(|s| s.key == key) else {
            self.message = "This source is not available".into();
            return;
        };
        if !service.available {
            self.message = service.status.clone();
            return;
        }
        self.source = service.key;
        self.name = service.name;
        self.favorites_supported = key == "mixed" || service.favorites;
        self.mode = PlaybackMode::MyVibe;
        self.paused = false;
        self.start();
    }

    pub fn can_like(&self) -> bool {
        self.media_track()
            .map_or(self.favorites_supported, |track| {
                self.track_favorites(track)
            })
    }

    pub fn can_shuffle_favorites(&self) -> bool {
        self.services
            .iter()
            .any(|s| s.key == self.source && s.favorites_shuffle)
    }

    fn track_favorites(&self, track: &Track) -> bool {
        let key = if track.source.is_empty() {
            self.source
        } else {
            &track.source
        };
        self.services.iter().any(|s| s.key == key && s.favorites)
    }

    fn retry_hint(&self) -> String {
        format!("{} to retry", self.mode.key())
    }

    pub fn next(&mut self) {
        if self.loading {
            return;
        }
        if self.stopped {
            self.selected = self.queue.next(self.selected.take());
            self.trim_cache();
            return;
        }
        self.advance(Feedback::Skipped);
    }

    pub fn previous(&mut self) {
        if self.loading {
            return;
        }
        let current = self.media_track().cloned();
        let Some(track) = self.queue.previous(current.as_ref()) else {
            self.message = "No previous track yet".into();
            return;
        };
        if self.stopped {
            self.selected = Some(track);
            self.trim_cache();
            return;
        }
        self.finish(Feedback::Skipped);
        self.load(Some(track));
    }

    pub fn play_history(&mut self, entry: &Entry) {
        if self.dislikes.contains(&entry.source, &entry.track) {
            self.message = "This track is disliked; u undoes the most recent dislike".into();
            return;
        }
        if !self
            .services
            .iter()
            .any(|s| s.key == entry.source && s.available)
        {
            self.message = "The track's source is not connected".into();
            return;
        }
        if let Some(track) = self.finish(Feedback::Skipped) {
            self.queue.remember(track);
        }
        self.selected = None;
        self.stopped = false;
        self.paused = false;
        let mut track = entry.track.clone();
        if track.source.is_empty() && (self.services.len() > 1 || entry.source != self.source) {
            track.source = entry.source.clone();
        }
        track.batch.clear();
        self.load(Some(track));
    }

    pub fn like(&mut self) {
        let Some(track) = self.track().cloned() else {
            self.message = "Select a track to change favorites".into();
            return;
        };
        if !self.can_like() {
            self.message = "This source does not support favorites".into();
            return;
        }
        if self.like_pending {
            self.message = "Favorites are still updating…".into();
            return;
        }
        self.like_pending = true;
        self.message = "Updating favorites…".into();
        self.request(Request::ToggleFavorite(self.epoch, track));
    }

    pub fn can_dislike(&self) -> bool {
        self.media_track()
            .is_some_and(|track| !self.dislikes.contains(self.source, track))
    }

    pub fn can_undo_dislike(&self) -> bool {
        !self.dislikes.is_empty()
    }

    pub fn dislike(&mut self) {
        let Some(track) = self.media_track().cloned() else {
            self.message = "Select a track to dislike".into();
            return;
        };
        let entry = match self.dislikes.block(self.source, &track) {
            Ok(entry) => entry,
            Err(error) => {
                self.message = format!("Dislike was not saved: {error:#}");
                return;
            }
        };
        self.queue
            .upcoming
            .retain(|track| !self.dislikes.contains(self.source, track));
        self.queue
            .past
            .retain(|track| !self.dislikes.contains(self.source, track));
        self.finish(Feedback::Skipped);
        self.selected = None;
        self.preparing = false;
        self.loading = false;
        self.evict(&track);
        self.trim_cache();
        if self.stopped {
            self.selected = self.queue.next(None);
            self.trim_cache();
        } else {
            self.play_next();
        }
        self.request(Request::SetDislike(entry.track, true));
        self.message = format!("Disliked: {} · u Undo", track.title);
    }

    pub fn undo_dislike(&mut self) {
        match self.dislikes.undo() {
            Ok(Some(entry)) => {
                let key = entry.track.cache_key();
                self.seen.retain(|seen| {
                    seen != &key && !(entry.source == self.source && seen == &entry.track.id)
                });
                self.exhausted = false;
                self.filtered_batches = 0;
                self.message = format!("Dislike undone: {}", entry.track.title);
                self.request(Request::SetDislike(entry.track, false));
            }
            Ok(None) => self.message = "No dislike to undo".into(),
            Err(error) => self.message = format!("Dislike undo was not saved: {error:#}"),
        }
    }

    fn finish(&mut self, kind: Feedback) -> Option<Track> {
        self.seeking = None;
        let listened = self.scrubbing.take().map(|scrub| scrub.listened);
        self.cancel.store(true, Ordering::Relaxed);
        self.audio.sink.stop();
        if let Some(playing) = self.current.take() {
            self.request(Request::Feedback(
                self.epoch,
                playing.track.clone(),
                kind,
                listened.unwrap_or_else(|| playing.listened()),
            ));
            return Some(playing.track);
        }
        None
    }

    pub fn pause(&mut self) {
        self.paused = !self.paused;
        if self.paused {
            self.audio.sink.pause();
        } else {
            self.audio.sink.play();
        }
    }

    pub fn change_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.audio.sink.set_volume(self.volume);
    }

    pub fn change_speed(&mut self, delta: f32) {
        self.set_speed(f64::from(self.speed + delta));
    }

    fn set_speed(&mut self, speed: f64) {
        if !speed.is_finite() {
            return;
        }
        self.speed = (speed.clamp(MIN_SPEED, MAX_SPEED) * 20.0).round() as f32 / 20.0;
        if !self.is_scrubbing() {
            self.audio.sink.set_speed(self.speed);
        }
    }

    fn control(&mut self, command: Command) {
        match command {
            Command::Play => {
                if self.paused {
                    self.pause();
                }
                self.stopped = false;
                if let Some(seeking) = &self.seeking
                    && !self.preparing
                {
                    self.seek_to(seeking.position);
                    return;
                }
                if self.current.is_none() && !self.preparing && self.can_play() {
                    let track = self.selected.take().or_else(|| self.queue.next(None));
                    self.load(track);
                }
            }
            Command::Pause if self.can_pause() && !self.paused => self.pause(),
            Command::PlayPause => {
                if (self.current.is_some() || self.preparing)
                    && (self.seeking.is_none() || self.preparing)
                {
                    self.pause();
                } else {
                    self.control(Command::Play);
                }
            }
            Command::Stop => {
                if let Some(track) = self.finish(Feedback::Skipped) {
                    self.selected = Some(track);
                }
                self.cancel_prefetch();
                self.stopped = true;
                self.preparing = false;
                self.loading = false;
                self.message.clear();
            }
            Command::Next if self.can_next() => self.next(),
            Command::Previous if self.can_previous() => self.previous(),
            Command::Volume(volume) if volume.is_finite() => {
                self.volume = volume.clamp(0.0, 1.0) as f32;
                self.audio.sink.set_volume(self.volume);
            }
            Command::Rate(speed) => self.set_speed(speed),
            Command::Seek(offset) if offset.is_finite() && self.can_seek() => {
                let position = self.seconds() + offset;
                if self
                    .track()
                    .and_then(|t| t.duration)
                    .is_some_and(|duration| position > duration)
                {
                    self.advance(Feedback::Skipped);
                } else {
                    self.seek_to(position.max(0.0));
                }
            }
            Command::SetPosition(id, position) if id == self.playback_id() => {
                self.seek_to(position)
            }
            _ => {}
        }
    }

    pub fn seek_to(&mut self, position: f64) {
        if !self.can_seek() || !position.is_finite() || position < 0.0 {
            return;
        }
        if self.scrubbing.is_none()
            && self.preparing
            && self
                .seeking
                .as_ref()
                .is_some_and(|seek| (seek.position - position).abs() < 0.001)
        {
            return;
        }
        let playing = self.current.as_ref().unwrap();
        if self.scrubbing.is_none()
            && self.seeking.is_none()
            && (playing.seconds() - position).abs() < 0.001
        {
            return;
        }
        if playing
            .track
            .duration
            .is_some_and(|duration| position > duration)
        {
            return;
        }
        let track = playing.track.clone();
        let buffer = playing.buffer.clone();
        let listened = self
            .scrubbing
            .take()
            .map_or_else(|| playing.listened(), |scrub| scrub.listened);
        self.cancel.store(true, Ordering::Relaxed);
        self.audio.sink.stop();
        self.seeking = Some(Seeking { position, listened });
        self.serial += 1;
        self.active_serial = self.serial;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.loading = true;
        self.preparing = true;
        self.message = "Seeking…".into();
        prepare(
            &self.events,
            self.epoch,
            self.serial,
            track,
            move || Ok(buffer),
            self.cancel.clone(),
            Preparation::Playback(position),
        );
    }

    pub fn is_scrubbing(&self) -> bool {
        self.scrubbing.is_some()
    }

    pub fn scrub_to(&mut self, position: f64, speed: f32) {
        if !self.can_seek() || !position.is_finite() || position < 0.0 || !speed.is_finite() {
            return;
        }
        let playing = self.current.as_ref().unwrap();
        let position = playing
            .track
            .duration
            .map_or(position, |duration| position.min(duration));
        let speed = if speed < 0.0 {
            -speed.abs().clamp(2.0, 4.0)
        } else {
            speed.clamp(2.0, 4.0)
        };
        let scrub = self.scrubbing.get_or_insert_with(|| Scrubbing {
            position,
            speed,
            active_speed: speed,
            listened: self
                .seeking
                .as_ref()
                .map_or_else(|| playing.listened(), |seek| seek.listened),
            dispatched: None,
        });
        scrub.position = position;
        scrub.speed = speed;
        self.message = format!(
            "Scrubbing {} {:.1}x",
            if speed < 0.0 { "←" } else { "→" },
            speed.abs()
        );
        self.prepare_scrub();
    }

    fn prepare_scrub(&mut self) -> bool {
        let Some(scrub) = &mut self.scrubbing else {
            return false;
        };
        if self.preparing
            || scrub.dispatched.is_some_and(|(position, started)| {
                (position - scrub.position).abs() < 0.001
                    || started.elapsed() < Duration::from_millis(50)
            })
        {
            return false;
        }
        let Some(playing) = &self.current else {
            return false;
        };
        let position = scrub.position;
        let reverse = scrub.speed < 0.0;
        if scrub.dispatched.is_none() {
            self.audio.sink.stop();
        }
        scrub.dispatched = Some((position, Instant::now()));
        scrub.active_speed = scrub.speed;
        self.seeking = Some(Seeking {
            position,
            listened: scrub.listened,
        });
        self.cancel.store(true, Ordering::Relaxed);
        self.serial += 1;
        self.active_serial = self.serial;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.preparing = true;
        prepare(
            &self.events,
            self.epoch,
            self.serial,
            playing.track.clone(),
            {
                let buffer = playing.buffer.clone();
                move || Ok(buffer)
            },
            self.cancel.clone(),
            Preparation::Scrub { position, reverse },
        );
        true
    }

    pub fn end_scrub(&mut self) {
        if let Some(scrub) = &self.scrubbing {
            self.seek_to(scrub.position);
        }
    }

    fn advance(&mut self, kind: Feedback) {
        let finished = self.finish(kind);
        let track = self.queue.next(finished);
        self.load(track);
    }

    fn play_next(&mut self) {
        let track = self.queue.next(None);
        self.load(track);
    }

    fn load(&mut self, track: Option<Track>) {
        let track = track.filter(|track| !self.dislikes.contains(self.source, track));
        if let Some(track) = track {
            self.selected = Some(track.clone());
            self.loading = true;
            self.preparing = true;
            self.message = format!("Loading: {}", track.title);
            self.trim_cache();
            if let Some(prefetch) = self.prefetch.take() {
                if prefetch.matches(&track) {
                    self.active_serial = prefetch.serial;
                    self.playback_serial = self.active_serial;
                    self.cancel = prefetch.cancel;
                    match prefetch.result {
                        Some(Ok(prepared)) => {
                            self.handle(Event::Prepared(
                                self.epoch,
                                self.active_serial,
                                track,
                                Ok(prepared),
                            ));
                            return;
                        }
                        None => return,
                        Some(Err(error))
                            if error
                                .downcast_ref::<crate::backend::RateLimited>()
                                .is_some_and(|limit| limit.active()) =>
                        {
                            self.handle(Event::Prepared(
                                self.epoch,
                                self.active_serial,
                                track,
                                Err(error),
                            ));
                            return;
                        }
                        Some(Err(_)) => self.cancel.store(true, Ordering::Relaxed),
                    }
                } else {
                    prefetch.cancel();
                }
            }
            self.serial += 1;
            self.active_serial = self.serial;
            self.playback_serial = self.active_serial;
            self.cancel = Arc::new(AtomicBool::new(false));
            if !self.cache.contains_key(&track.cache_key())
                && self.warming.contains_key(&track.cache_key())
            {
                return;
            }
            self.prepare_track(self.serial, track, self.cancel.clone());
        } else {
            self.cancel_prefetch();
            self.trim_cache();
            if !self.exhausted {
                self.more();
            } else {
                self.loading = false;
                self.message = format!("The queue has ended · {} to restart", self.mode.key());
            }
        }
    }

    fn cancel_prefetch(&mut self) {
        if let Some(prefetch) = self.prefetch.take() {
            prefetch.cancel();
        }
    }

    fn prefetch_next(&mut self) {
        if self.current.is_none() || self.preparing {
            return;
        }
        self.trim_cache();
        self.warm_previous();
        let Some(track) = self.queue.upcoming.front().cloned() else {
            self.cancel_prefetch();
            return;
        };
        if self.prefetch.as_ref().is_some_and(|p| p.matches(&track)) {
            return;
        }
        self.cancel_prefetch();
        self.serial += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.prefetch = Some(Prefetch {
            serial: self.serial,
            track: track.clone(),
            cancel: cancel.clone(),
            result: None,
        });
        if !self.cache.contains_key(&track.cache_key())
            && self.warming.contains_key(&track.cache_key())
        {
            return;
        }
        self.prepare_track(self.serial, track, cancel);
    }

    fn cache_keys(&self) -> Vec<String> {
        [
            self.media_track(),
            self.queue.past.back(),
            self.queue.upcoming.front(),
        ]
        .into_iter()
        .flatten()
        .map(Track::cache_key)
        .collect()
    }

    fn trim_cache(&mut self) {
        let keep = self.cache_keys();
        self.cache.retain(|key, buffer| {
            if keep.contains(key) {
                return true;
            }
            buffer.cancel();
            false
        });
        self.warming.retain(|key, cancel| {
            if keep.contains(key) {
                return true;
            }
            cancel.store(true, Ordering::Relaxed);
            false
        });
    }

    fn warm_previous(&mut self) {
        let Some(track) = self.queue.past.back().cloned() else {
            return;
        };
        let key = track.cache_key();
        if self.cache.contains_key(&key) || self.warming.contains_key(&key) {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.warming.insert(key, cancel.clone());
        self.request(Request::Warm(self.epoch, track, cancel));
    }

    fn clear_cache(&mut self) {
        for buffer in self.cache.values() {
            buffer.cancel();
        }
        self.cache.clear();
        for (_, cancel) in self.warming.drain() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn evict(&mut self, track: &Track) {
        if self
            .current
            .as_ref()
            .is_some_and(|p| p.track.cache_key() == track.cache_key())
        {
            return;
        }
        if let Some(cache) = self.cache.remove(&track.cache_key()) {
            cache.cancel();
        }
    }

    fn more(&mut self) {
        if !self.batch_pending {
            self.batch_pending = true;
            self.request(Request::More(
                self.epoch,
                self.seen.iter().cloned().collect(),
            ));
        }
        if self.current.is_none() {
            self.loading = true;
        }
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Cached(epoch, serial, track, buffer)
                if epoch == self.epoch
                    && !self.dislikes.contains(self.source, &track)
                    && (serial == self.active_serial
                        || self.prefetch.as_ref().is_some_and(|p| p.serial == serial)) =>
            {
                self.cache.insert(track.cache_key(), buffer);
                self.trim_cache();
            }
            Event::Warmed(epoch, track, cancel, result) if epoch == self.epoch => {
                let key = track.cache_key();
                if self
                    .warming
                    .get(&key)
                    .is_some_and(|pending| Arc::ptr_eq(pending, &cancel))
                    && self.cache_keys().contains(&key)
                {
                    self.warming.remove(&key);
                    let failure = match result {
                        Ok(buffer) => {
                            self.cache.insert(key, buffer);
                            None
                        }
                        Err(error) => Some(error),
                    };
                    let pending = if self.preparing
                        && self.seeking.is_none()
                        && self
                            .selected
                            .as_ref()
                            .is_some_and(|t| t.cache_key() == track.cache_key())
                    {
                        Some((
                            self.active_serial,
                            self.cancel.clone(),
                            self.selected.as_ref().unwrap().clone(),
                        ))
                    } else {
                        self.prefetch
                            .as_ref()
                            .filter(|p| {
                                p.track.cache_key() == track.cache_key() && p.result.is_none()
                            })
                            .map(|p| (p.serial, p.cancel.clone(), p.track.clone()))
                    };
                    if let Some((serial, cancel, track)) = pending {
                        if let Some(error) = failure
                            && error
                                .downcast_ref::<crate::backend::RateLimited>()
                                .is_some_and(|limit| limit.active())
                        {
                            self.handle(Event::Prepared(self.epoch, serial, track, Err(error)));
                        } else {
                            self.prepare_track(serial, track, cancel);
                        }
                    }
                }
            }
            Event::Control(command) => self.control(command),
            Event::Batch(epoch, result) if epoch == self.epoch => {
                self.batch_pending = false;
                match result {
                    Ok(tracks) => {
                        let before = self.queue.upcoming.len();
                        let mut batch_ids = HashSet::new();
                        let mut excluded = false;
                        for track in tracks {
                            if self.dislikes.contains(self.source, &track) {
                                excluded = true;
                                if !self.seen.contains(&track.cache_key()) {
                                    self.seen.push_back(track.cache_key());
                                    if self.seen.len() > 100 {
                                        self.seen.pop_front();
                                    }
                                }
                                continue;
                            }
                            let source = if track.source.is_empty() {
                                self.source
                            } else {
                                &track.source
                            };
                            let repeats = self
                                .services
                                .iter()
                                .any(|service| service.key == source && service.allow_repeats);
                            if (self.mode == PlaybackMode::MyVibe
                                && !repeats
                                && self.seen.contains(&track.cache_key()))
                                || !batch_ids.insert(track.cache_key())
                            {
                                continue;
                            }
                            self.seen.push_back(track.cache_key());
                            if self.seen.len() > 100 {
                                self.seen.pop_front();
                            }
                            self.queue.upcoming.push_back(track);
                        }
                        self.exhausted = self.queue.upcoming.len() == before;
                        let retry = self.exhausted && excluded && self.filtered_batches < 3;
                        if retry {
                            self.filtered_batches += 1;
                            self.exhausted = false;
                        } else if !self.exhausted {
                            self.filtered_batches = 0;
                        }
                        if self.current.is_none() && !self.preparing && !self.stopped {
                            self.loading = false;
                            self.play_next();
                        }
                        self.prefetch_next();
                        if retry {
                            self.more();
                        }
                    }
                    Err(error) => {
                        if !self.preparing {
                            self.loading = false;
                        }
                        self.exhausted = true;
                        self.message = format!("{error} · {}", self.retry_hint());
                    }
                }
            }
            Event::Prepared(epoch, serial, track, result)
                if epoch == self.epoch
                    && self.prefetch.as_ref().is_some_and(|p| p.serial == serial) =>
            {
                if result.is_err() {
                    self.evict(&track);
                }
                self.prefetch.as_mut().unwrap().result = Some(result);
            }
            Event::Prepared(epoch, serial, mut track, result)
                if epoch == self.epoch
                    && self.preparing
                    && serial == self.active_serial
                    && !self.dislikes.contains(self.source, &track) =>
            {
                self.loading = false;
                self.preparing = false;
                match result {
                    Ok(prepared) => {
                        let prepared = *prepared;
                        if track.duration.is_none() {
                            track.duration = prepared.duration;
                        }
                        self.audio.play(
                            prepared.source,
                            self.paused && self.scrubbing.is_none(),
                            self.volume,
                            self.scrubbing
                                .as_ref()
                                .map_or(self.speed, |scrub| scrub.active_speed.abs()),
                        );
                        let seeking = self.seeking.take();
                        if seeking.is_some() {
                            self.seek_revision += 1;
                        } else {
                            let entry = Entry::new(self.source, &track);
                            history::push(&mut self.history, entry.clone());
                            self.request(Request::History(self.epoch, entry));
                            self.request(Request::Feedback(
                                self.epoch,
                                track.clone(),
                                Feedback::Started,
                                0.0,
                            ));
                            if self.track_favorites(&track) {
                                self.request(Request::FavoriteState(self.epoch, track.clone()));
                            }
                        }
                        self.current = Some(Playing {
                            track,
                            progress: prepared.progress,
                            samples_per_second: prepared.samples_per_second,
                            quality: prepared.quality,
                            buffer: prepared.buffer,
                            position: prepared.position,
                            listened: seeking.map_or(0.0, |s| s.listened),
                        });
                        self.selected = None;
                        if self.scrubbing.is_none() {
                            self.message.clear();
                        }
                        if self.queue.upcoming.len() < 2 && !self.exhausted {
                            self.more();
                        }
                        self.prefetch_next();
                    }
                    Err(error) => {
                        if self.seeking.is_none() {
                            self.evict(&track);
                        }
                        self.message = format!("{error} · n for next, {}", self.retry_hint());
                    }
                }
            }
            Event::Favorite(epoch, track, changed, result) if epoch == self.epoch => {
                if changed {
                    self.like_pending = false;
                }
                match result {
                    Ok(Some(liked)) => {
                        self.liked.retain(|(id, _)| id != &track.cache_key());
                        self.liked.push_back((track.cache_key(), liked));
                        if self.liked.len() > HISTORY_LIMIT {
                            self.liked.pop_front();
                        }
                        if self.mode == PlaybackMode::FavoritesShuffle && !liked {
                            self.queue
                                .upcoming
                                .retain(|queued| queued.cache_key() != track.cache_key());
                            self.trim_cache();
                            self.prefetch_next();
                            if self.queue.upcoming.len() < 2 && !self.exhausted {
                                self.more();
                            }
                        }
                        if changed {
                            self.message = if liked {
                                format!("♥ Added to favorites: {}", track.title)
                            } else {
                                format!("♡ Removed from favorites: {}", track.title)
                            };
                        }
                    }
                    Ok(None) => {}
                    Err(error) => self.message = format!("Favorites: {error}"),
                }
            }
            Event::Dislike(track, disliked, result)
                if self.dislikes.contains(self.source, &track) == disliked =>
            {
                let action = if disliked {
                    "Disliked"
                } else {
                    "Dislike undone"
                };
                self.message = match result {
                    Ok(true) => format!("{action}: {} · synced with service", track.title),
                    Ok(false) => format!("{action}: {} · saved locally", track.title),
                    Err(error) => format!(
                        "{action}: {} · saved locally; service sync failed: {error}",
                        track.title
                    ),
                };
            }
            Event::Warning(epoch, message) if epoch == self.epoch => self.message = message,
            _ => {}
        }
    }

    pub fn tick(&mut self) -> bool {
        if self.scrubbing.is_some() {
            return self.prepare_scrub();
        }
        if let Some(playing) = &self.current
            && !self.preparing
            && self.seeking.is_none()
            && self.audio.sink.empty()
        {
            let read_error = playing.progress.error.lock().unwrap().clone();
            let incomplete = read_error.is_some()
                || playing
                    .track
                    .duration
                    .is_some_and(|d| playing.seconds() + 5.0 < d);
            self.advance(if incomplete {
                Feedback::Skipped
            } else {
                Feedback::Finished
            });
            if incomplete {
                self.message = read_error
                    .unwrap_or_else(|| "The stream ended before the track finished".into());
            }
            return true;
        }
        false
    }

    pub fn queue_len(&self) -> usize {
        self.queue.upcoming.len()
    }

    pub fn is_liked(&self) -> bool {
        self.favorite_state() == Some(true)
    }

    pub fn favorite_state(&self) -> Option<bool> {
        let track = self.track()?;
        self.liked
            .iter()
            .find(|(id, _)| id == &track.cache_key())
            .map(|(_, liked)| *liked)
    }

    pub fn quality(&self) -> Option<&str> {
        self.current
            .as_ref()
            .map(|playing| playing.quality.as_str())
    }

    pub fn track(&self) -> Option<&Track> {
        self.current.as_ref().map(|p| &p.track)
    }
    pub fn media_track(&self) -> Option<&Track> {
        self.track().or(self.selected.as_ref())
    }
    pub fn playback_status(&self) -> &'static str {
        if self.stopped
            || (self.current.is_none() && !self.preparing)
            || (self.seeking.is_some() && !self.preparing)
        {
            "Stopped"
        } else if self.paused {
            "Paused"
        } else {
            "Playing"
        }
    }
    pub fn playback_id(&self) -> (u64, u64) {
        (self.epoch, self.playback_serial)
    }
    pub fn can_next(&self) -> bool {
        !self.loading && (!self.queue.upcoming.is_empty() || !self.exhausted)
    }
    pub fn can_previous(&self) -> bool {
        !self.loading && !self.queue.past.is_empty()
    }
    pub fn can_play(&self) -> bool {
        self.media_track().is_some() || !self.queue.upcoming.is_empty()
    }
    pub fn can_pause(&self) -> bool {
        self.media_track().is_some()
    }
    pub fn can_seek(&self) -> bool {
        self.current.is_some() && !self.stopped
    }
    pub fn seconds(&self) -> f64 {
        if let Some(scrub) = &self.scrubbing {
            return scrub.position;
        }
        self.seeking.as_ref().map_or_else(
            || self.current.as_ref().map_or(0.0, Playing::seconds),
            |s| s.position,
        )
    }
    pub fn buffering(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|p| p.progress.buffering.load(Ordering::Relaxed))
    }

    pub fn levels(&self) -> Levels {
        if self.paused && !self.is_scrubbing()
            || self.preparing
            || self.buffering()
            || self.current.is_none()
        {
            return Levels::default();
        }
        let mut levels = Levels::unpack(
            self.current
                .as_ref()
                .unwrap()
                .progress
                .levels
                .load(Ordering::Relaxed),
        );
        levels.rms *= self.volume;
        levels.bass *= self.volume;
        levels.peak *= self.volume;
        levels.pulse *= self.volume;
        levels
    }

    pub fn shutdown(&mut self) -> Receiver<()> {
        self.finish(Feedback::Skipped);
        self.cancel_prefetch();
        self.clear_cache();
        let (done, receiver) = mpsc::channel();
        self.request(Request::Stop(done));
        receiver
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel_prefetch();
        self.clear_cache();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Input;
    use std::{
        sync::Mutex,
        time::{Duration, Instant},
    };

    fn track(id: usize) -> Track {
        Track {
            source: String::new(),
            id: id.to_string(),
            title: format!("Track {id}"),
            artist: String::new(),
            duration: Some(120.0),
            cover_url: None,
            genre: None,
            batch: format!("batch-{id}"),
        }
    }

    fn favorite_track(id: usize) -> Track {
        let mut track = track(id);
        track.batch.clear();
        track
    }

    #[test]
    fn local_shuffle_repeats_through_the_engine_and_keeps_cached_audio() {
        if std::env::var_os("PLAVNO_LOCAL_TEST_CHILD").is_none() {
            let config =
                std::env::temp_dir().join(format!("plavno-local-alsa-{}.conf", std::process::id()));
            std::fs::write(&config, "pcm.!default { type null }\n").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::tests::local_shuffle_repeats_through_the_engine_and_keeps_cached_audio", "--nocapture"])
                .env("PLAVNO_LOCAL_TEST_CHILD", "1")
                .env("ALSA_CONFIG_PATH", &config)
                .output().unwrap();
            std::fs::remove_file(config).unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = std::env::temp_dir().join(format!("plavno-local-engine-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"),
            root.join("single.flac"),
        )
        .unwrap();
        let backend = crate::backend::LocalDirectory::new(root.clone()).unwrap();
        let services = backend.services();
        let backend =
            crate::backend::Multi::new(vec![Box::new(backend)], services, Some("local")).unwrap();
        let (mut engine, _, events) = Engine::new(Box::new(backend)).unwrap();
        engine.paused = true;
        engine.start();
        pump(&mut engine, &events, |engine| {
            engine.current.is_some() && !engine.queue.upcoming.is_empty()
        });
        assert!(!engine.can_tune_wave());
        let id = engine.track().unwrap().id.clone();
        let cached = engine.current.as_ref().unwrap().buffer.clone();
        for _ in 0..5 {
            engine.control(Command::Next);
            pump(&mut engine, &events, |engine| {
                engine.current.is_some() && !engine.queue.upcoming.is_empty()
            });
            assert_eq!(engine.track().unwrap().id, id);
            assert!(Arc::ptr_eq(
                &cached,
                &engine.current.as_ref().unwrap().buffer
            ));
            assert!(!engine.exhausted);
        }
        let done = engine.shutdown();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn vk_supports_my_music_shuffle() {
        let (mut engine, _, _) =
            Engine::new(Box::new(crate::backend::Vk::new(std::path::PathBuf::new()))).unwrap();
        assert!(engine.can_like());
        assert!(engine.can_shuffle_favorites());
        let epoch = engine.epoch;
        engine.start_mode(PlaybackMode::FavoritesShuffle);
        assert_eq!(engine.epoch, epoch + 1);
        assert_eq!(engine.mode, PlaybackMode::FavoritesShuffle);
    }

    #[test]
    fn previous_then_next_replays_tracks_without_losing_the_wave_queue() {
        let mut queue = TrackQueue {
            upcoming: (1..=4).map(track).collect(),
            ..TrackQueue::default()
        };
        let first = queue.next(None).unwrap();
        let second = queue.next(Some(first)).unwrap();
        let third = queue.next(Some(second)).unwrap();
        let second = queue.previous(Some(&third)).unwrap();
        let first = queue.previous(Some(&second)).unwrap();
        assert_eq!(first.id, "1");
        let second = queue.next(Some(first)).unwrap();
        assert_eq!(second.id, "2");
        assert_eq!(second.batch, "batch-2");
        let third = queue.next(Some(second)).unwrap();
        assert_eq!(third.id, "3");
        assert_eq!(third.batch, "batch-3");
        assert_eq!(queue.next(Some(third)).unwrap().id, "4");
    }

    #[test]
    fn previous_at_the_start_preserves_the_upcoming_queue() {
        let mut queue = TrackQueue {
            upcoming: (1..=3).map(track).collect(),
            ..TrackQueue::default()
        };
        let current = queue.next(None).unwrap();
        assert!(queue.previous(Some(&current)).is_none());
        assert_eq!(queue.next(Some(current)).unwrap().id, "2");
    }

    #[test]
    fn previous_can_recover_the_last_track_after_the_queue_ends() {
        let mut queue = TrackQueue {
            upcoming: [track(1)].into(),
            ..TrackQueue::default()
        };
        let first = queue.next(None).unwrap();
        assert!(queue.next(Some(first)).is_none());
        assert_eq!(queue.previous(None).unwrap().id, "1");
    }

    #[test]
    fn playback_history_is_bounded() {
        let mut queue = TrackQueue::default();
        for id in 1..=HISTORY_LIMIT + 3 {
            queue.next(Some(track(id)));
        }
        assert_eq!(queue.past.len(), HISTORY_LIMIT);
        assert_eq!(queue.past.front().unwrap().id, "4");
        assert_eq!(queue.previous(Some(&track(200))).unwrap().id, "103");
        assert_eq!(queue.next(None).unwrap().id, "200");
    }

    #[derive(Default)]
    struct ServiceLog {
        wave: usize,
        mode: PlaybackMode,
        resolved: Vec<(String, String)>,
        feedback: Vec<(String, String, Feedback, f64)>,
        gate: Option<(String, Receiver<()>)>,
        fail: Option<String>,
        rate_limited: bool,
        favorite: bool,
        favorite_changes: usize,
        favorite_fail: bool,
        dislikes: Vec<(String, bool)>,
        dislike_fail: bool,
        more_batches: VecDeque<Vec<Track>>,
        more_history: Vec<Vec<String>>,
    }

    struct TestBackend(Arc<Mutex<ServiceLog>>);

    impl Backend for TestBackend {
        fn name(&self) -> &'static str {
            "Test"
        }

        fn supports_favorites(&self) -> bool {
            true
        }
        fn supports_favorites_shuffle(&self) -> bool {
            true
        }

        fn favorite_state(&mut self, track: &Track) -> Result<Option<bool>> {
            let log = self.0.lock().unwrap();
            Ok(Some(if log.mode == PlaybackMode::FavoritesShuffle {
                track.id != "1" || log.favorite
            } else {
                track.id == "1" && log.favorite
            }))
        }

        fn toggle_favorite(&mut self, _: &Track) -> Result<bool> {
            let mut log = self.0.lock().unwrap();
            log.favorite_changes += 1;
            if log.favorite_fail {
                anyhow::bail!("Favorite change failed");
            }
            log.favorite = !log.favorite;
            Ok(log.favorite)
        }
        fn set_dislike(&mut self, track: &Track, disliked: bool) -> Result<bool> {
            let mut log = self.0.lock().unwrap();
            log.dislikes.push((track.id.clone(), disliked));
            anyhow::ensure!(!log.dislike_fail, "Dislike sync failed");
            Ok(true)
        }

        fn start(&mut self) -> Result<Vec<Track>> {
            let mut log = self.0.lock().unwrap();
            log.mode = PlaybackMode::MyVibe;
            log.wave += 1;
            Ok((1..=3)
                .map(|id| {
                    let mut track = track(id);
                    track.batch = format!("wave-{}", log.wave);
                    track.duration = Some(60.0);
                    track
                })
                .collect())
        }

        fn start_mode(&mut self, mode: PlaybackMode) -> Result<Vec<Track>> {
            if mode == PlaybackMode::MyVibe {
                return self.start();
            }
            let mut log = self.0.lock().unwrap();
            log.mode = mode;
            log.favorite = true;
            Ok((1..=3).map(favorite_track).collect())
        }

        fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
            let mut log = self.0.lock().unwrap();
            log.more_history.push(history.to_vec());
            if let Some(batch) = log.more_batches.pop_front() {
                return Ok(batch);
            }
            Ok(if log.mode == PlaybackMode::FavoritesShuffle {
                (1..=3)
                    .filter(|id| *id != 1 || log.favorite)
                    .map(favorite_track)
                    .collect()
            } else {
                Vec::new()
            })
        }

        fn resolve(&mut self, track: &Track) -> Result<Input> {
            let gate = {
                let mut log = self.0.lock().unwrap();
                log.resolved.push((track.cache_key(), track.batch.clone()));
                if log.fail.as_ref() == Some(&track.cache_key()) {
                    log.fail = None;
                    if log.rate_limited {
                        return Err(
                            crate::backend::RateLimited::new(Duration::from_secs(60)).into()
                        );
                    }
                    anyhow::bail!("Temporary error");
                }
                if log
                    .gate
                    .as_ref()
                    .is_some_and(|(id, _)| id == &track.cache_key())
                {
                    log.gate.take().map(|(_, receiver)| receiver)
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                gate.recv_timeout(Duration::from_secs(5))?;
            }
            Ok(Input::File(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"),
            ))
        }

        fn feedback(&mut self, track: &Track, kind: Feedback, seconds: f64) -> Result<()> {
            self.0.lock().unwrap().feedback.push((
                track.cache_key(),
                track.batch.clone(),
                kind,
                seconds,
            ));
            Ok(())
        }
    }

    fn pump(engine: &mut Engine, events: &Receiver<Event>, done: impl Fn(&Engine) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(engine) {
            assert!(
                Instant::now() < deadline,
                "Engine did not reach expected state: {}",
                engine.message
            );
            if let Ok(event) = events.recv_timeout(Duration::from_millis(10)) {
                engine.handle(event);
            }
        }
    }

    #[test]
    fn scrubbing_coalesces_cached_previews_and_restores_exact_position_pause_and_feedback() {
        if std::env::var_os("PLAVNO_SCRUB_TEST_CHILD").is_none() {
            let config =
                std::env::temp_dir().join(format!("plavno-scrub-alsa-{}.conf", std::process::id()));
            std::fs::write(&config, "pcm.!default { type null }\n").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::tests::scrubbing_coalesces_cached_previews_and_restores_exact_position_pause_and_feedback", "--nocapture"])
                .env("PLAVNO_SCRUB_TEST_CHILD", "1")
                .env("ALSA_CONFIG_PATH", &config)
                .output();
            std::fs::remove_file(config).unwrap();
            let result = result.unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let log = Arc::new(Mutex::new(ServiceLog::default()));
        let (mut engine, _, events) = Engine::new(Box::new(TestBackend(log.clone()))).unwrap();
        engine.paused = true;
        assert_eq!(engine.speed, 1.0);
        for _ in 0..19 {
            engine.change_speed(-0.05);
        }
        assert_eq!(engine.speed, MIN_SPEED as f32);
        assert_eq!(engine.audio.sink.speed(), MIN_SPEED as f32);
        for _ in 0..39 {
            engine.change_speed(0.05);
        }
        assert_eq!(engine.speed, MAX_SPEED as f32);
        for speed in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            engine.control(Command::Rate(speed));
            assert_eq!(engine.speed, MAX_SPEED as f32);
        }
        engine.control(Command::Rate(1.25));
        engine.start();
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.audio.sink.speed(), 1.25);
        let playing = engine.current.as_ref().unwrap();
        playing
            .progress
            .samples
            .store(playing.samples_per_second * 2, Ordering::Relaxed);
        let playback = engine.playback_id();
        let resolved = log.lock().unwrap().resolved.len();
        engine.scrub_to(12.0, 4.0);
        let serial = engine.active_serial;
        assert_eq!(engine.seconds(), 12.0);
        // A busy pointer replaces only the pending target, even while decoding is in flight.
        for step in 1..=100 {
            engine.scrub_to(12.0 + f64::from(step) / 10.0, -6.0);
        }
        assert_eq!(engine.active_serial, serial);
        assert_eq!(engine.seconds(), 22.0);
        pump(&mut engine, &events, |engine| !engine.preparing);
        assert!(engine.is_scrubbing());
        assert!(engine.paused);
        assert!(!engine.audio.sink.is_paused());
        assert_eq!(engine.audio.sink.speed(), 4.0);
        engine.change_speed(0.05);
        assert_eq!(engine.speed, 1.3);
        assert_eq!(engine.audio.sink.speed(), 4.0);
        engine
            .scrubbing
            .as_mut()
            .unwrap()
            .dispatched
            .as_mut()
            .unwrap()
            .1 = Instant::now() - Duration::from_millis(100);
        assert!(engine.tick());
        pump(&mut engine, &events, |engine| !engine.preparing);
        assert_eq!(engine.audio.sink.speed(), 4.0);
        engine.audio.sink.stop();
        assert!(!engine.tick(), "A finished preview advanced the track");
        assert_eq!(engine.playback_id(), playback);
        // Finish while the final preview is still preparing, as a rapid mouse release does.
        engine
            .scrubbing
            .as_mut()
            .unwrap()
            .dispatched
            .as_mut()
            .unwrap()
            .1 = Instant::now() - Duration::from_millis(100);
        engine.scrub_to(23.0, -8.0);
        assert!(engine.preparing);
        engine.end_scrub();
        pump(&mut engine, &events, |engine| !engine.preparing);
        assert!(!engine.is_scrubbing());
        assert_eq!(engine.seconds(), 23.0);
        assert!(engine.paused && engine.audio.sink.is_paused());
        assert_eq!(engine.audio.sink.speed(), 1.3);
        assert_eq!(engine.current.as_ref().unwrap().listened(), 2.0);
        assert_eq!(engine.playback_id(), playback);
        assert_eq!(engine.history.len(), 1);
        assert_eq!(log.lock().unwrap().resolved.len(), resolved);
        assert_eq!(log.lock().unwrap().feedback.len(), 1);
        engine.control(Command::Play);
        engine.scrub_to(24.0, 3.0);
        pump(&mut engine, &events, |engine| !engine.preparing);
        engine.end_scrub();
        pump(&mut engine, &events, |engine| !engine.preparing);
        assert!(!engine.paused && !engine.audio.sink.is_paused());
        assert_eq!(engine.audio.sink.speed(), 1.3);
        engine.pause();
        engine.next();
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.audio.sink.speed(), 1.3);
        engine.seek_to(10.0);
        pump(&mut engine, &events, |engine| !engine.preparing);
        assert_eq!(engine.seconds(), 10.0);
        assert_eq!(engine.audio.sink.speed(), 1.3);
    }

    fn prefetch_ready(engine: &Engine) -> bool {
        engine
            .prefetch
            .as_ref()
            .is_some_and(|p| matches!(p.result, Some(Ok(_))))
    }

    // ALSA's null device runs in a child process, so tests never change global audio settings.
    #[cfg(target_os = "linux")]
    #[test]
    fn dislikes_filter_navigation_history_and_favorites_without_depending_on_service_sync() {
        if std::env::var_os("PLAVNO_DISLIKE_TEST_CHILD").is_none() {
            let config = std::env::temp_dir()
                .join(format!("plavno-dislike-alsa-{}.conf", std::process::id()));
            std::fs::write(&config, "pcm.!default { type null }\n").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::tests::dislikes_filter_navigation_history_and_favorites_without_depending_on_service_sync", "--nocapture"])
                .env("PLAVNO_DISLIKE_TEST_CHILD", "1").env("ALSA_CONFIG_PATH", &config).output();
            std::fs::remove_file(config).unwrap();
            let result = result.unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let log = Arc::new(Mutex::new(ServiceLog {
            dislike_fail: true,
            ..ServiceLog::default()
        }));
        let (mut engine, _, events) = Engine::new(Box::new(TestBackend(log.clone()))).unwrap();
        engine.paused = true;
        engine.start();
        pump(&mut engine, &events, prefetch_ready);
        let entry = engine.history[0].clone();
        let old_buffer = engine.current.as_ref().unwrap().buffer.clone();
        let next_cancel = engine.prefetch.as_ref().unwrap().cancel.clone();
        engine.dislike();
        assert_eq!(engine.track().unwrap().id, "2");
        assert!(!next_cancel.load(Ordering::Relaxed));
        assert_eq!(old_buffer.buffered_bytes(), 0);
        assert!(engine.dislikes.contains(engine.source, &track(1)));
        assert!(!engine.queue.past.iter().any(|track| track.id == "1"));
        pump(&mut engine, &events, |engine| {
            engine.message.contains("service sync failed")
        });
        assert!(engine.dislikes.contains(engine.source, &track(1)));
        let playback = engine.playback_id();
        engine.play_history(&entry);
        assert_eq!(engine.playback_id(), playback);
        assert_eq!(engine.track().unwrap().id, "2");
        assert!(engine.message.contains("disliked"));
        engine.mode = PlaybackMode::FavoritesShuffle;
        engine.handle(Event::Batch(engine.epoch, Ok(vec![track(1), track(4)])));
        assert!(!engine.queue.upcoming.iter().any(|track| track.id == "1"));
        assert!(engine.queue.upcoming.iter().any(|track| track.id == "4"));
        let mut foreign = track(1);
        foreign.source = "vk".into();
        engine.handle(Event::Batch(engine.epoch, Ok(vec![foreign])));
        assert!(
            engine
                .queue
                .upcoming
                .iter()
                .any(|track| track.source == "vk" && track.id == "1")
        );
        log.lock().unwrap().dislike_fail = false;
        engine.undo_dislike();
        assert!(!engine.dislikes.contains(engine.source, &track(1)));
        assert_eq!(engine.playback_id(), playback);
        pump(&mut engine, &events, |engine| {
            engine.message.contains("synced with service")
        });
        let message = engine.message.clone();
        let mut stale = track(1);
        stale.source = engine.source.into();
        engine.handle(Event::Dislike(
            stale,
            true,
            Err(anyhow::anyhow!("stale failure")),
        ));
        assert_eq!(engine.message, message);
        engine.play_history(&entry);
        pump(&mut engine, &events, |engine| {
            engine.track().is_some_and(|track| track.id == "1")
        });
        assert_eq!(
            log.lock().unwrap().dislikes,
            [("1".into(), true), ("1".into(), false)]
        );
        engine.control(Command::Stop);
        engine.dislike();
        assert_eq!(engine.playback_status(), "Stopped");
        assert!(engine.track().is_none());
        assert!(engine.media_track().is_none_or(|track| track.id != "1"));

        let log = Arc::new(Mutex::new(ServiceLog {
            more_batches: VecDeque::from([vec![track(1)], vec![track(4)]]),
            ..ServiceLog::default()
        }));
        let (mut engine, _, events) = Engine::new(Box::new(TestBackend(log.clone()))).unwrap();
        engine.paused = true;
        for id in 1..=3 {
            engine.dislikes.block(engine.source, &track(id)).unwrap();
        }
        engine.start();
        pump(&mut engine, &events, |engine| {
            engine.track().is_some_and(|track| track.id == "4")
        });
        assert!(log.lock().unwrap().more_history[0].contains(&"1".into()));

        let log = Arc::new(Mutex::new(ServiceLog {
            more_batches: (0..10).map(|_| (1..=3).map(track).collect()).collect(),
            ..ServiceLog::default()
        }));
        let (mut engine, _, events) = Engine::new(Box::new(TestBackend(log.clone()))).unwrap();
        for id in 1..=3 {
            engine.dislikes.block(engine.source, &track(id)).unwrap();
        }
        engine.start();
        pump(&mut engine, &events, |engine| {
            engine.exhausted && !engine.batch_pending && !engine.loading
        });
        assert_eq!(log.lock().unwrap().more_history.len(), 3);
        assert!(log.lock().unwrap().resolved.is_empty());
    }

    #[test]
    fn prefetch_reuses_prepared_and_pending_streams_and_cancels_stale_work() {
        if std::env::var_os("PLAVNO_PREFETCH_TEST_CHILD").is_none() {
            let config =
                std::env::temp_dir().join(format!("plavno-test-alsa-{}.conf", std::process::id()));
            std::fs::write(&config, "pcm.!default { type null }\n").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::tests::prefetch_reuses_prepared_and_pending_streams_and_cancels_stale_work", "--nocapture"])
                .env("PLAVNO_PREFETCH_TEST_CHILD", "1")
                .env("ALSA_CONFIG_PATH", &config)
                .output();
            std::fs::remove_file(config).unwrap();
            let result = result.unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        vk_supports_my_music_shuffle();
        let throttled = Arc::new(Mutex::new(ServiceLog {
            fail: Some("2".into()),
            rate_limited: true,
            ..ServiceLog::default()
        }));
        let (mut limited_engine, _, limited_events) =
            Engine::new(Box::new(TestBackend(throttled.clone()))).unwrap();
        limited_engine.paused = true;
        limited_engine.start();
        pump(&mut limited_engine, &limited_events, |e| {
            e.prefetch
                .as_ref()
                .is_some_and(|p| matches!(p.result, Some(Err(_))))
        });
        limited_engine.next();
        pump(&mut limited_engine, &limited_events, |e| !e.preparing);
        assert_eq!(
            throttled
                .lock()
                .unwrap()
                .resolved
                .iter()
                .filter(|(id, _)| id == "2")
                .count(),
            1,
            "A throttled prefetch was resolved again immediately"
        );
        assert!(limited_engine.message.contains("429"));
        drop(limited_engine);
        let log = Arc::new(Mutex::new(ServiceLog {
            favorite: true,
            ..ServiceLog::default()
        }));
        let (mut engine, _, events) = Engine::new(Box::new(TestBackend(log.clone()))).unwrap();
        engine.paused = true;
        engine.start();
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, "1");
        pump(&mut engine, &events, |e| e.favorite_state().is_some());
        assert!(
            engine.is_liked(),
            "Existing server favorite is not displayed"
        );
        engine.like();
        engine.like();
        pump(&mut engine, &events, |e| !e.like_pending);
        assert!(!engine.is_liked());
        assert!(engine.message.contains("Removed"));
        assert_eq!(
            log.lock().unwrap().favorite_changes,
            1,
            "Duplicate favorite mutation"
        );
        engine.like();
        pump(&mut engine, &events, |e| !e.like_pending);
        assert!(engine.is_liked());
        log.lock().unwrap().favorite_fail = true;
        engine.like();
        pump(&mut engine, &events, |e| !e.like_pending);
        assert!(
            engine.is_liked(),
            "Failure changed the confirmed favorite state"
        );
        assert!(engine.message.contains("failed"));
        log.lock().unwrap().favorite_fail = false;
        engine.handle(Event::Favorite(
            engine.epoch - 1,
            track(1),
            true,
            Ok(Some(false)),
        ));
        assert!(
            engine.is_liked(),
            "Stale response changed the current session"
        );
        engine.message.clear();
        {
            let log = log.lock().unwrap();
            assert_eq!(
                log.resolved,
                [("1".into(), "wave-1".into()), ("2".into(), "wave-1".into())]
            );
            assert_eq!(log.feedback.len(), 1);
            assert!(matches!(log.feedback[0].2, Feedback::Started));
            assert_eq!(log.feedback[0].0, "1");
        }
        let playback_id = engine.playback_id();
        assert_eq!(engine.history.len(), 1, "Prefetch appears in history");
        let saved_entry = engine.history[0].clone();
        let prefetched = engine.prefetch.as_ref().unwrap().cancel.clone();
        let playing = engine.current.as_ref().unwrap();
        playing
            .progress
            .samples
            .store(playing.samples_per_second * 2, Ordering::Relaxed);
        engine.control(Command::Seek(10.0));
        engine.control(Command::Seek(5.0));
        assert_eq!(engine.seconds(), 17.0);
        pump(&mut engine, &events, |e| !e.preparing);
        assert!(engine.seeking.is_none());
        assert_eq!(engine.seconds(), 17.0);
        assert_eq!(engine.playback_id(), playback_id);
        assert!(engine.paused);
        assert!(!prefetched.load(Ordering::Relaxed));
        assert_eq!(log.lock().unwrap().resolved.len(), 2);
        engine.control(Command::SetPosition((99, 99), 3.0));
        engine.seek_to(-1.0);
        engine.seek_to(61.0);
        assert_eq!(engine.seconds(), 17.0);
        engine.control(Command::SetPosition(playback_id, 3.0));
        pump(&mut engine, &events, |e| !e.preparing);
        assert_eq!(engine.seconds(), 3.0);
        assert_eq!(engine.seek_revision, 2);
        assert_eq!(engine.history.len(), 1, "Seek duplicates history");
        assert_eq!(
            log.lock().unwrap().feedback.len(),
            1,
            "Seek sends playback feedback"
        );
        let playing = engine.current.as_ref().unwrap();
        playing
            .progress
            .samples
            .store(playing.samples_per_second / 2, Ordering::Relaxed);
        assert_eq!(playing.listened(), 2.5);
        engine.next();
        assert_eq!(engine.track().unwrap().id, "2");
        assert!(!engine.loading);
        assert!(engine.paused);
        assert_eq!(engine.volume, 1.0);
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.cache.len(), 3);
        assert!(engine.cache.contains_key("1"));
        let evicted = engine.cache.get("3").unwrap().clone();
        let stale = engine.prefetch.as_ref().unwrap();
        let stale_serial = stale.serial;
        let stale_cancel = stale.cancel.clone();
        engine.previous();
        assert!(stale_cancel.load(Ordering::Relaxed));
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, "1");
        assert_eq!(engine.cache.len(), 2);
        assert!(!engine.cache.contains_key("3"));
        assert_eq!(evicted.buffered_bytes(), 0, "Evicted audio remains in RAM");
        for id in ["1", "2"] {
            assert_eq!(
                log.lock()
                    .unwrap()
                    .resolved
                    .iter()
                    .filter(|(resolved, batch)| resolved == id && batch == "wave-1")
                    .count(),
                1,
                "Returning to a cached track resolved its stream again"
            );
        }
        engine.handle(Event::Prepared(
            engine.epoch,
            stale_serial,
            track(3),
            Err(anyhow::anyhow!("Stale error")),
        ));
        assert!(engine.message.is_empty());

        let (release, gate) = mpsc::channel();
        log.lock().unwrap().gate = Some(("3".into(), gate));
        engine.next();
        pump(&mut engine, &events, |_| {
            log.lock()
                .unwrap()
                .resolved
                .iter()
                .filter(|(id, _)| id == "3")
                .count()
                == 2
        });
        let pending_serial = engine.prefetch.as_ref().unwrap().serial;
        engine.next();
        assert!(engine.loading && engine.preparing);
        assert!(engine.current.is_none());
        assert!(engine.prefetch.is_none());
        assert_eq!(engine.active_serial, pending_serial);
        release.send(()).unwrap();
        pump(&mut engine, &events, |e| {
            e.track().is_some_and(|t| t.id == "3")
        });
        assert_eq!(
            log.lock()
                .unwrap()
                .resolved
                .iter()
                .filter(|(id, _)| id == "3")
                .count(),
            2
        );

        let (release, gate) = mpsc::channel();
        log.lock().unwrap().gate = Some(("1".into(), gate));
        engine.previous();
        pump(&mut engine, &events, |_| {
            log.lock()
                .unwrap()
                .resolved
                .iter()
                .filter(|(id, _)| id == "1")
                .count()
                == 2
        });
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, "2");
        engine.seek_to(6.0);
        pump(&mut engine, &events, |e| !e.preparing);
        assert_eq!(
            engine.seconds(),
            6.0,
            "A blocked backend delayed seeking cached audio"
        );
        let cancelled = engine.prefetch.as_ref().unwrap().cancel.clone();
        engine.start();
        assert!(cancelled.load(Ordering::Relaxed));
        release.send(()).unwrap();
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().batch, "wave-2");
        assert_eq!(engine.prefetch.as_ref().unwrap().track.batch, "wave-2");

        let cancelled = engine.prefetch.as_ref().unwrap().cancel.clone();
        log.lock().unwrap().fail = Some("2".into());
        engine.start();
        assert!(cancelled.load(Ordering::Relaxed));
        pump(&mut engine, &events, |e| {
            e.prefetch
                .as_ref()
                .is_some_and(|p| matches!(p.result, Some(Err(_))))
        });
        assert_eq!(engine.track().unwrap().id, "1");
        assert!(engine.message.is_empty());
        engine.next();
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, "2");
        assert_eq!(
            log.lock()
                .unwrap()
                .resolved
                .iter()
                .filter(|(id, batch)| id == "2" && batch == "wave-3")
                .count(),
            2
        );
        let prefetch = engine.prefetch.as_ref().unwrap();
        engine.handle(Event::Prepared(
            engine.epoch,
            prefetch.serial,
            prefetch.track.clone(),
            Err(anyhow::anyhow!("Prefetched audio read failed")),
        ));
        assert!(!engine.cache.contains_key("3"));
        engine.next();
        pump(&mut engine, &events, |e| {
            e.track().is_some_and(|t| t.id == "3") && !e.preparing
        });
        assert_eq!(
            log.lock()
                .unwrap()
                .resolved
                .iter()
                .filter(|(id, batch)| id == "3" && batch == "wave-3")
                .count(),
            2,
            "Retry reused a failed audio cache"
        );
        engine.previous();
        pump(&mut engine, &events, prefetch_ready);
        engine.control(Command::Pause);
        engine.control(Command::Pause);
        assert!(engine.paused);
        engine.control(Command::Volume(0.25));
        assert_eq!(engine.volume, 0.25);
        let cancelled = engine.prefetch.as_ref().unwrap().cancel.clone();
        engine.control(Command::Stop);
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(engine.playback_status(), "Stopped");
        assert!(engine.current.is_none());
        assert_eq!(engine.media_track().unwrap().id, "2");
        engine.handle(Event::Batch(engine.epoch, Ok(vec![track(4)])));
        assert_eq!(engine.playback_status(), "Stopped");
        engine.control(Command::Next);
        assert_eq!(engine.media_track().unwrap().id, "3");
        assert_eq!(engine.playback_status(), "Stopped");
        engine.control(Command::Previous);
        assert_eq!(engine.media_track().unwrap().id, "2");
        engine.control(Command::Play);
        engine.control(Command::Pause);
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, "2");
        assert_eq!(engine.playback_status(), "Paused");
        assert_eq!(engine.seconds(), 0.0);
        engine.advance(Feedback::Finished);
        assert_eq!(engine.track().unwrap().id, "3");
        assert!(!engine.loading);
        engine.previous();
        pump(&mut engine, &events, prefetch_ready);
        let before = engine.playback_id();
        let mut foreign = saved_entry.clone();
        foreign.source = "other-service".into();
        engine.play_history(&foreign);
        assert_eq!(engine.playback_id(), before);
        let upcoming: Vec<_> = engine.queue.upcoming.iter().map(Track::cache_key).collect();
        let history_count = engine.history.len();
        pump(&mut engine, &events, |e| {
            e.cache.contains_key(&saved_entry.track.id)
        });
        let resolutions = log
            .lock()
            .unwrap()
            .resolved
            .iter()
            .filter(|(id, _)| id == &saved_entry.track.id)
            .count();
        engine.play_history(&saved_entry);
        assert!(
            !engine.paused,
            "Selecting a history entry does not start playback"
        );
        engine.control(Command::Pause);
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, saved_entry.track.id);
        assert!(engine.track().unwrap().batch.is_empty());
        assert_eq!(engine.history.len(), history_count + 1);
        assert_eq!(
            engine
                .queue
                .upcoming
                .iter()
                .map(Track::cache_key)
                .collect::<Vec<_>>(),
            upcoming
        );
        assert_eq!(
            log.lock()
                .unwrap()
                .resolved
                .iter()
                .filter(|(id, _)| id == &saved_entry.track.id)
                .count(),
            resolutions,
            "History replay bypassed the existing audio buffer"
        );

        let saved_prefetch = engine.prefetch.take();
        let buffer = engine.current.as_ref().unwrap().buffer.clone();
        let mut same_track = engine.track().unwrap().clone();
        same_track.batch = "another-batch".into();
        engine.serial += 1;
        engine.prefetch = Some(Prefetch {
            serial: engine.serial,
            track: same_track.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
            result: None,
        });
        engine.handle(Event::Prepared(
            engine.epoch,
            engine.serial,
            same_track.clone(),
            Err(anyhow::anyhow!("Prefetch failed for the current track")),
        ));
        assert!(
            buffer.buffered_bytes() > 0,
            "A failed prefetch cleared the current track's shared buffer"
        );
        assert!(Arc::ptr_eq(
            engine.cache.get(&same_track.id).unwrap(),
            &buffer
        ));
        engine.prefetch = saved_prefetch;

        let cancelled = engine.prefetch.as_ref().unwrap().cancel.clone();
        let old_epoch = engine.epoch;
        engine.start_mode(PlaybackMode::FavoritesShuffle);
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(!engine.paused);
        assert_eq!(engine.mode, PlaybackMode::FavoritesShuffle);
        engine.paused = true;
        engine.handle(Event::Batch(old_epoch, Ok(vec![track(99)])));
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, "1");
        assert!(engine.track().unwrap().batch.is_empty());
        for expected in ["2", "3", "1"] {
            engine.next();
            pump(&mut engine, &events, prefetch_ready);
            assert_eq!(engine.track().unwrap().id, expected);
            assert!(!engine.exhausted, "Favorites stopped at the end of a pass");
            assert!(engine.cache.len() <= 3);
        }
        engine.more();
        pump(&mut engine, &events, |e| !e.batch_pending);
        assert!(engine.queue.upcoming.iter().any(|t| t.id == "1"));
        engine.like();
        pump(&mut engine, &events, |e| !e.like_pending);
        assert!(!engine.is_liked());
        assert_eq!(engine.track().unwrap().id, "1");
        assert!(engine.queue.upcoming.iter().all(|t| t.id != "1"));
        pump(&mut engine, &events, prefetch_ready);
        engine.next();
        engine.previous();
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().id, "1");
        assert!(!engine.is_liked());
        engine.start_mode(PlaybackMode::MyVibe);
        engine.paused = true;
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.mode, PlaybackMode::MyVibe);
        assert_eq!(engine.track().unwrap().batch, "wave-4");
        assert!(engine.queue.upcoming.iter().all(|t| t.id != "99"));
        let cancelled = engine.prefetch.as_ref().unwrap().cancel.clone();
        engine
            .shutdown()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(cancelled.load(Ordering::Relaxed));
        let log = log.lock().unwrap();
        assert!(
            log.feedback
                .iter()
                .all(|(id, batch, _, seconds)| *seconds == 0.0
                    || (id == "1" && batch == "wave-1" && *seconds == 2.5))
        );
        assert!(
            log.feedback
                .iter()
                .any(|(id, batch, kind, seconds)| id == "1"
                    && batch == "wave-1"
                    && matches!(kind, Feedback::Skipped)
                    && *seconds == 2.5)
        );
        assert!(log.feedback.iter().any(|(id, batch, kind, _)| id == "2"
            && batch == "wave-3"
            && matches!(kind, Feedback::Finished)));
        assert!(!log.feedback.iter().any(|(id, batch, kind, _)| id == "2"
            && batch == "wave-2"
            && matches!(kind, Feedback::Started)));
        drop(log);
        drop(engine);
        provider_navigation_and_history();
    }
    fn provider_navigation_and_history() {
        struct Named {
            key: &'static str,
            backend: TestBackend,
        }
        impl Backend for Named {
            fn key(&self) -> &'static str {
                self.key
            }
            fn name(&self) -> &'static str {
                self.key
            }
            fn supports_favorites(&self) -> bool {
                self.key == "yandex"
            }
            fn start(&mut self) -> Result<Vec<Track>> {
                self.backend.start()
            }
            fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
                self.backend.more(history)
            }
            fn resolve(&mut self, track: &Track) -> Result<Input> {
                self.backend.resolve(track)
            }
            fn feedback(&mut self, track: &Track, kind: Feedback, seconds: f64) -> Result<()> {
                self.backend.feedback(track, kind, seconds)
            }
            fn favorite_state(&mut self, track: &Track) -> Result<Option<bool>> {
                self.backend.favorite_state(track)
            }
            fn toggle_favorite(&mut self, track: &Track) -> Result<bool> {
                self.backend.toggle_favorite(track)
            }
        }
        let yandex = Arc::new(Mutex::new(ServiceLog {
            favorite: true,
            ..ServiceLog::default()
        }));
        let spotify = Arc::new(Mutex::new(ServiceLog::default()));
        let backends: Vec<Box<dyn Backend>> = vec![
            Box::new(Named {
                key: "yandex",
                backend: TestBackend(yandex.clone()),
            }),
            Box::new(Named {
                key: "spotify",
                backend: TestBackend(spotify.clone()),
            }),
        ];
        let services = backends.iter().flat_map(|b| b.services()).collect();
        let multi = crate::backend::Multi::new(backends, services, Some("yandex")).unwrap();
        let (mut engine, _, events) = Engine::new(Box::new(multi)).unwrap();
        engine.start();
        engine.paused = true;
        pump(&mut engine, &events, |e| prefetch_ready(e) && e.is_liked());
        let saved = engine.history.back().unwrap().clone();
        assert_eq!(saved.source, "yandex");
        engine.start_source("spotify");
        engine.paused = true;
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().source, "spotify");
        assert!(!engine.can_like());
        assert_eq!(engine.favorite_state(), None);
        assert_eq!(engine.history.back().unwrap().source, "spotify");
        let spotify_buffer = engine.current.as_ref().unwrap().buffer.clone();
        engine.play_history(&saved);
        engine.paused = true;
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().source, "yandex");
        assert!(engine.can_like());
        assert!(engine.is_liked());
        assert_eq!(
            engine.source, "spotify",
            "History replaced the selected service wave"
        );
        assert_eq!(engine.history.back().unwrap().source, "yandex");
        assert!(engine.cache.contains_key("yandex:1") && engine.cache.contains_key("spotify:1"));
        assert!(!Arc::ptr_eq(
            &engine.current.as_ref().unwrap().buffer,
            &spotify_buffer
        ));
        assert!(spotify_buffer.buffered_bytes() > 0);
        engine.previous();
        pump(&mut engine, &events, prefetch_ready);
        assert_eq!(engine.track().unwrap().source, "spotify");
        assert!(Arc::ptr_eq(
            &engine.current.as_ref().unwrap().buffer,
            &spotify_buffer
        ));
        engine
            .shutdown()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }
}
