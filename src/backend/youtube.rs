mod likes;

use super::{Backend, Feedback, Input, RateLimited, Track, web};
use anyhow::{Context, Result, ensure};
use plavno_platform::process::Process;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct YouTube {
    profile: PathBuf,
    program: OsString,
    seed: Option<String>,
    session: Option<Session>,
    enable_node: bool,
    api: ureq::Agent,
    #[cfg(test)]
    api_base: Option<String>,
}

struct Session {
    radio: Option<String>,
    personal: bool,
    playlist: String,
    offset: usize,
    cookies: Option<PathBuf>,
    account: String,
    auth_user: String,
    liked: VecDeque<(String, bool)>,
    favorites: Option<VecDeque<Track>>,
    favorites_last: Option<String>,
}

fn temporary_path(kind: &str) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    Ok(std::env::temp_dir().join(format!(
        "plavno-ytdlp-{kind}-{}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )))
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(path) = &self.cookies {
            let _ = fs::remove_file(path);
            let _ = fs::remove_dir(path.parent().unwrap());
        }
    }
}

fn seed_id(seed: &str) -> Result<String> {
    let id = if seed.starts_with("https://") {
        let url = url::Url::parse(seed).context("Invalid YouTube seed URL")?;
        ensure!(
            matches!(
                url.host_str(),
                Some("youtube.com" | "www.youtube.com" | "music.youtube.com" | "youtu.be")
            ),
            "Use a YouTube video ID or URL"
        );
        url.query_pairs()
            .find(|(key, _)| key == "v")
            .map(|(_, value)| value.into_owned())
            .or_else(|| {
                url.path_segments()
                    .and_then(|mut parts| parts.next_back().map(str::to_owned))
            })
            .context("Missing YouTube video ID")?
    } else {
        seed.to_owned()
    };
    ensure!(valid_id(&id), "Invalid YouTube video ID");
    Ok(id)
}

fn valid_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

impl YouTube {
    pub fn new(profile: PathBuf) -> Self {
        Self {
            profile,
            program: std::env::var_os("PLAVNO_YT_DLP").unwrap_or_else(|| "yt-dlp".into()),
            seed: None,
            session: None,
            enable_node: true,
            api: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(20)))
                .build()
                .into(),
            #[cfg(test)]
            api_base: None,
        }
    }

    pub fn with_seed(mut self, seed: Option<&str>) -> Self {
        self.seed = seed.map(str::to_owned);
        self
    }

    fn session(&mut self) -> Result<&Session> {
        if self.session.is_none() {
            let config = if self.profile.try_exists()? {
                web::profile(&self.profile)?
            } else {
                json!({})
            };
            ensure!(
                config.is_object(),
                "The YouTube profile must be a JSON object"
            );
            let radio = self
                .seed
                .as_deref()
                .or(config["seed"].as_str())
                .filter(|s| !s.is_empty())
                .map(seed_id)
                .transpose()?;
            let playlist = config["playlist"]
                .as_str()
                .filter(|id| !id.is_empty())
                .unwrap_or("RDMM")
                .to_owned();
            ensure!(
                !playlist.is_empty()
                    && playlist.len() <= 256
                    && playlist
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
                "Invalid YouTube playlist ID"
            );
            let mut auth_user = config["auth_user"].as_str().unwrap_or("0").to_owned();
            let cookies = if let Some(path) = config["cookies"].as_str() {
                Some(web::private(std::path::Path::new(path))?)
            } else if let Some(path) = config["headers"].as_str() {
                let headers = web::profile(std::path::Path::new(path))?;
                if let Some(value) = headers
                    .as_object()
                    .and_then(|h| {
                        h.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("x-goog-authuser"))
                    })
                    .and_then(|(_, v)| v.as_str())
                {
                    auth_user = value.to_owned();
                }
                let header = headers
                    .as_object()
                    .and_then(|headers| {
                        headers
                            .iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case("cookie"))
                    })
                    .and_then(|(_, value)| value.as_str())
                    .context("Browser headers must contain a Cookie header")?;
                Some(web::Cookies::from_header(header, "https://youtube.com/")?.netscape())
            } else {
                None
            };
            ensure!(
                radio.is_some() || config["playlist"].is_string() || cookies.is_some(),
                "Configure a YouTube seed or playlist with plavno login ytmusic, or use --seed"
            );
            ensure!(
                !auth_user.is_empty()
                    && auth_user.len() <= 10
                    && auth_user.bytes().all(|b| b.is_ascii_digit()),
                "Invalid YouTube account index"
            );
            let mut digest = Sha256::new();
            digest.update(cookies.as_deref().unwrap_or("public"));
            if auth_user != "0" {
                digest.update(format!(":{auth_user}"));
            }
            let account = format!("{:x}", digest.finalize());
            let mut session = Session {
                personal: radio.is_none() && playlist == "RDMM",
                radio,
                playlist,
                offset: 1,
                cookies: None,
                account,
                auth_user,
                liked: VecDeque::new(),
                favorites: None,
                favorites_last: None,
            };
            if let Some(cookies) = cookies {
                let cookies = web::Cookies::parse(&cookies)?.netscape();
                let directory = temporary_path("cookies")?;
                fs::create_dir(&directory)?;
                session.cookies = Some(directory.join("cookies.txt"));
                plavno_platform::private_directory(&directory)?;
                plavno_platform::private_options()
                    .write(true)
                    .create_new(true)
                    .open(session.cookies.as_ref().unwrap())?
                    .write_all(cookies.as_bytes())?;
            }
            self.session = Some(session);
        }
        Ok(self.session.as_ref().unwrap())
    }

    fn command(&mut self) -> Result<Command> {
        let program = self.program.clone();
        let enable_node = self.enable_node;
        #[cfg(test)]
        let fixture_directory = self.profile.parent().unwrap().to_path_buf();
        let session = self.session()?;
        let mut command = Command::new(program);
        #[cfg(test)]
        command.env("PLAVNO_YTDLP_FIXTURE_DIR", fixture_directory);
        command.args([
            "--ignore-config",
            "--no-cache-dir",
            "--no-progress",
            "--dump-single-json",
            "--socket-timeout",
            "10",
            "--retries",
            "0",
            "--extractor-retries",
            "0",
        ]);
        if enable_node {
            command.args(["--js-runtimes", "node"]);
        }
        if let Some(cookies) = &session.cookies {
            command.arg("--cookies").arg(cookies);
        }
        Ok(command)
    }

    fn execute(&mut self, command: &mut Command) -> Result<Value> {
        self.check_cooldown()?;
        let (success, output, errors) = run(command)?;
        let errors = String::from_utf8_lossy(&errors);
        if http_status(&errors, 429) {
            let wait = regex_lite::Regex::new(r"(?i)retry-after[:= ]+([0-9]+)")
                .unwrap()
                .captures(&errors)
                .and_then(|c| c[1].parse::<u64>().ok())
                .unwrap_or(60)
                .max(1);
            return Err(self.limit(wait)?);
        }
        if !success
            && self.enable_node
            && errors.lines().any(|line| {
                line.trim() == "yt-dlp: error: no such option: --js-runtimes"
                    || line.trim() == "yt-dlp: error: unrecognized arguments: --js-runtimes node"
            })
        {
            // Older yt-dlp exits during argument parsing, before any network request.
            self.enable_node = false;
            let mut legacy = Command::new(command.get_program());
            #[cfg(test)]
            legacy.env("PLAVNO_YTDLP_FIXTURE_DIR", self.profile.parent().unwrap());
            let mut args = command.get_args();
            while let Some(arg) = args.next() {
                if arg == "--js-runtimes" {
                    args.next();
                } else {
                    legacy.arg(arg);
                }
            }
            return self.execute(&mut legacy);
        }
        ensure!(success, "{}", failure(&errors));
        serde_json::from_slice(&output).context("Invalid yt-dlp JSON response; update yt-dlp")
    }

    fn wave(&mut self, history: &[String]) -> Result<Vec<Track>> {
        if self.session()?.personal && self.session()?.radio.is_none() {
            let anchor = self.personal_seed()?;
            self.session.as_mut().unwrap().radio = Some(anchor);
        }
        let session = self.session()?;
        let seed = session.radio.clone();
        let mut address = url::Url::parse("https://www.youtube.com/watch")?;
        let (start, end) = if let Some(seed) = &seed {
            address
                .query_pairs_mut()
                .append_pair("v", seed)
                .append_pair(
                    "list",
                    &if session.personal {
                        "RDMM".into()
                    } else {
                        format!("RD{seed}")
                    },
                );
            (1, 26)
        } else {
            address.set_path("/playlist");
            address
                .query_pairs_mut()
                .append_pair("list", &session.playlist);
            (session.offset, session.offset + 24)
        };
        let value = self.playlist(address.as_str(), start, end)?;
        let mut seen: HashSet<String> = history.iter().cloned().collect();
        if let Some(seed) = seed {
            seen.insert(seed);
        }
        let tracks = tracks(&value, &mut seen)?;
        let session = self.session.as_mut().unwrap();
        if session.radio.is_some() || session.playlist.starts_with("RD") {
            if let Some(last) = tracks.last() {
                session.radio = Some(last.id.clone());
            }
        } else {
            session.offset = end + 1;
        }
        Ok(tracks)
    }

    fn playlist(&mut self, address: &str, start: usize, end: usize) -> Result<Value> {
        let mut command = self.command()?;
        command.args([
            "--flat-playlist",
            "--yes-playlist",
            "--playlist-items",
            &format!("{start}:{end}"),
            "--",
            address,
        ]);
        self.execute(&mut command)
    }
}

