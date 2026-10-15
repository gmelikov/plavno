mod bridge;
mod local;
mod multi;
mod rate_limit;
mod soundcloud;
mod vk;
pub(crate) mod web;
mod yandex;
mod yandex_cache;

pub use bridge::Bridge;
pub use local::{Local, LocalDirectory};
pub use multi::Multi;
pub use rate_limit::RateLimited;
pub use soundcloud::SoundCloud;
pub use vk::Vk;
pub use yandex::Yandex;
pub use yandex::http as yandex_http;
pub use yandex::network_error_at;

use anyhow::Result;
use std::path::PathBuf;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Track {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip)]
    pub batch: String,
}

impl Track {
    pub fn cache_key(&self) -> String {
        if self.source.is_empty() {
            self.id.clone()
        } else {
            format!("{}:{}", self.source, self.id)
        }
    }
}

#[derive(Clone)]
pub struct Service {
    pub key: &'static str,
    pub name: &'static str,
    pub status: String,
    pub available: bool,
    pub favorites: bool,
    pub favorites_shuffle: bool,
    pub wave_styles: bool,
    pub allow_repeats: bool,
}

#[derive(Clone)]
pub enum Input {
    Http {
        url: String,
        aes128_key: Option<[u8; 16]>,
        hint: &'static str,
        quality: String,
    },
    Stream {
        url: String,
        key: Option<[u8; 16]>,
        iv: [u8; 16],
        skip: u64,
        hint: &'static str,
        quality: String,
    },
    Pipe {
        program: PathBuf,
        args: Vec<String>,
        request: serde_json::Value,
        quality: String,
    },
    File(PathBuf),
}

#[derive(Clone, Copy)]
pub enum Feedback {
    Started,
    Finished,
    Skipped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackMode {
    #[default]
    MyVibe,
    FavoritesShuffle,
}

impl PlaybackMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::MyVibe => "My Vibe",
            Self::FavoritesShuffle => "Favorites Shuffle",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::MyVibe => "w",
            Self::FavoritesShuffle => "f",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveStyle {
    Calm,
    #[default]
    Balanced,
    Energetic,
    Work,
}

impl WaveStyle {
    pub const ALL: [Self; 4] = [Self::Calm, Self::Balanced, Self::Energetic, Self::Work];

    pub fn label(self) -> &'static str {
        match self {
            Self::Calm => "Calm",
            Self::Balanced => "Balanced",
            Self::Energetic => "Energetic",
            Self::Work => "Work",
        }
    }

    pub fn description(self, source: &str) -> &'static str {
        match self {
            Self::Calm => "Relaxed tracks",
            Self::Balanced => "Personal recommendations without mood filters",
            Self::Energetic => "Active tracks",
            Self::Work if source == "yandex" => "Calm ambient without vocals",
            Self::Work => "Calm instrumental tracks",
        }
    }
}

pub trait Backend: Send {
    fn suspend(&mut self) {}
    fn services(&self) -> Vec<Service> {
        vec![Service {
            key: self.key(),
            name: self.name(),
            status: "Ready".into(),
            available: true,
            favorites: self.supports_favorites(),
            favorites_shuffle: self.supports_favorites_shuffle(),
            wave_styles: self.supports_wave_styles(),
            allow_repeats: self.allows_repeats(),
        }]
    }
    fn start_source(&mut self, source: &str, mode: PlaybackMode) -> Result<Vec<Track>> {
        anyhow::ensure!(source == self.key(), "Unknown source");
        self.start_mode(mode)
    }
    fn key(&self) -> &'static str {
        self.name()
    }
    fn name(&self) -> &'static str;
    fn supports_wave_styles(&self) -> bool {
        false
    }
    fn allows_repeats(&self) -> bool {
        false
    }
    fn set_wave_style(&mut self, style: WaveStyle) -> Result<()> {
        anyhow::ensure!(
            style == WaveStyle::Balanced,
            "This source does not support wave styles"
        );
        Ok(())
    }
    fn start_with_style(
        &mut self,
        source: &str,
        mode: PlaybackMode,
        style: WaveStyle,
    ) -> Result<Vec<Track>> {
        if self.supports_wave_styles() {
            self.set_wave_style(style)?;
        }
        self.start_source(source, mode)
    }
    fn start(&mut self) -> Result<Vec<Track>>;
    fn start_mode(&mut self, mode: PlaybackMode) -> Result<Vec<Track>> {
        anyhow::ensure!(
            mode == PlaybackMode::MyVibe,
            "This source does not support Favorites Shuffle"
        );
        self.start()
    }
    fn more(&mut self, history: &[String]) -> Result<Vec<Track>>;
    fn resolve(&mut self, track: &Track) -> Result<Input>;
    fn feedback(&mut self, track: &Track, kind: Feedback, seconds: f64) -> Result<()>;
    fn supports_favorites(&self) -> bool {
        false
    }
    fn supports_favorites_shuffle(&self) -> bool {
        false
    }
    fn favorite_state(&mut self, _track: &Track) -> Result<Option<bool>> {
        Ok(None)
    }
    fn toggle_favorite(&mut self, _track: &Track) -> Result<bool> {
        anyhow::bail!("This source does not support favorites")
    }
    fn set_dislike(&mut self, _track: &Track, _disliked: bool) -> Result<bool> {
        Ok(false)
    }
}
