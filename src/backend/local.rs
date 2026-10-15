use super::{Backend, Feedback, Input, Track};
use anyhow::{Context, Result, ensure};
use rodio::Source;
use std::{
    collections::HashSet,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::multi::random_index;

pub struct Local(pub PathBuf);

impl Backend for Local {
    fn key(&self) -> &'static str {
        "local"
    }
    fn name(&self) -> &'static str {
        "Local file"
    }

    fn start(&mut self) -> Result<Vec<Track>> {
        ensure!(self.0.is_file(), "Audio file not found");
        Ok(vec![Track {
            source: String::new(),
            id: self.0.canonicalize()?.to_string_lossy().into(),
            title: self
                .0
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            artist: String::new(),
            duration: None,
            cover_url: None,
            genre: None,
            batch: String::new(),
        }])
    }

    fn more(&mut self, _: &[String]) -> Result<Vec<Track>> {
        Ok(Vec::new())
    }

    fn resolve(&mut self, track: &Track) -> Result<Input> {
        Ok(Input::File(track_path(track)?))
    }

    fn feedback(&mut self, _: &Track, _: Feedback, _: f64) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct ScanState {
    files: Vec<PathBuf>,
    done: bool,
    error: Option<String>,
}

struct Scan {
    state: Arc<(Mutex<ScanState>, Condvar)>,
    cancel: Arc<AtomicBool>,
}

impl Scan {
    fn start(root: PathBuf, mut random: File) -> Result<Self> {
        let scan = Self {
            state: Arc::default(),
            cancel: Arc::default(),
        };
        let state = scan.state.clone();
        let cancel = scan.cancel.clone();
        std::thread::Builder::new()
            .name("local-scan".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let mut directories = vec![root.clone()];
                    while !directories.is_empty() && !cancel.load(Ordering::Relaxed) {
                        let index = random_index(directories.len(), &mut random)?;
                        let directory = directories.swap_remove(index);
                        let entries = match fs::read_dir(&directory) {
                            Ok(entries) => entries,
                            Err(error) if directory == root => return Err(error.into()),
                            Err(_) => continue,
                        };
                        for entry in entries.flatten() {
                            if cancel.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            let Ok(kind) = entry.file_type() else {
                                continue;
                            };
                            if kind.is_dir() {
                                directories.push(entry.path());
                            } else if kind.is_file() && supported(&entry.path()) {
                                state.0.lock().unwrap().files.push(entry.path());
                                state.1.notify_one();
                            }
                        }
                    }
                    Ok(())
                })();
                let mut data = state.0.lock().unwrap();
                data.done = true;
                data.error = result.err().map(|error| error.to_string());
                state.1.notify_one();
            })?;
        Ok(scan)
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct LocalDirectory {
    root: PathBuf,
    scan: Option<Scan>,
    discovered: usize,
    remaining: Vec<usize>,
    invalid: HashSet<usize>,
    last: Option<usize>,
    random: File,
}

impl LocalDirectory {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            root,
            scan: None,
            discovered: 0,
            remaining: Vec::new(),
            invalid: HashSet::new(),
            last: None,
            random: File::open("/dev/urandom")?,
        })
    }

    fn next_batch(&mut self) -> Result<Vec<Track>> {
        let scan = self.scan.as_ref().context("Local scan has not started")?;
        let mut tracks = Vec::new();
        loop {
            let mut data = scan.state.0.lock().unwrap();
            self.remaining.extend(self.discovered..data.files.len());
            self.discovered = data.files.len();
            if self.remaining.is_empty() && !tracks.is_empty() {
                return Ok(tracks);
            }
            while self.remaining.is_empty() && !data.done {
                data = scan.state.1.wait(data).unwrap();
                self.remaining.extend(self.discovered..data.files.len());
                self.discovered = data.files.len();
            }
            if let Some(error) = &data.error {
                anyhow::bail!("Cannot scan local directory: {error}");
            }
            if self.remaining.is_empty() {
                self.remaining
                    .extend((0..self.discovered).filter(|i| !self.invalid.contains(i)));
            }
            ensure!(
                !self.remaining.is_empty(),
                "No playable audio files found in {}",
                self.root.display()
            );
            let mut index = random_index(self.remaining.len(), &mut self.random)?;
            if tracks.is_empty()
                && self.remaining.len() > 1
                && Some(self.remaining[index]) == self.last
            {
                index = (index + 1 + random_index(self.remaining.len() - 1, &mut self.random)?)
                    % self.remaining.len();
            }
            let id = self.remaining.swap_remove(index);
            let path = data.files[id].clone();
            drop(data);
            match local_track(&path) {
                Ok(track) => {
                    self.last = Some(id);
                    tracks.push(track);
                }
                Err(_) => {
                    self.invalid.insert(id);
                }
            }
            if tracks.len() == 6 {
                return Ok(tracks);
            }
        }
    }
}

