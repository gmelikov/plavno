use super::{Backend, Feedback, Input, Track};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Bridge {
    key: &'static str,
    name: &'static str,
    profile: PathBuf,
    seed: Option<String>,
    worker: Option<Worker>,
}

impl Bridge {
    pub fn new(key: &'static str, name: &'static str, profile: PathBuf) -> Self {
        Self {
            key,
            name,
            profile,
            seed: None,
            worker: None,
        }
    }

    pub fn with_seed(mut self, seed: Option<&str>) -> Self {
        self.seed = seed.map(str::to_owned);
        self
    }

    fn command(&self) -> (PathBuf, Vec<String>) {
        if self.key == "spotify" {
            (
                std::env::var_os("PLAVNO_SPOTIFY_HELPER")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("plavno-spotify")),
                vec![],
            )
        } else {
            (
                std::env::var_os("PLAVNO_YOUTUBE_HELPER")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("plavno-youtube")),
                vec![],
            )
        }
    }

    fn call(&mut self, method: &str, track: Option<&Track>, history: &[String]) -> Result<Value> {
        if self.worker.is_none() {
            let (program, args) = self.command();
            let mut child = Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| {
                    format!(
                        "{} adapter is unavailable; see README setup instructions",
                        self.name
                    )
                })?;
            self.worker = Some(Worker {
                stdin: child.stdin.take().unwrap(),
                stdout: BufReader::new(child.stdout.take().unwrap()),
                child,
            });
        }
        let request = json!({"source":self.key,"profile":self.profile,"seed":self.seed,"method":method,"track":track,"history":history});
        let result = (|| {
            let worker = self.worker.as_mut().unwrap();
            serde_json::to_writer(&mut worker.stdin, &request)?;
            worker.stdin.write_all(b"\n")?;
            worker.stdin.flush()?;
            ensure!(
                plavno_platform::wait_readable(
                    worker.stdout.get_ref(),
                    std::time::Duration::from_secs(65)
                )?,
                "The service adapter did not respond within 65 seconds"
            );
            let mut line = String::new();
            ensure!(
                worker
                    .stdout
                    .by_ref()
                    .take(8 * 1024 * 1024)
                    .read_line(&mut line)?
                    > 0,
                "The service adapter stopped; check its dependencies and credentials"
            );
            let response: Value =
                serde_json::from_str(&line).context("Invalid adapter response")?;
            if let Some(error) = response["error"].as_str() {
                anyhow::bail!("{error}");
            }
            Ok(response["result"].clone())
        })();
        if result.is_err() {
            self.worker = None;
        }
        result
    }
}

impl Backend for Bridge {
    fn suspend(&mut self) {
        self.worker = None;
    }
    fn key(&self) -> &'static str {
        self.key
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn start(&mut self) -> Result<Vec<Track>> {
        self.worker = None;
        serde_json::from_value(self.call("start", None, &[])?).context("Invalid wave tracks")
    }
    fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
        serde_json::from_value(self.call("more", None, history)?).context("Invalid wave tracks")
    }
    fn resolve(&mut self, track: &Track) -> Result<Input> {
        let value = self.call("resolve", Some(track), &[])?;
        let url = value["url"]
            .as_str()
            .context("The service did not provide a full audio stream")?
            .to_owned();
        ensure!(url.starts_with("https://"), "Insecure service stream URL");
        if self.key == "spotify" {
            let bytes = value["key"]
                .as_array()
                .context("Missing Spotify audio key")?;
            let key: Vec<u8> = bytes
                .iter()
                .map(|b| {
                    b.as_u64()
                        .filter(|b| *b < 256)
                        .map(|b| b as u8)
                        .context("Invalid audio key")
                })
                .collect::<Result<_>>()?;
            let key: [u8; 16] = key
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid audio key length"))?;
            let hint = match value["hint"].as_str() {
                Some("flac") => "flac",
                Some("mp3") => "mp3",
                _ => "ogg",
            };
            Ok(Input::Stream {
                url,
                key: Some(key),
                iv: [
                    0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f,
                    0x63, 0x0d, 0x93,
                ],
                skip: value["skip"].as_u64().unwrap_or(0),
                hint,
                quality: value["quality"].as_str().unwrap_or("Spotify audio").into(),
            })
        } else {
            let quality = value["quality"]
                .as_str()
                .unwrap_or("YouTube Music audio")
                .to_owned();
            if value["transcode"].as_bool() == Some(true) {
                return super::web::transcode(url, quality);
            }
            Ok(Input::Http {
                url,
                aes128_key: None,
                hint: "m4a",
                quality,
            })
        }
    }
    fn feedback(&mut self, _: &Track, _: Feedback, _: f64) -> Result<()> {
        Ok(())
    }
}
