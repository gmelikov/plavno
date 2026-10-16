use anyhow::{Context, Result, ensure};
use rustypipe::{
    client::{ClientType, RustyPipe},
    model::{AudioCodec, AudioStream, TrackItem},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};

fn private(path: &Path) -> Result<String> {
    ensure!(
        plavno_platform::private_file(path)?,
        "Connection files must be regular files with permissions 0600"
    );
    fs::read_to_string(path).context("Cannot read connection file")
}

fn seed_id(seed: &str) -> Result<String> {
    if seed.starts_with("https://") {
        let url = url::Url::parse(seed)?;
        if let Some((_, id)) = url.query_pairs().find(|(key, _)| key == "v") {
            return Ok(id.into_owned());
        }
        return url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .context("Missing seed video ID");
    }
    Ok(seed.to_owned())
}

fn connection_profile(path: &Path, seed: Option<&str>) -> Result<Value> {
    let mut config = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && seed.is_some() => json!({}),
        _ => serde_json::from_str(&private(path)?).context("Invalid connection profile")?,
    };
    ensure!(
        config.is_object(),
        "Connection profile must be a JSON object"
    );
    if let Some(seed) = seed {
        config["seed"] = json!(seed);
    }
    Ok(config)
}

fn find_mix(value: &Value) -> Option<String> {
    match value {
        Value::Object(obj) => obj
            .get("playlistId")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("RD"))
            .map(str::to_owned)
            .or_else(|| obj.values().find_map(find_mix)),
        Value::Array(items) => items.iter().find_map(find_mix),
        _ => None,
    }
}

fn best_audio(streams: &[AudioStream]) -> Option<&AudioStream> {
    streams
        .iter()
        .filter(|s| {
            s.drm_track_type.is_none()
                && s.drm_systems.is_empty()
                && s.url.starts_with("https://")
                && s.track.as_ref().is_none_or(|track| track.is_default)
        })
        .max_by_key(|s| {
            (
                s.average_bitrate.max(s.bitrate),
                s.channels.unwrap_or(2),
                u8::from(s.codec == AudioCodec::Opus),
            )
        })
}

struct Service {
    client: RustyPipe,
    authenticated: bool,
    seed: String,
    playlist: String,
}

