use anyhow::{Context, Result, ensure};
use librespot_core::{
    Session, SpotifyId, SpotifyUri, authentication::Credentials, cache::Cache,
    config::SessionConfig,
};
use librespot_metadata::audio::{AudioItem, UniqueFields, file::AudioFileFormat};
use librespot_protocol::storage_resolve::StorageResolveResponse;
use protobuf::Message;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, Write},
    path::Path,
    time::Duration,
};

struct Spotify {
    session: Session,
    seed: String,
}

fn private(path: &Path) -> Result<()> {
    ensure!(
        plavno_platform::private_file(path)?,
        "Spotify credentials must be regular files with permissions 0600"
    );
    Ok(())
}

impl Spotify {
    async fn connect(path: &str) -> Result<Self> {
        let path = Path::new(path);
        private(path)?;
        let profile: Value = serde_json::from_slice(&fs::read(path)?)?;
        let seed = profile["seed"]
            .as_str()
            .context("Spotify needs a seed track URI; run plavno login spotify")?
            .to_owned();
        ensure!(
            matches!(SpotifyUri::from_uri(&seed)?, SpotifyUri::Track { .. }),
            "Spotify seed must be a spotify:track: URI"
        );
        let directory = path
            .parent()
            .context("Missing profile directory")?
            .join("spotify-session");
        if directory.exists() {
            ensure!(
                fs::symlink_metadata(&directory)?.file_type().is_dir(),
                "Invalid Spotify session directory"
            );
        }
        fs::create_dir_all(&directory)?;
        plavno_platform::private_directory(&directory)?;
        let credentials_path = directory.join("credentials.json");
        if fs::symlink_metadata(&credentials_path).is_ok() {
            private(&credentials_path)?;
        }
        let cache = Cache::new(Some(&directory), None, None, None)?;
        let credentials = if let Some(credentials) = cache.credentials() {
            credentials
        } else {
            Credentials::with_access_token(
                profile["token"]
                    .as_str()
                    .context("Spotify needs an access token with the streaming scope")?,
            )
        };
        let mut config = SessionConfig::default();
        if let Some(client) = profile["client_id"].as_str() {
            config.client_id = client.into();
        }
        let session = Session::new(config, Some(cache));
        session.connect(credentials, true).await?;
        ensure!(
            session
                .user_data()
                .attributes
                .get("type")
                .is_none_or(|kind| kind == "premium"),
            "Spotify streaming requires Premium"
        );
        Ok(Self { session, seed })
    }

