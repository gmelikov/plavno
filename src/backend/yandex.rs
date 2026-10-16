use super::yandex_cache::{Cache, now};
use super::{Backend, Feedback, Input, PlaybackMode, Track, WaveStyle};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{BufReader, Read},
    time::{Duration, Instant},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub struct Yandex {
    http: ureq::Agent,
    token: String,
    base: String,
    session: Option<String>,
    uid: Option<String>,
    favorites: Option<(HashSet<String>, Instant)>,
    shuffle: Option<FavoritesQueue>,
    style: WaveStyle,
    streams: Cache,
}

#[derive(Default)]
struct FavoritesQueue {
    remaining: VecDeque<String>,
    last: Option<String>,
    fresh_cycle: bool,
}

const FAVORITES_BATCH: usize = 32;

pub fn http() -> ureq::Agent {
    http_agent(true)
}

fn http_agent(status_errors: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(status_errors)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .build()
        .into()
}

pub fn network_error(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(401 | 403) => {
            anyhow!("Access denied: check your token and Yandex Plus subscription")
        }
        ureq::Error::StatusCode(code) => anyhow!("The server returned HTTP {code}"),
        ureq::Error::Timeout(stage) => anyhow!("Request timed out during {stage}"),
        ureq::Error::HostNotFound => anyhow!("DNS lookup failed"),
        ureq::Error::Io(error) => match error.raw_os_error() {
            Some(code) => anyhow!(
                "Network I/O: {} (OS {code})",
                std::io::Error::from_raw_os_error(code)
            ),
            None => anyhow!("Network I/O: {:?}", error.kind()),
        },
        ureq::Error::Rustls(error) => anyhow!("TLS: {error}"),
        ureq::Error::Tls(_) | ureq::Error::TlsRequired => anyhow!("TLS connection failed"),
        ureq::Error::InvalidProxyUrl => anyhow!("Invalid proxy configuration"),
        ureq::Error::ConnectProxyFailed(_) => anyhow!("Proxy connection failed"),
        ureq::Error::ConnectionFailed => anyhow!("Connection failed"),
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => {
            anyhow!("HTTP redirect failed")
        }
        ureq::Error::BadUri(_) | ureq::Error::RequireHttpsOnly(_) => anyhow!("Invalid request URL"),
        ureq::Error::Protocol(_) | ureq::Error::BodyStalled => anyhow!("Invalid HTTP response"),
        ureq::Error::Json(error) => anyhow!(
            "Invalid JSON response at line {}, column {}",
            error.line(),
            error.column()
        ),
        ureq::Error::BodyExceedsLimit(_) => anyhow!("HTTP body exceeded the size limit"),
        _ => anyhow!("HTTP request failed"),
    }
}

pub fn network_error_at(url: &str, error: ureq::Error) -> anyhow::Error {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown host".into());
    anyhow!("{host}: {}", network_error(error))
}

impl Yandex {
    pub fn new(token: String) -> Self {
        Self {
            http: http_agent(false),
            token,
            base: "https://api.music.yandex.net".into(),
            session: None,
            uid: None,
            favorites: None,
            shuffle: None,
            style: WaveStyle::default(),
            streams: Cache::default(),
        }
    }

    pub fn with_cache(token: String, directory: std::path::PathBuf) -> Self {
        let streams = Cache::persistent(directory, &token);
        Self {
            streams,
            ..Self::new(token)
        }
    }

    fn api_response(
        &mut self,
        path: &str,
        response: ureq::http::Response<ureq::Body>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        self.check_rate_limit(&response, false)?;
        let status = response.status().as_u16();
        if status >= 400 {
            return Err(self.api_error(path, ureq::Error::StatusCode(status)));
        }
        Ok(response)
    }

