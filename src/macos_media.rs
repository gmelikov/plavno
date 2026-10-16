use crate::engine::{Command, Engine, Event, MAX_SPEED, MIN_SPEED};
use anyhow::{Context, Result};
use block2::RcBlock;
use image::RgbImage;
use objc2::{
    AnyThread, MainThreadMarker,
    rc::{Retained, autoreleasepool},
    runtime::AnyObject,
};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSImage};
use objc2_foundation::{
    NSArray, NSData, NSDate, NSDefaultRunLoopMode, NSMutableDictionary, NSNumber, NSRunLoop,
    NSSize, NSString,
};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPChangePlaybackRateCommandEvent, MPMediaItemArtwork,
    MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyDefaultPlaybackRate,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus,
};
use std::{
    cell::RefCell,
    io::Cursor,
    ptr::NonNull,
    sync::{Arc, Mutex, mpsc::Sender},
};

type Handler = RcBlock<dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus>;

#[derive(Clone, PartialEq)]
struct Snapshot {
    id: (u64, u64),
    title: Option<String>,
    artist: String,
    duration: Option<f64>,
    position: f64,
    rate: f64,
    status: &'static str,
    capabilities: [bool; 5],
    seek_revision: u64,
}

impl Snapshot {
    fn from_engine(engine: &Engine) -> Self {
        let track = engine.media_track();
        Self {
            id: engine.playback_id(),
            title: track.map(|track| track.title.clone()),
            artist: track.map_or_else(String::new, |track| track.artist.clone()),
            duration: track
                .and_then(|track| track.duration)
                .filter(|d| d.is_finite() && *d >= 0.0),
            position: engine.seconds().max(0.0),
            rate: f64::from(engine.speed),
            status: engine.playback_status(),
            capabilities: [
                engine.can_next(),
                engine.can_previous(),
                engine.can_play(),
                engine.can_pause(),
                engine.can_seek(),
            ],
            seek_revision: engine.seek_revision,
        }
    }

    fn enabled(&self) -> [bool; 8] {
        let [next, previous, play, pause, seek] = self.capabilities;
        [
            play,
            pause,
            play || pause,
            self.title.is_some(),
            next,
            previous,
            seek,
            self.title.is_some(),
        ]
    }
}

#[derive(Clone, Copy)]
enum Action {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Position(f64),
    Rate(f64),
}

fn dispatch(
    action: Action,
    snapshot: &Mutex<Option<Snapshot>>,
    events: &Sender<Event>,
) -> MPRemoteCommandHandlerStatus {
    let state = snapshot.lock().unwrap();
    let Some(state) = state.as_ref() else {
        return MPRemoteCommandHandlerStatus::NoActionableNowPlayingItem;
    };
    let enabled = state.enabled();
    let (index, command) = match action {
        Action::Play => (0, Command::Play),
        Action::Pause => (1, Command::Pause),
        Action::Toggle => (2, Command::PlayPause),
        Action::Stop => (3, Command::Stop),
        Action::Next => (4, Command::Next),
        Action::Previous => (5, Command::Previous),
        Action::Position(position)
            if position.is_finite()
                && position >= 0.0
                && state.duration.is_none_or(|d| position <= d) =>
        {
            (6, Command::SetPosition(state.id, position))
        }
        Action::Rate(rate) if rate.is_finite() && (MIN_SPEED..=MAX_SPEED).contains(&rate) => {
            (7, Command::Rate(rate))
        }
        _ => return MPRemoteCommandHandlerStatus::CommandFailed,
    };
    if !enabled[index] {
        return MPRemoteCommandHandlerStatus::NoActionableNowPlayingItem;
    }
    if events.send(Event::Control(command)).is_ok() {
        MPRemoteCommandHandlerStatus::Success
    } else {
        MPRemoteCommandHandlerStatus::CommandFailed
    }
}

struct Binding {
    command: Retained<MPRemoteCommand>,
    target: Retained<AnyObject>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        // Remove only our target, including when initialization exits early.
        unsafe {
            self.command.removeTarget(Some(&self.target));
            self.command.setEnabled(false);
        }
    }
}