    async fn wave(&mut self, history: &[String]) -> Result<Value> {
        let previous: Vec<_> = history
            .iter()
            .filter_map(|s| SpotifyUri::from_uri(s).ok())
            .filter_map(|uri| {
                if let SpotifyUri::Track { id } = uri {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        let data = self
            .session
            .spclient()
            .get_apollo_station("tracks", &self.seed, Some(20), previous, true)
            .await?;
        let value: Value = serde_json::from_slice(&data)?;
        let mut ids = Vec::new();
        collect_tracks(&value, &mut ids);
        let mut seen: HashSet<String> = history.iter().cloned().collect();
        let mut results = Vec::new();
        for uri in ids
            .into_iter()
            .filter(|uri| seen.insert(uri.clone()))
            .take(20)
        {
            let Ok(item) = AudioItem::get_file(&self.session, SpotifyUri::from_uri(&uri)?).await
            else {
                continue;
            };
            if item.availability.is_err() {
                continue;
            }
            let artist = match &item.unique_fields {
                UniqueFields::Track { artists, .. } => artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => String::new(),
            };
            results.push(json!({"id":item.uri,"title":item.name,"artist":artist,"duration":f64::from(item.duration_ms)/1000.,"cover_url":item.covers.first().map(|c| c.url.as_str())}));
        }
        ensure!(
            !results.is_empty(),
            "Spotify returned no playable radio tracks; try another seed"
        );
        if let Some(uri) = results.last().and_then(|t| t["id"].as_str()) {
            self.seed = uri.into();
        }
        Ok(Value::Array(results))
    }

    async fn resolve(&self, uri: &str) -> Result<Value> {
        let uri = SpotifyUri::from_uri(uri)?;
        let SpotifyUri::Track { id } = uri else {
            anyhow::bail!("Only Spotify music tracks are supported");
        };
        let item = AudioItem::get_file(&self.session, uri).await?;
        ensure!(
            item.availability.is_ok(),
            "This Spotify track is unavailable for this account"
        );
        let (format, file) = [
            AudioFileFormat::FLAC_FLAC_24BIT,
            AudioFileFormat::FLAC_FLAC,
            AudioFileFormat::OGG_VORBIS_320,
            AudioFileFormat::MP3_320,
            AudioFileFormat::OGG_VORBIS_160,
            AudioFileFormat::MP3_160,
            AudioFileFormat::OGG_VORBIS_96,
        ]
        .into_iter()
        .find_map(|format| item.files.get(&format).map(|file| (format, *file)))
        .context("No supported full audio format is available")?;
        let key = self.session.audio_key().request(id, file).await?;
        let storage = self.session.spclient().get_audio_storage(&file).await?;
        let storage = StorageResolveResponse::parse_from_bytes(&storage)?;
        let url = storage
            .cdnurl
            .first()
            .context("Spotify did not provide an audio CDN URL")?;
        let (hint, skip, quality) = match format {
            AudioFileFormat::FLAC_FLAC_24BIT => ("flac", 0, "Spotify FLAC 24-bit"),
            AudioFileFormat::FLAC_FLAC => ("flac", 0, "Spotify FLAC"),
            AudioFileFormat::OGG_VORBIS_320 => ("ogg", 167, "Spotify Vorbis 320 kbps"),
            AudioFileFormat::OGG_VORBIS_160 => ("ogg", 167, "Spotify Vorbis 160 kbps"),
            AudioFileFormat::OGG_VORBIS_96 => ("ogg", 167, "Spotify Vorbis 96 kbps"),
            AudioFileFormat::MP3_320 => ("mp3", 0, "Spotify MP3 320 kbps"),
            _ => ("mp3", 0, "Spotify MP3 160 kbps"),
        };
        Ok(json!({"url":url,"key":key.0,"hint":hint,"skip":skip,"quality":quality}))
    }
}

fn collect_tracks(value: &Value, ids: &mut Vec<String>) {
    match value {
        Value::String(uri) if uri.starts_with("spotify:track:") => ids.push(uri.clone()),
        Value::Array(values) => {
            for value in values {
                collect_tracks(value, ids);
            }
        }
        Value::Object(values) => {
            if let Some(id) = values
                .get("gid")
                .and_then(Value::as_str)
                .and_then(|id| SpotifyId::from_base16(id).ok())
                && let Ok(id) = id.to_base62()
            {
                ids.push(format!("spotify:track:{id}"));
            }
            for value in values.values() {
                collect_tracks(value, ids);
            }
        }
        _ => {}
    }
}

async fn handle(adapter: &mut Option<Spotify>, request: &Value) -> Result<Value> {
    if adapter.is_none() {
        *adapter = Some(
            Spotify::connect(
                request["profile"]
                    .as_str()
                    .context("Missing profile path")?,
            )
            .await?,
        );
    }
    let spotify = adapter.as_mut().unwrap();
    match request["method"].as_str() {
        Some("start" | "more") => {
            let history: Vec<String> = request["history"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect();
            spotify.wave(&history).await
        }
        Some("resolve") => {
            spotify
                .resolve(
                    request["track"]["id"]
                        .as_str()
                        .context("Missing track ID")?,
                )
                .await
        }
        _ => anyhow::bail!("Unknown Spotify operation"),
    }
}

fn main() {
    #[cfg(unix)]
    unsafe {
        libc::umask(0o077);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Spotify runtime");
    let mut adapter = None;
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let response = (|| -> Result<Value> {
            let request: Value = serde_json::from_str(&line?)?;
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(60), handle(&mut adapter, &request))
                    .await
                    .context("Spotify request timed out")?
            })
        })();
        let response = match response {
            Ok(result) => json!({"result":result}),
            // SDK errors can contain credentials and signed CDN URLs.
            Err(_) => {
                json!({"error":"Spotify request failed; check Premium, credentials with streaming scope, seed URI and network access"})
            }
        };
        if serde_json::to_writer(&mut stdout, &response).is_err()
            || stdout.write_all(b"\n").is_err()
            || stdout.flush().is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn radio_track_ids_are_extracted_from_both_wire_formats() {
        let mut ids = vec![];
        collect_tracks(
            &json!({"tracks":[{"uri":"spotify:track:abc"},{"gid":"00000000000000000000000000000001"}],"label":"other"}),
            &mut ids,
        );
        assert!(ids.contains(&"spotify:track:abc".into()));
        assert!(ids.iter().any(|id| id.ends_with("0000000000000000000001")));
    }
}
