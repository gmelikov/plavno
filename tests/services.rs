use std::{fs, path::PathBuf, process::Command, time::SystemTime};

struct ConfigDir(PathBuf);

impl Drop for ConfigDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn sources_lists_yandex_with_and_without_a_connected_account() {
    let suffix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let config = ConfigDir(
        std::env::temp_dir().join(format!("plavno-services-{}-{suffix}", std::process::id())),
    );
    fs::create_dir(&config.0).unwrap();
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
            ["yandex", "soundcloud", "spotify", "vk", "ytmusic", "mixed"]
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
