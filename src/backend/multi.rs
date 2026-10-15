use super::{Backend, Feedback, Input, PlaybackMode, Service, Track, WaveStyle};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::Read,
};

struct Slot {
    backend: Box<dyn Backend>,
    queue: VecDeque<Track>,
    started: bool,
    failed: bool,
}

pub struct Multi {
    slots: Vec<Slot>,
    services: Vec<Service>,
    selected: &'static str,
    random: File,
}

impl Multi {
    pub fn new(
        backends: Vec<Box<dyn Backend>>,
        mut services: Vec<Service>,
        selected: Option<&str>,
    ) -> Result<Self> {
        let connected = backends.len();
        services.push(Service {
            key: "mixed",
            name: "Wave Shuffle",
            available: connected > 1,
            favorites: false,
            favorites_shuffle: false,
            wave_styles: false,
            status: if connected > 1 {
                "Random source for each track".into()
            } else {
                "Connect at least two services".into()
            },
        });
        let selected = if let Some(key) = selected {
            services
                .iter()
                .find(|s| s.key == key && s.available)
                .context("The requested source is not connected; run plavno sources")?
                .key
        } else {
            services
                .iter()
                .find(|s| s.available)
                .map_or("yandex", |s| s.key)
        };
        Ok(Self {
            slots: backends
                .into_iter()
                .map(|backend| Slot {
                    backend,
                    queue: VecDeque::new(),
                    started: false,
                    failed: false,
                })
                .collect(),
            services,
            selected,
            random: File::open("/dev/urandom")?,
        })
    }

    fn slot(&mut self, key: &str) -> Result<&mut Slot> {
        self.slots
            .iter_mut()
            .find(|s| s.backend.key() == key)
            .context("This source is not connected")
    }

    fn source<'a>(&'a self, track: &'a Track) -> &'a str {
        if track.source.is_empty() {
            self.selected
        } else {
            &track.source
        }
    }

    fn tag(key: &str, tracks: &mut [Track]) {
        for track in tracks {
            track.source = key.into();
        }
    }

    fn mixed(&mut self, history: &[String]) -> Result<Vec<Track>> {
        let mut tracks = Vec::new();
        let mut seen: HashSet<String> = history.iter().cloned().collect();
        let mut last_error = None;
        while tracks.len() < 6 {
            let available: Vec<_> = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.failed)
                .map(|(i, _)| i)
                .collect();
            if available.is_empty() {
                break;
            }
            let index = available[random_index(available.len(), &mut self.random)?];
            let slot = &mut self.slots[index];
            let key = slot.backend.key();
            if slot.queue.is_empty() {
                let prefix = format!("{key}:");
                let history: Vec<_> = seen
                    .iter()
                    .filter_map(|id| id.strip_prefix(&prefix).map(str::to_owned))
                    .collect();
                let result = if slot.started {
                    slot.backend.more(&history)
                } else {
                    slot.backend.start()
                };
                slot.started = true;
                match result {
                    Ok(mut batch) => {
                        Self::tag(key, &mut batch);
                        batch.retain(|t| !seen.contains(&t.cache_key()));
                        if batch.is_empty() {
                            slot.failed = true;
                            continue;
                        }
                        slot.queue.extend(batch);
                    }
                    Err(error) => {
                        slot.failed = true;
                        last_error = Some(format!("{}: {error}", slot.backend.name()));
                        continue;
                    }
                }
            }
            if let Some(track) = slot.queue.pop_front()
                && seen.insert(track.cache_key())
            {
                tracks.push(track);
            }
        }
        if tracks.is_empty()
            && let Some(error) = last_error
        {
            anyhow::bail!("No wave is available: {error}");
        }
        Ok(tracks)
    }
}

fn random_index(upper: usize, random: &mut impl Read) -> Result<usize> {
    let bound = upper as u64;
    let limit = u64::MAX - u64::MAX % bound;
    loop {
        let mut bytes = [0; 8];
        random.read_exact(&mut bytes)?;
        let value = u64::from_le_bytes(bytes);
        if value < limit {
            return Ok((value % bound) as usize);
        }
    }
}

