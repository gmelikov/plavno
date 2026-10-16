use anyhow::Result;
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    path::Path,
};
use symphonia::core::{
    io::MediaSourceStream,
    meta::{MetadataBuilder, MetadataRevision, StandardTagKey, StandardVisualKey, Value},
    probe::Hint,
};

// Keep a reference to the audio file instead of copying pictures into track history.
pub const COVER_PREFIX: &str = "embedded:";
const MAX_COVER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub genre: Option<String>,
    pub cover: Option<Vec<u8>>,
    front_cover: bool,
}

impl Metadata {
    fn merge(&mut self, revision: &MetadataRevision) {
        let text = |key| {
            let values: Vec<_> = revision
                .tags()
                .iter()
                .filter(|tag| tag.std_key == Some(key))
                .filter_map(|tag| match &tag.value {
                    Value::String(value) => {
                        let value = value.trim_matches(|c: char| c.is_whitespace() || c == '\0');
                        (!value.is_empty()).then_some(value)
                    }
                    _ => None,
                })
                .collect();
            (!values.is_empty()).then(|| values.join(", "))
        };
        self.title = self
            .title
            .take()
            .or_else(|| text(StandardTagKey::TrackTitle));
        self.artist = self
            .artist
            .take()
            .or_else(|| text(StandardTagKey::Artist).or_else(|| text(StandardTagKey::AlbumArtist)));
        self.genre = self.genre.take().or_else(|| text(StandardTagKey::Genre));
        for visual in revision.visuals() {
            if visual.data.is_empty() || visual.data.len() > MAX_COVER_BYTES {
                continue;
            }
            let front = visual.usage == Some(StandardVisualKey::FrontCover);
            if self.cover.is_none() || (front && !self.front_cover) {
                self.cover = Some(visual.data.to_vec());
                self.front_cover = front;
            }
        }
    }
}

pub fn read(path: &Path) -> Result<Metadata> {
    let stream = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }
    let mut probed = symphonia::default::get_probe().format(
        &hint,
        stream,
        &Default::default(),
        &Default::default(),
    )?;
    let mut metadata = Metadata::default();
    // ID3 tags may precede the container; FLAC, Vorbis and MP4 keep tags inside it.
    if let Some(mut log) = probed.metadata.get()
        && let Some(revision) = log.skip_to_latest()
    {
        metadata.merge(revision);
    }
    if let Some(revision) = probed.format.metadata().skip_to_latest() {
        metadata.merge(revision);
    }
    // The probe in Symphonia 0.5 does not read the legacy ID3v1 footer.
    if metadata.title.is_none() || metadata.artist.is_none() {
        let mut stream = probed.format.into_inner();
        let mut builder = MetadataBuilder::new();
        if stream.seek(SeekFrom::End(-128)).is_ok()
            && symphonia_metadata::id3v1::read_id3v1(&mut stream, &mut builder).is_ok()
        {
            metadata.merge(&builder.metadata());
        }
    }
    Ok(metadata)
}
