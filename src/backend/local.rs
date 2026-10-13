use super::{Backend, Feedback, Input, Track};
use anyhow::{Result, ensure};
use std::path::PathBuf;

pub struct Local(pub PathBuf);

impl Backend for Local {
    fn key(&self) -> &'static str {
        "local"
    }
    fn name(&self) -> &'static str {
        "Local file"
    }

    fn start(&mut self) -> Result<Vec<Track>> {
        ensure!(self.0.is_file(), "Audio file not found");
        Ok(vec![Track {
            source: String::new(),
            id: self.0.canonicalize()?.to_string_lossy().into(),
            title: self
                .0
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            artist: String::new(),
            duration: None,
            cover_url: None,
            batch: String::new(),
        }])
    }

    fn more(&mut self, _: &[String]) -> Result<Vec<Track>> {
        Ok(Vec::new())
    }

    fn resolve(&mut self, track: &Track) -> Result<Input> {
        Ok(Input::File(PathBuf::from(&track.id)))
    }

    fn feedback(&mut self, _: &Track, _: Feedback, _: f64) -> Result<()> {
        Ok(())
    }
}
