use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    io::{self, BufRead, Read, Write},
    process::{Command, Stdio},
};

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
        .stderr(Stdio::null())
        .spawn()
        .context("Missing stream dependency: install ffmpeg")?;
    if let Err(error) = child.stdin.take().unwrap().write_all(list.as_bytes()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("Cannot start audio conversion");
    }
    ensure!(
        child.wait()?.success(),
        "Audio stream failed; check stream access and ffmpeg support"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
