use super::{RateLimited, yandex::FileDownload};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc2822};

const TTL: u64 = 300;
const MAX_ENTRIES: usize = 64;
const MAX_BYTES: u64 = 512 * 1024;

pub fn now() -> u64 {
    OffsetDateTime::now_utc().unix_timestamp().max(0) as u64
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    created: u64,
    info: FileDownload,
}

#[derive(Serialize, Deserialize)]
struct State {
    version: u8,
    entries: HashMap<String, Entry>,
    blocked_until: u64,
    #[serde(default)]
    cdn_until: u64,
    failures: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            entries: HashMap::new(),
            blocked_until: 0,
            cdn_until: 0,
            failures: 0,
        }
    }
}

#[derive(Default)]
pub struct Cache {
    path: Option<PathBuf>,
    state: State,
}

impl Cache {
    pub fn persistent(directory: PathBuf, token: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"plavno/yandex/v1\0");
        for policy in [
            super::yandex::FILE_QUALITY,
            super::yandex::FILE_TRANSPORTS,
            super::yandex::FILE_CODECS,
        ] {
            digest.update(policy.as_bytes());
            digest.update([0]);
        }
        digest.update(token.as_bytes());
        let path = directory.join(format!("{:x}.json", digest.finalize()));
        let state = Self::read(&path).unwrap_or_default();
        Self {
            path: Some(path),
            state,
        }
    }

    fn read(path: &std::path::Path) -> Option<State> {
        if std::fs::symlink_metadata(path).ok()?.len() > MAX_BYTES {
            return None;
        }
        let state: State = serde_json::from_str(&super::web::private(path).ok()?).ok()?;
        (state.version == 1 && state.entries.len() <= MAX_ENTRIES).then_some(state)
    }

    fn save(&self) {
        if let Some(path) = &self.path
            && let Ok(bytes) = serde_json::to_vec(&self.state)
            && bytes.len() as u64 <= MAX_BYTES
        {
            let _ = crate::config::save_private(path, &bytes);
        }
    }

    fn lock(path: &Path) -> std::io::Result<File> {
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        Ok(lock)
    }

    fn update<T>(&mut self, change: impl FnOnce(&mut State) -> T) -> T {
        let lock = self.path.as_ref().and_then(|path| Self::lock(path).ok());
        if lock.is_some()
            && let Some(mut state) = self.path.as_ref().and_then(|path| Self::read(path))
        {
            if self.state.blocked_until > state.blocked_until {
                state.blocked_until = self.state.blocked_until;
                state.failures = state.failures.max(self.state.failures);
            }
            state.cdn_until = state.cdn_until.max(self.state.cdn_until);
            self.state = state;
        }
        let result = change(&mut self.state);
        if lock.is_some() {
            self.save();
        }
        result
    }

    pub fn get(&mut self, id: &str, now: u64) -> Option<FileDownload> {
        self.state
            .entries
            .retain(|_, entry| entry.created <= now && now - entry.created < TTL);
        self.state.entries.get(id).map(|entry| entry.info.clone())
    }

    pub fn remove(&mut self, id: &str) {
        self.update(|state| {
            state.entries.remove(id);
        });
    }

    pub fn insert(&mut self, id: &str, info: FileDownload, now: u64) {
        self.update(|state| {
            state
                .entries
                .retain(|_, entry| entry.created <= now && now - entry.created < TTL);
            state
                .entries
                .insert(id.into(), Entry { created: now, info });
            while state.entries.len() > MAX_ENTRIES {
                let oldest = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.created)
                    .map(|(id, _)| id.clone())
                    .unwrap();
                state.entries.remove(&oldest);
            }
            if state.blocked_until <= now {
                state.blocked_until = 0;
                state.cdn_until = 0;
                state.failures = 0;
            }
        });
    }

    pub fn guard(&mut self, now: u64) -> Result<()> {
        self.refresh_limits();
        Self::check(self.state.blocked_until, now)
    }

    pub fn guard_cdn(&mut self, now: u64) -> Result<()> {
        self.refresh_limits();
        Self::check(self.state.cdn_until, now)
    }

    fn refresh_limits(&mut self) {
        if let Some(path) = &self.path
            && let Some(state) = Self::read(path)
        {
            self.state.blocked_until = self.state.blocked_until.max(state.blocked_until);
            self.state.cdn_until = self.state.cdn_until.max(state.cdn_until);
            self.state.failures = self.state.failures.max(state.failures);
        }
    }

    fn check(until: u64, now: u64) -> Result<()> {
        if until > now {
            return Err(RateLimited::new(Duration::from_secs(until - now)).into());
        }
        Ok(())
    }

    pub fn throttled(&mut self, header: Option<&str>, now: u64) -> RateLimited {
        self.throttle(header, now, false)
    }

    pub fn throttled_cdn(&mut self, header: Option<&str>, now: u64) -> RateLimited {
        self.throttle(header, now, true)
    }

    fn throttle(&mut self, header: Option<&str>, now: u64, cdn: bool) -> RateLimited {
        let explicit = header.and_then(|header| {
            header.trim().parse::<u64>().ok().or_else(|| {
                OffsetDateTime::parse(header.trim(), &Rfc2822)
                    .ok()
                    .map(|date| (date.unix_timestamp().max(0) as u64).saturating_sub(now))
            })
        });
        let wait = self.update(|state| {
            let fallback = (60u64 << state.failures.min(4)).min(900);
            let wait = explicit.unwrap_or(fallback).max(1);
            state.failures = state.failures.saturating_add(1);
            state.blocked_until = state.blocked_until.max(now.saturating_add(wait));
            if cdn {
                state.cdn_until = state.cdn_until.max(now.saturating_add(wait));
            }
            if cdn {
                state.cdn_until
            } else {
                state.blocked_until
            }
            .saturating_sub(now)
        });
        RateLimited::new(Duration::from_secs(wait))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn info() -> FileDownload {
        serde_json::from_value(
            serde_json::json!({"codec":"mp3","quality":"high","bitrate":320,
            "transport":"raw","url":"https://example.test/signed?token=secret"}),
        )
        .unwrap()
    }

    #[test]
    fn metadata_cache_is_private_account_scoped_bounded_and_expires_after_restart() {
        let directory = std::env::temp_dir().join(format!(
            "plavno-yandex-cache-{}-{}",
            std::process::id(),
            now()
        ));
        let mut cache = Cache::persistent(directory.clone(), "private-account-token");
        for id in 0..100 {
            cache.insert(&id.to_string(), info(), 1000 + id);
        }
        assert_eq!(cache.state.entries.len(), MAX_ENTRIES);
        assert!(cache.get("0", 1100).is_none());
        let path = cache.path.clone().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("private-account-token")
        );
        let mut restarted = Cache::persistent(directory.clone(), "private-account-token");
        assert!(restarted.get("99", 1100).is_some());
        assert!(restarted.get("99", 1399).is_none());
        assert!(
            Cache::persistent(directory.clone(), "other-account-token")
                .get("99", 1100)
                .is_none()
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            Cache::persistent(directory.clone(), "private-account-token")
                .get("99", 1100)
                .is_none()
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, "malformed JSON").unwrap();
        assert!(
            Cache::persistent(directory.clone(), "private-account-token")
                .get("99", 1100)
                .is_none()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retry_after_and_backoff_survive_restart_and_are_seen_by_an_existing_instance() {
        let directory = std::env::temp_dir().join(format!(
            "plavno-yandex-quota-{}-{}",
            std::process::id(),
            now()
        ));
        let mut cache = Cache::persistent(directory.clone(), "token");
        let mut existing = Cache::persistent(directory.clone(), "token");
        let limit = cache.throttled(Some("3600"), 1000);
        assert!(limit.active());
        assert!(existing.guard(4599).is_err());
        let mut restarted = Cache::persistent(directory.clone(), "token");
        assert!(restarted.guard(4599).is_err());
        assert!(restarted.guard(4600).is_ok());
        restarted.throttled(None, 4600);
        assert_eq!(restarted.state.blocked_until, 4720);
        restarted.insert("1", info(), 4720);
        restarted.throttled(None, 4720);
        assert_eq!(restarted.state.blocked_until, 4780);
        for _ in 0..10 {
            restarted.throttled(Some("invalid"), 5000);
        }
        assert_eq!(restarted.state.blocked_until, 5900);
        restarted.throttled(Some("Wed, 21 Oct 2015 07:28:00 GMT"), 1_445_412_420);
        assert_eq!(restarted.state.blocked_until, 1_445_412_480);
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn in_flight_success_and_removal_preserve_another_instances_cooldown() {
        let directory =
            std::env::temp_dir().join(format!("plavno-cache-in-flight-{}", std::process::id()));
        let mut resolving = Cache::persistent(directory.clone(), "token");
        let mut limited = Cache::persistent(directory.clone(), "token");
        resolving.guard(1000).unwrap();
        limited.throttled(Some("120"), 1000);
        resolving.insert("1", info(), 1001);
        let mut restarted = Cache::persistent(directory.clone(), "token");
        assert!(restarted.guard(1002).is_err());
        limited.throttled(Some("1"), 1002);
        assert_eq!(limited.state.blocked_until, 1120);
        resolving.remove("1");
        assert!(
            Cache::persistent(directory.clone(), "token")
                .guard(1119)
                .is_err()
        );
        restarted.insert("2", info(), 1120);
        assert!(
            Cache::persistent(directory.clone(), "token")
                .guard(1120)
                .is_ok()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_cache_writers_preserve_each_others_entries() {
        use std::sync::{Arc, Barrier};
        let directory =
            std::env::temp_dir().join(format!("plavno-cache-writers-{}", std::process::id()));
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|worker| {
                let mut cache = Cache::persistent(directory.clone(), "token");
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for index in 0..20 {
                        cache.insert(&format!("{worker}-{index}"), info(), 1000);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let mut cache = Cache::persistent(directory.clone(), "token");
        for worker in 0..2 {
            for index in 0..20 {
                assert!(cache.get(&format!("{worker}-{index}"), 1001).is_some());
            }
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn api_and_cdn_deadlines_are_independent_and_legacy_files_keep_their_limit() {
        let mut cache = Cache::default();
        cache.throttled_cdn(Some("120"), 1000);
        cache.throttled(Some("600"), 1001);
        assert!(cache.guard_cdn(1119).is_err());
        assert!(cache.guard_cdn(1120).is_ok());
        assert!(cache.guard(1120).is_err());
        cache.insert("1", info(), 1120);
        assert!(cache.guard_cdn(1120).is_ok());
        assert!(cache.guard(1600).is_err());
        assert!(cache.guard(1601).is_ok());
        let legacy =
            serde_json::json!({"version":1,"entries":{},"blocked_until":2000,"failures":2});
        let state: State = serde_json::from_value(legacy).unwrap();
        assert_eq!(state.blocked_until, 2000);
        assert_eq!(state.cdn_until, 0);
    }
}