    fn check_rate_limit(
        &mut self,
        response: &ureq::http::Response<ureq::Body>,
        cdn: bool,
    ) -> Result<()> {
        if response.status().as_u16() == 429 {
            let retry = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok());
            return Err(if cdn {
                self.streams.throttled_cdn(retry, now())
            } else {
                self.streams.throttled(retry, now())
            }
            .into());
        }
        Ok(())
    }

    fn api_error(&self, path: &str, error: ureq::Error) -> anyhow::Error {
        let mut parts: Vec<_> = path.split('?').next().unwrap_or(path).split('/').collect();
        if parts.get(1) == Some(&"users") && parts.len() > 2 {
            parts[2] = "{user}";
        } else if parts.get(1..3) == Some(&["rotor", "session"][..])
            && parts.len() > 3
            && parts[3] != "new"
        {
            parts[3] = "{session}";
        }
        anyhow!(
            "Yandex {}: {}",
            parts.join("/"),
            network_error_at(&self.base, error)
        )
    }

    pub fn diagnose(&mut self) -> Value {
        let mut checks = Vec::new();
        let mut stage = "account";
        let mut started = Instant::now();
        let result = (|| -> Result<()> {
            self.ensure_uid()?;
            checks.push(
                json!({"stage": stage, "ok": true, "elapsed_ms": started.elapsed().as_millis()}),
            );
            stage = "favorites";
            started = Instant::now();
            self.refresh_favorites(true)?;
            checks.push(
                json!({"stage": stage, "ok": true, "elapsed_ms": started.elapsed().as_millis()}),
            );
            let Some(id) = self.favorites.as_ref().unwrap().0.iter().min().cloned() else {
                checks.push(json!({"stage": "audio", "skipped": "No favorite tracks to probe"}));
                return Ok(());
            };
            stage = "track_metadata";
            started = Instant::now();
            let value = self.post_form("/tracks", [("track-ids", id.as_str())])?;
            let track =
                parse_track(&value[0], "")?.context("The selected favorite is unavailable")?;
            checks.push(
                json!({"stage": stage, "ok": true, "elapsed_ms": started.elapsed().as_millis()}),
            );
            stage = "audio_resolution";
            started = Instant::now();
            let Input::Http { url, quality, .. } = self.resolve(&track)? else {
                bail!("Unexpected audio transport");
            };
            checks.push(json!({"stage": stage, "ok": true, "quality": quality, "elapsed_ms": started.elapsed().as_millis()}));
            stage = "audio_cdn";
            started = Instant::now();
            let host = url::Url::parse(&url)?
                .host_str()
                .unwrap_or_default()
                .to_owned();
            let mut response = self
                .http
                .get(&url)
                .header("Range", "bytes=0-1023")
                .call()
                .map_err(|error| network_error_at(&url, error))?;
            let status = response.status().as_u16();
            self.check_rate_limit(&response, true)?;
            ensure!(
                status == 206,
                "Audio CDN {host}: HTTP {status}; expected a partial response"
            );
            let bytes = response
                .body_mut()
                .with_config()
                .limit(1025)
                .read_to_vec()
                .map_err(|error| network_error_at(&url, error))?;
            ensure!(
                !bytes.is_empty() && bytes.len() <= 1024,
                "Audio CDN {host}: invalid response size"
            );
            checks.push(
                json!({"stage": stage, "host": host, "ok": true, "http_status": status,
                "bytes_read": bytes.len(), "elapsed_ms": started.elapsed().as_millis()}),
            );
            Ok(())
        })();
        if let Err(error) = result {
            checks.push(json!({"stage": stage, "ok": false, "error": format!("{error:#}"), "elapsed_ms": started.elapsed().as_millis()}));
        }
        json!({"source": "yandex", "proxy_configured": self.http.config().proxy().is_some(), "checks": checks})
    }

    fn request(&mut self, path: &str, body: Option<Value>) -> Result<Value> {
        self.streams.guard(now())?;
        let url = format!("{}{path}", self.base);
        let auth = format!("OAuth {}", self.token);
        let response = match body {
            Some(body) => self
                .http
                .post(&url)
                .header("Authorization", &auth)
                .send_json(body),
            None => self.http.get(&url).header("Authorization", &auth).call(),
        };
        self.decode_result(path, response)
    }

    fn post_form<'a>(
        &mut self,
        path: &str,
        fields: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Value> {
        self.streams.guard(now())?;
        let response = self
            .http
            .post(&format!("{}{path}", self.base))
            .header("Authorization", format!("OAuth {}", self.token))
            .send_form(fields);
        self.decode_result(path, response)
    }

    fn decode_result(
        &mut self,
        path: &str,
        response: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Value> {
        let response = response.map_err(|error| self.api_error(path, error))?;
        let mut response = self.api_response(path, response)?;
        let mut value: Value = response
            .body_mut()
            .read_json()
            .map_err(|error| self.api_error(path, error))?;
        ensure!(value.get("error").is_none(), "The API reported an error");
        value
            .get_mut("result")
            .map(Value::take)
            .context("The API response is missing result")
    }

    fn send_event(&mut self, batch: &str, mut event: Value) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .context("My Vibe session has not started")?;
        event["timestamp"] = OffsetDateTime::now_utc().format(&Rfc3339)?.into();
        self.request(
            &format!("/rotor/session/{session}/feedback"),
            Some(json!({"event": event, "batchId": batch, "from": "plavno"})),
        )?;
        Ok(())
    }

    fn ensure_uid(&mut self) -> Result<()> {
        if self.uid.is_none() {
            let account = self.request("/account/status", None)?;
            let uid = match &account["account"]["uid"] {
                Value::String(uid) => uid.clone(),
                Value::Number(uid) => uid.to_string(),
                _ => bail!("The API did not return an account ID"),
            };
            ensure!(
                !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()),
                "Invalid account ID"
            );
            self.uid = Some(uid);
        }
        Ok(())
    }

    fn refresh_favorites(&mut self, force: bool) -> Result<()> {
        if !force
            && self
                .favorites
                .as_ref()
                .is_some_and(|(_, time)| time.elapsed() < Duration::from_secs(60))
        {
            return Ok(());
        }
        self.ensure_uid()?;
        let value = self.request(
            &format!(
                "/users/{}/likes/tracks?if-modified-since-revision=0",
                self.uid.as_ref().unwrap()
            ),
            None,
        )?;
        let tracks = value["library"]["tracks"]
            .as_array()
            .context("The API did not return the favorites list")?;
        let ids: Result<HashSet<String>> = tracks
            .iter()
            .map(|track| {
                let id = match &track["id"] {
                    Value::String(id) => id.clone(),
                    Value::Number(id) => id.to_string(),
                    _ => bail!("The API returned a favorite without a track ID"),
                };
                ensure!(
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
                    "Invalid favorite track ID"
                );
                Ok(id)
            })
            .collect();
        self.favorites = Some((ids?, Instant::now()));
        Ok(())
    }

    fn refill_favorites(&mut self) -> Result<()> {
        self.refresh_favorites(true)?;
        let mut ids: Vec<_> = self.favorites.as_ref().unwrap().0.iter().cloned().collect();
        ensure!(!ids.is_empty(), "No favorite tracks yet; like tracks first");
        let mut random = BufReader::new(
            plavno_platform::Random::new().context("Failed to open the random source")?,
        );
        let queue = self.shuffle.as_mut().unwrap();
        shuffle_favorites(&mut ids, queue.last.as_deref(), &mut random)?;
        queue.remaining = ids.into();
        queue.fresh_cycle = true;
        Ok(())
    }

    fn favorites_batch(&mut self) -> Result<Vec<Track>> {
        if self.shuffle.as_ref().unwrap().remaining.is_empty() {
            self.refill_favorites()?;
        }
        loop {
            let ids: Vec<_> = self
                .shuffle
                .as_ref()
                .unwrap()
                .remaining
                .iter()
                .take(FAVORITES_BATCH)
                .cloned()
                .collect();
            let mut fields: Vec<_> = ids.iter().map(|id| ("track-ids", id.as_str())).collect();
            fields.push(("with-positions", "True"));
            let value = self.post_form("/tracks", fields)?;
            let values = value
                .as_array()
                .context("The API did not return track metadata")?;
            let mut tracks = HashMap::new();
            for value in values {
                if let Some(track) = parse_track(value, "")? {
                    tracks.insert(track.id.clone(), track);
                }
            }
            let mut result: Vec<_> = ids.iter().filter_map(|id| tracks.remove(id)).collect();
            let queue = self.shuffle.as_mut().unwrap();
            queue.remaining.drain(..ids.len());
            if queue.fresh_cycle
                && result.first().map(|t| &t.id) == queue.last.as_ref()
                && !result.is_empty()
            {
                if result.len() > 1 {
                    result.swap(0, 1);
                } else if !queue.remaining.is_empty() {
                    // Try the rest of this pass before repeating the previous track.
                    queue.remaining.push_back(result.pop().unwrap().id);
                    continue;
                }
            }
            if let Some(last) = result.last() {
                queue.last = Some(last.id.clone());
                queue.fresh_cycle = false;
                return Ok(result);
            }
            ensure!(
                !queue.remaining.is_empty(),
                "No playable favorite tracks are available"
            );
        }
    }
}