fn run(command: &mut Command) -> Result<(bool, Vec<u8>, Vec<u8>)> {
    let mut child = Process::spawn(command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()))
        .map_err(|error| if error.kind() == io::ErrorKind::NotFound { anyhow::anyhow!("YouTube requires yt-dlp; install it and make it available on PATH (Debian/Ubuntu: sudo apt install yt-dlp)") } else { anyhow::anyhow!("Cannot start yt-dlp: {error}") })?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let output = std::thread::spawn(move || bounded_output(stdout, 8 * 1024 * 1024));
    let errors = std::thread::spawn(move || bounded_output(stderr, 64 * 1024));
    let deadline = Instant::now() + Duration::from_secs(65);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                child.terminate();
                let _ = child.wait();
                break result.and_then(|_| {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "yt-dlp did not respond within 65 seconds",
                    ))
                });
            }
        }
    };
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("yt-dlp output reader stopped"))??;
    let errors = errors
        .join()
        .map_err(|_| anyhow::anyhow!("yt-dlp error reader stopped"))??;
    Ok((status?.success(), output, errors))
}

fn bounded_output(mut reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.by_ref().take(limit + 1).read_to_end(&mut data)?;
    io::copy(&mut reader, &mut io::sink())?;
    ensure!(
        data.len() as u64 <= limit,
        "yt-dlp output exceeded its size limit"
    );
    Ok(data)
}