impl Service {
    async fn new(path: &Path, seed: Option<&str>) -> Result<Self> {
        let config = connection_profile(path, seed)?;
        let storage = path
            .parent()
            .context("Missing profile directory")?
            .join("youtube-session");
        if let Ok(meta) = fs::symlink_metadata(&storage) {
            ensure!(
                meta.file_type().is_dir(),
                "Invalid YouTube session directory"
            );
        }
        fs::create_dir_all(&storage)?;
        plavno_platform::private_directory(&storage)?;
        let cache = storage.join("rustypipe_cache.json");
        if fs::symlink_metadata(&cache).is_ok() {
            private(&cache)?;
        }
        let mut builder = RustyPipe::builder()
            .storage_dir(storage)
            .no_reporter()
            .timeout(Duration::from_secs(20));
        if let Some(binary) = config["botguard"].as_str() {
            builder = builder.botguard_bin(binary);
        }
        let client = builder.build()?;
        let mut authenticated = false;
        if let Some(cookies) = config["cookies"].as_str() {
            client
                .user_auth_set_cookie_txt(&private(Path::new(cookies))?)
                .await?;
            authenticated = true;
        } else if let Some(headers) = config["headers"].as_str() {
            let headers: Value = serde_json::from_str(&private(Path::new(headers))?)?;
            let cookie = headers
                .as_object()
                .and_then(|obj| {
                    obj.iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("cookie"))
                })
                .and_then(|(_, value)| value.as_str())
                .context("Browser headers must contain a cookie header")?;
            client.user_auth_set_cookie(cookie).await?;
            authenticated = true;
        }
        let mut service = Self {
            client,
            authenticated,
            seed: seed_id(config["seed"].as_str().unwrap_or_default())?,
            playlist: config["playlist"].as_str().unwrap_or_default().to_owned(),
        };
        if service.seed.is_empty() && service.playlist.is_empty() {
            ensure!(
                authenticated,
                "Configure a seed track, playlist, or authenticated YouTube cookies"
            );
            let home = service
                .query()
                .raw(
                    ClientType::DesktopMusic,
                    "browse",
                    &json!({"browseId":"FEmusic_home"}),
                )
                .await?;
            service.playlist = find_mix(&serde_json::from_str::<Value>(&home)?)
                .context("No recommended mix found; configure a seed track or playlist")?;
        }
        Ok(service)
    }

    fn query(&self) -> rustypipe::client::RustyPipeQuery {
        if self.authenticated {
            self.client.query().authenticated()
        } else {
            self.client.query().unauthenticated()
        }
    }

    async fn wave(&mut self, history: &[String]) -> Result<Value> {
        let radio = if !self.seed.is_empty() {
            self.query().music_radio_track(&self.seed).await?
        } else if self.playlist.starts_with("RD") {
            self.query().music_radio(&self.playlist).await?
        } else {
            self.query().music_radio_playlist(&self.playlist).await?
        };
        let tracks: Vec<_> = radio
            .items
            .into_iter()
            .filter(|item| !history.contains(&item.id))
            .take(25)
            .map(track)
            .collect();
        if let Some(id) = tracks.last().and_then(|t| t["id"].as_str()) {
            self.seed = id.into();
            self.playlist.clear();
        }
        Ok(json!(tracks))
    }

    async fn resolve(&self, id: &str) -> Result<Value> {
        let player = if self.authenticated {
            self.query()
                .player_from_clients(
                    id,
                    &[
                        ClientType::DesktopMusic,
                        ClientType::Desktop,
                        ClientType::Tv,
                    ],
                )
                .await?
        } else {
            self.query().player(id).await?
        };
        ensure!(!player.details.is_live, "Live streams are not supported");
        let audio = best_audio(&player.audio_streams).context("No playable full audio stream")?;
        let transcode = audio.codec != AudioCodec::Mp4a;
        let codec = match audio.codec {
            AudioCodec::Mp4a => "AAC",
            AudioCodec::Opus => "Opus",
            _ => "Audio",
        };
        Ok(json!({"url":audio.url,"hint":"m4a","transcode":transcode,
            "quality":format!("YouTube Music · {codec} {} kbps",audio.average_bitrate.max(audio.bitrate)/1000)}))
    }
}

