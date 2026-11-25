use crate::backend::Track;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const LIMIT: usize = 2000;
const LOG_BYTES_LIMIT: u64 = 8 * 1024 * 1024;
const COMPACT_CHECK_BYTES: u64 = 64 * 1024;
const COMPACT_AFTER: usize = LIMIT + 256;

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
    let base = plavno_platform::state_home()?;
    Ok(base.join("plavno/history.jsonl"))
}

pub fn load(path: &Path) -> Result<VecDeque<Entry>> {
    if let Some(file) = open_if_exists(path)? {
        return Ok(read_log(file)?.0);
    }
    let legacy = read_legacy(&path.with_extension("json"));
    // A writer can publish the JSONL log and remove the old file during startup.
    match open_if_exists(path)? {
        Some(file) => Ok(read_log(file)?.0),
        None => legacy,
    }
}

fn open_if_exists(path: &Path) -> Result<Option<fs::File>> {
    match fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Failed to open history"),
    }
}

fn read_legacy(path: &Path) -> Result<VecDeque<Entry>> {
    let Some(file) = open_if_exists(path)? else {
        return Ok(VecDeque::new());
    };
    ensure!(
        file.metadata()?.len() <= LOG_BYTES_LIMIT,
        "The history file is too large"
    );
    let mut entries: VecDeque<Entry> =
        serde_json::from_reader(BufReader::new(file)).context("Failed to read legacy history")?;
    while entries.len() > LIMIT {
        entries.pop_front();
    }
    Ok(entries)
}

fn read_log(mut file: fs::File) -> Result<(VecDeque<Entry>, usize)> {
    let size = file.metadata()?.len();
    ensure!(size <= LOG_BYTES_LIMIT, "The history file is too large");
    file.rewind()?;
    let mut reader = BufReader::new(file.take(size));
    let mut recent = VecDeque::new();
    let mut count = 0;
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        count += 1;
        match serde_json::from_slice(&line) {
            Ok(entry) => push(&mut recent, entry),
            // Skip truncated records. Each following line can still be read independently.
            Err(error) if error.is_eof() || !line.ends_with(b"\n") => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Invalid history line {count}"));
            }
        }
        line.clear();
    }
    Ok((recent, count))
}

fn encode(entry: &Entry) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    Ok(line)
}

pub fn save_entry(path: &Path, entry: Entry) -> Result<()> {
    let parent = path.parent().context("The history directory is not set")?;
    plavno_platform::private_directory(parent)?;
    let lock = plavno_platform::private_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join("history.lock"))?;
    lock.lock().context("Failed to lock history")?;
    let line = encode(&entry)?;
    ensure!(
        line.len() as u64 <= LOG_BYTES_LIMIT,
        "The history entry is too large"
    );
    let mut file = match plavno_platform::private_options()
        .read(true)
        .append(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy = path.with_extension("json");
            let entries = read_legacy(&legacy)?;
            replace_log(path, &entries, &line)?;
            return match fs::remove_file(legacy) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("Failed to remove migrated history"),
            };
        }
        Err(error) => return Err(error).context("Failed to open history for appending"),
    };
    let size = file.metadata()?.len();
    ensure!(size <= LOG_BYTES_LIMIT, "The history file is too large");
    let new_size = size + line.len() as u64;
    let mut last = *b"\n";
    if size > 0 {
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
    }
    let needs_rewrite = last != *b"\n" || new_size > LOG_BYTES_LIMIT;
    // Check the number of records periodically without reading the log on each play.
    if needs_rewrite || size / COMPACT_CHECK_BYTES != new_size / COMPACT_CHECK_BYTES {
        let (entries, count) = read_log(file.try_clone()?)?;
        if needs_rewrite || count + 1 >= COMPACT_AFTER {
            drop(file);
            return replace_log(path, &entries, &line);
        }
    }
    file.write_all(&line).context("Failed to append to history")
}

