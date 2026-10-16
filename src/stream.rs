use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fmt,
    io::{self, BufRead, Read, Write},
    process::{Command, Stdio},
};

const ERROR_PREFIX: &str = "PLAVNO_STREAM_ERROR=";

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    MissingFfmpeg,
    Http(u16),
    Timeout,
    Tls,
    InvalidAudio,
    Failed,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFfmpeg => f.write_str("ffmpeg not found; install it or set PLAVNO_FFMPEG"),
            Self::Http(status) => write!(f, "Audio CDN returned HTTP {status}"),
            Self::Timeout => f.write_str("Audio download timed out"),
            Self::Tls => f.write_str("Audio download failed: TLS connection error"),
            Self::InvalidAudio => f.write_str("ffmpeg could not recognize the audio stream"),
            Self::Failed => {
                f.write_str("Audio conversion failed; check stream access and ffmpeg support")
            }
        }
    }
}

impl std::error::Error for Failure {}

fn read_diagnostics(mut reader: impl Read) -> String {
    let mut bytes = Vec::new();
    let _ = reader.by_ref().take(64 * 1024).read_to_end(&mut bytes);
    let _ = io::copy(&mut reader, &mut io::sink());
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn read_failure(reader: impl Read) -> Option<Failure> {
    read_diagnostics(reader).lines().find_map(|line| {
        let failure: Failure = serde_json::from_str(line.strip_prefix(ERROR_PREFIX)?).ok()?;
        match failure {
            Failure::Http(status) if !(400..600).contains(&status) => None,
            _ => Some(failure),
        }
    })
}

fn conversion_failure(stderr: &str) -> Failure {
    let status =
        regex_lite::Regex::new(r"(?i)(?:HTTP error |server returned )([45][0-9]{2})").unwrap();
    if let Some(code) = status.captures(stderr).and_then(|c| c[1].parse().ok()) {
        return Failure::Http(code);
    }
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("timed out") {
        Failure::Timeout
    } else if lower.contains("[tls ")
        || lower.contains("tls handshake")
        || lower.contains("certificate")
    {
        Failure::Tls
    } else if lower.contains("invalid data found") || lower.contains("could not find codec") {
        Failure::InvalidAudio
    } else {
        Failure::Failed
    }
}

fn playlist(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).context("Invalid stream URL")?;
    ensure!(
        parsed.scheme() == "https"
            || parsed.scheme() == "http"
                && parsed
                    .host_str()
                    .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "[::1]")),
        "Insecure stream URL"
    );
    ensure!(!url.contains(['\n', '\r', '\0']), "Invalid stream URL");
    let escaped = parsed.as_str().replace('\'', "'\\''");
    Ok(format!(
        "ffconcat version 1.0\nfile '{escaped}'\noption rw_timeout 10000000\noption user_agent '{}'\n",
        crate::backend::web::USER_AGENT
    ))
}

pub fn run() -> Result<()> {
    let result = convert();
    if let Err(error) = &result {
        let failure = error
            .downcast_ref::<Failure>()
            .copied()
            .unwrap_or(Failure::Failed);
        eprintln!("{ERROR_PREFIX}{}", serde_json::to_string(&failure)?);
    }
    result
}

fn convert() -> Result<()> {
    let mut line = String::new();
    io::stdin().lock().take(1024 * 1024).read_line(&mut line)?;
    let request: Value =
        serde_json::from_str(&line).map_err(|_| anyhow::anyhow!("Invalid stream request"))?;
    let list = playlist(request["url"].as_str().context("Missing stream URL")?)?;
    let program = std::env::var_os("PLAVNO_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let mut child = Command::new(program)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-protocol_whitelist",
            "pipe,http,https,tcp,tls,crypto,data",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            "pipe:0",
            "-vn",
            "-c:a",
            "flac",
            "-flush_packets",
            "1",
            "-f",
            "flac",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| Failure::MissingFfmpeg)?;
    let stderr = child.stderr.take().unwrap();
    let diagnostics = std::thread::spawn(move || conversion_failure(&read_diagnostics(stderr)));
    if let Err(error) = child.stdin.take().unwrap().write_all(list.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = diagnostics.join();
        return Err(error).context("Cannot start audio conversion");
    }
    let status = child.wait()?;
    let failure = diagnostics.join().unwrap_or(Failure::Failed);
    if !status.success() {
        return Err(failure.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_errors_keep_the_cause_and_never_forward_signed_urls() {
        for (raw, expected) in [
            (
                "[https] HTTP error 403 Forbidden https://cdn.example/?secret=token",
                "Audio CDN returned HTTP 403",
            ),
            (
                "Server returned 429 Too Many Requests",
                "Audio CDN returned HTTP 429",
            ),
            (
                "Error opening input: Connection timed out",
                "Audio download timed out",
            ),
            (
                "[tls @ 0x123] Error in the pull function",
                "Audio download failed: TLS connection error",
            ),
            (
                "Invalid data found when processing input",
                "ffmpeg could not recognize the audio stream",
            ),
        ] {
            let failure = conversion_failure(raw);
            assert_eq!(failure.to_string(), expected);
            let encoded = format!(
                "{ERROR_PREFIX}{}\n",
                serde_json::to_string(&failure).unwrap()
            );
            assert_eq!(
                read_failure(encoded.as_bytes()).unwrap().to_string(),
                expected
            );
            assert!(!encoded.contains("secret"));
        }
        assert!(
            read_failure(b"raw private error https://cdn.example/?secret=token".as_slice())
                .is_none()
        );
        assert!(read_failure(b"PLAVNO_STREAM_ERROR={\"http\":200}".as_slice()).is_none());
        assert!(read_failure(b"PLAVNO_STREAM_ERROR=\"secret-token\"".as_slice()).is_none());
    }

    #[test]
    fn signed_urls_are_passed_in_stdin_without_playlist_injection_or_local_file_access() {
        let value = playlist("https://cdn.example/audio?token=secret&name=O'Reilly").unwrap();
        assert!(value.contains("O%27Reilly"));
        assert_eq!(value.lines().count(), 4);
        assert!(playlist("https://cdn.example/audio\nfile '/private/file'").is_err());
        assert!(playlist("file:///private/file").is_err());
        assert!(playlist("http://cdn.example/audio").is_err());
        assert!(playlist("http://127.0.0.1:1234/audio.mp3").is_ok());
    }
}