fn track(item: TrackItem) -> Value {
    let cover = item.cover.iter().max_by_key(|t| t.width).map(|t| &t.url);
    json!({"id":item.id,"title":item.name,"artist":item.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "),"duration":item.duration,"cover_url":cover})
}

fn rpc_response(result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({"result":value}),
        Err(error) => {
            use rustypipe::error::{Error, ExtractionError};
            let reason = match error.downcast_ref::<Error>() {
                Some(Error::Http(_)) => "network request failed".into(),
                Some(Error::HttpStatus(status, _)) => format!("HTTP {status}"),
                Some(Error::Auth(_)) => "authentication failed; refresh cookies".into(),
                Some(Error::Extraction(ExtractionError::Unavailable { reason, .. })) => {
                    format!("content unavailable: {reason}")
                }
                Some(Error::Extraction(ExtractionError::Botguard(_))) => {
                    "Botguard failed; check rustypipe-botguard availability".into()
                }
                Some(Error::Extraction(ExtractionError::Deobfuscation(_))) => {
                    "stream signature extraction failed".into()
                }
                Some(Error::Extraction(ExtractionError::InvalidData(_))) => {
                    "unexpected service response".into()
                }
                _ => "request failed; check cookies, seed, network, and rustypipe-botguard availability".into(),
            };
            json!({"error":format!("YouTube Music: {reason}")})
        }
    }
}

fn main() -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::umask(0o077);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut service = None;
    for line in stdin.lock().lines() {
        let response = (|| -> Result<Value> {
            let request: Value = serde_json::from_str(&line?).context("Invalid adapter request")?;
            let method = request["method"].as_str().context("Missing method")?;
            ensure!(
                matches!(method, "start" | "more" | "resolve"),
                "Unknown adapter method"
            );
            if service.is_none() || method == "start" {
                let path = PathBuf::from(request["profile"].as_str().context("Missing profile")?);
                service = Some(runtime.block_on(Service::new(&path, request["seed"].as_str()))?);
            }
            let service = service.as_mut().unwrap();
            let value = if method == "resolve" {
                runtime.block_on(
                    service.resolve(
                        request["track"]["id"]
                            .as_str()
                            .context("Missing track ID")?,
                    ),
                )?
            } else {
                let history = request["history"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                runtime.block_on(service.wave(&history))?
            };
            Ok(value)
        })();
        let response = rpc_response(response);
        serde_json::to_writer(&mut stdout, &response)?;
        writeln!(stdout)?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_seed_works_without_a_profile_and_preserves_saved_auth_and_defaults() {
        let directory = std::env::temp_dir().join(format!(
            "plavno-youtube-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        plavno_platform::private_directory(&directory).unwrap();
        let path = directory.join("ytmusic.json");
        assert_eq!(
            connection_profile(&path, Some("override")).unwrap(),
            json!({"seed":"override"})
        );
        assert!(!path.exists());
        assert!(connection_profile(&path, None).is_err());
        let saved =
            br#"{"seed":"saved","playlist":"RDmix","cookies":"cookies.txt","botguard":"botguard"}"#;
        use std::io::Write;
        plavno_platform::private_options()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap()
            .write_all(saved)
            .unwrap();
        let mut expected: Value = serde_json::from_slice(saved).unwrap();
        assert_eq!(connection_profile(&path, None).unwrap(), expected);
        expected["seed"] = json!("override");
        assert_eq!(
            connection_profile(&path, Some("override")).unwrap(),
            expected
        );
        assert_eq!(fs::read(&path).unwrap(), saved);
        fs::write(&path, b"null").unwrap();
        assert!(connection_profile(&path, Some("override")).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rpc_response_matches_the_core_protocol_and_keeps_errors_private() {
        let tracks = json!([{"id":"video123","title":"Track","artist":"Artist"}]);
        let encoded = serde_json::to_string(&rpc_response(Ok(tracks.clone()))).unwrap();
        let received: Value = serde_json::from_str(&encoded).unwrap();
        let decoded: Vec<Value> = serde_json::from_value(received["result"].clone()).unwrap();
        assert_eq!(json!(decoded), tracks);
        let error = rpc_response(Err(anyhow::anyhow!("https://cdn.example/?secret=token")));
        assert!(error["error"].as_str().unwrap().contains("YouTube Music"));
        assert!(!error.to_string().contains("secret"));
        let error = rpc_response(Err(rustypipe::error::Error::HttpStatus(
            403,
            "https://cdn.example/?secret=token".into(),
        )
        .into()));
        assert_eq!(error["error"], "YouTube Music: HTTP 403");
    }

    #[test]
    fn seeds_and_recommended_mix_ids_are_preserved() {
        assert_eq!(
            seed_id("https://music.youtube.com/watch?v=video123&list=RDmix").unwrap(),
            "video123"
        );
        assert_eq!(seed_id("https://youtu.be/video123").unwrap(), "video123");
        assert_eq!(
            find_mix(
                &json!({"contents":[{"playlistId":"ordinary"},{"nested":{"playlistId":"RDmix"}}]})
            ),
            Some("RDmix".into())
        );
    }

    #[test]
    fn highest_bitrate_full_stream_is_selected_and_drm_is_excluded() {
        let make = |codec: &str, rate: u32| {
            serde_json::from_value::<AudioStream>(json!({
                "url":"https://cdn.example/audio","itag":1,"bitrate":rate,"average_bitrate":rate,
                "size":1000,"mime":"audio/mp4","format":"m4a","codec":codec,"drm_systems":[]
            }))
            .unwrap()
        };
        let mut protected = make("mp4a", 512000);
        protected.drm_track_type = Some(rustypipe::model::DrmTrackType::Audio);
        let streams = [
            make("opus", 160000),
            make("mp4a", 256000),
            make("mp4a", 128000),
            protected,
        ];
        assert_eq!(best_audio(&streams).unwrap().average_bitrate, 256000);
    }
}