impl Backend for LocalDirectory {
    fn key(&self) -> &'static str {
        "local"
    }
    fn name(&self) -> &'static str {
        "Local music"
    }
    fn allows_repeats(&self) -> bool {
        true
    }
    fn suspend(&mut self) {
        self.scan = None;
    }
    fn start(&mut self) -> Result<Vec<Track>> {
        self.suspend();
        let root = self
            .root
            .canonicalize()
            .context("Local directory not found")?;
        ensure!(root.is_dir(), "Local music path must be a directory");
        self.discovered = 0;
        self.remaining.clear();
        self.invalid.clear();
        self.last = None;
        self.scan = Some(Scan::start(root, self.random.try_clone()?)?);
        self.next_batch()
    }
    fn more(&mut self, _: &[String]) -> Result<Vec<Track>> {
        self.next_batch()
    }
    fn resolve(&mut self, track: &Track) -> Result<Input> {
        let path = track_path(track)?;
        ensure!(path.is_file(), "Local audio file is no longer available");
        Ok(Input::File(path))
    }
    fn feedback(&mut self, _: &Track, _: Feedback, _: f64) -> Result<()> {
        Ok(())
    }
}

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp3" | "flac" | "m4a" | "mp4" | "ogg" | "oga"
            )
        })
}

fn local_track(path: &Path) -> Result<Track> {
    let decoder = rodio::Decoder::try_from(File::open(path)?)?;
    Ok(Track {
        source: String::new(),
        id: url::Url::from_file_path(path)
            .map_err(|_| anyhow::anyhow!("Invalid local audio path"))?
            .into(),
        title: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
        artist: String::new(),
        duration: decoder.total_duration().map(|d| d.as_secs_f64()),
        cover_url: None,
        genre: None,
        batch: String::new(),
    })
}

