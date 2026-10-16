use crate::engine::{Command, Engine, Event, MAX_SPEED, MIN_SPEED};
use anyhow::{Context, Result};
use image::RgbImage;
use std::{
    cell::{Cell, RefCell},
    io::Cursor,
    sync::{Arc, Mutex, mpsc::Sender},
};
use windows::{
    Foundation::{EventRegistrationToken, TimeSpan, TypedEventHandler},
    Media::*,
    Storage::Streams::{DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference},
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            WinRT::{
                ISystemMediaTransportControlsInterop, RO_INIT_MULTITHREADED, RoInitialize,
                RoUninitialize,
            },
        },
        UI::{Shell::SetCurrentProcessExplicitAppUserModelID, WindowsAndMessaging::*},
    },
    core::{HSTRING, factory, w},
};

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
            title: track.map(|t| t.title.clone()),
            artist: track.map_or_else(String::new, |t| t.artist.clone()),
            duration: track
                .and_then(|t| t.duration)
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
}

#[derive(Clone, Copy)]
enum Action {
    Button(SystemMediaTransportControlsButton),
    Position(f64),
    Rate(f64),
}

fn dispatch(action: Action, state: &Mutex<Option<Snapshot>>, events: &Sender<Event>) {
    let state = state.lock().unwrap();
    let Some(state) = state.as_ref() else {
        return;
    };
    let [next, previous, play, pause, seek] = state.capabilities;
    let command = match action {
        Action::Button(SystemMediaTransportControlsButton::Play) if play => Command::Play,
        Action::Button(SystemMediaTransportControlsButton::Pause) if pause => Command::Pause,
        Action::Button(SystemMediaTransportControlsButton::Stop) if state.title.is_some() => {
            Command::Stop
        }
        Action::Button(SystemMediaTransportControlsButton::Next) if next => Command::Next,
        Action::Button(SystemMediaTransportControlsButton::Previous) if previous => {
            Command::Previous
        }
        Action::Position(position) if seek && position.is_finite() && position >= 0.0 => {
            Command::SetPosition(
                state.id,
                state.duration.map_or(position, |end| position.min(end)),
            )
        }
        Action::Rate(rate)
            if state.title.is_some()
                && rate.is_finite()
                && (MIN_SPEED..=MAX_SPEED).contains(&rate) =>
        {
            Command::Rate(rate)
        }
        _ => return,
    };
    let _ = events.send(Event::Control(command));
}

pub struct Runtime;
impl Runtime {
    pub fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

struct Window {
    hwnd: HWND,
    instance: HINSTANCE,
}
impl Window {
    fn new() -> Result<Self> {
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: w!("plavnoMediaControls"),
                ..Default::default()
            };
            anyhow::ensure!(
                RegisterClassW(&class) != 0,
                "Cannot register media controls window"
            );
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!("plavno"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                instance,
                None,
            ) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    let _ = UnregisterClassW(class.lpszClassName, instance);
                    return Err(error.into());
                }
            };
            Ok(Self { hwnd, instance })
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(w!("plavnoMediaControls"), self.instance);
        }
    }
}

enum Binding {
    Button(EventRegistrationToken),
    Position(EventRegistrationToken),
    Rate(EventRegistrationToken),
}

pub struct MediaControls {
    center: SystemMediaTransportControls,
    bindings: Vec<Binding>,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    last: RefCell<Option<Snapshot>>,
    artwork: RefCell<Option<(Arc<RgbImage>, RandomAccessStreamReference)>>,
    failed: Cell<bool>,
    _window: Window,
    _apartment: Runtime,
}
impl MediaControls {
    pub fn new(engine: &Engine, events: Sender<Event>) -> Result<Self> {
        Self::create(Snapshot::from_engine(engine), events)
    }

