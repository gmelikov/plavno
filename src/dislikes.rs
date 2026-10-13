use crate::{backend::Track, config, history};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct DislikedTrack {
    pub source: String,
    pub track: Track,
}

impl DislikedTrack {
    fn new(source: &str, track: &Track) -> Self {
        let mut track = track.clone();
        if track.source.is_empty() {
            track.source = source.into();
        }
        track.batch.clear();
        track.cover_url = None;
        Self {
            source: track.source.clone(),
            track,
        }
    }
}

#[derive(Default)]
pub struct Dislikes {
    entries: Vec<DislikedTrack>,
    keys: HashSet<String>,
    path: Option<PathBuf>,
}

fn read(path: &Path) -> Result<Vec<DislikedTrack>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("Cannot open disliked tracks"),
    };
    ensure!(
        file.metadata()?.len() <= 16 * 1024 * 1024,
        "The disliked tracks file is too large"
    );
    let entries: Vec<DislikedTrack> =
        serde_json::from_reader(file).context("Cannot read disliked tracks")?;
    ensure!(
        entries.iter().all(|entry| !entry.source.is_empty()
            && !entry.track.id.is_empty()
            && entry.source == entry.track.source),
        "Invalid disliked track identity"
    );
    Ok(entries)
}

impl Dislikes {
    pub fn open() -> Result<Self> {
        if cfg!(test) {
            return Ok(Self::default());
        }
        Self::load(history::path()?.with_file_name("dislikes.json"))
    }

    fn load(path: PathBuf) -> Result<Self> {
        let mut value = Self {
            path: Some(path.clone()),
            ..Self::default()
        };
        value.replace(read(&path)?);
        Ok(value)
    }

    fn replace(&mut self, entries: Vec<DislikedTrack>) {
        self.keys = entries
            .iter()
            .map(|entry| entry.track.cache_key())
            .collect();
        self.entries = entries;
    }

    pub fn contains(&self, source: &str, track: &Track) -> bool {
        let source = if track.source.is_empty() {
            source
        } else {
            &track.source
        };
        self.keys.contains(&format!("{source}:{}", track.id))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn update(&mut self, entry: DislikedTrack, disliked: bool) -> Result<()> {
        let lock = if let Some(path) = &self.path {
            let parent = path
                .parent()
                .context("The disliked tracks directory is not set")?;
            fs::create_dir_all(parent)?;
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(parent.join("dislikes.lock"))?;
            lock.lock().context("Cannot lock disliked tracks")?;
            Some(lock)
        } else {
            None
        };
        let mut entries = if let Some(path) = &self.path {
            read(path)?
        } else {
            self.entries.clone()
        };
        let key = entry.track.cache_key();
        entries.retain(|saved| saved.track.cache_key() != key);
        if disliked {
            entries.push(entry);
        }
        if let Some(path) = &self.path {
            config::save_private(path, &serde_json::to_vec(&entries)?)
                .context("Cannot save disliked tracks")?;
        }
        self.replace(entries);
        drop(lock);
        Ok(())
    }

    pub fn block(&mut self, source: &str, track: &Track) -> Result<DislikedTrack> {
        let entry = DislikedTrack::new(source, track);
        self.update(entry.clone(), true)?;
        Ok(entry)
    }

    pub fn undo(&mut self) -> Result<Option<DislikedTrack>> {
        let Some(entry) = self.entries.last().cloned() else {
            return Ok(None);
        };
        self.update(entry.clone(), false)?;
        Ok(Some(entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn track() -> Track {
        Track {
            source: String::new(),
            id: "1".into(),
            title: "Track".into(),
            artist: "Artist".into(),
            duration: None,
            cover_url: None,
            batch: "session".into(),
        }
    }

    #[test]
    fn dislikes_survive_restart_keep_service_ids_separate_and_preserve_corrupt_state() {
        let root = std::env::temp_dir().join(format!("plavno-dislikes-{}", std::process::id()));
        let path = root.join("dislikes.json");
        let mut first = Dislikes::load(path.clone()).unwrap();
        first.block("yandex", &track()).unwrap();
        let mut second = Dislikes::load(path.clone()).unwrap();
        assert!(second.contains("yandex", &track()));
        assert!(!second.contains("vk", &track()));
        second.block("vk", &track()).unwrap();
        first.block("soundcloud", &track()).unwrap();
        let mut restored = Dislikes::load(path.clone()).unwrap();
        for source in ["yandex", "vk", "soundcloud"] {
            assert!(restored.contains(source, &track()));
        }
        assert_eq!(restored.undo().unwrap().unwrap().source, "soundcloud");
        assert!(
            !Dislikes::load(path.clone())
                .unwrap()
                .contains("soundcloud", &track())
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!fs::read_to_string(&path).unwrap().contains("session"));
        fs::write(&path, b"broken").unwrap();
        assert!(restored.block("spotify", &track()).is_err());
        assert!(!restored.contains("spotify", &track()));
        assert_eq!(fs::read(&path).unwrap(), b"broken");
        fs::remove_dir_all(root).unwrap();
    }
}
