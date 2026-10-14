use crate::engine::{Command, Engine, Event, MAX_SPEED, MIN_SPEED};
use anyhow::{Context, Result, ensure};
use dbus::{
    Message, MethodErr, Path,
    arg::{PropMap, RefArg, Variant},
    blocking::{Connection, stdintf::org_freedesktop_dbus::RequestNameReply},
    channel::{MatchingReceiver, Sender},
    message::MatchRule,
};
use dbus_crossroads::{Context as CallContext, Crossroads};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

const NAME: &str = "org.mpris.MediaPlayer2.plavno";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Clone, PartialEq)]
struct TrackInfo {
    path: Path<'static>,
    title: String,
    artist: String,
    length: Option<i64>,
    cover_url: Option<String>,
}

#[derive(Clone)]
struct Snapshot {
    epoch: u64,
    serial: u64,
    seek_revision: u64,
    status: &'static str,
    track: Option<TrackInfo>,
    position: i64,
    volume: f64,
    rate: f64,
    capabilities: [bool; 5],
}

const CAPABILITIES: [&str; 5] = [
    "CanGoNext",
    "CanGoPrevious",
    "CanPlay",
    "CanPause",
    "CanSeek",
];

impl Snapshot {
    fn from_engine(engine: &Engine) -> Self {
        let (epoch, serial) = engine.playback_id();
        Self {
            epoch,
            serial,
            seek_revision: engine.seek_revision,
            status: engine.playback_status(),
            track: engine.media_track().map(|track| TrackInfo {
                path: Path::new(format!(
                    "/plavno/track/t{epoch}_{serial}_{}",
                    track
                        .id
                        .bytes()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                ))
                .unwrap(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                cover_url: track.cover_url.clone(),
                length: track
                    .duration
                    .filter(|d| d.is_finite() && *d >= 0.0)
                    .map(|d| (d * 1_000_000.0) as i64),
            }),
            position: (engine.seconds() * 1_000_000.0) as i64,
            volume: f64::from(engine.volume),
            rate: f64::from(engine.speed),
            capabilities: [
                engine.can_next(),
                engine.can_previous(),
                engine.can_play(),
                engine.can_pause(),
                engine.can_seek(),
            ],
        }
    }

    fn metadata(&self) -> PropMap {
        let mut map = PropMap::new();
        if let Some(track) = &self.track {
            property(&mut map, "mpris:trackid", track.path.clone());
            property(&mut map, "xesam:title", track.title.clone());
            if let Some(url) = &track.cover_url {
                property(&mut map, "mpris:artUrl", url.clone());
            }
            if !track.artist.is_empty() {
                property(&mut map, "xesam:artist", vec![track.artist.clone()]);
            }
            if let Some(length) = track.length {
                property(&mut map, "mpris:length", length);
            }
        }
        map
    }

    fn changes(&self, old: &Self) -> PropMap {
        let mut map = PropMap::new();
        if self.status != old.status {
            property(&mut map, "PlaybackStatus", self.status.to_string());
        }
        if self.track != old.track {
            property(&mut map, "Metadata", self.metadata());
        }
        if self.volume != old.volume {
            property(&mut map, "Volume", self.volume);
        }
        if self.rate != old.rate {
            property(&mut map, "Rate", self.rate);
        }
        for (index, name) in CAPABILITIES.iter().enumerate() {
            if self.capabilities[index] != old.capabilities[index] {
                property(&mut map, name, self.capabilities[index]);
            }
        }
        map
    }
}

fn property(map: &mut PropMap, name: &str, value: impl RefArg + 'static) {
    map.insert(name.into(), Variant(Box::new(value)));
}

struct Object {
    snapshot: Arc<Mutex<Snapshot>>,
    events: mpsc::Sender<Event>,
}

impl Object {
    fn command(&self, command: Command) -> std::result::Result<(), MethodErr> {
        self.events
            .send(Event::Control(command))
            .map_err(|_| MethodErr::failed("The player has stopped"))
    }
}

fn interfaces(snapshot: Arc<Mutex<Snapshot>>, events: mpsc::Sender<Event>) -> Crossroads {
    let mut cr = Crossroads::new();
    let root = cr.register("org.mpris.MediaPlayer2", |b| {
        b.method(
            "Raise",
            (),
            (),
            |_: &mut CallContext, _: &mut Object, ()| Ok(()),
        );
        b.method(
            "Quit",
            (),
            (),
            |_: &mut CallContext, object: &mut Object, ()| object.command(Command::Quit),
        );
        for (name, value) in [
            ("CanQuit", true),
            ("CanRaise", false),
            ("HasTrackList", false),
        ] {
            b.property::<bool, _>(name)
                .get(move |_, _: &mut Object| Ok(value))
                .emits_changed_const();
        }
        b.property::<String, _>("Identity")
            .get(|_, _: &mut Object| Ok("plavno".into()))
            .emits_changed_const();
        for name in ["SupportedUriSchemes", "SupportedMimeTypes"] {
            b.property::<Vec<String>, _>(name)
                .get(|_, _: &mut Object| Ok(Vec::new()))
                .emits_changed_const();
        }
    });
    let player = cr.register(PLAYER, |b| {
        for (name, command) in [
            ("Next", Command::Next),
            ("Previous", Command::Previous),
            ("Pause", Command::Pause),
            ("PlayPause", Command::PlayPause),
            ("Stop", Command::Stop),
            ("Play", Command::Play),
        ] {
            b.method(
                name,
                (),
                (),
                move |_: &mut CallContext, object: &mut Object, ()| object.command(command),
            );
        }
        b.method(
            "Seek",
            ("Offset",),
            (),
            |_: &mut CallContext, object: &mut Object, (offset,): (i64,)| {
                if object.snapshot.lock().unwrap().capabilities[4] {
                    object.command(Command::Seek(offset as f64 / 1_000_000.0))?;
                }
                Ok(())
            },
        );
        b.method(
            "SetPosition",
            ("TrackId", "Position"),
            (),
            |_: &mut CallContext, object: &mut Object, (track, position): (Path<'static>, i64)| {
                let state = object.snapshot.lock().unwrap();
                if state.capabilities[4]
                    && position >= 0
                    && state.track.as_ref().is_some_and(|current| {
                        current.path == track
                            && current.length.is_none_or(|length| position <= length)
                    })
                {
                    object.command(Command::SetPosition(
                        (state.epoch, state.serial),
                        position as f64 / 1_000_000.0,
                    ))?;
                }
                Ok(())
            },
        );
        b.signal::<(i64,), _>("Seeked", ("Position",));
        b.property::<String, _>("PlaybackStatus")
            .get(|_, object: &mut Object| Ok(object.snapshot.lock().unwrap().status.into()));
        b.property::<PropMap, _>("Metadata")
            .get(|_, object: &mut Object| Ok(object.snapshot.lock().unwrap().metadata()));
        b.property::<i64, _>("Position")
            .get(|_, object: &mut Object| Ok(object.snapshot.lock().unwrap().position))
            .emits_changed_false();
        b.property::<f64, _>("Volume")
            .get(|_, object: &mut Object| Ok(object.snapshot.lock().unwrap().volume))
            .set(|_, object: &mut Object, volume| {
                if !volume.is_finite() {
                    return Err(MethodErr::invalid_arg(&volume));
                }
                object.command(Command::Volume(volume))?;
                Ok(None)
            });
        for (index, name) in CAPABILITIES.iter().enumerate() {
            b.property::<bool, _>(*name)
                .get(move |_, object: &mut Object| {
                    Ok(object.snapshot.lock().unwrap().capabilities[index])
                });
        }
        b.property::<bool, _>("CanControl")
            .get(|_, _: &mut Object| Ok(true))
            .emits_changed_const();
        for (name, rate) in [("MinimumRate", MIN_SPEED), ("MaximumRate", MAX_SPEED)] {
            b.property::<f64, _>(name)
                .get(move |_, _: &mut Object| Ok(rate))
                .emits_changed_const();
        }
        b.property::<f64, _>("Rate")
            .get(|_, object: &mut Object| Ok(object.snapshot.lock().unwrap().rate))
            .set(|_, object: &mut Object, rate| {
                if rate == 0.0 {
                    object.command(Command::Pause)?;
                } else if rate.is_finite() && (MIN_SPEED..=MAX_SPEED).contains(&rate) {
                    object.command(Command::Rate(rate))?;
                } else {
                    return Err(MethodErr::invalid_arg(&rate));
                }
                Ok(None)
            });
    });
    cr.insert(PATH, &[root, player], Object { snapshot, events });
    cr
}

pub struct Mpris {
    snapshot: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Mpris {
    pub fn new(engine: &Engine, events: mpsc::Sender<Event>) -> Result<Self> {
        Self::serve(Snapshot::from_engine(engine), events)
    }

    fn serve(initial: Snapshot, events: mpsc::Sender<Event>) -> Result<Self> {
        let connection = Connection::new_session().context("No connection to the session D-Bus")?;
        if connection.request_name(NAME, false, false, true)? != RequestNameReply::PrimaryOwner {
            let name = format!("{NAME}.instance{}", std::process::id());
            ensure!(
                connection.request_name(name, false, false, true)?
                    == RequestNameReply::PrimaryOwner,
                "The MPRIS name is already in use"
            );
        }
        let snapshot = Arc::new(Mutex::new(initial.clone()));
        let mut cr = interfaces(snapshot.clone(), events.clone());
        connection.start_receive(
            MatchRule::new_method_call(),
            Box::new(move |message, connection| {
                let _ = cr.handle_message(message, connection);
                true
            }),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let state = snapshot.clone();
        let worker = std::thread::Builder::new()
            .name("mpris".into())
            .spawn(move || {
                let mut previous = initial;
                while !stopped.load(Ordering::Relaxed) {
                    if let Err(error) = connection.process(Duration::from_millis(250)) {
                        let epoch = state.lock().unwrap().epoch;
                        let _ = events.send(Event::Warning(epoch, format!("MPRIS: {error}")));
                        break;
                    }
                    let current = state.lock().unwrap().clone();
                    let changes = current.changes(&previous);
                    if !changes.is_empty() {
                        let message = Message::new_signal(
                            PATH,
                            "org.freedesktop.DBus.Properties",
                            "PropertiesChanged",
                        )
                        .unwrap()
                        .append3(PLAYER, changes, Vec::<String>::new());
                        if connection.send(message).is_err() {
                            break;
                        }
                    }
                    if current.track == previous.track
                        && (current.seek_revision != previous.seek_revision
                            || (current.status == "Stopped"
                                && previous.status != "Stopped"
                                && current.position < previous.position))
                    {
                        let message = Message::new_signal(PATH, PLAYER, "Seeked")
                            .unwrap()
                            .append1(current.position);
                        if connection.send(message).is_err() {
                            break;
                        }
                    }
                    previous = current;
                }
            })?;
        Ok(Self {
            snapshot,
            stop,
            worker: Some(worker),
        })
    }

    pub fn update(&self, engine: &Engine) {
        *self.snapshot.lock().unwrap() = Snapshot::from_engine(engine);
    }
}

impl Drop for Mpris {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
    use std::time::Instant;

    #[test]
    fn dbus_controls_properties_signals_and_name_lifetime() {
        if std::env::var_os("PLAVNO_MPRIS_TEST_CHILD").is_none() {
            let result = std::process::Command::new("dbus-run-session")
                .arg("--")
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mpris::tests::dbus_controls_properties_signals_and_name_lifetime",
                    "--nocapture",
                ])
                .env("PLAVNO_MPRIS_TEST_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let initial = Snapshot {
            epoch: 1,
            serial: 1,
            seek_revision: 0,
            status: "Playing",
            position: 12_000_000,
            volume: 1.0,
            rate: 1.0,
            track: Some(TrackInfo {
                path: Path::new("/plavno/track/test").unwrap(),
                title: "Test track".into(),
                artist: "Test artist".into(),
                length: Some(60_000_000),
                cover_url: Some("https://example.com/first.jpg".into()),
            }),
            capabilities: [true, false, true, true, true],
        };
        let (events, receiver) = mpsc::channel();
        let server = Mpris::serve(initial, events.clone()).unwrap();
        let client = Connection::new_session().unwrap();
        let proxy = client.with_proxy(NAME, PATH, Duration::from_secs(2));
        assert_eq!(
            proxy
                .get::<String>("org.mpris.MediaPlayer2", "Identity")
                .unwrap(),
            "plavno"
        );
        assert_eq!(
            proxy.get::<String>(PLAYER, "PlaybackStatus").unwrap(),
            "Playing"
        );
        assert_eq!(proxy.get::<f64>(PLAYER, "Volume").unwrap(), 1.0);
        assert_eq!(proxy.get::<i64>(PLAYER, "Position").unwrap(), 12_000_000);
        assert!(!proxy.get::<bool>(PLAYER, "CanGoPrevious").unwrap());
        assert!(proxy.get::<bool>(PLAYER, "CanSeek").unwrap());
        let metadata = proxy.get::<PropMap>(PLAYER, "Metadata").unwrap();
        assert_eq!(
            metadata["mpris:trackid"].0.arg_type(),
            dbus::arg::ArgType::ObjectPath
        );
        assert_eq!(metadata["mpris:length"].0.as_i64(), Some(60_000_000));
        assert_eq!(metadata["xesam:title"].0.as_str(), Some("Test track"));
        assert_eq!(
            metadata["mpris:artUrl"].0.as_str(),
            Some("https://example.com/first.jpg")
        );
        assert_eq!(
            metadata["mpris:artUrl"].0.arg_type(),
            dbus::arg::ArgType::String
        );
        let (xml,): (String,) = proxy
            .method_call("org.freedesktop.DBus.Introspectable", "Introspect", ())
            .unwrap();
        assert!(xml.contains("org.mpris.MediaPlayer2.Player") && xml.contains("PropertiesChanged"));

        for (name, expected) in [
            ("Pause", Command::Pause),
            ("Play", Command::Play),
            ("PlayPause", Command::PlayPause),
            ("Next", Command::Next),
            ("Previous", Command::Previous),
            ("Stop", Command::Stop),
        ] {
            proxy.method_call::<(), _, _, _>(PLAYER, name, ()).unwrap();
            let Event::Control(command) = receiver.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("Missing command");
            };
            assert_eq!(
                std::mem::discriminant(&command),
                std::mem::discriminant(&expected)
            );
        }
        proxy.set(PLAYER, "Volume", 0.25f64).unwrap();
        assert!(
            matches!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), Event::Control(Command::Volume(v)) if v == 0.25)
        );
        assert!(proxy.set(PLAYER, "Volume", f64::NAN).is_err());
        assert_eq!(proxy.get::<f64>(PLAYER, "MinimumRate").unwrap(), 0.1);
        assert_eq!(proxy.get::<f64>(PLAYER, "MaximumRate").unwrap(), 2.0);
        assert_eq!(proxy.get::<f64>(PLAYER, "Rate").unwrap(), 1.0);
        proxy.set(PLAYER, "Rate", 0.1f64).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Control(Command::Rate(0.1))
        ));
        proxy.set(PLAYER, "Rate", 1.25f64).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Control(Command::Rate(1.25))
        ));
        for rate in [0.05, 2.5, -1.0, f64::NAN, f64::INFINITY] {
            assert!(proxy.set(PLAYER, "Rate", rate).is_err());
        }
        proxy.set(PLAYER, "Rate", 0.0f64).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Control(Command::Pause)
        ));
        proxy
            .method_call::<(), _, _, _>(PLAYER, "Seek", (10i64,))
            .unwrap();
        assert!(
            matches!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), Event::Control(Command::Seek(v)) if v == 0.00001)
        );
        proxy
            .method_call::<(), _, _, _>(
                PLAYER,
                "SetPosition",
                (Path::new("/plavno/track/test").unwrap(), 10i64),
            )
            .unwrap();
        assert!(
            matches!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), Event::Control(Command::SetPosition((1,1), v)) if v == 0.00001)
        );
        for (track, position) in [
            ("/plavno/track/stale", 10i64),
            ("/plavno/track/test", -1i64),
            ("/plavno/track/test", 60_000_001i64),
        ] {
            proxy
                .method_call::<(), _, _, _>(
                    PLAYER,
                    "SetPosition",
                    (Path::new(track).unwrap(), position),
                )
                .unwrap();
        }
        assert!(receiver.try_recv().is_err());

        let changes = Arc::new(Mutex::new(Vec::new()));
        let recorded = changes.clone();
        client
            .add_match(
                MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged"),
                move |(interface, changed, _): (String, PropMap, Vec<String>), _, _| {
                    if interface == PLAYER {
                        recorded.lock().unwrap().push(changed);
                    }
                    true
                },
            )
            .unwrap();
        {
            let mut state = server.snapshot.lock().unwrap();
            state.status = "Paused";
            state.volume = 0.25;
            state.rate = 1.25;
            state.position = 13_000_000;
            state.track.as_mut().unwrap().title = "Another track".into();
            state.track.as_mut().unwrap().cover_url = Some("https://example.com/second.jpg".into());
            state.capabilities[1] = true;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while changes.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "PropertiesChanged not received");
            client.process(Duration::from_millis(50)).unwrap();
        }
        {
            let changes = changes.lock().unwrap();
            let changed = &changes[0];
            for name in [
                "PlaybackStatus",
                "Volume",
                "Rate",
                "Metadata",
                "CanGoPrevious",
            ] {
                assert!(changed.contains_key(name), "Missing {name}");
            }
            assert!(!changed.contains_key("Position"));
        }
        assert_eq!(
            proxy.get::<String>(PLAYER, "PlaybackStatus").unwrap(),
            "Paused"
        );
        assert_eq!(proxy.get::<f64>(PLAYER, "Volume").unwrap(), 0.25);
        assert_eq!(proxy.get::<i64>(PLAYER, "Position").unwrap(), 13_000_000);

        for cover in [Some("https://example.com/updated.jpg"), None] {
            changes.lock().unwrap().clear();
            server
                .snapshot
                .lock()
                .unwrap()
                .track
                .as_mut()
                .unwrap()
                .cover_url = cover.map(str::to_owned);
            let deadline = Instant::now() + Duration::from_secs(3);
            while changes.lock().unwrap().is_empty() {
                assert!(
                    Instant::now() < deadline,
                    "Cover metadata change not received"
                );
                client.process(Duration::from_millis(50)).unwrap();
            }
            assert!(changes.lock().unwrap()[0].contains_key("Metadata"));
            let metadata = proxy.get::<PropMap>(PLAYER, "Metadata").unwrap();
            assert_eq!(
                metadata.get("mpris:artUrl").and_then(|v| v.0.as_str()),
                cover
            );
        }

        let seeked = Arc::new(Mutex::new(Vec::new()));
        let recorded = seeked.clone();
        client
            .add_match(
                MatchRule::new_signal(PLAYER, "Seeked"),
                move |(position,): (i64,), _, _| {
                    recorded.lock().unwrap().push(position);
                    true
                },
            )
            .unwrap();
        for (index, position) in [30_000_000, 5_000_000].into_iter().enumerate() {
            server.snapshot.lock().unwrap().position = position;
            let deadline = Instant::now() + Duration::from_millis(550);
            while Instant::now() < deadline {
                client.process(Duration::from_millis(50)).unwrap();
            }
            assert_eq!(seeked.lock().unwrap().len(), index, "Premature Seeked");
            server.snapshot.lock().unwrap().seek_revision += 1;
            let deadline = Instant::now() + Duration::from_secs(3);
            while seeked.lock().unwrap().len() <= index {
                assert!(Instant::now() < deadline, "Seeked not received");
                client.process(Duration::from_millis(50)).unwrap();
            }
            assert_eq!(seeked.lock().unwrap()[index], position);
        }

        let second = Mpris::serve(server.snapshot.lock().unwrap().clone(), events).unwrap();
        let instance_name = format!("{NAME}.instance{}", std::process::id());
        let instance = client.with_proxy(instance_name, PATH, Duration::from_secs(2));
        assert_eq!(
            instance
                .get::<String>("org.mpris.MediaPlayer2", "Identity")
                .unwrap(),
            "plavno"
        );
        proxy
            .method_call::<(), _, _, _>("org.mpris.MediaPlayer2", "Quit", ())
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Control(Command::Quit)
        ));
        drop(second);
        drop(server);
        let bus = client.with_proxy(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            Duration::from_secs(2),
        );
        let (owned,): (bool,) = bus
            .method_call("org.freedesktop.DBus", "NameHasOwner", (NAME,))
            .unwrap();
        assert!(!owned);
    }
}