pub struct MediaControls {
    _application: Retained<NSApplication>,
    center: Retained<MPNowPlayingInfoCenter>,
    bindings: Vec<Binding>,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    last: RefCell<Option<Snapshot>>,
    artwork: RefCell<Option<(Arc<RgbImage>, Retained<MPMediaItemArtwork>)>>,
}

impl MediaControls {
    pub fn new(engine: &Engine, events: Sender<Event>) -> Result<Self> {
        Self::create(Snapshot::from_engine(engine), events)
    }

    fn create(initial: Snapshot, events: Sender<Event>) -> Result<Self> {
        let marker =
            MainThreadMarker::new().context("macOS media controls require the main thread")?;
        let application = NSApplication::sharedApplication(marker);
        application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        application.finishLaunching();
        let snapshot = Arc::new(Mutex::new(Some(initial)));
        let mut bindings = Vec::new();
        // All Objective-C objects stay on the main thread. Remote blocks only enqueue
        // commands; neither the audio engine nor the UI is called from a Cocoa callback.
        unsafe {
            let commands = MPRemoteCommandCenter::sharedCommandCenter();
            let entries = [
                (commands.playCommand(), Action::Play),
                (commands.pauseCommand(), Action::Pause),
                (commands.togglePlayPauseCommand(), Action::Toggle),
                (commands.stopCommand(), Action::Stop),
                (commands.nextTrackCommand(), Action::Next),
                (commands.previousTrackCommand(), Action::Previous),
            ];
            for (command, action) in entries {
                Self::bind(
                    &mut bindings,
                    command,
                    snapshot.clone(),
                    events.clone(),
                    move |_| Some(action),
                );
            }
            Self::bind(
                &mut bindings,
                Retained::into_super(commands.changePlaybackPositionCommand()),
                snapshot.clone(),
                events.clone(),
                |event| {
                    event
                        .downcast_ref::<MPChangePlaybackPositionCommandEvent>()
                        .map(|event| Action::Position(event.positionTime()))
                },
            );
            let rate = commands.changePlaybackRateCommand();
            let rates = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0].map(NSNumber::new_f64);
            rate.setSupportedPlaybackRates(&NSArray::from_retained_slice(&rates));
            Self::bind(
                &mut bindings,
                Retained::into_super(rate),
                snapshot.clone(),
                events,
                |event| {
                    event
                        .downcast_ref::<MPChangePlaybackRateCommandEvent>()
                        .map(|event| Action::Rate(f64::from(event.playbackRate())))
                },
            );
            let controls = Self {
                _application: application,
                center: MPNowPlayingInfoCenter::defaultCenter(),
                bindings,
                snapshot,
                last: RefCell::new(None),
                artwork: RefCell::new(None),
            };
            let state = controls.snapshot.lock().unwrap().as_ref().unwrap().clone();
            controls.publish(state, None);
            Ok(controls)
        }
    }

    unsafe fn bind(
        bindings: &mut Vec<Binding>,
        command: Retained<MPRemoteCommand>,
        snapshot: Arc<Mutex<Option<Snapshot>>>,
        events: Sender<Event>,
        action: impl Fn(&MPRemoteCommandEvent) -> Option<Action> + 'static,
    ) {
        let handler: Handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            // MediaPlayer supplies a live event for the duration of this callback.
            let action = action(unsafe { event.as_ref() });
            action.map_or(MPRemoteCommandHandlerStatus::CommandFailed, |action| {
                dispatch(action, &snapshot, &events)
            })
        });
        let target = unsafe { command.addTargetWithHandler(&handler) };
        bindings.push(Binding { command, target });
    }

    pub fn poll(&self) {
        autoreleasepool(|_| {
            // A nonblocking pass also services the main dispatch queue while the TUI
            // owns the main thread; paused/hidden views still poll at least every 50ms.
            NSRunLoop::mainRunLoop()
                .runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &NSDate::distantPast());
        });
    }

    pub fn update(&self, engine: &Engine, cover: Option<&Arc<RgbImage>>) {
        self.publish(Snapshot::from_engine(engine), cover);
    }

    fn publish(&self, snapshot: Snapshot, cover: Option<&Arc<RgbImage>>) {
        *self.snapshot.lock().unwrap() = Some(snapshot.clone());
        autoreleasepool(|_| unsafe {
            for (binding, enabled) in self.bindings.iter().zip(snapshot.enabled()) {
                binding.command.setEnabled(enabled);
            }
            let cover = if snapshot.status != "Stopped" {
                cover
            } else {
                None
            };
            let same_cover = match (&*self.artwork.borrow(), cover) {
                (Some((old, _)), Some(new)) => Arc::ptr_eq(old, new),
                (None, None) => true,
                _ => false,
            };
            if !same_cover {
                *self.artwork.borrow_mut() =
                    cover.and_then(|cover| make_artwork(cover).map(|art| (cover.clone(), art)));
            }
            // The OS advances elapsed time itself; only resynchronise once per second,
            // or immediately after a seek, state change, track change or new cover.
            let mut stamp = snapshot.clone();
            stamp.position = stamp.position.floor();
            if self.last.borrow().as_ref() == Some(&stamp) && same_cover {
                return;
            }
            if let Some(title) = &snapshot.title
                && snapshot.status != "Stopped"
            {
                let info = NSMutableDictionary::<NSString, AnyObject>::new();
                info.insert(MPMediaItemPropertyTitle, &NSString::from_str(title));
                info.insert(
                    MPMediaItemPropertyArtist,
                    &NSString::from_str(&snapshot.artist),
                );
                if let Some(duration) = snapshot.duration {
                    info.insert(
                        MPMediaItemPropertyPlaybackDuration,
                        &NSNumber::new_f64(duration),
                    );
                }
                info.insert(
                    MPNowPlayingInfoPropertyElapsedPlaybackTime,
                    &NSNumber::new_f64(snapshot.position),
                );
                info.insert(
                    MPNowPlayingInfoPropertyPlaybackRate,
                    &NSNumber::new_f64(if snapshot.status == "Playing" {
                        snapshot.rate
                    } else {
                        0.0
                    }),
                );
                info.insert(
                    MPNowPlayingInfoPropertyDefaultPlaybackRate,
                    &NSNumber::new_f64(snapshot.rate),
                );
                if let Some((_, artwork)) = &*self.artwork.borrow() {
                    info.insert(MPMediaItemPropertyArtwork, &**artwork);
                }
                self.center.setNowPlayingInfo(Some(&info));
            } else {
                self.center.setNowPlayingInfo(None);
            }
            self.center.setPlaybackState(match snapshot.status {
                "Playing" => MPNowPlayingPlaybackState::Playing,
                "Paused" => MPNowPlayingPlaybackState::Paused,
                _ => MPNowPlayingPlaybackState::Stopped,
            });
            *self.last.borrow_mut() = Some(stamp);
        });
    }
}

fn make_artwork(image: &RgbImage) -> Option<Retained<MPMediaItemArtwork>> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image.clone())
        .write_to(&mut bytes, image::ImageFormat::Png)
        .ok()?;
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bytes.get_ref()))?;
    let size = image.size();
    // The immutable image is retained by the copied block for as long as MediaPlayer
    // needs it, including artwork requests arriving after the track has changed.
    let handler = RcBlock::new(move |_: NSSize| NonNull::from(&*image));
    Some(unsafe {
        MPMediaItemArtwork::initWithBoundsSize_requestHandler(
            MPMediaItemArtwork::alloc(),
            size,
            &handler,
        )
    })
}

impl Drop for MediaControls {
    fn drop(&mut self) {
        *self.snapshot.lock().unwrap() = None;
        self.bindings.clear();
        unsafe {
            self.center.setNowPlayingInfo(None);
            self.center
                .setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
    }
}

#[cfg(feature = "test-audio")]
#[path = "macos_media_regression.rs"]
pub mod regression;