fn http_status(errors: &str, status: u16) -> bool {
    regex_lite::Regex::new(&format!(
        r"(?i)(?:HTTP (?:Error )?|server returned ){status}\b"
    ))
    .unwrap()
    .is_match(errors)
}

fn failure(errors: &str) -> &'static str {
    if http_status(errors, 403) {
        "YouTube returned HTTP 403; update yt-dlp or refresh saved cookies"
    } else if errors.contains("Requested format is not available") {
        if errors.contains("challenge") || errors.contains("JavaScript") {
            "YouTube audio requires a JavaScript challenge solver; install Node.js 22+ or Deno and yt-dlp-ejs (see README)"
        } else {
            "YouTube did not offer the requested audio format; update yt-dlp and check access to the track"
        }
    } else if errors.contains("playlist type is unviewable") {
        "YouTube cannot open this playlist without a starting video; use --seed or choose another playlist"
    } else if errors.contains("Sign in") || errors.contains("cookies") {
        "YouTube requires sign-in; configure a private Netscape cookie file with plavno login ytmusic"
    } else if errors.contains("timed out") {
        "YouTube request timed out"
    } else {
        "yt-dlp could not load YouTube; update yt-dlp and check the seed, playlist, cookies, and network"
    }
}

fn tracks(value: &Value, seen: &mut HashSet<String>) -> Result<Vec<Track>> {
    let entries = value["entries"]
        .as_array()
        .context("yt-dlp did not return a playlist; configure a seed or playlist")?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let id = entry["id"].as_str()?;
            if !valid_id(id)
                || matches!(
                    entry["live_status"].as_str(),
                    Some("is_live" | "is_upcoming")
                )
                || !seen.insert(id.into())
            {
                return None;
            }
            let cover = entry["thumbnails"]
                .as_array()
                .and_then(|v| v.iter().rev().find_map(|v| v["url"].as_str()))
                .map(str::to_owned);
            Some(Track {
                source: String::new(),
                id: id.into(),
                title: entry["title"].as_str().unwrap_or(id).into(),
                artist: entry["artist"]
                    .as_str()
                    .or(entry["channel"].as_str())
                    .or(entry["uploader"].as_str())
                    .unwrap_or_default()
                    .into(),
                duration: entry["duration"].as_f64(),
                cover_url: cover,
                genre: None,
                batch: String::new(),
            })
        })
        .take(25)
        .collect())
}

