use super::{Backend, Feedback, Input, Track, web};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct SoundCloud {
    profile: PathBuf,
    config: Option<Value>,
    http: ureq::Agent,
    api: String,
    site: String,
    client: String,
    token: Option<String>,
    cookies: web::Cookies,
    seed: String,
    seed_override: Option<String>,
    user_agent: String,
}

impl SoundCloud {
    pub fn new(profile: PathBuf) -> Self {
        Self {
            profile,
            config: None,
            http: super::yandex_http(),
            api: "https://api-v2.soundcloud.com/".into(),
            site: "https://soundcloud.com/".into(),
            client: String::new(),
            token: None,
            cookies: web::Cookies::default(),
            seed: String::new(),
            seed_override: None,
            user_agent: web::USER_AGENT.into(),
        }
    }

    pub fn with_seed(mut self, seed: Option<&str>) -> Self {
        self.seed_override = seed.map(str::to_owned);
        self
    }

    fn init(&mut self) -> Result<()> {
        if self.config.is_none() {
            let config = match std::fs::symlink_metadata(&self.profile) {
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && self.seed_override.is_some() =>
                {
                    serde_json::json!({})
                }
                _ => web::profile(&self.profile)?,
            };
            self.user_agent = web::user_agent(&config)?;
            self.client = config["client_id"].as_str().unwrap_or_default().into();
            self.token = config["token"].as_str().map(str::to_owned);
            if let Some(path) = config["cookies"].as_str() {
                self.cookies = web::Cookies::read(Path::new(path))?;
                self.cookies.seed_agent(&self.http)?;
                if self.token.is_none() {
                    self.token = self
                        .cookies
                        .header(&self.site)?
                        .split("; ")
                        .find_map(|part| part.strip_prefix("oauth_token=").map(str::to_owned));
                }
            }
            self.config = Some(config);
        }
        if self.client.is_empty() {
            self.refresh_client()?;
        }
        Ok(())
    }

    fn page(&self, url: &str) -> Result<String> {
        self.http
            .get(url)
            .header("User-Agent", &self.user_agent)
            .call()
            .map_err(|_| anyhow::anyhow!("SoundCloud page request failed"))?
            .body_mut()
            .read_to_string()
            .map_err(|_| anyhow::anyhow!("Cannot read SoundCloud page"))
    }