impl Backend for Multi {
    fn key(&self) -> &'static str {
        self.selected
    }
    fn name(&self) -> &'static str {
        self.services
            .iter()
            .find(|s| s.key == self.selected)
            .map_or("Music sources", |s| s.name)
    }
    fn services(&self) -> Vec<Service> {
        self.services.clone()
    }
    fn supports_favorites(&self) -> bool {
        self.selected == "mixed"
            || self
                .services
                .iter()
                .any(|s| s.key == self.selected && s.favorites)
    }
    fn supports_favorites_shuffle(&self) -> bool {
        self.services
            .iter()
            .any(|s| s.key == self.selected && s.favorites_shuffle)
    }
    fn start(&mut self) -> Result<Vec<Track>> {
        self.start_source(self.selected, PlaybackMode::MyVibe)
    }
    fn start_with_style(
        &mut self,
        source: &str,
        mode: PlaybackMode,
        style: WaveStyle,
    ) -> Result<Vec<Track>> {
        if source == "mixed" {
            ensure!(
                style == WaveStyle::Balanced,
                "Select a service to tune its wave"
            );
        } else {
            self.slot(source)?.backend.set_wave_style(style)?;
        }
        self.start_source(source, mode)
    }
    fn start_source(&mut self, source: &str, mode: PlaybackMode) -> Result<Vec<Track>> {
        let service = self
            .services
            .iter()
            .find(|s| s.key == source)
            .context("Unknown source")?;
        ensure!(service.available, "{}", service.status);
        ensure!(
            source != "mixed" || mode == PlaybackMode::MyVibe,
            "Select a service to play its favorites"
        );
        self.selected = service.key;
        for slot in &mut self.slots {
            if source != "mixed" && slot.backend.key() != source {
                slot.backend.suspend();
            }
            slot.queue.clear();
            slot.started = false;
            slot.failed = false;
        }
        if source == "mixed" {
            return self.mixed(&[]);
        }
        let mut tracks = self.slot(source)?.backend.start_mode(mode)?;
        Self::tag(source, &mut tracks);
        Ok(tracks)
    }
    fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
        if self.selected == "mixed" {
            return self.mixed(history);
        }
        let source = self.selected;
        let prefix = format!("{source}:");
        let history: Vec<_> = history
            .iter()
            .map(|id| id.strip_prefix(&prefix).unwrap_or(id).to_owned())
            .collect();
        let mut tracks = self.slot(source)?.backend.more(&history)?;
        Self::tag(source, &mut tracks);
        Ok(tracks)
    }
    fn resolve(&mut self, track: &Track) -> Result<Input> {
        let key = self.source(track).to_owned();
        self.slot(&key)?.backend.resolve(track)
    }
    fn feedback(&mut self, track: &Track, kind: Feedback, seconds: f64) -> Result<()> {
        let key = self.source(track).to_owned();
        self.slot(&key)?.backend.feedback(track, kind, seconds)
    }
    fn favorite_state(&mut self, track: &Track) -> Result<Option<bool>> {
        let key = self.source(track).to_owned();
        self.slot(&key)?.backend.favorite_state(track)
    }
    fn toggle_favorite(&mut self, track: &Track) -> Result<bool> {
        let key = self.source(track).to_owned();
        self.slot(&key)?.backend.toggle_favorite(track)
    }
    fn set_dislike(&mut self, track: &Track, disliked: bool) -> Result<bool> {
        let key = self.source(track).to_owned();
        self.slot(&key)?.backend.set_dislike(track, disliked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    type Calls = Arc<Mutex<Vec<String>>>;
    struct Fake {
        key: &'static str,
        calls: Calls,
        fail: bool,
    }
    impl Backend for Fake {
        fn key(&self) -> &'static str {
            self.key
        }
        fn name(&self) -> &'static str {
            self.key
        }
        fn set_wave_style(&mut self, style: WaveStyle) -> Result<()> {
            ensure!(
                self.key == "yandex" || style == WaveStyle::Balanced,
                "Unsupported wave style"
            );
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:style:{}", self.key, style.label()));
            Ok(())
        }
        fn start(&mut self) -> Result<Vec<Track>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:start", self.key));
            ensure!(!self.fail, "Test service unavailable");
            Ok(vec![Track {
                source: String::new(),
                id: "1".into(),
                title: self.key.into(),
                artist: "Test".into(),
                duration: Some(60.0),
                cover_url: None,
                genre: None,
                batch: "session".into(),
            }])
        }
        fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:more:{}", self.key, history.join(",")));
            Ok(vec![])
        }
        fn resolve(&mut self, track: &Track) -> Result<Input> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:resolve:{}", self.key, track.id));
            Ok(Input::File("fixture".into()))
        }
        fn feedback(&mut self, track: &Track, _: Feedback, _: f64) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:feedback:{}", self.key, track.id));
            Ok(())
        }
        fn favorite_state(&mut self, track: &Track) -> Result<Option<bool>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:favorite:{}", self.key, track.id));
            Ok(Some(true))
        }
        fn toggle_favorite(&mut self, track: &Track) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:toggle:{}", self.key, track.id));
            Ok(false)
        }
        fn set_dislike(&mut self, track: &Track, disliked: bool) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:dislike:{}:{disliked}", self.key, track.id));
            Ok(true)
        }
    }
    fn setup(fail_first: bool) -> (Multi, Calls) {
        let calls = Arc::new(Mutex::new(vec![]));
        let backends: Vec<Box<dyn Backend>> = ["yandex", "spotify"]
            .into_iter()
            .enumerate()
            .map(|(index, key)| {
                Box::new(Fake {
                    key,
                    calls: calls.clone(),
                    fail: fail_first && index == 0,
                }) as Box<dyn Backend>
            })
            .collect();
        let services = backends.iter().flat_map(|b| b.services()).collect();
        (
            Multi::new(backends, services, Some("mixed")).unwrap(),
            calls,
        )
    }
    #[test]
    fn mixed_preserves_provider_identity_and_routes_every_track_operation() {
        let (mut multi, calls) = setup(false);
        let tracks = multi.start().unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, tracks[1].id);
        assert_ne!(tracks[0].cache_key(), tracks[1].cache_key());
        for track in tracks {
            multi.resolve(&track).unwrap();
            multi.feedback(&track, Feedback::Started, 0.0).unwrap();
            assert_eq!(multi.favorite_state(&track).unwrap(), Some(true));
            assert!(!multi.toggle_favorite(&track).unwrap());
            assert!(multi.set_dislike(&track, true).unwrap());
            assert!(multi.set_dislike(&track, false).unwrap());
        }
        let log = calls.lock().unwrap();
        for source in ["yandex", "spotify"] {
            assert!(log.contains(&format!("{source}:dislike:1:true")));
            assert!(log.contains(&format!("{source}:dislike:1:false")));
            for operation in ["resolve", "feedback", "favorite", "toggle"] {
                assert!(log.contains(&format!("{source}:{operation}:1")));
            }
        }
    }
    #[test]
    fn failed_service_does_not_break_other_waves_and_single_source_history_is_unwrapped() {
        let (mut multi, calls) = setup(true);
        let tracks = multi.start().unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].source, "spotify");
        multi.start_source("spotify", PlaybackMode::MyVibe).unwrap();
        multi.more(&["spotify:1".into()]).unwrap();
        assert!(calls.lock().unwrap().contains(&"spotify:more:1".into()));
        assert!(
            multi
                .start_source("mixed", PlaybackMode::FavoritesShuffle)
                .is_err()
        );
        let legacy = serde_json::from_value::<Track>(
            serde_json::json!({"id":"1","title":"Old","artist":"","duration":null}),
        )
        .unwrap();
        assert!(legacy.source.is_empty());
        assert_eq!(legacy.cache_key(), "1");
    }
    #[test]
    fn entropy_selection_is_unbiased_and_handles_rejected_values() {
        let mut values =
            std::io::Cursor::new([u64::MAX.to_le_bytes(), 5_u64.to_le_bytes()].concat());
        assert_eq!(random_index(3, &mut values).unwrap(), 2);
    }

    #[test]
    fn styles_route_to_the_requested_source_before_starting_its_wave() {
        let (mut multi, calls) = setup(false);
        let tracks = multi
            .start_with_style("yandex", PlaybackMode::MyVibe, WaveStyle::Work)
            .unwrap();
        assert_eq!(tracks[0].source, "yandex");
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["yandex:style:Work", "yandex:start"]
        );
        assert!(
            multi
                .start_with_style("spotify", PlaybackMode::MyVibe, WaveStyle::Work)
                .is_err()
        );
        assert!(
            multi
                .start_with_style("mixed", PlaybackMode::MyVibe, WaveStyle::Calm)
                .is_err()
        );
    }
}
