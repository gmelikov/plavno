#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::PathBuf, process::Command, time::SystemTime};

struct ConfigDir(PathBuf);

impl ConfigDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "plavno-services-{}-{suffix}-{index}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_plavno"));
        command
            .env("XDG_CONFIG_HOME", &self.0)
            .env_remove("YANDEX_MUSIC_TOKEN");
        command
    }
}

impl Drop for ConfigDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn version_is_available_without_an_account_or_terminal() {
    let config = ConfigDir::new();
    for flag in ["--version", "-V"] {
        let output = config.command().arg(flag).output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("plavno {}\n", env!("CARGO_PKG_VERSION"))
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn sources_lists_yandex_with_and_without_a_connected_account() {
    let config = ConfigDir::new();
    for connected in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_plavno"));
        command
            .arg("sources")
            .env("XDG_CONFIG_HOME", &config.0)
            .env_remove("YANDEX_MUSIC_TOKEN");
        if connected {
            command.env("YANDEX_MUSIC_TOKEN", "fixture-token");
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let keys: Vec<_> = text
            .lines()
            .map(|line| line.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "yandex",
                "soundcloud",
                "spotify",
                "vk",
                "ytmusic",
                "local",
                "mixed"
            ]
        );
        let yandex = text.lines().next().unwrap();
        assert!(yandex.contains("Yandex Music"));
        assert!(yandex.contains(if connected {
            "Ready"
        } else {
            "Connect with: plavno login yandex"
        }));
    }
}

#[test]
fn local_directory_is_saved_privately_listed_and_can_be_overridden_without_changing_settings() {
    let config = ConfigDir::new();
    let music = config.0.join("music with spaces");
    let other = config.0.join("other");
    fs::create_dir(&music).unwrap();
    fs::create_dir(&other).unwrap();
    let output = config.command().arg("local").arg(&music).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let settings = config.0.join("plavno/local.json");
    let saved = fs::read(&settings).unwrap();
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let value: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(
        value["directory"].as_str().unwrap(),
        music.to_str().unwrap()
    );
    let sources = config.command().arg("sources").output().unwrap();
    assert!(sources.status.success());
    let sources = String::from_utf8(sources.stdout).unwrap();
    assert!(sources.lines().any(|line| line.starts_with("local ")
        && line.contains(music.to_str().unwrap())
        && line.contains("recursive shuffle")));

    for source in ["local", "mixed"] {
        let output = config
            .command()
            .args(["--source", source, "--directory"])
            .arg(&other)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        if source == "local" {
            assert!(error.contains("interactive terminal"), "{error}");
        } else {
            assert!(error.contains("not connected"), "{error}");
        }
        assert_eq!(fs::read(&settings).unwrap(), saved);
    }
    let output = config
        .command()
        .arg("--directory")
        .arg(&other)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    fs::remove_dir(&music).unwrap();
    let output = config.command().arg("sources").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Directory unavailable"));
}

#[test]
fn invalid_local_configuration_and_arguments_do_not_break_other_sources_or_overwrite_settings() {
    let config = ConfigDir::new();
    assert!(
        !config
            .command()
            .arg("local")
            .arg(config.0.join("missing"))
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!config.0.join("plavno/local.json").exists());
    assert!(
        !config
            .command()
            .arg("--directory")
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = config
        .command()
        .args(["--file", "track.flac", "--directory", "."])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be combined"));
    config
        .command()
        .arg("local")
        .arg(&config.0)
        .output()
        .unwrap();
    fs::write(config.0.join("plavno/local.json"), b"{}").unwrap();
    let output = config
        .command()
        .arg("sources")
        .env("YANDEX_MUSIC_TOKEN", "fixture-token")
        .output()
        .unwrap();
    assert!(output.status.success());
    let sources = String::from_utf8(output.stdout).unwrap();
    assert!(sources.contains("Yandex Music     Ready"));
    assert!(sources.contains("Invalid local directory settings"));
}