impl Backend for Yandex {
    fn key(&self) -> &'static str {
        "yandex"
    }
    fn name(&self) -> &'static str {
        "Yandex Music"
    }

    fn start(&mut self) -> Result<Vec<Track>> {
        self.shuffle = None;
        let mut seeds = vec![if self.style == WaveStyle::Work {
            "genre:ambientgenre"
        } else {
            "user:onyourwave"
        }];
        match self.style {
            WaveStyle::Calm => seeds.push("settingMoodEnergy:calm"),
            WaveStyle::Energetic => seeds.push("settingMoodEnergy:active"),
            WaveStyle::Work => {
                seeds.extend(["settingMoodEnergy:calm", "settingLanguage:without-words"])
            }
            WaveStyle::Balanced => {}
        }
        let response = self.request(
            "/rotor/session/new",
            Some(json!({
                "seeds": seeds,
                "includeTracksInResponse": true,
                "includeWaveModel": false,
                "interactive": true
            })),
        )?;
        let id = response["radioSessionId"]
            .as_str()
            .context("The response is missing the My Vibe session")?;
        ensure!(
            id.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "Invalid session ID"
        );
        self.session = Some(id.into());
        let tracks = parse_batch(&response)?;
        ensure!(!tracks.is_empty(), "My Vibe returned an empty queue");
        self.send_event(&tracks[0].batch, json!({"type": "radioStarted"}))?;
        Ok(tracks)
    }

    fn supports_wave_styles(&self) -> bool {
        true
    }

    fn set_wave_style(&mut self, style: WaveStyle) -> Result<()> {
        self.style = style;
        Ok(())
    }

    fn start_mode(&mut self, mode: PlaybackMode) -> Result<Vec<Track>> {
        match mode {
            PlaybackMode::MyVibe => self.start(),
            PlaybackMode::FavoritesShuffle => {
                self.session = None;
                self.shuffle = Some(FavoritesQueue::default());
                self.favorites_batch()
            }
        }
    }

    fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
        if self.shuffle.is_some() {
            return self.favorites_batch();
        }
        let session = self
            .session
            .as_ref()
            .context("My Vibe session has not started")?;
        let value = self.request(
            &format!("/rotor/session/{session}/tracks"),
            Some(json!({"queue": history})),
        )?;
        ensure!(
            value["unknownSession"] != true,
            "My Vibe session expired; press w"
        );
        parse_batch(&value)
    }

    fn resolve(&mut self, track: &Track) -> Result<Input> {
        if let Some(info) = self.streams.get(&track.id, now()) {
            if let Ok(input) = info.clone().into_input() {
                self.streams.guard_cdn(now())?;
                let mut response = self
                    .http
                    .get(&info.url)
                    .header("Range", "bytes=0-0")
                    .call()
                    .map_err(|error| network_error_at(&info.url, error))?;
                let status = response.status().as_u16();
                self.check_rate_limit(&response, true)?;
                match status {
                    200 | 206 => {
                        response
                            .body_mut()
                            .as_reader()
                            .read_exact(&mut [0u8; 1])
                            .map_err(|error| network_error_at(&info.url, error.into()))?;
                        return Ok(input);
                    }
                    401 | 403 | 404 | 410 => {}
                    _ => return Err(network_error_at(&info.url, ureq::Error::StatusCode(status))),
                }
            }
            self.streams.remove(&track.id);
        }
        self.streams.guard(now())?;
        let timestamp = OffsetDateTime::now_utc().unix_timestamp();
        let sign = file_info_sign(&track.id, timestamp);
        let response = self
            .http
            .get(&format!("{}/get-file-info", self.base))
            .header("Authorization", &format!("OAuth {}", self.token))
            .header(
                "X-Yandex-Music-Client",
                "YandexMusicDesktopAppWindows/5.95.0",
            )
            .query("ts", timestamp.to_string())
            .query("trackId", &track.id)
            .query("quality", FILE_QUALITY)
            .query("codecs", FILE_CODECS)
            .query("transports", FILE_TRANSPORTS)
            .query("sign", sign)
            .call();
        let value = self.decode_result("/get-file-info", response)?;
        let info: FileDownload = serde_json::from_value(value["downloadInfo"].clone())
            .context("The API did not return audio stream information")?;
        let input = info.clone().into_input()?;
        self.streams.insert(&track.id, info, now());
        Ok(input)
    }

    fn feedback(&mut self, track: &Track, kind: Feedback, seconds: f64) -> Result<()> {
        if track.batch.is_empty() {
            return Ok(());
        }
        let mut event = json!({"trackId": track.id});
        event["type"] = match kind {
            Feedback::Started => "trackStarted",
            Feedback::Finished => "trackFinished",
            Feedback::Skipped => "skip",
        }
        .into();
        if !matches!(kind, Feedback::Started) {
            event["totalPlayedSeconds"] = seconds.into();
        }
        self.send_event(&track.batch, event)
    }

    fn supports_favorites(&self) -> bool {
        true
    }
    fn set_dislike(&mut self, track: &Track, disliked: bool) -> Result<bool> {
        self.ensure_uid()?;
        let action = if disliked { "add-multiple" } else { "remove" };
        let value = self.post_form(
            &format!(
                "/users/{}/dislikes/tracks/{action}",
                self.uid.as_ref().unwrap()
            ),
            [("track-ids", track.id.as_str())],
        )?;
        ensure!(
            value["revision"].as_u64().is_some(),
            "The API did not confirm the dislike change"
        );
        Ok(true)
    }
    fn supports_favorites_shuffle(&self) -> bool {
        true
    }

    fn favorite_state(&mut self, track: &Track) -> Result<Option<bool>> {
        self.refresh_favorites(false)?;
        Ok(Some(self.favorites.as_ref().unwrap().0.contains(&track.id)))
    }

    fn toggle_favorite(&mut self, track: &Track) -> Result<bool> {
        self.refresh_favorites(true)?;
        let liked = !self.favorites.as_ref().unwrap().0.contains(&track.id);
        let action = if liked { "add-multiple" } else { "remove" };
        let result = (|| -> Result<()> {
            let value = self.post_form(
                &format!(
                    "/users/{}/likes/tracks/{action}",
                    self.uid.as_ref().unwrap()
                ),
                [("track-ids", track.id.as_str())],
            )?;
            ensure!(
                value["revision"].as_u64().is_some(),
                "The API did not confirm the favorites change"
            );
            Ok(())
        })();
        if let Err(error) = result {
            self.favorites = None;
            return Err(error);
        }
        let favorites = &mut self.favorites.as_mut().unwrap().0;
        if liked {
            favorites.insert(track.id.clone());
        } else {
            favorites.remove(&track.id);
            if let Some(queue) = &mut self.shuffle {
                queue.remaining.retain(|id| id != &track.id);
            }
        }
        Ok(liked)
    }
}

// Only advertise formats supported by the decoder. The server selects the best available quality.
pub(super) const FILE_QUALITY: &str = "lossless";
pub(super) const FILE_TRANSPORTS: &str = "encraw";
pub(super) const FILE_CODECS: &str = "flac,flac-mp4,aac-mp4,mp3";

fn file_info_sign(track_id: &str, timestamp: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"kzqU4XhfCaY6B6JTHODeq5").unwrap();
    mac.update(
        format!(
            "{timestamp}{track_id}lossless{}encraw",
            FILE_CODECS.replace(',', "")
        )
        .as_bytes(),
    );
    STANDARD_NO_PAD.encode(mac.finalize().into_bytes())
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct FileDownload {
    codec: String,
    quality: String,
    bitrate: u32,
    transport: String,
    url: String,
    key: Option<String>,
}

impl FileDownload {
    fn into_input(self) -> Result<Input> {
        let hint = match self.codec.as_str() {
            "flac" => "flac",
            "flac-mp4" | "aac-mp4" => "m4a",
            "mp3" => "mp3",
            _ => bail!("The API returned an unsupported codec"),
        };
        ensure!(
            self.url.starts_with("https://") || self.url.starts_with("http://"),
            "Invalid audio URL"
        );
        let aes128_key = match self.transport.as_str() {
            "raw" => None,
            "encraw" => {
                let key = self
                    .key
                    .context("The API did not return the audio stream key")?;
                ensure!(
                    key.len() == 32 && key.is_ascii(),
                    "Invalid audio stream key"
                );
                let mut bytes = [0u8; 16];
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&key[i * 2..i * 2 + 2], 16)
                        .context("Invalid audio stream key")?;
                }
                Some(bytes)
            }
            _ => bail!("The API returned an unsupported transport"),
        };
        let codec = if self.codec.starts_with("flac") {
            "FLAC · lossless"
        } else if self.codec == "mp3" {
            "MP3"
        } else {
            "AAC"
        };
        let quality = if self.bitrate > 0 {
            format!("{codec} · {} kbps", self.bitrate)
        } else {
            codec.into()
        };
        ensure!(
            self.quality != "lossless" || self.codec.starts_with("flac"),
            "The API returned inconsistent audio quality"
        );
        Ok(Input::Http {
            url: self.url,
            aes128_key,
            hint,
            quality,
        })
    }
}