    fn refresh_client(&mut self) -> Result<()> {
        let html = self.page(&self.site)?;
        let scripts = regex_lite::Regex::new(r#"<script[^>]+src="([^"]+)""#).unwrap();
        let client = regex_lite::Regex::new(r#"client_id\s*:\s*"([a-zA-Z0-9]{32})""#).unwrap();
        for script in scripts
            .captures_iter(&html)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(8)
        {
            let url = url::Url::parse(&self.site)?.join(&script[1])?;
            let host = url.host_str().unwrap_or_default();
            if !(host.ends_with(".sndcdn.com") || host == "sndcdn.com" || host == "soundcloud.com")
            {
                continue;
            }
            if let Ok(body) = self.page(url.as_str())
                && let Some(value) = client.captures(&body)
            {
                self.client = value[1].into();
                return Ok(());
            }
        }
        anyhow::bail!(
            "Cannot discover SoundCloud client ID; configure client_id in the connection profile"
        )
    }

    fn get(&mut self, address: &str, query: &[(&str, String)]) -> Result<Value> {
        self.init()?;
        for attempt in 0..2 {
            let mut url = url::Url::parse(address).context("Invalid SoundCloud API URL")?;
            url.query_pairs_mut()
                .append_pair("client_id", &self.client)
                .extend_pairs(query.iter().map(|(k, v)| (*k, v)));
            let mut request = self
                .http
                .get(url.as_str())
                .header("User-Agent", &self.user_agent);
            if let Some(token) = &self.token {
                request = request.header("Authorization", format!("OAuth {token}"));
            }
            match request.call() {
                Ok(mut response) => {
                    return response
                        .body_mut()
                        .read_json()
                        .map_err(|_| anyhow::anyhow!("Invalid SoundCloud API response"));
                }
                Err(ureq::Error::StatusCode(401 | 403))
                    if attempt == 0
                        && self
                            .config
                            .as_ref()
                            .is_some_and(|v| v["client_id"].as_str().is_none()) =>
                {
                    self.refresh_client()?
                }
                Err(ureq::Error::StatusCode(status)) => anyhow::bail!(
                    "SoundCloud request failed (HTTP {status}); check session and client ID"
                ),
                Err(_) => anyhow::bail!("SoundCloud connection failed"),
            }
        }
        unreachable!()
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.api)
    }

    fn wave(&mut self, history: &[String]) -> Result<Vec<Track>> {
        self.init()?;
        ensure!(
            self.seed.starts_with("https://soundcloud.com/")
                || self.seed.starts_with("https://www.soundcloud.com/"),
            "Configure a SoundCloud seed track URL"
        );
        let mut station = self
            .seed
            .replace("https://www.soundcloud.com/", "https://soundcloud.com/");
        if !station.contains("/stations/track/") {
            station = station.replacen("soundcloud.com/", "soundcloud.com/stations/track/", 1);
        }
        let resolved = self.get(&self.endpoint("resolve"), &[("url", station)])?;
        let station = resolved["id"]
            .as_str()
            .filter(|id| id.starts_with("soundcloud:track-stations:"))
            .context("SoundCloud did not return a track station")?;
        let data = self.get(
            &self.endpoint(&format!("stations/{station}/tracks")),
            &[("limit", "50".into()), ("linked_partitioning", "1".into())],
        )?;
        let items = data["collection"]
            .as_array()
            .context("Missing SoundCloud station tracks")?;
        let mut tracks = Vec::new();
        for item in items {
            let item = item.get("track").unwrap_or(item);
            if let Some(track) = metadata(item)
                && !history.contains(&track.id)
            {
                if let Some(link) = item["permalink_url"].as_str() {
                    self.seed = link.into();
                }
                tracks.push(track);
                if tracks.len() == 20 {
                    break;
                }
            }
        }
        ensure!(
            !tracks.is_empty(),
            "No playable tracks in the SoundCloud station"
        );
        Ok(tracks)
    }
}

fn metadata(item: &Value) -> Option<Track> {
    if matches!(item["policy"].as_str(), Some("BLOCK" | "SNIP"))
        || item["access"].as_str() == Some("preview")
    {
        return None;
    }
    Some(Track {
        source: String::new(),
        id: item["id"].as_u64()?.to_string(),
        title: web::text(item["title"].as_str()?),
        artist: item["publisher_metadata"]["artist"]
            .as_str()
            .or_else(|| item["user"]["username"].as_str())
            .unwrap_or_default()
            .into(),
        duration: item["duration"].as_f64().map(|ms| ms / 1000.0),
        cover_url: item["artwork_url"]
            .as_str()
            .map(|url| url.replace("-large.", "-t500x500.")),
        genre: item["genre"]
            .as_str()
            .filter(|genre| !genre.trim().is_empty())
            .map(str::to_owned),
        batch: String::new(),
    })
}

fn format_rank(format: &Value) -> Option<(bool, u32)> {
    let protocol = format["format"]["protocol"].as_str().unwrap_or_default();
    let preset = format["preset"].as_str().unwrap_or_default();
    if format["snipped"].as_bool() == Some(true)
        || preset.contains("preview")
        || preset.starts_with("abr_")
        || !matches!(protocol, "progressive" | "http" | "hls" | "encrypted-hls")
    {
        return None;
    }
    let premium = format["quality"].as_str() == Some("hq");
    let rate = regex_lite::Regex::new(r"(\d+)k")
        .unwrap()
        .captures(preset)
        .and_then(|m| m[1].parse().ok())
        .unwrap_or(if premium { 256 } else { 128 });
    Some((premium, rate))
}

impl Backend for SoundCloud {
    fn key(&self) -> &'static str {
        "soundcloud"
    }
    fn name(&self) -> &'static str {
        "SoundCloud"
    }
    fn start(&mut self) -> Result<Vec<Track>> {
        self.init()?;
        self.seed = self
            .seed_override
            .as_deref()
            .or_else(|| self.config.as_ref().unwrap()["seed"].as_str())
            .unwrap_or_default()
            .into();
        self.wave(&[])
    }
    fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
        self.wave(history)
    }
    fn resolve(&mut self, track: &Track) -> Result<Input> {
        ensure!(
            track.id.bytes().all(|c| c.is_ascii_digit()) && !track.id.is_empty(),
            "Invalid SoundCloud track ID"
        );
        let info = self.get(&self.endpoint(&format!("tracks/{}", track.id)), &[])?;
        ensure!(
            metadata(&info).is_some(),
            "SoundCloud track is blocked or only available as a preview"
        );
        if info["downloadable"].as_bool() == Some(true)
            && info["has_downloads_left"].as_bool() == Some(true)
            && let Ok(download) = self.get(
                &self.endpoint(&format!("tracks/{}/download", track.id)),
                &[],
            )
            && let Some(url) = download["redirectUri"].as_str()
            && url.starts_with("https://")
            && let Ok(response) = self.http.head(url).call()
        {
            let mime = response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .unwrap_or_default()
                .split(';')
                .next()
                .unwrap_or_default()
                .trim();
            let hint = match mime {
                "audio/flac" | "audio/x-flac" => Some("flac"),
                "audio/mpeg" | "audio/mp3" => Some("mp3"),
                "audio/mp4" => Some("m4a"),
                _ => None,
            };
            if let Some(hint) = hint {
                return Ok(Input::Http {
                    headers: Vec::new(),
                    url: url.into(),
                    aes128_key: None,
                    hint,
                    quality: "SoundCloud · original upload".into(),
                });
            }
            return web::transcode(url.into(), "SoundCloud · original upload".into());
        }
        let mut formats: Vec<_> = info["media"]["transcodings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| format_rank(f).map(|rank| (rank, f)))
            .collect();
        formats.sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
        for ((_, rate), format) in formats {
            let Some(endpoint) = format["url"].as_str() else {
                continue;
            };
            let endpoint_url = url::Url::parse(endpoint)?;
            let host = endpoint_url.host_str().unwrap_or_default();
            let api_host = url::Url::parse(&self.api)?
                .host_str()
                .unwrap_or_default()
                .to_owned();
            ensure!(
                host == api_host || host.ends_with(".soundcloud.com"),
                "Unexpected SoundCloud stream endpoint"
            );
            let Ok(resolved) = self.get(endpoint, &[]) else {
                continue;
            };
            let Some(url) = resolved["url"]
                .as_str()
                .filter(|u| !u.contains("/preview/") && !u.contains("/0/30/"))
            else {
                continue;
            };
            ensure!(
                url.starts_with("https://"),
                "Insecure SoundCloud stream URL"
            );
            let mime = format["format"]["mime_type"].as_str().unwrap_or_default();
            let mime = mime.split(';').next().unwrap_or_default().trim();
            let protocol = format["format"]["protocol"].as_str().unwrap_or_default();
            let codec = if mime.contains("opus") || mime.contains("ogg") {
                "Opus"
            } else if mime.contains("mp4") || mime.contains("aac") {
                "AAC"
            } else {
                "MP3"
            };
            let quality = format!("SoundCloud · {codec} {rate} kbps");
            if matches!(protocol, "progressive" | "http")
                && matches!(mime, "audio/mpeg" | "audio/mp3" | "audio/mp4")
            {
                return Ok(Input::Http {
                    headers: Vec::new(),
                    url: url.into(),
                    aes128_key: None,
                    hint: if mime == "audio/mp4" { "m4a" } else { "mp3" },
                    quality,
                });
            }
            return web::transcode(url.into(), quality);
        }
        anyhow::bail!("No playable full SoundCloud audio stream")
    }
    fn feedback(&mut self, _: &Track, _: Feedback, _: f64) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_seed_overrides_the_saved_station_and_survives_restart_without_writes() {
        let directory = std::env::temp_dir().join(format!(
            "plavno-soundcloud-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = directory.join("soundcloud.json");
        let saved = br#"{"seed":"https://soundcloud.com/artist/saved","token":"fixture-token","client_id":"fixture-client"}"#;
        crate::config::save_private(&profile, saved).unwrap();
        let item = serde_json::json!({"id":12,"title":"Track","permalink_url":"https://soundcloud.com/artist/next"});
        let (base, server) = web::fixture_server(|_| {
            (0..3)
                .flat_map(|_| {
                    [
                        json!({"id":"soundcloud:track-stations:12"}).to_string(),
                        json!({"collection":[{"track":item}]}).to_string(),
                    ]
                })
                .collect()
        });
        let mut backend = SoundCloud::new(profile.clone())
            .with_seed(Some("https://soundcloud.com/artist/override"));
        backend.api = base;
        assert_eq!(backend.start().unwrap()[0].id, "12");
        assert_eq!(backend.more(&[]).unwrap()[0].id, "12");
        assert_eq!(backend.start().unwrap()[0].id, "12");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 6);
        for (index, seed) in [(0, "override"), (2, "next"), (4, "override")] {
            assert!(requests[index].contains(&format!("%2Fartist%2F{seed}")));
        }
        assert!(requests.iter().all(|request| {
            request.contains("client_id=fixture-client")
                && request
                    .to_lowercase()
                    .contains("authorization: oauth fixture-token")
        }));
        assert_eq!(std::fs::read(profile).unwrap(), saved);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_station_and_resolution_choose_the_highest_full_stream() {
        let item = json!({"id":12,"title":"Full track","genre":"Deep House","duration":120000,"policy":"ALLOW","user":{"username":"Artist"},"permalink_url":"https://soundcloud.com/artist/track"});
        let (base, server) = web::fixture_server(|base| {
            let mut media = item.clone();
            media["media"] = json!({"transcodings":[
                {"url":format!("{base}low"),"preset":"mp3_128k","quality":"sq","format":{"protocol":"progressive","mime_type":"audio/mpeg"}},
                {"url":format!("{base}high"),"preset":"aac_256k","quality":"hq","format":{"protocol":"progressive","mime_type":"audio/mp4"}},
                {"url":format!("{base}preview"),"preset":"aac_512k","snipped":true,"format":{"protocol":"hls","mime_type":"audio/mp4"}}
            ]});
            vec![
                json!({"id":"soundcloud:track-stations:12"}).to_string(),
                json!({"collection":[{"track":item.clone()}]}).to_string(),
                media.to_string(),
                json!({"url":"https://cdn.example/full.m4a"}).to_string(),
            ]
        });
        let mut backend = SoundCloud::new(PathBuf::new());
        backend.api = base;
        backend.client = "test-client".into();
        backend.token = Some("test-token".into());
        backend.config =
            Some(json!({"seed":"https://soundcloud.com/artist/track","client_id":"test-client"}));
        let tracks = backend.start().unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "12");
        assert_eq!(tracks[0].genre.as_deref(), Some("Deep House"));
        match backend.resolve(&tracks[0]).unwrap() {
            Input::Http {
                url, hint, quality, ..
            } => {
                assert_eq!(url, "https://cdn.example/full.m4a");
                assert_eq!(hint, "m4a");
                assert!(quality.contains("256 kbps"));
            }
            _ => panic!("Highest supported stream did not use the native HTTP buffer"),
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].contains("stations%2Ftrack"));
        assert!(requests.iter().all(|r| {
            r.to_ascii_lowercase()
                .contains("authorization: oauth test-token")
        }));
        assert!(requests[3].starts_with("GET /high?"));
    }

    #[test]
    fn full_tracks_and_best_formats_exclude_previews_and_drm() {
        assert!(metadata(&json!({"id":1,"title":"Preview","policy":"SNIP"})).is_none());
        assert!(format_rank(&json!({"preset":"aac_256k","quality":"hq","snipped":true,"format":{"protocol":"hls"}})).is_none());
        assert!(
            format_rank(&json!({"preset":"aac_256k","format":{"protocol":"cbc-hls"}})).is_none()
        );
        assert!(
            format_rank(&json!({"preset":"aac_256k","quality":"hq","format":{"protocol":"hls"}}))
                > format_rank(&json!({"preset":"mp3_128k","format":{"protocol":"progressive"}}))
        );
        let track = metadata(&json!({"id":12,"title":"A &amp; B","duration":120000,"user":{"username":"Artist"},"artwork_url":"https://cdn.example/cover-large.jpg"})).unwrap();
        assert_eq!(track.title, "A & B");
        assert_eq!(track.duration, Some(120.0));
        assert_eq!(
            track.cover_url.as_deref(),
            Some("https://cdn.example/cover-t500x500.jpg")
        );
    }
}
