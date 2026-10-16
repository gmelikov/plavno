mod artwork;
mod audio;
mod backend;
mod browser_login;
mod cached_frame;
mod choreography;
mod config;
mod controls;
mod dance_balance;
mod dance_motion;
mod dance_space;
mod dancer;
mod dislikes;
mod engine;
mod flow_view;
mod history;
mod history_view;
mod local_metadata;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg(not(target_os = "linux"))]
#[path = "mpris_stub.rs"]
mod mpris;
mod sixel;
mod source_view;
mod spring;
mod stream;
mod text_selection;
mod tui;
mod visualization;
mod wave;

use anyhow::{Result, bail};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Err(error) = run() {
        eprintln!("plavno: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("plavno {VERSION}");
        return Ok(());
    }
    if args.len() == 1 && args[0] == "--stream-audio" {
        return stream::run();
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "plavno {VERSION} — a lightweight music TUI\n\n  plavno --version              show version\n  plavno                        play the first connected service\n  plavno --source SOURCE        yandex / soundcloud / spotify / vk / ytmusic / local / mixed\n  plavno --favorites            shuffle favorites (default: Yandex; --source vk: My Music)\n  plavno --no-wave              start with visualization hidden\n  plavno --dance                show the wave and dancing silhouette\n  plavno --dance-style STYLE    auto / flow / groove / pulse / house / hiphop / rave\n  plavno --renderer MODE        dancer output: auto / braille / sixel\n  plavno --dance-skin SKIN       sixel appearance: detailed (default) / silhouette\n  plavno --sixel-update MODE    diff (default) / full\n  plavno login [SOURCE]         connect a service (default: yandex; SoundCloud/VK: browser)\n  plavno login yandex           paste a Yandex Music OAuth token\n  plavno login SOURCE --manual  paste a JSON profile without browser automation\n  plavno diagnose yandex       check API access and audio CDN connectivity\n  plavno diagnose vk           inspect the VK response layout (no session values)\n  plavno sources                list service configuration\n  plavno local DIRECTORY        save the local music directory (recursive shuffle)\n  plavno --directory DIRECTORY  use a local directory for this run\n  plavno --file FILE            play a local file\n\nSpace pause · x stop · p/n tracks · ←/→ ±5s · Shift+←/→ ±30s\nClick controls · drag progress · scroll bar to seek, Speed to change rate, elsewhere volume · h or / history\nHistory: type to search · ↑/↓ select · Enter play · Esc back\ns sources · m Wave Shuffle · w restart wave · f Favorites Shuffle\ng Flow settings · v toggle visualization · b wave/dance · l ♥/♡ favorites · d dislike · u undo dislike\n+/- volume · [/] playback speed · q quit"
        );
        return Ok(());
    }
    if args.len() == 1 && args[0] == "login" {
        return config::login();
    }
    if let [command, source] = args.as_slice()
        && command == "login"
    {
        let source = source.to_str().unwrap_or_default();
        return if source == "yandex" {
            config::login()
        } else {
            config::login_provider(source)
        };
    }
    if let [command, source, flag] = args.as_slice()
        && command == "login"
        && flag == "--manual"
    {
        return config::login_manual(source.to_str().unwrap_or_default());
    }
    if let [command, source] = args.as_slice()
        && command == "diagnose"
    {
        let report = match source.to_str() {
            Some("yandex") => config::yandex_with_token(config::load()?).diagnose(),
            Some("vk") => backend::Vk::new(config::profile_path("vk")?).diagnose()?,
            _ => bail!("diagnose requires yandex or vk"),
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if args.len() == 1 && args[0] == "sources" {
        let (backends, services) = config::backends()?;
        let sources = backend::Multi::new(backends, services, None)?;
        for source in backend::Backend::services(&sources) {
            println!("{:<12} {:<16} {}", source.key, source.name, source.status);
        }
        return Ok(());
    }
    if let [command, directory] = args.as_slice()
        && command == "local"
    {
        let directory = config::save_local_directory(std::path::Path::new(directory))?;
        println!(
            "Local music: {}\nPlay with: plavno --source local",
            directory.display()
        );
        return Ok(());
    }
    let mut favorites = false;
    let mut no_wave = false;
    let mut visual_mode = visualization::Mode::Wave;
    let mut dance_style = dance_motion::Style::Auto;
    let mut renderer = visualization::Renderer::Braille;
    let mut sixel_update = sixel::UpdateMode::default();
    let mut skin = dancer::skin::Skin::default();
    let mut selected = None;
    let mut file = None;
    let mut directory = None;
    let mut arguments = args.iter();
    while let Some(arg) = arguments.next() {
        if arg == "--favorites" {
            favorites = true;
        } else if arg == "--no-wave" {
            no_wave = true;
        } else if arg == "--dance" {
            visual_mode = visualization::Mode::Dance;
        } else if arg == "--dance-style" {
            dance_style = arguments
                .next()
                .and_then(|arg| arg.to_str())
                .and_then(dance_motion::Style::parse)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--dance-style requires auto, flow, groove, pulse, house, hiphop, or rave"
                    )
                })?;
            visual_mode = visualization::Mode::Dance;
        } else if arg == "--renderer" {
            renderer = match arguments.next().and_then(|arg| arg.to_str()) {
                Some("auto" | "braille") => visualization::Renderer::Braille,
                Some("sixel") => visualization::Renderer::Sixel,
                _ => bail!("--renderer requires auto, braille, or sixel"),
            };
        } else if arg == "--dance-skin" {
            skin = match arguments.next().and_then(|arg| arg.to_str()) {
                Some("detailed") => dancer::skin::Skin::Detailed,
                Some("silhouette") => dancer::skin::Skin::Silhouette,
                _ => bail!("--dance-skin requires detailed or silhouette"),
            };
        } else if arg == "--sixel-update" {
            sixel_update = match arguments.next().and_then(|arg| arg.to_str()) {
                Some("full") => sixel::UpdateMode::Full,
                Some("diff") => sixel::UpdateMode::Diff,
                _ => bail!("--sixel-update requires full or diff"),
            };
        } else if arg == "--source" {
            selected = Some(
                arguments
                    .next()
                    .and_then(|a| a.to_str())
                    .ok_or_else(|| anyhow::anyhow!("--source requires a source name"))?,
            );
        } else if arg == "--directory" {
            directory = Some(PathBuf::from(arguments.next().ok_or_else(|| {
                anyhow::anyhow!("--directory requires a directory path")
            })?));
        } else if arg == "--file" {
            file =
                Some(PathBuf::from(arguments.next().ok_or_else(|| {
                    anyhow::anyhow!("--file requires a file path")
                })?));
        } else {
            bail!("Unknown arguments; run plavno --help");
        }
    }
    let backend: Box<dyn backend::Backend> = if let Some(file) = file {
        anyhow::ensure!(
            selected.is_none() && !favorites && directory.is_none(),
            "--file cannot be combined with --source, --directory or --favorites"
        );
        Box::new(backend::Local(file))
    } else {
        if directory.is_some() && selected.is_none() {
            selected = Some("local");
        }
        if favorites && selected.is_none() {
            selected = Some("yandex");
        }
        let (backends, services) = config::backends_with_directory(directory)?;
        Box::new(backend::Multi::new(backends, services, selected)?)
    };
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "The TUI requires an interactive terminal"
    );
    let (mut engine, events, receiver) = engine::Engine::new(backend)?;
    if favorites {
        engine.mode = backend::PlaybackMode::FavoritesShuffle;
    }
    #[cfg(target_os = "linux")]
    let mpris = match mpris::Mpris::new(&engine, events.clone()) {
        Ok(mpris) => Some(mpris),
        Err(error) => {
            eprintln!("plavno: MPRIS unavailable: {error:#}");
            None
        }
    };
    #[cfg(not(target_os = "linux"))]
    let mpris = None;
    tui::run(
        engine,
        events,
        receiver,
        mpris,
        !no_wave,
        visualization::Options {
            mode: visual_mode,
            style: dance_style,
            renderer,
            sixel_update,
            skin,
        },
    )
}