fn track_path(track: &Track) -> Result<PathBuf> {
    if track.id.starts_with("file:") {
        url::Url::parse(&track.id)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Invalid local audio path"))
    } else {
        Ok(PathBuf::from(&track.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        time::{Duration, SystemTime},
    };

    struct Library(PathBuf);

    impl Library {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let suffix = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let index = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "plavno-local-{}-{suffix}-{index}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn track(&self, name: impl AsRef<Path>) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"),
                &path,
            )
            .unwrap();
            path
        }
    }

    impl Drop for Library {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn wait_for_scan(scan: &Scan) {
        let (data, timeout) = scan
            .state
            .1
            .wait_timeout_while(
                scan.state.0.lock().unwrap(),
                Duration::from_secs(5),
                |data| !data.done,
            )
            .unwrap();
        assert!(!timeout.timed_out(), "Local scanner did not finish");
        assert!(data.error.is_none());
    }

    #[test]
    fn recursive_shuffle_discovers_every_file_without_symlink_cycles_and_repeats_only_after_a_pass()
    {
        let library = Library::new();
        let expected: HashSet<_> = (0..14)
            .map(|i| {
                let path = library.track(format!("artist-{}/album/track-{i}.FLAC", i % 3));
                local_track(&path).unwrap().id
            })
            .collect();
        fs::write(library.0.join("notes.txt"), b"not audio").unwrap();
        symlink(&library.0, library.0.join("loop")).unwrap();
        symlink(
            library.0.join("artist-0/album/track-0.FLAC"),
            library.0.join("duplicate.flac"),
        )
        .unwrap();
        let mut backend = LocalDirectory::new(library.0.clone()).unwrap();
        let mut tracks = backend.start().unwrap();
        wait_for_scan(backend.scan.as_ref().unwrap());
        let mut found: HashSet<_> = tracks.iter().map(|track| track.id.clone()).collect();
        while found.len() < expected.len() {
            tracks = backend.more(&[]).unwrap();
            for track in &tracks {
                assert!(
                    found.insert(track.id.clone()),
                    "Repeated before the end of the pass"
                );
                assert!(track.duration.unwrap() > 0.0);
            }
        }
        assert_eq!(found, expected);
        let last = tracks.last().unwrap().id.clone();
        let next = backend.more(&[]).unwrap();
        assert_ne!(last, next[0].id);
        let cancel = backend.scan.as_ref().unwrap().cancel.clone();
        backend.suspend();
        assert!(cancel.load(Ordering::Relaxed));

        let added = library.track("new.flac");
        backend.start().unwrap();
        wait_for_scan(backend.scan.as_ref().unwrap());
        assert!(
            backend
                .scan
                .as_ref()
                .unwrap()
                .state
                .0
                .lock()
                .unwrap()
                .files
                .contains(&added)
        );
    }

    #[test]
    fn playback_can_start_before_scan_completion_and_accept_later_discoveries() {
        let library = Library::new();
        let first = library.track("first.flac");
        let second = library.track("subdir/second.flac");
        let mut backend = LocalDirectory::new(library.0.clone()).unwrap();
        let state = Arc::new((
            Mutex::new(ScanState {
                files: vec![first.clone()],
                ..ScanState::default()
            }),
            Condvar::new(),
        ));
        backend.scan = Some(Scan {
            state: state.clone(),
            cancel: Arc::default(),
        });
        assert_eq!(
            track_path(&backend.next_batch().unwrap()[0]).unwrap(),
            first
        );
        assert!(!state.0.lock().unwrap().done);
        state.0.lock().unwrap().files.push(second.clone());
        assert_eq!(track_path(&backend.more(&[]).unwrap()[0]).unwrap(), second);
    }

    #[test]
    fn invalid_and_deleted_files_are_handled_without_losing_playable_tracks() {
        let library = Library::new();
        let valid = library.track("track-музыка.flac");
        let corrupt = library.0.join("corrupt.flac");
        fs::write(&corrupt, b"not audio").unwrap();
        let mut backend = LocalDirectory::new(library.0.clone()).unwrap();
        backend.scan = Some(Scan {
            state: Arc::new((
                Mutex::new(ScanState {
                    files: vec![corrupt, library.0.join("removed.flac"), valid.clone()],
                    done: true,
                    error: None,
                }),
                Condvar::new(),
            )),
            cancel: Arc::default(),
        });
        for _ in 0..3 {
            let tracks = backend.next_batch().unwrap();
            assert_eq!(tracks.len(), 1);
            assert!(
                matches!(backend.resolve(&tracks[0]).unwrap(), Input::File(path) if path == valid)
            );
        }
        fs::remove_file(valid).unwrap();
        assert!(backend.next_batch().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_audio_paths_round_trip_without_loss() {
        use std::os::unix::ffi::OsStringExt;
        let library = Library::new();
        let path = library.track(std::ffi::OsString::from_vec(b"track-\xff.flac".to_vec()));
        assert_eq!(track_path(&local_track(&path).unwrap()).unwrap(), path);
    }

    #[test]
    fn empty_missing_and_file_roots_report_errors_and_single_file_still_plays_once() {
        let library = Library::new();
        let mut empty = LocalDirectory::new(library.0.clone()).unwrap();
        assert!(
            empty
                .start()
                .unwrap_err()
                .to_string()
                .contains("No playable audio files")
        );
        let mut missing = LocalDirectory::new(library.0.join("missing")).unwrap();
        assert!(missing.start().is_err());
        let path = library.track("single.flac");
        assert!(LocalDirectory::new(path.clone()).unwrap().start().is_err());
        let mut single = Local(path.clone());
        let tracks = single.start().unwrap();
        assert_eq!(tracks.len(), 1);
        assert!(matches!(single.resolve(&tracks[0]).unwrap(), Input::File(file) if file == path));
        assert!(single.more(&[]).unwrap().is_empty());
    }
}
