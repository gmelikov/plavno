use anyhow::{Context, Result, ensure};
use std::{
    env, fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

pub const JSON_PROVIDERS: [(&str, &str); 4] = [
    ("soundcloud", "SoundCloud"),
    ("spotify", "Spotify"),
    ("vk", "VK Music"),
    ("ytmusic", "YouTube Music"),
];

pub fn profile_path(source: &str) -> Result<PathBuf> {
    ensure!(
        JSON_PROVIDERS.iter().any(|(key, _)| *key == source),
        "Unknown source"
    );
    Ok(token_path()?.with_file_name(format!("{source}.json")))
}

type Registry = (
    Vec<Box<dyn crate::backend::Backend>>,
    Vec<crate::backend::Service>,
);

pub fn backends() -> Result<Registry> {
    use crate::backend::{Backend, Bridge, Service, SoundCloud, Vk};
    let mut backends: Vec<Box<dyn Backend>> = Vec::new();
    let mut services = Vec::new();
    match load() {
        Ok(token) => {
            backends.push(Box::new(yandex_with_token(token)));
            services.push(Service {
                key: "yandex",
                name: "Yandex Music",
                status: "Ready".into(),
                available: true,
                favorites: true,
                favorites_shuffle: true,
                wave_styles: true,
            });
        }
        Err(_) => services.push(Service {
            key: "yandex",
            name: "Yandex Music",
            status: "Connect with: plavno login yandex".into(),
            available: false,
            favorites: true,
            favorites_shuffle: true,
            wave_styles: true,
        }),
    }
    for (key, name) in JSON_PROVIDERS {
        let path = profile_path(key)?;
        let ready = fs::symlink_metadata(&path)
            .is_ok_and(|meta| meta.file_type().is_file() && meta.permissions().mode() & 0o077 == 0);
        services.push(Service {
            key,
            name,
            available: ready,
            favorites: key == "vk",
            favorites_shuffle: key == "vk",
            wave_styles: key == "vk",
            status: if ready {
                if matches!(key, "spotify" | "ytmusic") {
                    "Configured · native adapter required".into()
                } else {
                    "Configured · native backend".into()
                }
            } else {
                format!("Connect with: plavno login {key}")
            },
        });
        if ready {
            backends.push(match key {
                "soundcloud" => Box::new(SoundCloud::new(path)),
                "vk" => Box::new(Vk::new(path)),
                _ => Box::new(Bridge::new(key, name, path)),
            });
        }
    }
    Ok((backends, services))
}

pub fn yandex_with_token(token: String) -> crate::backend::Yandex {
    let directory = env::var_os("XDG_CACHE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    match directory {
        Some(directory) => {
            crate::backend::Yandex::with_cache(token, directory.join("plavno/yandex-streams"))
        }
        None => crate::backend::Yandex::new(token),
    }
}

pub fn login_provider(source: &str) -> Result<()> {
    if matches!(source, "soundcloud" | "vk") {
        return crate::browser_login::login(source);
    }
    login_manual(source)
}

pub fn login_manual(source: &str) -> Result<()> {
    let path = profile_path(source)?;
    println!("Enter a JSON connection profile. See README for fields and optional dependencies.");
    let value = rpassword::prompt_password("Connection profile (hidden input): ")?;
    let profile: serde_json::Value =
        serde_json::from_str(&value).map_err(|_| anyhow::anyhow!("Invalid JSON profile"))?;
    ensure!(
        profile.as_object().is_some_and(|obj| !obj.is_empty()),
        "The profile must be a non-empty JSON object"
    );
    save_private(&path, serde_json::to_vec(&profile)?.as_slice())?;
    println!("Profile saved to {}", path.display());
    Ok(())
}

pub(crate) fn save_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing configuration directory")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temp = parent.join(format!(".profile-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn token_path() -> Result<PathBuf> {
    let base = match env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("plavno/yandex.token"))
}

pub fn load() -> Result<String> {
    if let Ok(token) = env::var("YANDEX_MUSIC_TOKEN") {
        ensure!(!token.trim().is_empty(), "YANDEX_MUSIC_TOKEN is empty");
        return Ok(token.trim().into());
    }
    let path = token_path()?;
    let meta = fs::symlink_metadata(&path).context("To sign in, run: plavno login yandex")?;
    ensure!(
        meta.file_type().is_file() && meta.permissions().mode() & 0o077 == 0,
        "The token file must have permissions 0600 and must not be a symlink"
    );
    let token = fs::read_to_string(path)?;
    ensure!(
        !token.trim().is_empty(),
        "The token is empty; run plavno login yandex"
    );
    Ok(token.trim().into())
}

pub fn wave_styles() -> Result<std::collections::HashMap<String, crate::backend::WaveStyle>> {
    let path = token_path()?.with_file_name("wave-styles.json");
    if !path.exists() {
        return Ok(Default::default());
    }
    serde_json::from_str(&crate::backend::web::private(&path)?)
        .context("Invalid wave style settings")
}

pub fn save_wave_styles(
    styles: &std::collections::HashMap<String, crate::backend::WaveStyle>,
) -> Result<()> {
    let path = token_path()?.with_file_name("wave-styles.json");
    save_private(&path, &serde_json::to_vec_pretty(styles)?)
}

pub fn login() -> Result<()> {
    let token = rpassword::prompt_password("Yandex Music OAuth token (hidden input): ")?;
    ensure!(!token.trim().is_empty(), "The token is empty");
    let path = token_path()?;
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temp = parent.join(format!(".token-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(token.trim().as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result?;
    println!("Token saved to {}", path.display());
    Ok(())
}