    fn create(initial: Snapshot, events: Sender<Event>) -> Result<Self> {
        let apartment = Runtime::new()?;
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(w!("plavno"))?;
        }
        // Console HWNDs can belong to conhost/Windows Terminal, so use our own
        // hidden top-level window as required by GetForWindow.
        let window = Window::new()?;
        let interop: ISystemMediaTransportControlsInterop =
            factory::<SystemMediaTransportControls, _>()?;
        let center: SystemMediaTransportControls = unsafe { interop.GetForWindow(window.hwnd)? };
        let state = Arc::new(Mutex::new(Some(initial.clone())));
        let mut controls = Self {
            center,
            bindings: Vec::new(),
            snapshot: state.clone(),
            last: RefCell::new(None),
            artwork: RefCell::new(None),
            failed: Cell::new(false),
            _window: window,
            _apartment: apartment,
        };
        let (snapshot, sender) = (state.clone(), events.clone());
        controls
            .bindings
            .push(Binding::Button(controls.center.ButtonPressed(
                &TypedEventHandler::new(
                    move |_, args: &Option<SystemMediaTransportControlsButtonPressedEventArgs>| {
                        if let Some(args) = args {
                            dispatch(Action::Button(args.Button()?), &snapshot, &sender);
                        }
                        Ok(())
                    },
                ),
            )?));
        let (snapshot, sender) = (state.clone(), events.clone());
        controls.bindings.push(Binding::Position(
            controls
                .center
                .PlaybackPositionChangeRequested(&TypedEventHandler::new(
                    move |_, args: &Option<PlaybackPositionChangeRequestedEventArgs>| {
                        if let Some(args) = args {
                            dispatch(
                                Action::Position(
                                    args.RequestedPlaybackPosition()?.Duration as f64
                                        / 10_000_000.0,
                                ),
                                &snapshot,
                                &sender,
                            );
                        }
                        Ok(())
                    },
                ))?,
        ));
        controls
            .bindings
            .push(Binding::Rate(controls.center.PlaybackRateChangeRequested(
                &TypedEventHandler::new(
                    move |_, args: &Option<PlaybackRateChangeRequestedEventArgs>| {
                        if let Some(args) = args {
                            dispatch(Action::Rate(args.RequestedPlaybackRate()?), &state, &events);
                        }
                        Ok(())
                    },
                ),
            )?));
        controls.publish(initial, None)?;
        Ok(controls)
    }

    pub fn poll(&self) {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, self._window.hwnd, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    pub fn update(&self, engine: &Engine, cover: Option<&Arc<RgbImage>>) {
        if let Err(error) = self.publish(Snapshot::from_engine(engine), cover) {
            if !self.failed.replace(true) {
                eprintln!("plavno: Windows media controls update failed: {error:#}");
            }
        } else {
            self.failed.set(false);
        }
    }

    fn publish(&self, snapshot: Snapshot, cover: Option<&Arc<RgbImage>>) -> Result<()> {
        *self.snapshot.lock().unwrap() = Some(snapshot.clone());
        let mut stamp = snapshot.clone();
        stamp.position = stamp.position.floor();
        let cover = cover.filter(|_| snapshot.status != "Stopped");
        let same_cover = match (&*self.artwork.borrow(), cover) {
            (Some((old, _)), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        if self.last.borrow().as_ref() == Some(&stamp) && same_cover {
            return Ok(());
        }
        let center = &self.center;
        let [next, previous, play, pause, seek] = snapshot.capabilities;
        center.SetIsNextEnabled(next)?;
        center.SetIsPreviousEnabled(previous)?;
        center.SetIsPlayEnabled(play)?;
        center.SetIsPauseEnabled(pause)?;
        center.SetIsStopEnabled(snapshot.title.is_some())?;
        center.SetPlaybackRate(snapshot.rate)?;
        center.SetPlaybackStatus(match snapshot.status {
            "Playing" => MediaPlaybackStatus::Playing,
            "Paused" => MediaPlaybackStatus::Paused,
            _ => MediaPlaybackStatus::Stopped,
        })?;
        let display = center.DisplayUpdater()?;
        if !same_cover {
            *self.artwork.borrow_mut() = cover
                .map(|cover| make_artwork(cover).map(|art| (cover.clone(), art)))
                .transpose()?;
        }
        display.ClearAll()?;
        if let Some(title) = &snapshot.title {
            display.SetType(MediaPlaybackType::Music)?;
            display.MusicProperties()?.SetTitle(&HSTRING::from(title))?;
            display
                .MusicProperties()?
                .SetArtist(&HSTRING::from(&snapshot.artist))?;
            display.SetAppMediaId(&HSTRING::from(format!(
                "{}:{}",
                snapshot.id.0, snapshot.id.1
            )))?;
            if let Some((_, art)) = &*self.artwork.borrow() {
                display.SetThumbnail(art)?;
            }
        }
        display.Update()?;
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
        let duration = snapshot
            .duration
            .unwrap_or(snapshot.position)
            .max(snapshot.position);
        timeline.SetStartTime(span(0.0))?;
        timeline.SetEndTime(span(duration))?;
        timeline.SetMinSeekTime(span(if seek { 0.0 } else { snapshot.position }))?;
        timeline.SetMaxSeekTime(span(if seek { duration } else { snapshot.position }))?;
        timeline.SetPosition(span(snapshot.position))?;
        center.UpdateTimelineProperties(&timeline)?;
        center.SetIsEnabled(snapshot.title.is_some())?;
        *self.last.borrow_mut() = Some(stamp);
        Ok(())
    }
}

fn span(seconds: f64) -> TimeSpan {
    TimeSpan {
        Duration: (seconds * 10_000_000.0) as i64,
    }
}

fn make_artwork(image: &RgbImage) -> Result<RandomAccessStreamReference> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image.clone()).write_to(&mut bytes, image::ImageFormat::Png)?;
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    writer.WriteBytes(bytes.get_ref())?;
    writer
        .StoreAsync()?
        .get()
        .context("Cannot store Windows thumbnail")?;
    writer.DetachStream()?;
    stream.Seek(0)?;
    Ok(RandomAccessStreamReference::CreateFromStream(&stream)?)
}

impl Drop for MediaControls {
    fn drop(&mut self) {
        *self.snapshot.lock().unwrap() = None;
        for binding in self.bindings.drain(..) {
            let _ = match binding {
                Binding::Button(token) => self.center.RemoveButtonPressed(token),
                Binding::Position(token) => {
                    self.center.RemovePlaybackPositionChangeRequested(token)
                }
                Binding::Rate(token) => self.center.RemovePlaybackRateChangeRequested(token),
            };
        }
        let _ = self.center.SetIsEnabled(false);
        let _ = self.center.SetPlaybackStatus(MediaPlaybackStatus::Stopped);
        if let Ok(display) = self.center.DisplayUpdater() {
            let _ = display.ClearAll();
            let _ = display.Update();
        }
    }
}

#[cfg(feature = "test-audio")]
#[path = "windows_media_regression.rs"]
pub mod regression;
