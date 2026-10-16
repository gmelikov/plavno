use anyhow::{Context, Result, ensure};
use std::{env, fs, io::Write, path::PathBuf};

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
    backends_with_overrides(None, None)
}

pub fn backends_with_overrides(
    directory: Option<PathBuf>,
    seed: Option<(&str, &str)>,
) -> Result<Registry> {
    use crate::backend::{Backend, Bridge, Service, SoundCloud, Vk, YouTube};
    if let Some((source, seed)) = seed {
        ensure!(
            matches!(source, "ytmusic" | "soundcloud" | "spotify"),
            "--seed requires --source ytmusic, soundcloud or spotify"
        );
        ensure!(!seed.trim().is_empty(), "--seed requires a track ID or URL");
    }
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
                allow_repeats: false,
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
            allow_repeats: false,
        }),
    }
    for (key, name) in JSON_PROVIDERS {
        let path = profile_path(key)?;
        let seed = seed
            .filter(|(source, _)| *source == key)
            .map(|(_, seed)| seed);
        let ready = plavno_platform::private_file(&path).unwrap_or(false)
            || (seed.is_some() && matches!(key, "ytmusic" | "soundcloud"));
        services.push(Service {
            key,
            name,
            available: ready,
            favorites: key == "vk",
            favorites_shuffle: key == "vk",
            wave_styles: key == "vk",
            allow_repeats: false,
            status: if seed.is_some() && ready {
                "Session radio override".into()
            } else if ready {
                if key == "spotify" {
                    "Configured · native adapter required".into()
                } else if key == "ytmusic" {
                    "Configured · yt-dlp required".into()
                } else {
                    "Configured · native backend".into()
                }
            } else {
                format!("Connect with: plavno login {key}")
            },
        });
        if ready {
            backends.push(match key {
                "soundcloud" => Box::new(SoundCloud::new(path).with_seed(seed)),
                "vk" => Box::new(Vk::new(path)),
                "ytmusic" => Box::new(YouTube::new(path).with_seed(seed)),
                _ => Box::new(Bridge::new(key, name, path).with_seed(seed)),
            });
        }
    }
    let local = match directory {
        Some(path) => Ok(Some(validate_local_directory(&path)?)),
        None => local_directory(),
    };
    let (available, status) = match local {
        Ok(Some(path)) if path.is_dir() => {
            let status = format!("{} · recursive shuffle", path.display());
            backends.push(Box::new(crate::backend::LocalDirectory::new(path)?));
            (true, status)
        }
        Ok(Some(path)) => (false, format!("Directory unavailable: {}", path.display())),
        Ok(None) => (false, "Configure with: plavno local DIRECTORY".into()),
        Err(error) => (false, format!("Local settings unavailable: {error:#}")),
    };
    services.push(Service {
        key: "local",
        name: "Local music",
        status,
        available,
        favorites: false,
        favorites_shuffle: false,
        wave_styles: false,
        allow_repeats: true,
    });
    Ok((backends, services))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LocalSettings {
    directory: PathBuf,
}

fn local_directory() -> Result<Option<PathBuf>> {
    let path = token_path()?.with_file_name("local.json");
    if !path.try_exists()? {
        return Ok(None);
    }
    let settings: LocalSettings = serde_json::from_str(&crate::backend::web::private(&path)?)
        .context("Invalid local directory settings")?;
    Ok(Some(settings.directory))
}

fn validate_local_directory(path: &std::path::Path) -> Result<PathBuf> {
    let path = path.canonicalize().context("Local directory not found")?;
    ensure!(path.is_dir(), "Local music path must be a directory");
    Ok(path)
}

pub fn save_local_directory(path: &std::path::Path) -> Result<PathBuf> {
    let directory = validate_local_directory(path)?;
    let settings = LocalSettings {
        directory: directory.clone(),
    };
    save_private(
        &token_path()?.with_file_name("local.json"),
        &serde_json::to_vec_pretty(&settings)?,
    )?;
    Ok(directory)
}

pub fn yandex_with_token(token: String) -> crate::backend::Yandex {
    let directory = plavno_platform::cache_home().ok();
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
    plavno_platform::private_directory(parent)?;
    let temp = parent.join(format!(".profile-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = plavno_platform::private_options()
            .write(true)
            .create_new(true)
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
    let base = plavno_platform::config_home()?;
    Ok(base.join("plavno/yandex.token"))
}

pub fn load() -> Result<String> {
    if let Ok(token) = env::var("YANDEX_MUSIC_TOKEN") {
        ensure!(!token.trim().is_empty(), "YANDEX_MUSIC_TOKEN is empty");
        return Ok(token.trim().into());
    }
    let path = token_path()?;
    ensure!(
        plavno_platform::private_file(&path).context("To sign in, run: plavno login yandex")?,
        "The token file must be private and must not be a symlink"
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
    save_private(&path, token.trim().as_bytes())?;
    println!("Token saved to {}", path.display());
    Ok(())
}
