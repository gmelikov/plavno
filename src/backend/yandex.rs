use super::{Backend, Feedback, Input, PlaybackMode, Track, WaveStyle};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::File,
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
}

#[derive(Default)]
struct FavoritesQueue {
    remaining: VecDeque<String>,
    last: Option<String>,
    fresh_cycle: bool,
}

const FAVORITES_BATCH: usize = 32;

pub fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
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
        _ => anyhow!("Connection error or timeout"),
    }
}

impl Yandex {
    pub fn new(token: String) -> Self {
        Self {
            http: http(),
            token,
            base: "https://api.music.yandex.net".into(),
            session: None,
            uid: None,
            favorites: None,
            shuffle: None,
            style: WaveStyle::default(),
        }
    }

    fn request(&self, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let auth = format!("OAuth {}", self.token);
        let mut response = match body {
            Some(body) => self
                .http
                .post(&url)
                .header("Authorization", &auth)
                .send_json(body),
            None => self.http.get(&url).header("Authorization", &auth).call(),
        }
        .map_err(network_error)?;
        let value: Value = response
            .body_mut()
            .read_json()
            .context("Invalid API response")?;
        ensure!(value.get("error").is_none(), "The API reported an error");
        value
            .get("result")
            .cloned()
            .context("The API response is missing result")
    }

    fn send_event(&self, batch: &str, mut event: Value) -> Result<()> {
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
        let mut random =
            BufReader::new(File::open("/dev/urandom").context("Failed to open the random source")?);
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
            let mut response = self
                .http
                .post(&format!("{}/tracks", self.base))
                .header("Authorization", &format!("OAuth {}", self.token))
                .send_form(fields)
                .map_err(network_error)?;
            let value: Value = response
                .body_mut()
                .read_json()
                .context("Invalid track metadata response")?;
            ensure!(
                value.get("error").is_none(),
                "The API reported a track metadata error"
            );
            let values = value["result"]
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
        let timestamp = OffsetDateTime::now_utc().unix_timestamp();
        let sign = file_info_sign(&track.id, timestamp);
        let mut response = self
            .http
            .get(&format!("{}/get-file-info", self.base))
            .header("Authorization", &format!("OAuth {}", self.token))
            .header(
                "X-Yandex-Music-Client",
                "YandexMusicDesktopAppWindows/5.95.0",
            )
            .query("ts", timestamp.to_string())
            .query("trackId", &track.id)
            .query("quality", "lossless")
            .query("codecs", FILE_CODECS)
            .query("transports", "encraw")
            .query("sign", sign)
            .call()
            .map_err(network_error)?;
        let value: Value = response
            .body_mut()
            .read_json()
            .context("Invalid API response")?;
        ensure!(
            value.get("error").is_none(),
            "The API reported an audio quality error"
        );
        let info: FileDownload = serde_json::from_value(value["result"]["downloadInfo"].clone())
            .context("The API did not return audio stream information")?;
        info.into_input()
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
        let mut response = self
            .http
            .post(&format!(
                "{}/users/{}/dislikes/tracks/{action}",
                self.base,
                self.uid.as_ref().unwrap()
            ))
            .header("Authorization", format!("OAuth {}", self.token))
            .send_form([("track-ids", track.id.as_str())])
            .map_err(network_error)?;
        let value: Value = response
            .body_mut()
            .read_json()
            .context("Invalid API response")?;
        ensure!(
            value.get("error").is_none() && value["result"]["revision"].as_u64().is_some(),
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
            let mut response = self
                .http
                .post(&format!(
                    "{}/users/{}/likes/tracks/{action}",
                    self.base,
                    self.uid.as_ref().unwrap()
                ))
                .header("Authorization", &format!("OAuth {}", self.token))
                .send_form([("track-ids", track.id.as_str())])
                .map_err(network_error)?;
            let value: Value = response
                .body_mut()
                .read_json()
                .context("Invalid API response")?;
            ensure!(
                value.get("error").is_none() && value["result"]["revision"].as_u64().is_some(),
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
const FILE_CODECS: &str = "flac,flac-mp4,aac-mp4,mp3";

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

#[derive(Deserialize)]
struct FileDownload {
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
                assert_eq!(line, format!("{request} HTTP/1.1\r\n"));
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