fn parse_batch(value: &Value) -> Result<Vec<Track>> {
    let batch = value["batchId"]
        .as_str()
        .context("The response is missing batchId")?;
    let sequence = value["sequence"]
        .as_array()
        .context("The response is missing the track queue")?;
    sequence
        .iter()
        .filter_map(|item| parse_track(&item["track"], batch).transpose())
        .collect()
}

fn parse_track(track: &Value, batch: &str) -> Result<Option<Track>> {
    if track["available"] == false || track.is_null() {
        return Ok(None);
    }
    let id = match &track["id"] {
        Value::String(id) => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => bail!("The track is missing an ID"),
    };
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
        "Invalid track ID"
    );
    Ok(Some(Track {
        source: String::new(),
        id,
        title: track["title"].as_str().unwrap_or("Untitled").into(),
        artist: track["artists"]
            .as_array()
            .map(|artists| {
                artists
                    .iter()
                    .filter_map(|a| a["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        duration: track["durationMs"].as_f64().map(|ms| ms / 1000.0),
        cover_url: cover_url(track),
        genre: track["albums"]
            .as_array()
            .and_then(|albums| albums.iter().find_map(|album| album["genre"].as_str()))
            .map(str::to_owned),
        batch: batch.into(),
    }))
}

fn random_index(upper: usize, random: &mut impl Read) -> Result<usize> {
    let bound = upper as u64;
    let limit = u64::MAX - u64::MAX % bound;
    loop {
        let mut bytes = [0; 8];
        random
            .read_exact(&mut bytes)
            .context("Failed to read randomness")?;
        let value = u64::from_le_bytes(bytes);
        if value < limit {
            return Ok((value % bound) as usize);
        }
    }
}

pub(super) fn shuffle_favorites(
    ids: &mut [String],
    previous: Option<&str>,
    random: &mut impl Read,
) -> Result<()> {
    for index in (1..ids.len()).rev() {
        let other = random_index(index + 1, random)?;
        ids.swap(index, other);
    }
    if ids.len() > 1 && ids.first().map(String::as_str) == previous {
        let other = 1 + random_index(ids.len() - 1, random)?;
        ids.swap(0, other);
    }
    Ok(())
}

fn cover_url(track: &Value) -> Option<String> {
    fn nonempty(value: &Value) -> Option<&str> {
        value.as_str().map(str::trim).filter(|s| !s.is_empty())
    }
    let uri = nonempty(&track["coverUri"]).or_else(|| {
        track["albums"]
            .as_array()?
            .iter()
            .find_map(|album| nonempty(&album["coverUri"]))
    })?;
    let uri = uri.replace("%%", "400x400");
    Some(
        if uri.starts_with("https://") || uri.starts_with("http://") {
            uri
        } else {
            format!("https://{}", uri.trim_start_matches("//"))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_server(
        replies: Vec<(&'static str, u16)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::{
            io::{BufRead, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let audio_url = format!("{base}/audio?sign=private-fixture");
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (path, status) in replies {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(2))
                        }
                        Err(error) => panic!("Missing request: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(
                    request.starts_with(&format!("GET {path}")),
                    "Unexpected request endpoint"
                );
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.is_empty());
                    headers.push_str(&line.to_ascii_lowercase());
                }
                if path == "/audio" {
                    assert!(headers.contains("range: bytes=0-0"));
                    assert!(!headers.contains("authorization:"));
                } else {
                    assert!(headers.contains("authorization: oauth test-token"));
                    assert!(request.contains("quality=lossless"));
                }
                requests.push(request);
                let body = if path == "/get-file-info?" && status == 200 {
                    json!({"result":{"downloadInfo":{"codec":"flac-mp4","quality":"lossless","bitrate":0,
                        "transport":"encraw","url":audio_url,"key":"000102030405060708090a0b0c0d0e0f"}}}).to_string()
                } else {
                    "x".into()
                };
                write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nRetry-After: 120\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (base, server)
    }

    fn stream_track() -> Track {
        parse_track(&json!({"id":12,"title":"Fixture"}), "batch")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn album_genre_survives_history_and_old_records_remain_readable() {
        let track = parse_track(
            &json!({"id":12,"title":"Fixture",
            "albums":[{"genre":"ambientgenre"}]}),
            "batch",
        )
        .unwrap()
        .unwrap();
        assert_eq!(track.genre.as_deref(), Some("ambientgenre"));
        let mut saved = serde_json::to_value(track).unwrap();
        let restored: Track = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(restored.genre.as_deref(), Some("ambientgenre"));
        saved.as_object_mut().unwrap().remove("genre");
        let legacy: Track = serde_json::from_value(saved).unwrap();
        assert_eq!(legacy.genre, None);
    }

    #[test]
    fn signed_stream_cache_reuses_metadata_across_restarts_and_refreshes_an_expired_url_once() {
        let directory =
            std::env::temp_dir().join(format!("plavno-stream-reuse-{}", std::process::id()));
        let (base, server) = stream_server(vec![
            ("/get-file-info?", 200),
            ("/audio", 206),
            ("/audio", 403),
            ("/get-file-info?", 200),
        ]);
        let mut backend = Yandex::with_cache("test-token".into(), directory.clone());
        backend.base = base.clone();
        let track = stream_track();
        assert!(matches!(
            backend.resolve(&track).unwrap(),
            Input::Http {
                aes128_key: Some(_),
                ..
            }
        ));
        drop(backend);
        let mut restarted = Yandex::with_cache("test-token".into(), directory.clone());
        restarted.base = base;
        let mut new_batch = track.clone();
        new_batch.batch = "new-session".into();
        assert!(restarted.resolve(&new_batch).is_ok());
        assert!(restarted.resolve(&new_batch).is_ok());
        let requests = server.join().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.starts_with("GET /get-file-info?"))
                .count(),
            2
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn expired_metadata_uses_a_fresh_resolution_without_probing_the_old_url() {
        let (base, server) =
            stream_server(vec![("/get-file-info?", 200), ("/get-file-info?", 200)]);
        let mut backend = Yandex::new("test-token".into());
        backend.base = base;
        let track = stream_track();
        backend.resolve(&track).unwrap();
        let info = backend.streams.get(&track.id, now()).unwrap();
        backend.streams.insert(&track.id, info, now() - 301);
        backend.resolve(&track).unwrap();
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn http_429_blocks_api_calls_across_restarts_and_resumes_after_the_deadline() {
        let directory =
            std::env::temp_dir().join(format!("plavno-stream-quota-{}", std::process::id()));
        let (base, server) =
            stream_server(vec![("/get-file-info?", 429), ("/get-file-info?", 200)]);
        let mut backend = Yandex::with_cache("test-token".into(), directory.clone());
        backend.base = base.clone();
        let track = stream_track();
        let error = backend.resolve(&track).err().unwrap();
        assert!(
            error
                .downcast_ref::<super::super::RateLimited>()
                .unwrap()
                .active()
        );
        assert!(
            backend
                .resolve(&track)
                .err()
                .unwrap()
                .downcast_ref::<super::super::RateLimited>()
                .is_some()
        );
        assert!(
            backend
                .request("/account/status", None)
                .err()
                .unwrap()
                .downcast_ref::<super::super::RateLimited>()
                .is_some()
        );
        drop(backend);
        let mut restarted = Yandex::with_cache("test-token".into(), directory.clone());
        restarted.base = base.clone();
        assert!(
            restarted
                .resolve(&track)
                .err()
                .unwrap()
                .downcast_ref::<super::super::RateLimited>()
                .is_some()
        );
        // Advance the persisted deadline without sleeping on the wall clock.
        let path = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .unwrap();
        let mut state: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        state["blocked_until"] = json!(now() - 1);
        crate::config::save_private(&path, &serde_json::to_vec(&state).unwrap()).unwrap();
        restarted = Yandex::with_cache("test-token".into(), directory.clone());
        restarted.base = base;
        assert!(restarted.resolve(&track).is_ok());
        assert_eq!(server.join().unwrap().len(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn api_cooldown_allows_cached_cdn_streams_but_blocks_expired_links_and_form_requests() {
        let directory =
            std::env::temp_dir().join(format!("plavno-cached-cooldown-{}", std::process::id()));
        let (base, server) = stream_server(vec![
            ("/get-file-info?", 200),
            ("/audio", 206),
            ("/audio", 403),
        ]);
        let track = stream_track();
        let mut backend = Yandex::with_cache("test-token".into(), directory.clone());
        backend.base = base.clone();
        backend.resolve(&track).unwrap();
        backend.streams.throttled(Some("120"), now());
        drop(backend);
        let mut restarted = Yandex::with_cache("test-token".into(), directory.clone());
        restarted.base = base;
        assert!(restarted.resolve(&track).is_ok());
        assert!(
            restarted
                .post_form("/tracks", [("track-ids", "12")])
                .unwrap_err()
                .is::<super::super::RateLimited>()
        );
        assert!(
            restarted
                .resolve(&track)
                .err()
                .unwrap()
                .is::<super::super::RateLimited>()
        );
        assert_eq!(server.join().unwrap().len(), 3);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cdn_cooldown_does_not_repeat_the_probe_or_resolve_a_new_link() {
        let directory =
            std::env::temp_dir().join(format!("plavno-cdn-cooldown-{}", std::process::id()));
        let (base, server) = stream_server(vec![("/get-file-info?", 200), ("/audio", 429)]);
        let mut backend = Yandex::with_cache("test-token".into(), directory.clone());
        backend.base = base.clone();
        let track = stream_track();
        backend.resolve(&track).unwrap();
        // This instance already holds the signed URL when the first one is throttled.
        let mut existing = Yandex::with_cache("test-token".into(), directory.clone());
        existing.base = base;
        assert!(
            backend
                .resolve(&track)
                .err()
                .unwrap()
                .is::<super::super::RateLimited>()
        );
        for instance in [&mut backend, &mut existing] {
            assert!(
                instance
                    .resolve(&track)
                    .err()
                    .unwrap()
                    .is::<super::super::RateLimited>()
            );
        }
        assert_eq!(server.join().unwrap().len(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transport_errors_identify_the_cause_without_exposing_secrets() {
        let url = "https://user:secret@example.test/signed-secret?token=secret";
        for error in [
            ureq::Error::BadUri(url.into()),
            ureq::Error::ConnectProxyFailed(url.into()),
            ureq::Error::Io(std::io::Error::other(url.to_owned())),
        ] {
            let message = network_error_at(url, error).to_string();
            assert!(message.contains("example.test"));
            assert!(!message.contains("secret"));
        }
        assert!(
            network_error(ureq::Error::HostNotFound)
                .to_string()
                .contains("DNS")
        );
        assert!(
            network_error(ureq::Error::Timeout(ureq::Timeout::RecvResponse))
                .to_string()
                .contains("receive response")
        );
        let backend = Yandex::new("secret".into());
        let error = backend.api_error(
            "/rotor/session/secret/tracks?token=secret",
            ureq::Error::HostNotFound,
        );
        assert!(
            error
                .to_string()
                .contains("/rotor/session/{session}/tracks")
        );
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn diagnosis_reports_completed_stages_and_handles_an_empty_library() {
        let (mut backend, server) = mock_api(vec![
            ("GET /account/status", json!({"account":{"uid":123}})),
            (
                "GET /users/123/likes/tracks?if-modified-since-revision=0",
                library(std::iter::empty()),
            ),
        ]);
        let report = backend.diagnose();
        assert_eq!(report["checks"][0]["ok"], true);
        assert_eq!(report["checks"][1]["ok"], true);
        assert!(report["checks"][2]["skipped"].is_string());
        assert_eq!(server.join().unwrap().len(), 2);
        let (mut backend, server) = mock_api(vec![
            ("GET /account/status", json!({"account":{"uid":123}})),
            (
                "GET /users/123/likes/tracks?if-modified-since-revision=0",
                json!({}),
            ),
        ]);
        let report = backend.diagnose();
        assert_eq!(report["checks"][0]["ok"], true);
        assert_eq!(report["checks"][1]["stage"], "favorites");
        assert_eq!(report["checks"][1]["ok"], false);
        assert!(!report.to_string().contains("test-token"));
        server.join().unwrap();
    }

    #[test]
    fn diagnosis_downloads_a_bounded_fragment_without_sending_credentials_to_cdn() {
        use std::{
            io::{BufRead, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/signed-secret?token=secret",
            listener.local_addr().unwrap()
        );
        let cdn = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty());
                headers.push_str(&line.to_ascii_lowercase());
            }
            assert!(headers.contains("range: bytes=0-1023"));
            assert!(!headers.contains("authorization:"));
            write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n{}", "a".repeat(1024)).unwrap();
        });
        let (mut backend, server) = mock_api(vec![
            ("GET /account/status", json!({"account":{"uid":123}})),
            (
                "GET /users/123/likes/tracks?if-modified-since-revision=0",
                library([12].into_iter()),
            ),
            ("POST /tracks", metadata([12].into_iter())),
            (
                "GET /get-file-info?",
                json!({"downloadInfo":{"codec":"mp3", "quality":"high", "bitrate":320, "transport":"raw", "url":url}}),
            ),
        ]);
        let report = backend.diagnose();
        assert_eq!(report["checks"].as_array().unwrap().len(), 5);
        assert!(
            report["checks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|check| check["ok"] == true),
            "{report}"
        );
        assert_eq!(report["checks"][4]["bytes_read"], 1024);
        assert!(!report.to_string().contains("secret"));
        assert!(!report.to_string().contains("test-token"));
        assert_eq!(server.join().unwrap().len(), 4);
        cdn.join().unwrap();
    }

    #[test]
    fn wave_styles_send_session_seeds_instead_of_changing_playback_speed() {
        let mut replies = Vec::new();
        for _ in WaveStyle::ALL {
            replies.push(("POST /rotor/session/new", json!({"radioSessionId":"style", "batchId":"batch", "sequence":[{"track":{"id":91,"title":"Track"}}]})));
            replies.push(("POST /rotor/session/style/feedback", json!({})));
        }
        let (mut backend, server) = mock_api(replies);
        for style in WaveStyle::ALL {
            backend
                .start_with_style("yandex", PlaybackMode::MyVibe, style)
                .unwrap();
        }
        let bodies = server.join().unwrap();
        let expected = [
            json!(["user:onyourwave", "settingMoodEnergy:calm"]),
            json!(["user:onyourwave"]),
            json!(["user:onyourwave", "settingMoodEnergy:active"]),
            json!([
                "genre:ambientgenre",
                "settingMoodEnergy:calm",
                "settingLanguage:without-words"
            ]),
        ];
        for (body, expected) in bodies.iter().step_by(2).zip(expected) {
            assert_eq!(
                serde_json::from_str::<Value>(body).unwrap()["seeds"],
                expected
            );
        }
    }

    fn mock_api(
        responses: Vec<(&'static str, Value)>,
    ) -> (Yandex, std::thread::JoinHandle<Vec<String>>) {
        use std::{io::BufRead, io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut backend = Yandex::new("test-token".into());
        backend.base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for (request, result) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if request.ends_with('?') {
                    assert!(line.starts_with(request), "Unexpected request path");
                } else {
                    assert_eq!(line, format!("{request} HTTP/1.1\r\n"));
                }
                let mut headers = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.is_empty(), "Incomplete HTTP headers");
                    headers.push_str(&line.to_ascii_lowercase());
                }
                assert!(headers.contains("authorization: oauth test-token"));
                let len = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map_or(0, |len| len.trim().parse::<usize>().unwrap());
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                bodies.push(String::from_utf8(body).unwrap());
                if request == "POST /tracks" {
                    assert!(headers.contains("content-type: application/x-www-form-urlencoded"));
                }
                let body = json!({"result":result}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            bodies
        });
        (backend, server)
    }

    fn library(ids: impl Iterator<Item = u32>) -> Value {
        json!({"library":{"tracks":ids.map(|id| json!({"id":id})).collect::<Vec<_>>()}})
    }

    fn metadata(ids: impl Iterator<Item = u32>) -> Value {
        json!(
            ids.map(|id| json!({"id":id,"title":format!("Track {id}"),
            "artists":[{"name":"Artist"}],"durationMs":60000,
            "coverUri":"avatars.yandex.net/cover/%%","available":id!=3}))
                .collect::<Vec<_>>()
        )
    }

    fn requested_ids(body: &str) -> Vec<String> {
        assert!(body.contains("with-positions=True"));
        body.split('&')
            .filter_map(|field| field.strip_prefix("track-ids=").map(str::to_owned))
            .collect()
    }

    #[test]
    fn dislikes_use_the_account_library_and_require_a_confirmed_revision() {
        let (mut backend, server) = mock_api(vec![
            ("GET /account/status", json!({"account":{"uid":123}})),
            (
                "POST /users/123/dislikes/tracks/add-multiple",
                json!({"revision":1}),
            ),
            (
                "POST /users/123/dislikes/tracks/remove",
                json!({"revision":2}),
            ),
            ("POST /users/123/dislikes/tracks/add-multiple", json!({})),
        ]);
        let track = Track {
            source: String::new(),
            id: "17".into(),
            title: "Track".into(),
            artist: String::new(),
            duration: None,
            cover_url: None,
            genre: None,
            batch: String::new(),
        };
        assert!(backend.set_dislike(&track, true).unwrap());
        assert!(backend.set_dislike(&track, false).unwrap());
        assert!(
            backend
                .set_dislike(&track, true)
                .unwrap_err()
                .to_string()
                .contains("confirm")
        );
        let bodies = server.join().unwrap();
        assert_eq!(
            &bodies[1..],
            ["track-ids=17", "track-ids=17", "track-ids=17"]
        );
    }

    #[test]
    fn favorites_shuffle_fetches_bounded_batches_refreshes_each_pass_and_restores_my_vibe() {
        let first_metadata = metadata((1..=37).rev().filter(|id| *id != 4).chain([99999]));
        let second_metadata = metadata((1..=38).rev().filter(|id| ![4, 7].contains(id)));
        let (mut backend, server) = mock_api(vec![
            ("GET /account/status", json!({"account":{"uid":12345}})),
            (
                "GET /users/12345/likes/tracks?if-modified-since-revision=0",
                library(1..=37),
            ),
            ("POST /tracks", first_metadata.clone()),
            ("POST /tracks", first_metadata),
            (
                "GET /users/12345/likes/tracks?if-modified-since-revision=0",
                library((1..=38).filter(|id| *id != 7)),
            ),
            ("POST /tracks", second_metadata.clone()),
            ("POST /tracks", second_metadata),
            (
                "POST /rotor/session/new",
                json!({"radioSessionId":"wave", "batchId":"wave-batch", "sequence":[{"track":{"id":91,"title":"Wave"}}]}),
            ),
            ("POST /rotor/session/wave/feedback", json!({})),
        ]);
        let first_batch = backend.start_mode(PlaybackMode::FavoritesShuffle).unwrap();
        assert!(backend.session.is_none());
        assert_eq!(backend.shuffle.as_ref().unwrap().remaining.len(), 5);
        let tail = backend.more(&[]).unwrap();
        let first: Vec<_> = first_batch.iter().chain(&tail).collect();
        assert_eq!(first.len(), 35);
        assert_eq!(
            first.iter().map(|t| t.id.clone()).collect::<HashSet<_>>(),
            (1..=37)
                .filter(|id| ![3, 4].contains(id))
                .map(|id| id.to_string())
                .collect()
        );
        assert!(
            first
                .iter()
                .all(|t| t.batch.is_empty() && t.cover_url.is_some() && t.duration == Some(60.0))
        );
        for kind in [Feedback::Started, Feedback::Finished, Feedback::Skipped] {
            backend.feedback(first[0], kind, 1.0).unwrap();
        }
        let second_batch = backend.more(&[]).unwrap();
        assert_ne!(second_batch.first().unwrap().id, first.last().unwrap().id);
        let second_tail = backend.more(&[]).unwrap();
        let second: Vec<_> = second_batch.iter().chain(&second_tail).collect();
        assert_eq!(second.len(), 35);
        assert_eq!(
            second.iter().map(|t| t.id.clone()).collect::<HashSet<_>>(),
            (1..=38)
                .filter(|id| ![3, 4, 7].contains(id))
                .map(|id| id.to_string())
                .collect()
        );
        let wave = backend.start_mode(PlaybackMode::MyVibe).unwrap();
        assert!(backend.shuffle.is_none());
        assert_eq!(wave[0].id, "91");
        assert_eq!(wave[0].batch, "wave-batch");
        let bodies = server.join().unwrap();
        for (body, tracks, expected) in [(&bodies[2], &first_batch, 32), (&bodies[3], &tail, 5)] {
            let ids = requested_ids(body);
            assert_eq!(ids.len(), expected);
            assert_eq!(
                tracks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
                ids.into_iter()
                    .filter(|id| id != "3" && id != "4")
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(requested_ids(&bodies[5]).len(), 32);
        assert_eq!(requested_ids(&bodies[6]).len(), 5);
        assert_eq!(
            serde_json::from_str::<Value>(&bodies[7]).unwrap()["seeds"],
            json!(["user:onyourwave"])
        );
        assert_eq!(
            serde_json::from_str::<Value>(&bodies[8]).unwrap()["event"]["type"],
            "radioStarted"
        );
    }

    #[test]
    fn favorites_shuffle_reports_empty_library_and_all_unavailable_tracks() {
        for empty in [true, false] {
            let mut responses = vec![
                ("GET /account/status", json!({"account":{"uid":12345}})),
                (
                    "GET /users/12345/likes/tracks?if-modified-since-revision=0",
                    library(1..if empty { 1 } else { 34 }),
                ),
            ];
            if !empty {
                responses.extend([("POST /tracks", json!([])), ("POST /tracks", json!([]))]);
            }
            let (mut backend, server) = mock_api(responses);
            let error = backend
                .start_mode(PlaybackMode::FavoritesShuffle)
                .unwrap_err()
                .to_string();
            assert!(error.contains(if empty {
                "No favorite tracks yet"
            } else {
                "No playable favorite tracks"
            }));
            server.join().unwrap();
        }
    }

    #[test]
    fn favorites_shuffle_keeps_pending_ids_on_metadata_error_and_repeats_single_track() {
        let (mut backend, server) = mock_api(vec![
            ("GET /account/status", json!({"account":{"uid":12345}})),
            (
                "GET /users/12345/likes/tracks?if-modified-since-revision=0",
                library(1..=1),
            ),
            ("POST /tracks", json!({"unexpected":true})),
            ("POST /tracks", metadata(1..=1)),
            (
                "GET /users/12345/likes/tracks?if-modified-since-revision=0",
                library(1..=1),
            ),
            ("POST /tracks", metadata(1..=1)),
        ]);
        assert!(backend.start_mode(PlaybackMode::FavoritesShuffle).is_err());
        assert_eq!(backend.shuffle.as_ref().unwrap().remaining.len(), 1);
        assert_eq!(backend.more(&[]).unwrap()[0].id, "1");
        assert_eq!(backend.more(&[]).unwrap()[0].id, "1");
        let bodies = server.join().unwrap();
        assert_eq!(bodies[2], bodies[3]);
    }

    #[test]
    fn favorites_shuffle_avoids_boundary_repeat_even_when_other_tracks_are_unavailable() {
        let (mut backend, server) = mock_api(vec![
            ("POST /tracks", metadata([1, 34].into_iter())),
            ("POST /tracks", metadata([1, 34].into_iter())),
        ]);
        backend.shuffle = Some(FavoritesQueue {
            remaining: (1..=34).map(|id| id.to_string()).collect(),
            last: Some("1".into()),
            fresh_cycle: true,
        });
        let tracks = backend.more(&[]).unwrap();
        assert_eq!(
            tracks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["34", "1"]
        );
        assert!(backend.shuffle.as_ref().unwrap().remaining.is_empty());
        assert_eq!(requested_ids(&server.join().unwrap()[1]), ["33", "34", "1"]);
    }

    #[test]
    fn shuffle_can_produce_every_permutation_and_avoids_adjacent_pass_repeats() {
        let mut permutations = HashSet::new();
        for first in 0_u64..3 {
            for second in 0_u64..2 {
                let mut ids = vec!["1".into(), "2".into(), "3".into()];
                let bytes = [first, second, 0]
                    .into_iter()
                    .flat_map(u64::to_le_bytes)
                    .collect::<Vec<_>>();
                shuffle_favorites(&mut ids, None, &mut bytes.as_slice()).unwrap();
                permutations.insert(ids);
                let mut ids = vec!["1".into(), "2".into(), "3".into()];
                shuffle_favorites(&mut ids, Some("1"), &mut bytes.as_slice()).unwrap();
                assert_ne!(ids[0], "1");
                assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 3);
            }
        }
        assert_eq!(permutations.len(), 6);
        let bytes = [u64::MAX, 5]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(random_index(3, &mut bytes.as_slice()).unwrap(), 2);
        shuffle_favorites(&mut [], None, &mut [].as_slice()).unwrap();
        shuffle_favorites(&mut ["1".into()], Some("1"), &mut [].as_slice()).unwrap();
    }

    #[test]
    fn favorite_toggle_reads_server_state_and_requires_confirmation_for_add_and_remove() {
        use std::{
            io::{BufRead, BufReader, Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (method, path, response, expected_body) in [
                (
                    "GET",
                    "/account/status",
                    json!({"account":{"uid":12345}}),
                    "",
                ),
                (
                    "GET",
                    "/users/12345/likes/tracks?if-modified-since-revision=0",
                    json!({"library":{"tracks":[{"id":12},{"id":"13"}]}}),
                    "",
                ),
                (
                    "GET",
                    "/users/12345/likes/tracks?if-modified-since-revision=0",
                    json!({"library":{"tracks":[{"id":12},{"id":"13"}]}}),
                    "",
                ),
                (
                    "POST",
                    "/users/12345/likes/tracks/remove",
                    json!({"revision":2}),
                    "track-ids=12",
                ),
                (
                    "GET",
                    "/users/12345/likes/tracks?if-modified-since-revision=0",
                    json!({"library":{"tracks":[{"id":"13"}]}}),
                    "",
                ),
                (
                    "POST",
                    "/users/12345/likes/tracks/add-multiple",
                    json!({"revision":3}),
                    "track-ids=12",
                ),
                (
                    "GET",
                    "/users/12345/likes/tracks?if-modified-since-revision=0",
                    json!({"library":{"tracks":[{"id":12},{"id":"13"}]}}),
                    "",
                ),
                (
                    "POST",
                    "/users/12345/likes/tracks/add-multiple",
                    json!({"ok":true}),
                    "track-ids=14",
                ),
                (
                    "GET",
                    "/users/12345/likes/tracks?if-modified-since-revision=0",
                    json!({"library":{"tracks":[{"id":12},{"id":"13"}]}}),
                    "",
                ),
                (
                    "POST",
                    "/users/12345/likes/tracks/add-multiple",
                    json!({"revision":4}),
                    "track-ids=14",
                ),
                (
                    "GET",
                    "/users/12345/likes/tracks?if-modified-since-revision=0",
                    json!({"library":{"tracks":[{"id":14}]}}),
                    "",
                ),
                (
                    "POST",
                    "/users/12345/likes/tracks/remove",
                    json!({"ok":true}),
                    "track-ids=14",
                ),
                (
                    "GET",
                    "/users/12345/likes/tracks?if-modified-since-revision=0",
                    json!({"library":{}}),
                    "",
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.starts_with(&format!("{method} {path} HTTP/1.1")));
                let headers = headers.to_ascii_lowercase();
                assert!(headers.contains("authorization: oauth test-token"));
                let len = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map(|v| v.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                assert_eq!(String::from_utf8(body).unwrap(), expected_body);
                if method == "POST" {
                    assert!(headers.contains("content-type: application/x-www-form-urlencoded"));
                }
                let body = json!({"result":response}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let mut backend = Yandex::new("test-token".into());
        backend.base = base;
        let mut track = Track {
            source: String::new(),
            id: "12".into(),
            title: "Track".into(),
            artist: String::new(),
            duration: None,
            cover_url: None,
            genre: None,
            batch: String::new(),
        };
        assert_eq!(backend.favorite_state(&track).unwrap(), Some(true));
        backend.shuffle = Some(FavoritesQueue {
            remaining: ["12".into(), "13".into()].into(),
            ..FavoritesQueue::default()
        });
        assert!(!backend.toggle_favorite(&track).unwrap());
        assert_eq!(
            backend.shuffle.as_ref().unwrap().remaining,
            ["13".to_owned()]
        );
        assert_eq!(backend.favorite_state(&track).unwrap(), Some(false));
        assert!(backend.toggle_favorite(&track).unwrap());
        track.id = "14".into();
        assert!(backend.toggle_favorite(&track).is_err());
        assert!(backend.favorites.is_none());
        assert!(backend.toggle_favorite(&track).unwrap());
        assert_eq!(backend.favorite_state(&track).unwrap(), Some(true));
        assert!(backend.toggle_favorite(&track).is_err());
        assert!(backend.favorites.is_none());
        assert!(backend.toggle_favorite(&track).is_err());
        server.join().unwrap();
    }

    #[test]
    fn file_info_signature_matches_an_independent_hmac_calculation() {
        assert_eq!(
            file_info_sign("4784420", 1668687184),
            "Z5G0tO9nt+tbiv04e4M/eaAG55adkZxC9/zHaUmsh+U"
        );
    }

    #[test]
    fn resolve_requests_lossless_and_reads_encrypted_flac_metadata() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            assert!(headers.starts_with("GET /get-file-info?"));
            assert!(headers.contains("quality=lossless"));
            assert!(headers.contains("transports=encraw"));
            assert!(headers.contains("codecs=flac%2Cflac-mp4%2Caac-mp4%2Cmp3"));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("x-yandex-music-client: yandexmusicdesktopappwindows/5.95.0")
            );
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: oauth test-token")
            );
            let body = json!({"result":{"downloadInfo":{
                "codec":"flac-mp4", "quality":"lossless", "bitrate":0,
                "transport":"encraw", "url":"https://example.test/audio",
                "key":"000102030405060708090a0b0c0d0e0f"
            }}})
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let mut backend = Yandex::new("test-token".into());
        backend.base = base;
        let track = Track {
            source: String::new(),
            id: "12".into(),
            title: String::new(),
            artist: String::new(),
            duration: None,
            cover_url: None,
            genre: None,
            batch: String::new(),
        };
        let result = backend.resolve(&track);
        server.join().unwrap();
        let Input::Http {
            aes128_key,
            hint,
            quality,
            ..
        } = result.unwrap()
        else {
            panic!("Expected HTTP")
        };
        assert_eq!(aes128_key, Some(std::array::from_fn(|i| i as u8)));
        assert_eq!(hint, "m4a");
        assert_eq!(quality, "FLAC · lossless");
    }

    #[test]
    fn reports_server_quality_and_rejects_missing_or_invalid_encryption_keys() {
        let value = json!({"codec":"mp3", "quality":"hq", "bitrate":320,
            "transport":"raw", "url":"https://example.test/audio"});
        let input = serde_json::from_value::<FileDownload>(value.clone())
            .unwrap()
            .into_input()
            .unwrap();
        let Input::Http {
            quality,
            aes128_key,
            ..
        } = input
        else {
            panic!("Expected HTTP")
        };
        assert_eq!(quality, "MP3 · 320 kbps");
        assert!(aes128_key.is_none());
        for key in [
            Value::Null,
            json!("short"),
            json!("z".repeat(32)),
            json!("é".repeat(16)),
        ] {
            let mut value = value.clone();
            value["transport"] = json!("encraw");
            value["key"] = key;
            assert!(
                serde_json::from_value::<FileDownload>(value)
                    .unwrap()
                    .into_input()
                    .is_err()
            );
        }
    }

    #[test]
    fn session_protocol_sends_feedback_before_requesting_more_tracks() {
        use std::{
            io::{BufRead, BufReader, Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let batch = json!({"radioSessionId":"session-1", "batchId":"batch-1", "sequence":[{"track":{"id":12,"title":"Track"}}]});
            for (path, response) in [
                ("/rotor/session/new", batch),
                ("/rotor/session/session-1/feedback", json!({"ok":true})),
                ("/rotor/session/session-1/feedback", json!({"ok":true})),
                (
                    "/rotor/session/session-1/tracks",
                    json!({"batchId":"batch-2","sequence":[]}),
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.starts_with(&format!("POST {path} HTTP/1.1")));
                let headers = headers.to_ascii_lowercase();
                assert!(headers.contains("authorization: oauth test-token"));
                let len: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                if path.ends_with("new") {
                    assert_eq!(body["seeds"], json!(["user:onyourwave"]));
                } else if body["event"]["type"] == "skip" {
                    assert_eq!(body["batchId"], "batch-1");
                    assert_eq!(body["event"]["trackId"], "12");
                    assert_eq!(body["event"]["totalPlayedSeconds"], 0.25);
                    assert!(body["event"]["timestamp"].as_str().unwrap().ends_with('Z'));
                } else if path.ends_with("tracks") {
                    assert_eq!(body["queue"], json!(["12"]));
                } else {
                    assert_eq!(body["event"]["type"], "radioStarted");
                }
                let text = json!({"result":response}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
        });
        let mut backend = Yandex::new("test-token".into());
        backend.base = base;
        let replay = Track {
            source: String::new(),
            id: "12".into(),
            title: "Replay".into(),
            artist: String::new(),
            duration: None,
            cover_url: None,
            genre: None,
            batch: String::new(),
        };
        for kind in [Feedback::Started, Feedback::Finished, Feedback::Skipped] {
            backend.feedback(&replay, kind, 1.0).unwrap();
        }
        let tracks = backend.start().unwrap();
        backend
            .feedback(&tracks[0], Feedback::Skipped, 0.25)
            .unwrap();
        assert!(backend.more(&["12".into()]).unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn batch_keeps_feedback_identity_and_filters_unavailable_items() {
        let tracks = parse_batch(&json!({"batchId": "batch", "sequence": [
            {"track": {"id": 12, "title": "Track", "durationMs": 42000, "artists": [{"name":"Artist"}]}},
            {"track": {"id": "13", "available": false}},
            {"type": "ad"}
        ]})).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "12");
        assert_eq!(tracks[0].batch, "batch");
        assert_eq!(tracks[0].duration, Some(42.0));
        assert_eq!(tracks[0].artist, "Artist");
    }

    #[test]
    fn cover_urls_use_track_or_album_artwork_and_expand_the_size() {
        for (track, expected) in [
            (
                json!({"coverUri":"avatars.yandex.net/get-music/album/%%", "albums":[{"coverUri":"other/%%"}]}),
                Some("https://avatars.yandex.net/get-music/album/400x400"),
            ),
            (
                json!({"albums":[{}, {"coverUri":"avatars.yandex.net/album/%%"}]}),
                Some("https://avatars.yandex.net/album/400x400"),
            ),
            (
                json!({"coverUri":"", "albums":[{"coverUri":"https://example.com/%%"}]}),
                Some("https://example.com/400x400"),
            ),
            (
                json!({"coverUri":"//example.com/image.jpg"}),
                Some("https://example.com/image.jpg"),
            ),
            (
                json!({"coverUri":"http://example.com/image.jpg"}),
                Some("http://example.com/image.jpg"),
            ),
            (json!({"coverUri":"  ", "albums":[{}]}), None),
            (json!({}), None),
        ] {
            let tracks = parse_batch(&json!({"batchId":"b", "sequence":[{"track":{
                "id":12, "coverUri":track["coverUri"], "albums":track["albums"]
            }}]}))
            .unwrap();
            assert_eq!(tracks[0].cover_url.as_deref(), expected);
        }
    }

    #[test]
    fn malformed_responses_are_errors_not_empty_queues() {
        assert!(parse_batch(&json!({"batchId":"b"})).is_err());
        assert!(
            parse_batch(&json!({"batchId":"b", "sequence":[{"track":{"id":"../bad"}}]})).is_err()
        );
    }
}