fn replace_log(path: &Path, entries: &VecDeque<Entry>, last: &[u8]) -> Result<()> {
    let mut size = last.len() as u64;
    let mut recent = VecDeque::new();
    for entry in entries.iter().rev().take(LIMIT - 1) {
        let line = encode(entry)?;
        size += line.len() as u64;
        if size > LOG_BYTES_LIMIT {
            break;
        }
        recent.push_front(line);
    }
    let temp = path.with_file_name(format!(".history-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let file = plavno_platform::private_options()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut writer = BufWriter::new(file);
        for line in recent {
            writer.write_all(&line)?;
        }
        writer.write_all(last)?;
        writer.flush()?;
        drop(writer);
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "plavno-history-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("history.jsonl")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
        let fixture = Fixture::new();
        let path = fixture.path();
        assert!(load(&path).unwrap().is_empty());
        save_entry(&path, entry(1)).unwrap();
        let first_line = fs::read(&path).unwrap();
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
        assert!(bytes.starts_with(&first_line));
        assert_eq!(bytes.split_inclusive(|byte| *byte == b'\n').count(), 2);
        assert!(!String::from_utf8(bytes).unwrap().contains("batch"));
        fs::write(&path, b"broken\n").unwrap();
        assert!(load(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken\n");
    }

    #[test]
    fn history_removes_the_original_after_successful_migration() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let legacy_path = path.with_extension("json");
        let mut entries = VecDeque::from([entry(1), entry(2)]);
        let mut legacy = serde_json::to_value(&entries).unwrap();
        for entry in legacy.as_array_mut().unwrap() {
            entry["track"].as_object_mut().unwrap().remove("cover_url");
        }
        let original = serde_json::to_vec(&legacy).unwrap();
        fs::write(&legacy_path, &original).unwrap();
        assert!(
            load(&path)
                .unwrap()
                .iter()
                .all(|entry| entry.track.cover_url.is_none())
        );
        assert!(!path.exists());
        assert_eq!(fs::read(&legacy_path).unwrap(), original);
        save_entry(&path, entry(3)).unwrap();
        assert!(!legacy_path.exists());
        save_entry(&path, entry(4)).unwrap();
        entries = load(&path).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.track.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2", "3", "4"],
        );
    }

    #[test]
    fn malformed_legacy_history_is_preserved() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let legacy = path.with_extension("json");
        fs::write(&legacy, b"broken").unwrap();
        assert!(save_entry(&path, entry(1)).is_err());
        assert!(!path.exists());
        assert_eq!(fs::read(legacy).unwrap(), b"broken");
    }

    #[test]
    fn failed_migration_keeps_the_legacy_file() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let legacy = path.with_extension("json");
        let original = serde_json::to_vec(&[entry(1)]).unwrap();
        fs::write(&legacy, &original).unwrap();
        fs::write(
            path.with_file_name(format!(".history-{}", std::process::id())),
            b"existing temporary file",
        )
        .unwrap();

        assert!(save_entry(&path, entry(2)).is_err());
        assert!(!path.exists());
        assert_eq!(fs::read(&legacy).unwrap(), original);
    }

    #[test]
    fn appending_recovers_an_incomplete_final_line() {
        let fixture = Fixture::new();
        let path = fixture.path();
        save_entry(&path, entry(1)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"source\":")
            .unwrap();
        assert_eq!(load(&path).unwrap().len(), 1);
        save_entry(&path, entry(2)).unwrap();
        let entries = load(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].track.id, "1");
        assert_eq!(entries[1].track.id, "2");
    }

    #[test]
    fn incomplete_history_line_does_not_hide_following_records() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut bytes = encode(&entry(1)).unwrap();
        bytes.extend_from_slice(b"{\"source\":\n");
        bytes.extend_from_slice(&encode(&entry(2)).unwrap());
        fs::write(&path, bytes).unwrap();
        save_entry(&path, entry(3)).unwrap();

        let entries = load(&path).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.track.id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
    }

    #[test]
    fn history_compacts_to_the_most_recent_plays() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut writer = BufWriter::new(fs::File::create(&path).unwrap());
        let mut size = 0;
        for id in 0..COMPACT_AFTER {
            let mut record = entry(id);
            record.track.title = "x".repeat(128);
            let line = encode(&record).unwrap();
            writer.write_all(&line).unwrap();
            size += line.len() as u64;
        }
        // Whitespace is valid inside a JSON line. Pad the last record up to the next check.
        let padding = COMPACT_CHECK_BYTES - 1 - size % COMPACT_CHECK_BYTES;
        writer.flush().unwrap();
        drop(writer);
        let mut writer = fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::End(-1)).unwrap();
        writer.write_all(&vec![b' '; padding as usize]).unwrap();
        writer.write_all(b"\n").unwrap();
        drop(writer);

        save_entry(&path, entry(COMPACT_AFTER)).unwrap();
        let (recent, count) = read_log(fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(count, LIMIT);
        assert_eq!(
            recent.front().unwrap().track.id,
            (COMPACT_AFTER + 1 - LIMIT).to_string()
        );
        assert_eq!(recent.back().unwrap().track.id, COMPACT_AFTER.to_string());
    }

    #[test]
    fn concurrent_history_writers_keep_each_record_once() {
        let fixture = Fixture::new();
        let path = fixture.path();
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let path = &path;
                scope.spawn(move || {
                    for id in worker * 25..(worker + 1) * 25 {
                        save_entry(path, entry(id)).unwrap();
                    }
                });
            }
        });
        let mut ids: Vec<usize> = load(&path)
            .unwrap()
            .iter()
            .map(|entry| entry.track.id.parse().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..100).collect::<Vec<_>>());
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
