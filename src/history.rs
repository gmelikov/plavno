use crate::backend::Track;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    env, fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const LIMIT: usize = 2000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub source: String,
    pub track: Track,
    pub played_at: u64,
}

impl Entry {
    pub fn new(source: &str, track: &Track) -> Self {
        let mut track = track.clone();
        track.batch.clear();
        Self {
            source: if track.source.is_empty() {
                source.into()
            } else {
                track.source.clone()
            },
            track,
            played_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        let title = self.track.title.to_lowercase();
        let artist = self.track.artist.to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| title.contains(word) || artist.contains(word))
    }
}

pub fn push(entries: &mut VecDeque<Entry>, entry: Entry) {
    entries.push_back(entry);
    while entries.len() > LIMIT {
        entries.pop_front();
    }
}

pub fn path() -> Result<PathBuf> {
    let base = match env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".local/state"),
    };
    Ok(base.join("plavno/history.json"))
}

pub fn load(path: &Path) -> Result<VecDeque<Entry>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(VecDeque::new()),
        Err(error) => return Err(error).context("Failed to open history"),
    };
    ensure!(
        file.metadata()?.len() <= 8 * 1024 * 1024,
        "The history file is too large"
    );
    let mut entries: VecDeque<Entry> =
        serde_json::from_reader(file).context("Failed to read history")?;
    while entries.len() > LIMIT {
        entries.pop_front();
    }
    Ok(entries)
}

pub fn save_entry(path: &Path, entry: Entry) -> Result<()> {
    let parent = path.parent().context("The history directory is not set")?;
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(parent.join("history.lock"))?;
    lock.lock().context("Failed to lock history")?;
    let mut entries = load(path)?;
    push(&mut entries, entry);
    let temp = parent.join(format!(".history-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        serde_json::to_writer(&mut file, &entries)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.context("Failed to save history")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: usize) -> Entry {
        Entry::new(
            "yandex",
            &Track {
                source: String::new(),
                id: id.to_string(),
                title: "Night wave".into(),
                artist: "My artist".into(),
                duration: Some(60.0),
                cover_url: Some("https://example.com/cover.jpg".into()),
                genre: None,
                batch: "old-session".into(),
            },
        )
    }

    #[test]
    fn history_survives_restart_preserves_sources_and_does_not_overwrite_corruption() {
        let dir = env::temp_dir().join(format!("plavno-history-{}", std::process::id()));
        let path = dir.join("history.json");
        assert!(load(&path).unwrap().is_empty());
        save_entry(&path, entry(1)).unwrap();
        let mut local = entry(2);
        local.source = "local".into();
        save_entry(&path, local).unwrap();
        let entries = load(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].track.id, "1");
        assert_eq!(entries[1].source, "local");
        assert_eq!(
            entries[0].track.cover_url.as_deref(),
            Some("https://example.com/cover.jpg")
        );
        assert!(entries[0].track.batch.is_empty());
        assert!(entries[0].matches("WAVE artist"));
        assert!(!entries[0].matches("wave other"));
        let bytes = fs::read(&path).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("batch"));
        let mut legacy = serde_json::to_value(&entries).unwrap();
        for entry in legacy.as_array_mut().unwrap() {
            entry["track"].as_object_mut().unwrap().remove("cover_url");
        }
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(
            load(&path)
                .unwrap()
                .iter()
                .all(|entry| entry.track.cover_url.is_none())
        );
        fs::write(&path, b"broken").unwrap();
        assert!(save_entry(&path, entry(3)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_keeps_recent_plays_in_order() {
        let mut entries = VecDeque::new();
        for id in 0..LIMIT + 5 {
            push(&mut entries, entry(id));
        }
        assert_eq!(entries.len(), LIMIT);
        assert_eq!(entries.front().unwrap().track.id, "5");
        assert_eq!(entries.back().unwrap().track.id, (LIMIT + 4).to_string());
    }
}