fn stream(value: &Value) -> Result<Input> {
    ensure!(
        value["is_live"].as_bool() != Some(true),
        "Live streams are not supported"
    );
    let url = value["url"]
        .as_str()
        .context("yt-dlp did not return a full audio stream")?
        .to_owned();
    ensure!(url.starts_with("https://"), "Insecure YouTube audio URL");
    let headers = value["http_headers"]
        .as_object()
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(key, value)| {
                    let value = value.as_str()?;
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "user-agent"
                            | "accept"
                            | "accept-language"
                            | "referer"
                            | "origin"
                            | "sec-fetch-mode"
                    )
                    .then(|| (key.clone(), value.to_owned()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let quality = format!(
        "YouTube Music · {} {} kbps",
        value["acodec"].as_str().unwrap_or("audio"),
        value["abr"].as_f64().unwrap_or(0.0).round()
    );
    if value["ext"].as_str() == Some("m4a") && value["protocol"].as_str() == Some("https") {
        Ok(Input::Http {
            url,
            headers,
            aes128_key: None,
            hint: "m4a",
            quality,
        })
    } else {
        web::transcode_http(url, quality, headers)
    }
}

impl Backend for YouTube {
    fn key(&self) -> &'static str {
        "ytmusic"
    }
    fn name(&self) -> &'static str {
        "YouTube Music"
    }
    fn supports_favorites(&self) -> bool {
        Self::has_favorites_profile(&self.profile)
    }
    fn supports_favorites_shuffle(&self) -> bool {
        self.supports_favorites()
    }
    fn favorite_state(&mut self, track: &Track) -> Result<Option<bool>> {
        if !self.supports_favorites() {
            return Ok(None);
        }
        self.read_rating(track)
    }
    fn toggle_favorite(&mut self, track: &Track) -> Result<bool> {
        self.rate(track)
    }
    fn start_mode(&mut self, mode: super::PlaybackMode) -> Result<Vec<Track>> {
        self.start_favorites(mode)
    }
    fn suspend(&mut self) {
        self.session = None;
    }
    fn start(&mut self) -> Result<Vec<Track>> {
        self.session = None;
        self.wave(&[])
    }
    fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
        if self.session()?.favorites.is_some() {
            self.favorite_batch(history)
        } else {
            self.wave(history)
        }
    }
    fn resolve(&mut self, track: &Track) -> Result<Input> {
        ensure!(valid_id(&track.id), "Invalid YouTube video ID");
        let mut command = self.command()?;
        command.args([
            "--no-playlist",
            "--format",
            "bestaudio[protocol=https]",
            "--",
            &format!("https://www.youtube.com/watch?v={}", track.id),
        ]);
        let value = self.execute(&mut command)?;
        stream(&value)
    }

    fn feedback(&mut self, _: &Track, _: Feedback, _: f64) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, sync::OnceLock};

    pub(super) struct Fixture(pub(super) PathBuf, PathBuf);
    impl Fixture {
        pub(super) fn new() -> Self {
            static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
            let executable = EXECUTABLE.get_or_init(|| {
                let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join(format!("mock-ytdlp-{}", std::process::id()));
                fs::create_dir_all(&directory).unwrap();
                let output = directory.join(format!("yt-dlp{}", std::env::consts::EXE_SUFFIX));
                let result = Command::new("rustc")
                    .arg("--edition=2024")
                    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_ytdlp.rs"))
                    .arg("-o")
                    .arg(&output)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                output
            });
            let directory = temporary_path("fixture").unwrap();
            fs::create_dir(&directory).unwrap();
            Self(directory, executable.clone())
        }
        fn program(&self) -> PathBuf {
            self.1.clone()
        }
        pub(super) fn backend(&self) -> YouTube {
            let mut youtube =
                YouTube::new(self.0.join("ytmusic.json")).with_seed(Some("5LGUgChapj4"));
            youtube.program = self.program().into_os_string();
            youtube
        }
        pub(super) fn reply(&self, number: usize, value: Value) {
            fs::write(
                self.0.join(format!("{number}.json")),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        }
        pub(super) fn calls(&self) -> Vec<Vec<String>> {
            fs::read_to_string(self.0.join("calls"))
                .unwrap_or_default()
                .lines()
                .map(|line| line.split('\0').map(str::to_owned).collect())
                .collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn public_radio_uses_flat_batches_and_only_resolves_selected_tracks() {
        let fixture = Fixture::new();
        let first = json!({"entries":[{"id":"5LGUgChapj4","title":"seed"},{"id":"9EcDe6qS0Xc","title":"Track","channel":"Artist","duration":181,"thumbnails":[{"url":"https://cover.test/a.jpg"}]},{"id":"9EcDe6qS0Xc"},{"id":"abcdefghijk","live_status":"is_live"},null]});
        fixture.reply(1, first.clone());
        fixture.reply(2, json!({"url":"https://cdn.test/audio?sig=private","ext":"webm","protocol":"https","acodec":"opus","abr":128,"http_headers":{"User-Agent":"fixture","Cookie":"secret","Host":"evil.test"}}));
        fixture.reply(
            3,
            json!({"entries":[{"id":"9EcDe6qS0Xc"},{"id":"abcdefghijk","title":"Next"}]}),
        );
        fixture.reply(4, first);
        let mut youtube = fixture.backend();
        let tracks = youtube.start().unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].artist, "Artist");
        assert_eq!(tracks[0].duration, Some(181.0));
        assert!(tracks[0].cover_url.is_some());
        let Input::Pipe { request, .. } = youtube.resolve(&tracks[0]).unwrap() else {
            panic!("Opus must use the converter")
        };
        assert_eq!(request["headers"], json!([["User-Agent", "fixture"]]));
        assert_eq!(request["direct"], true);
        assert_eq!(
            youtube.more(&[tracks[0].id.clone()]).unwrap()[0].id,
            "abcdefghijk"
        );
        youtube.start().unwrap();
        let calls = fixture.calls();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].contains(&"--flat-playlist".into()));
        assert!(calls[0].contains(&"1:26".into()));
        assert!(!calls[1].contains(&"--flat-playlist".into()));
        assert!(calls[1].contains(&"--no-playlist".into()));
        assert!(calls[2].last().unwrap().contains("v=9EcDe6qS0Xc"));
        assert_eq!(calls[0].last(), calls[3].last());
        assert!(!fixture.0.join("ytmusic.json").exists());
    }

    #[test]
    fn cookies_and_seed_overrides_preserve_profiles_and_clean_private_copies() {
        let fixture = Fixture::new();
        let cookies = fixture.0.join("original-cookies.txt");
        let data = b".youtube.com\tTRUE\t/\tTRUE\t0\tSID\tfixture-secret\n";
        crate::config::save_private(&cookies, data).unwrap();
        let profile = json!({"seed":"abcdefghijk","cookies":cookies});
        crate::config::save_private(
            &fixture.0.join("ytmusic.json"),
            &serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
        fixture.reply(1, json!({"entries":[{"id":"9EcDe6qS0Xc"}]}));
        let mut youtube = fixture.backend();
        youtube.start().unwrap();
        let calls = fixture.calls();
        assert!(calls[0].last().unwrap().contains("v=5LGUgChapj4"));
        let temporary = youtube.session.as_ref().unwrap().cookies.clone().unwrap();
        assert_ne!(temporary, cookies);
        assert!(plavno_platform::private_file(&temporary).unwrap());
        assert!(calls[0].contains(&temporary.to_string_lossy().into_owned()));
        assert!(!calls[0].iter().any(|arg| arg.contains("fixture-secret")));
        fs::write(&temporary, b"changed cookie jar").unwrap();
        youtube.suspend();
        assert!(!temporary.exists());
        assert_eq!(fs::read(cookies).unwrap(), data);
        assert_eq!(
            web::profile(&fixture.0.join("ytmusic.json")).unwrap(),
            profile
        );
    }

    #[test]
    fn rate_limits_survive_restarts_without_another_process_or_metadata_request() {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("1.err"),
            "HTTP Error 429: Too Many Requests\nRetry-After: 120\nhttps://cdn.test/?sig=private",
        )
        .unwrap();
        fs::write(fixture.0.join("1.status"), "1").unwrap();
        let mut youtube = fixture.backend();
        assert!(
            youtube
                .start()
                .unwrap_err()
                .downcast_ref::<RateLimited>()
                .unwrap()
                .active()
        );
        let mut restarted = fixture.backend();
        let error = restarted.start().unwrap_err();
        assert!(error.downcast_ref::<RateLimited>().unwrap().active());
        assert!(!error.to_string().contains("private"));
        assert_eq!(fixture.calls().len(), 1);
        let path = fixture.0.join("ytmusic-backoff.json");
        assert!(plavno_platform::private_file(&path).unwrap());
        let mut state = web::profile(&path).unwrap();
        state["until"] = json!(0);
        crate::config::save_private(&path, &serde_json::to_vec(&state).unwrap()).unwrap();
        fixture.reply(2, json!({"entries":[]}));
        restarted.start().unwrap();
        assert_eq!(fixture.calls().len(), 2);
        state["until"] = json!(u64::MAX);
        state["account"] = json!("another-account");
        crate::config::save_private(&path, &serde_json::to_vec(&state).unwrap()).unwrap();
        fixture.reply(3, json!({"entries":[]}));
        fixture.backend().start().unwrap();
        assert_eq!(fixture.calls().len(), 3);
    }

    #[test]
    fn audio_resolution_enables_node_without_an_extra_metadata_request() {
        let fixture = Fixture::new();
        fixture.reply(
            1,
            json!({"url":"https://cdn.test/audio","protocol":"https","ext":"m4a"}),
        );
        let mut youtube = fixture.backend();
        youtube
            .resolve(&Track {
                source: "ytmusic".into(),
                id: "WPK0ryGJaDQ".into(),
                title: "Track".into(),
                artist: "Artist".into(),
                duration: None,
                cover_url: None,
                genre: None,
                batch: String::new(),
            })
            .unwrap();
        let calls = fixture.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .windows(2)
                .any(|args| args == ["--js-runtimes", "node"])
        );
    }

    #[test]
    fn older_ytdlp_rejects_the_optional_runtime_flag_only_once() {
        let fixture = Fixture::new();
        let cookies = fixture.0.join("cookies.txt");
        crate::config::save_private(
            &cookies,
            b".youtube.com\tTRUE\t/\tTRUE\t0\tSID\tfixture-secret\n",
        )
        .unwrap();
        crate::config::save_private(
            &fixture.0.join("ytmusic.json"),
            &serde_json::to_vec(&json!({"cookies":cookies})).unwrap(),
        )
        .unwrap();
        fs::write(
            fixture.0.join("1.err"),
            "Usage: yt-dlp [OPTIONS] URL\n\nyt-dlp: error: no such option: --js-runtimes\n",
        )
        .unwrap();
        fs::write(fixture.0.join("1.status"), "2").unwrap();
        fixture.reply(2, json!({"entries":[{"id":"9EcDe6qS0Xc"}]}));
        fixture.reply(3, json!({"entries":[{"id":"abcdefghijk"}]}));
        let mut youtube = fixture.backend();
        assert_eq!(youtube.start().unwrap().len(), 1);
        assert_eq!(youtube.more(&[]).unwrap().len(), 1);
        let calls = fixture.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains(&"--js-runtimes".into()));
        for call in &calls[1..] {
            assert!(!call.contains(&"--js-runtimes".into()));
        }
        let without_runtime: Vec<_> = calls[0]
            .iter()
            .filter(|arg| !["--js-runtimes", "node"].contains(&arg.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            calls[1], without_runtime,
            "Local option fallback must preserve the request and private cookie arguments"
        );
    }

    #[test]
    fn challenge_failures_have_actionable_errors_without_repeating_metadata_requests() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("1.err"),"WARNING: Signature solving failed\nWARNING: n challenge solving failed [private URL]\nERROR: Requested format is not available\n").unwrap();
        fs::write(fixture.0.join("1.status"), "1").unwrap();
        let mut youtube = fixture.backend();
        let error = youtube.start().unwrap_err().to_string();
        assert!(error.contains("Node.js 22+") && error.contains("yt-dlp-ejs"));
        assert!(!error.contains("private URL"));
        assert_eq!(fixture.calls().len(), 1);
    }

    #[test]
    fn missing_dependency_and_unsafe_output_have_private_actionable_errors() {
        let fixture = Fixture::new();
        let mut youtube = fixture.backend();
        youtube.program = fixture.0.join("missing-yt-dlp").into_os_string();
        assert!(
            youtube
                .start()
                .unwrap_err()
                .to_string()
                .contains("sudo apt install yt-dlp")
        );
        assert!(seed_id("https://evil.test/watch?v=5LGUgChapj4").is_err());
        assert!(seed_id("--exec-command").is_err());
        assert_eq!(
            seed_id("https://youtu.be/5LGUgChapj4").unwrap(),
            "5LGUgChapj4"
        );
        assert!(stream(&json!({"is_live":true})).is_err());
        assert!(stream(&json!({"url":"file:///secret"})).is_err());
        assert!(failure("HTTP Error 403 https://cdn.test/?sig=private").contains("403"));
        assert!(!failure("https://cdn.test/?sig=private").contains("private"));
        assert!(
            failure("RDMM: YouTube said: This playlist type is unviewable.")
                .contains("starting video")
        );
        assert!(!http_status("[youtube] videoID429: loading metadata", 429));
        let Input::Http { headers, hint, .. } = stream(&json!({"url":"https://cdn.test/audio","protocol":"https","ext":"m4a","http_headers":{"Referer":"https://www.youtube.com/"}})).unwrap() else { panic!("AAC should use the native decoder") };
        assert_eq!(hint, "m4a");
        assert_eq!(
            headers,
            [("Referer".into(), "https://www.youtube.com/".into())]
        );
    }
}
