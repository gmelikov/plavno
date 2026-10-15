use crate::backend::{Input, network_error_at, yandex_http};
use anyhow::{Context, Result};
use ctr::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::Duration,
};

const BLOCK_SAMPLES: usize = 4096;
const PCM_BATCHES_PER_SECOND: usize = 5;
const BUFFER_BLOCKS: usize = 2 * PCM_BATCHES_PER_SECOND;

fn pcm_batch_samples(channels: u16, sample_rate: u32) -> usize {
    (sample_rate as usize).div_ceil(PCM_BATCHES_PER_SECOND) * usize::from(channels)
}

#[derive(Default)]
pub struct Progress {
    pub samples: AtomicU64,
    pub buffering: AtomicBool,
    pub error: Mutex<Option<String>>,
    pub levels: AtomicU64,
}

#[derive(Clone, Copy, Default)]
pub struct Levels {
    pub rms: f32,
    pub bass: f32,
    pub peak: f32,
    pub pulse: f32,
}

impl Levels {
    fn pack(self) -> u64 {
        [self.rms, self.bass, self.peak, self.pulse]
            .into_iter()
            .enumerate()
            .fold(0, |packed, (index, level)| {
                packed | (u64::from((level.clamp(0.0, 1.0) * 65535.0) as u16) << (index * 16))
            })
    }

    pub fn unpack(packed: u64) -> Self {
        let levels: [f32; 4] =
            std::array::from_fn(|index| ((packed >> (index * 16)) & 65535) as f32 / 65535.0);
        Self {
            rms: levels[0],
            bass: levels[1],
            peak: levels[2],
            pulse: levels[3],
        }
    }
}

struct PcmBlock {
    samples: Vec<f32>,
    levels: Vec<u64>,
}

struct Meter {
    low: Vec<f32>,
    channel: usize,
    coefficient: f32,
    samples_per_second: f32,
    baseline: f32,
    previous_bass: f32,
    rms_baseline: f32,
    previous_rms: f32,
    pulse: f32,
}

impl Meter {
    fn new(channels: u16, sample_rate: u32) -> Self {
        Self {
            low: vec![0.0; usize::from(channels)],
            channel: 0,
            coefficient: 1.0 - (-std::f32::consts::TAU * 180.0 / sample_rate as f32).exp(),
            samples_per_second: f32::from(channels) * sample_rate as f32,
            baseline: 0.0,
            previous_bass: 0.0,
            rms_baseline: 0.0,
            previous_rms: 0.0,
            pulse: 0.0,
        }
    }

    fn block(&mut self, samples: Vec<f32>) -> PcmBlock {
        let levels = samples
            .chunks(BLOCK_SAMPLES)
            .map(|chunk| self.analyze(chunk))
            .collect();
        PcmBlock { samples, levels }
    }

    fn analyze(&mut self, samples: &[f32]) -> u64 {
        let mut energy = 0.0;
        let mut bass_energy = 0.0;
        let mut peak = 0.0f32;
        for &sample in samples {
            energy += sample * sample;
            peak = peak.max(sample.abs());
            let low = &mut self.low[self.channel];
            *low += self.coefficient * (sample - *low);
            bass_energy += *low * *low;
            self.channel = (self.channel + 1) % self.low.len();
        }
        let count = samples.len().max(1) as f32;
        let bass = (bass_energy / count).sqrt();
        let rms = (energy / count).sqrt();
        let dt = samples.len() as f32 / self.samples_per_second;
        let onset =
            if bass > 0.025 && bass > self.previous_bass * 1.12 && bass > self.baseline * 1.1 {
                ((bass - self.previous_bass) * 2.0 / self.baseline.max(0.03)).clamp(0.0, 1.0)
            } else {
                0.0
            };
        let broadband_onset =
            if rms > 0.025 && rms > self.previous_rms * 1.1 && rms > self.rms_baseline * 1.06 {
                ((rms - self.previous_rms) * 2.0 / self.rms_baseline.max(0.03)).clamp(0.0, 1.0)
            } else {
                0.0
            };
        self.pulse = (self.pulse * (-dt / 0.09).exp()).max(onset.max(broadband_onset));
        let baseline_rate = 1.0 - (-dt / 1.5).exp();
        self.baseline += (bass - self.baseline) * baseline_rate;
        self.rms_baseline += (rms - self.rms_baseline) * baseline_rate;
        self.previous_bass = bass;
        self.previous_rms = rms;
        Levels {
            rms,
            bass,
            peak,
            pulse: self.pulse,
        }
        .pack()
    }
}

pub struct Prepared {
    pub source: PcmSource,
    pub progress: Arc<Progress>,
    pub duration: Option<f64>,
    pub samples_per_second: u64,
    pub quality: String,
    pub buffer: Arc<CachedAudio>,
    pub position: f64,
}

pub struct Audio {
    stream: OutputStream,
    pub sink: Sink,
}

impl Audio {
    pub fn new() -> Result<Self> {
        let mut stream = OutputStreamBuilder::open_default_stream()
            .context("Failed to open the audio device; check ALSA/PipeWire/PulseAudio")?;
        stream.log_on_drop(false);
        let sink = Sink::connect_new(stream.mixer());
        sink.set_volume(1.0);
        Ok(Self { stream, sink })
    }

    pub fn play(&mut self, source: PcmSource, paused: bool, volume: f32, speed: f32) {
        self.sink.stop();
        self.sink = Sink::connect_new(self.stream.mixer());
        self.sink.set_volume(volume);
        self.sink.set_speed(speed);
        if paused {
            self.sink.pause();
        }
        self.sink.append(source);
    }
}

trait ReadSeek: Read + Seek + Send + Sync {}
impl<T: Read + Seek + Send + Sync> ReadSeek for T {}

type AesCtr = ctr::Ctr128BE<aes::Aes128>;

struct StreamReader {
    reader: Box<dyn ReadSeek>,
    cancelled: Arc<AtomicBool>,
    progress: Arc<Progress>,
}

impl Read for StreamReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Ok(0);
        }
        let count = match self.reader.read(bytes) {
            Ok(count) => count,
            Err(error) => {
                *self.progress.error.lock().unwrap() = Some("Audio stream read error".into());
                return Err(error);
            }
        };
        Ok(count)
    }
}

const HTTP_CHUNK: usize = 256 * 1024;
const HTTP_SEEK_CHUNK: usize = 16 * 1024;

struct HttpReader {
    http: ureq::Agent,
    url: String,
    chunk: io::Cursor<Vec<u8>>,
    downloaded: u64,
    total: Option<u64>,
    cipher: Option<AesCtr>,
    next_chunk_size: usize,
    skip: u64,
}

impl HttpReader {
    fn new(url: String, key: Option<[u8; 16]>) -> Self {
        Self {
            http: yandex_http(),
            url,
            chunk: io::Cursor::new(Vec::new()),
            downloaded: 0,
            total: None,
            cipher: key.map(|key| AesCtr::new(&key.into(), &[0u8; 16].into())),
            next_chunk_size: HTTP_CHUNK,
            skip: 0,
        }
    }
}

impl Read for HttpReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.chunk.position() as usize == self.chunk.get_ref().len() {
            if self.total.is_some_and(|total| self.downloaded >= total) {
                return Ok(0);
            }
            let mut response = self
                .http
                .get(&self.url)
                .header(
                    "Range",
                    &format!(
                        "bytes={}-{}",
                        self.downloaded,
                        self.downloaded + self.next_chunk_size as u64 - 1
                    ),
                )
                .call()
                .map_err(|error| io::Error::other(network_error_at(&self.url, error)))?;
            let mut expected_length = None;
            match response.status().as_u16() {
                206 => {
                    let range = response
                        .headers()
                        .get("content-range")
                        .and_then(|h| h.to_str().ok())
                        .and_then(|h| h.strip_prefix("bytes "))
                        .and_then(|h| h.split_once('/'))
                        .ok_or_else(|| io::Error::other("Missing Content-Range"))?;
                    let (start, end) = range
                        .0
                        .split_once('-')
                        .ok_or_else(|| io::Error::other("Invalid Content-Range"))?;
                    let start: u64 = start
                        .parse()
                        .map_err(|_| io::Error::other("Invalid HTTP stream position"))?;
                    let end: u64 = end
                        .parse()
                        .map_err(|_| io::Error::other("Invalid HTTP stream position"))?;
                    if start != self.downloaded
                        || end < start
                        || end - start >= self.next_chunk_size as u64
                    {
                        return Err(io::Error::other("Unexpected HTTP stream position"));
                    }
                    self.total = Some(
                        range
                            .1
                            .parse()
                            .map_err(|_| io::Error::other("Invalid HTTP stream length"))?,
                    );
                    if self.total.is_none_or(|total| end >= total) {
                        return Err(io::Error::other("Invalid HTTP stream length"));
                    }
                    expected_length = Some(end - start + 1);
                }
                200 if self.downloaded == 0 => {}
                _ => return Err(io::Error::other("The server does not support HTTP Range")),
            }
            let mut data = response
                .body_mut()
                .with_config()
                .limit(HTTP_CHUNK as u64 + 1)
                .read_to_vec()
                .map_err(|error| io::Error::other(network_error_at(&self.url, error)))?;
            if data.len() > HTTP_CHUNK {
                return Err(io::Error::other(
                    "The server does not support bounded HTTP ranges",
                ));
            }
            if data.is_empty() || expected_length.is_some_and(|n| n != data.len() as u64) {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Empty HTTP stream",
                ));
            }
            if let Some(cipher) = &mut self.cipher {
                cipher
                    .try_seek(self.downloaded)
                    .map_err(|_| io::Error::other("Invalid encrypted stream position"))?;
                cipher
                    .try_apply_keystream(&mut data)
                    .map_err(|_| io::Error::other("Audio decryption error"))?;
            }
            self.downloaded += data.len() as u64;
            if self.total.is_none() {
                self.total = Some(self.downloaded);
            }
            if self.total.is_some_and(|n| self.downloaded > n) {
                return Err(io::Error::other("Invalid HTTP stream length"));
            }
            self.chunk = io::Cursor::new(data);
            self.next_chunk_size = HTTP_CHUNK;
        }
        self.chunk.read(bytes)
    }
}

impl Seek for HttpReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let current =
            self.downloaded - self.chunk.get_ref().len() as u64 + self.chunk.position() - self.skip;
        let position = match from {
            SeekFrom::Start(position) => position,
            SeekFrom::Current(offset) => current
                .checked_add_signed(offset)
                .ok_or_else(|| io::Error::other("Invalid audio position"))?,
            SeekFrom::End(offset) => {
                if self.total.is_none() {
                    self.read_exact(&mut [0u8; 1])?;
                }
                self.total
                    .context("Unknown audio length")
                    .map_err(io::Error::other)?
                    .saturating_sub(self.skip)
                    .checked_add_signed(offset)
                    .ok_or_else(|| io::Error::other("Invalid audio position"))?
            }
        };
        let physical = position
            .checked_add(self.skip)
            .ok_or_else(|| io::Error::other("Invalid audio position"))?;
        let start = self.downloaded - self.chunk.get_ref().len() as u64;
        if (start..=self.downloaded).contains(&physical) {
            self.chunk.set_position(physical - start);
        } else {
            self.chunk = io::Cursor::new(Vec::new());
            self.downloaded = physical;
            self.next_chunk_size = HTTP_SEEK_CHUNK;
        }
        Ok(position)
    }
}

struct CacheState {
    chunks: Mutex<BTreeMap<u64, Arc<[u8]>>>,
    reader: Mutex<CacheInputReader>,
    cancelled: AtomicBool,
    started: AtomicBool,
    waiting: AtomicUsize,
    len: u64,
    stream: Option<PipeState>,
}

struct PipeState {
    child: Mutex<Child>,
    progress: Mutex<PipeProgress>,
    changed: Condvar,
}

#[derive(Default)]
struct PipeProgress {
    len: u64,
    done: bool,
    failed: bool,
}

impl CacheState {
    fn length(&self) -> Option<u64> {
        self.stream.as_ref().map_or(Some(self.len), |stream| {
            let progress = stream.progress.lock().unwrap();
            progress.done.then_some(progress.len)
        })
    }
}

enum CacheInputReader {
    Http(Box<HttpReader>),
    File(File),
    Pipe,
}

impl CacheInputReader {
    fn chunk(&mut self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Http(reader) => {
                reader.seek(SeekFrom::Start(offset))?;
                reader.next_chunk_size = bytes.len();
                reader.read_exact(bytes)
            }
            Self::File(reader) => {
                reader.seek(SeekFrom::Start(offset))?;
                reader.read_exact(bytes)
            }
            Self::Pipe => Err(io::Error::other("The stream is not seekable at its source")),
        }
    }
}

pub struct CachedAudio {
    state: Arc<CacheState>,
    hint: String,
    quality: String,
    coarse_seek: bool,
}

impl CachedAudio {
    #[cfg(test)]
    pub fn buffered_bytes(&self) -> u64 {
        self.state
            .chunks
            .lock()
            .unwrap()
            .values()
            .map(|chunk| chunk.len() as u64)
            .sum()
    }

    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Relaxed);
        if let Some(stream) = &self.state.stream {
            let mut child = stream.child.lock().unwrap();
            // Helpers and their converters share a private process group.
            if child.try_wait().is_ok_and(|status| status.is_none()) {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGTERM);
                }
            }
            stream.changed.notify_all();
        }
        // A request already in flight may finish, but cannot repopulate the cache.
        self.state.chunks.lock().unwrap().clear();
    }

    pub fn start_download(&self) -> Result<()> {
        if self.state.stream.is_some() {
            return Ok(());
        }
        if self.state.started.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        let state = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("download".into())
            .spawn(move || {
                let mut offset = 0;
                while offset < state.len {
                    while state.waiting.load(Ordering::Relaxed) > 0
                        && !state.cancelled.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    match cache_chunk(&state, offset, HTTP_CHUNK, false) {
                        Ok((start, chunk)) => offset = start + chunk.len() as u64,
                        Err(_) => break,
                    }
                    std::thread::yield_now();
                }
            });
        if let Err(error) = spawn {
            self.state.started.store(false, Ordering::Relaxed);
            return Err(error.into());
        }
        Ok(())
    }
}

impl Drop for CachedAudio {
    fn drop(&mut self) {
        self.cancel();
    }
}

// The downloader owns state, not CachedAudio, so it cannot keep an evicted track alive.
fn cached_chunk(state: &CacheState, offset: u64) -> Option<(u64, Arc<[u8]>)> {
    state
        .chunks
        .lock()
        .unwrap()
        .range(..=offset)
        .next_back()
        .filter(|(start, bytes)| offset - **start < bytes.len() as u64)
        .map(|(start, bytes)| (*start, bytes.clone()))
}

struct ReadPriority<'a>(&'a AtomicUsize);
impl Drop for ReadPriority<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn cache_chunk(
    state: &CacheState,
    offset: u64,
    limit: usize,
    foreground: bool,
) -> io::Result<(u64, Arc<[u8]>)> {
    if state.cancelled.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Loading cancelled",
        ));
    }
    if let Some(chunk) = cached_chunk(state, offset) {
        return Ok(chunk);
    }
    if let Some(stream) = &state.stream {
        let mut progress = stream.progress.lock().unwrap();
        loop {
            if state.cancelled.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Loading cancelled",
                ));
            }
            if let Some(chunk) = cached_chunk(state, offset) {
                return Ok(chunk);
            }
            if progress.done {
                if progress.failed {
                    return Err(io::Error::other(
                        "The audio adapter failed; check its dependencies and service access",
                    ));
                }
                return Ok((offset, Arc::from([])));
            }
            progress = stream
                .changed
                .wait_timeout(progress, Duration::from_millis(250))
                .unwrap()
                .0;
        }
    }
    let _priority = foreground.then(|| {
        state.waiting.fetch_add(1, Ordering::Relaxed);
        ReadPriority(&state.waiting)
    });
    let mut reader = state.reader.lock().unwrap();
    if state.cancelled.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Loading cancelled",
        ));
    }
    if let Some(chunk) = cached_chunk(state, offset) {
        return Ok(chunk);
    }
    let next = state
        .chunks
        .lock()
        .unwrap()
        .range(offset..)
        .next()
        .map_or(state.len, |(start, _)| *start);
    let count = (next - offset).min(limit as u64) as usize;
    let mut bytes = vec![0; count];
    reader.chunk(offset, &mut bytes)?;
    let mut chunks = state.chunks.lock().unwrap();
    if state.cancelled.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Loading cancelled",
        ));
    }
    let chunk: Arc<[u8]> = bytes.into();
    chunks.insert(offset, chunk.clone());
    Ok((offset, chunk))
}

struct CacheReader {
    audio: Arc<CachedAudio>,
    position: u64,
}

impl Read for CacheReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let state = &self.audio.state;
        if bytes.is_empty() || state.length().is_some_and(|len| self.position >= len) {
            return Ok(0);
        }
        let (offset, chunk) = cache_chunk(state, self.position, bytes.len().min(HTTP_CHUNK), true)?;
        let start = (self.position - offset) as usize;
        let count = bytes.len().min(chunk.len() - start);
        bytes[..count].copy_from_slice(&chunk[start..start + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for CacheReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self
                .audio
                .state
                .length()
                .ok_or_else(|| io::Error::other("The stream length is not known yet"))?
                .checked_add_signed(offset),
        }
        .ok_or_else(|| io::Error::other("Invalid audio position"))?;
        Ok(self.position)
    }
}

pub fn cache_input(input: Input) -> Result<Arc<CachedAudio>> {
    if let Input::Pipe {
        program,
        args,
        request,
        quality,
    } = input
    {
        let mut child = Command::new(program)
            .args(args)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start the audio adapter")?;
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let send = (|| -> Result<()> {
            serde_json::to_writer(&mut stdin, &request)?;
            stdin.write_all(b"\n")?;
            Ok(())
        })();
        drop(stdin);
        if let Err(error) = send {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let audio = Arc::new(CachedAudio {
            state: Arc::new(CacheState {
                chunks: Mutex::new(BTreeMap::new()),
                reader: Mutex::new(CacheInputReader::Pipe),
                cancelled: AtomicBool::new(false),
                started: AtomicBool::new(true),
                waiting: AtomicUsize::new(0),
                len: 0,
                stream: Some(PipeState {
                    child: Mutex::new(child),
                    progress: Mutex::new(PipeProgress::default()),
                    changed: Condvar::new(),
                }),
            }),
            hint: "flac".into(),
            quality,
            coarse_seek: false,
        });
        let state = audio.state.clone();
        std::thread::Builder::new()
            .name("stream download".into())
            .spawn(move || {
                let stream = state.stream.as_ref().unwrap();
                let mut offset = 0;
                let mut failed = false;
                while !state.cancelled.load(Ordering::Relaxed) {
                    let mut bytes = vec![0; 32 * 1024];
                    match stdout.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(count) => {
                            bytes.truncate(count);
                            let mut progress = stream.progress.lock().unwrap();
                            if state.cancelled.load(Ordering::Relaxed) {
                                break;
                            }
                            state.chunks.lock().unwrap().insert(offset, bytes.into());
                            offset += count as u64;
                            progress.len = offset;
                            stream.changed.notify_all();
                        }
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }
                drop(stdout);
                let status = loop {
                    match stream.child.lock().unwrap().try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Err(error) => break Err(error),
                        Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    }
                };
                let mut progress = stream.progress.lock().unwrap();
                progress.failed = failed || status.is_err() || status.is_ok_and(|s| !s.success());
                progress.done = true;
                stream.changed.notify_all();
            })?;
        return Ok(audio);
    }
    let (reader, hint, quality, len, first, coarse_seek): (
        CacheInputReader,
        String,
        String,
        u64,
        Option<Vec<u8>>,
        bool,
    ) = match input {
        Input::File(path) => {
            let hint = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let quality = format!("Local {}", hint.to_uppercase());
            let file = File::open(path).context("Failed to open the audio file")?;
            let len = file.metadata()?.len();
            (
                CacheInputReader::File(file),
                hint,
                quality,
                len,
                None,
                false,
            )
        }
        Input::Http {
            url,
            aes128_key,
            hint,
            quality,
        } => {
            let mut reader = HttpReader::new(url, aes128_key);
            reader
                .read_exact(&mut [0u8; 1])
                .context("Failed to open the audio stream")?;
            let len = reader.total.context("Unknown audio stream length")?;
            let first = std::mem::take(reader.chunk.get_mut());
            (
                CacheInputReader::Http(Box::new(reader)),
                hint.into(),
                quality,
                len,
                Some(first),
                hint == "mp3",
            )
        }
        Input::Stream {
            url,
            key,
            iv,
            skip,
            hint,
            quality,
        } => {
            let mut reader = HttpReader::new(url, None);
            reader.cipher = key.map(|key| AesCtr::new(&key.into(), &iv.into()));
            reader.skip = skip;
            reader.downloaded = skip;
            reader
                .read_exact(&mut [0u8; 1])
                .context("Failed to open the audio stream")?;
            let len = reader
                .total
                .context("Unknown audio stream length")?
                .checked_sub(skip)
                .context("Invalid stream header length")?;
            let first = std::mem::take(reader.chunk.get_mut());
            (
                CacheInputReader::Http(Box::new(reader)),
                hint.into(),
                quality,
                len,
                Some(first),
                hint == "mp3",
            )
        }
        Input::Pipe { .. } => unreachable!(),
    };
    let mut chunks = BTreeMap::new();
    if let Some(first) = first {
        chunks.insert(0, first.into());
    }
    Ok(Arc::new(CachedAudio {
        state: Arc::new(CacheState {
            chunks: Mutex::new(chunks),
            reader: Mutex::new(reader),
            len,
            cancelled: AtomicBool::new(false),
            started: AtomicBool::new(false),
            waiting: AtomicUsize::new(0),
            stream: None,
        }),
        hint,
        quality,
        coarse_seek,
    }))
}

impl Seek for StreamReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Loading cancelled",
            ));
        }
        self.reader.seek(from)
    }
}

pub struct PcmSource {
    receiver: Receiver<PcmBlock>,
    block: std::vec::IntoIter<f32>,
    block_levels: std::vec::IntoIter<u64>,
    level_samples: usize,
    channels: u16,
    sample_rate: u32,
    progress: Arc<Progress>,
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.block.len() == 0 {
            match self.receiver.try_recv() {
                Ok(block) => {
                    self.block = block.samples.into_iter();
                    self.block_levels = block.levels.into_iter();
                    self.level_samples = 0;
                    self.progress.buffering.store(false, Ordering::Relaxed);
                }
                Err(TryRecvError::Empty) => {
                    self.progress.levels.store(0, Ordering::Relaxed);
                    self.progress.buffering.store(true, Ordering::Relaxed);
                    return Some(0.0);
                }
                Err(TryRecvError::Disconnected) => {
                    self.progress.levels.store(0, Ordering::Relaxed);
                    return None;
                }
            }
        }
        let sample = self.block.next()?;
        if self.level_samples == 0 {
            self.progress
                .levels
                .store(self.block_levels.next().unwrap_or(0), Ordering::Relaxed);
            self.level_samples = BLOCK_SAMPLES;
        }
        self.level_samples -= 1;
        self.progress.samples.fetch_add(1, Ordering::Relaxed);
        Some(sample)
    }
}

impl Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

pub fn prepare(buffer: Arc<CachedAudio>, cancelled: Arc<AtomicBool>) -> Result<Prepared> {
    prepare_at(buffer, cancelled, 0.0)
}

pub fn prepare_at(
    buffer: Arc<CachedAudio>,
    cancelled: Arc<AtomicBool>,
    position: f64,
) -> Result<Prepared> {
    prepare_segment(buffer, cancelled, position, None)
}

pub fn prepare_scrub_at(
    buffer: Arc<CachedAudio>,
    cancelled: Arc<AtomicBool>,
    position: f64,
    reverse: bool,
) -> Result<Prepared> {
    prepare_segment(buffer, cancelled, position, Some(reverse))
}

fn prepare_segment(
    buffer: Arc<CachedAudio>,
    cancelled: Arc<AtomicBool>,
    position: f64,
    scrub: Option<bool>,
) -> Result<Prepared> {
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "Loading cancelled");
    let position = Duration::try_from_secs_f64(position).context("Invalid audio position")?;
    let progress = Arc::new(Progress::default());
    let cache = &buffer;
    let reader: Box<dyn ReadSeek> = Box::new(CacheReader {
        audio: cache.clone(),
        position: 0,
    });
    let byte_len = cache.state.length();
    let reader = BufReader::with_capacity(
        64 * 1024,
        StreamReader {
            reader,
            cancelled: cancelled.clone(),
            progress: progress.clone(),
        },
    );
    let mut builder = Decoder::builder()
        .with_data(reader)
        .with_hint(&cache.hint)
        .with_seekable(byte_len.is_some())
        .with_coarse_seek(cache.coarse_seek);
    if let Some(len) = byte_len {
        builder = builder.with_byte_len(len);
    }
    let mut decoder = builder.build().context("Failed to decode audio")?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    let duration = decoder.total_duration().map(|d| d.as_secs_f64());
    let position = duration.map_or(position, |d| position.min(Duration::from_secs_f64(d)));
    let grain = Duration::from_millis(400);
    let start = if scrub == Some(true) {
        position.saturating_sub(grain)
    } else {
        position
    };
    let at_end = duration.is_some_and(|duration| start.as_secs_f64() >= duration);
    if !start.is_zero() && !at_end {
        decoder
            .try_seek(start)
            .map_err(|error| anyhow::anyhow!("Failed to seek the audio stream: {error}"))?;
    }
    let samples = scrub.map_or(BLOCK_SAMPLES, |reverse| {
        let length = if reverse { position - start } else { grain };
        (length.as_secs_f64() * f64::from(sample_rate)).round() as usize * usize::from(channels)
    });
    let mut first: Vec<f32> = if at_end {
        Vec::new()
    } else {
        decoder.by_ref().take(samples).collect()
    };
    if let Some(reverse) = scrub {
        let channels = usize::from(channels);
        first.truncate(first.len() / channels * channels);
        if reverse {
            let frames = first.len() / channels;
            for frame in 0..frames / 2 {
                for channel in 0..channels {
                    first.swap(
                        frame * channels + channel,
                        (frames - frame - 1) * channels + channel,
                    );
                }
            }
        }
        let frames = first.len() / channels;
        let fade = (sample_rate as usize / 500).max(1);
        for (frame, samples) in first.chunks_exact_mut(channels).enumerate() {
            let gain = frame.min(frames - frame - 1).min(fade) as f32 / fade as f32;
            for sample in samples {
                *sample *= gain;
            }
        }
    }
    anyhow::ensure!(
        !first.is_empty() || !position.is_zero() || scrub.is_some(),
        "The audio stream is empty"
    );
    cache.start_download()?;
    let mut meter = Meter::new(channels, sample_rate);
    let first = meter.block(first);
    let batch_samples = pcm_batch_samples(channels, sample_rate);
    let (sender, receiver) = mpsc::sync_channel(BUFFER_BLOCKS);
    if !first.samples.is_empty() && scrub.is_none() {
        std::thread::Builder::new()
            .name("decode".into())
            .spawn(move || {
                while !cancelled.load(Ordering::Relaxed) {
                    let mut block = Vec::with_capacity(batch_samples);
                    block.extend(decoder.by_ref().take(batch_samples));
                    if block.is_empty() || sender.send(meter.block(block)).is_err() {
                        break;
                    }
                }
            })?;
    }
    Ok(Prepared {
        source: PcmSource {
            receiver,
            block: first.samples.into_iter(),
            block_levels: first.levels.into_iter(),
            level_samples: 0,
            channels,
            sample_rate,
            progress: progress.clone(),
        },
        quality: format!("{} · {} Hz", cache.quality, sample_rate),
        samples_per_second: u64::from(channels) * u64::from(sample_rate),
        progress,
        duration,
        buffer,
        position: position.as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_grains_are_bounded_reverse_the_pcm_and_fade_their_edges() {
        let buffer = cache_input(Input::File(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"),
        ))
        .unwrap();
        let forward =
            prepare_scrub_at(buffer.clone(), Arc::new(AtomicBool::new(false)), 9.6, false).unwrap();
        let channels = usize::from(forward.source.channels());
        let rate = forward.source.sample_rate();
        let forward: Vec<_> = forward.source.collect();
        let reverse =
            prepare_scrub_at(buffer.clone(), Arc::new(AtomicBool::new(false)), 10.0, true).unwrap();
        let reverse: Vec<_> = reverse.source.collect();
        assert_eq!(
            forward.len(),
            (f64::from(rate) * 0.4).round() as usize * channels
        );
        assert_eq!(reverse.len(), forward.len());
        for (actual, expected) in reverse
            .chunks_exact(channels)
            .zip(forward.chunks_exact(channels).rev())
        {
            assert_eq!(actual, expected);
        }
        assert_eq!(reverse[0], 0.0);
        assert_eq!(*reverse.last().unwrap(), 0.0);
        let start =
            prepare_scrub_at(buffer.clone(), Arc::new(AtomicBool::new(false)), 0.0, true).unwrap();
        assert_eq!(start.source.count(), 0);
        let end = prepare_scrub_at(buffer, Arc::new(AtomicBool::new(false)), 60.0, false).unwrap();
        assert_eq!(end.source.count(), 0);
    }

    fn prepare(input: Input, cancelled: Arc<AtomicBool>) -> Result<Prepared> {
        super::prepare(cache_input(input)?, cancelled)
    }

    #[test]
    fn pcm_queue_covers_two_seconds_at_different_rates_and_channel_counts() {
        for rate in [22050, 44100, 48000, 48001, 96000, 192000] {
            for channels in [1, 2, 6] {
                let batch = pcm_batch_samples(channels, rate);
                let per_second = rate as usize * usize::from(channels);
                assert_eq!(batch % usize::from(channels), 0);
                assert!(batch * BUFFER_BLOCKS >= 2 * per_second);
                assert!(
                    batch * BUFFER_BLOCKS < 2 * per_second + BUFFER_BLOCKS * usize::from(channels)
                );
            }
        }
    }

    #[test]
    fn pcm_batches_keep_wave_levels_aligned_with_playback() {
        let (tx, rx) = mpsc::sync_channel(2);
        let progress = Arc::new(Progress::default());
        let mut meter = Meter::new(2, 48000);
        let first = meter.block(vec![0.1; BLOCK_SAMPLES]);
        let first_level = first.levels[0];
        let mut expected_samples = first.samples.clone();
        let mut expected_levels = vec![first_level; first.samples.len()];
        for length in [pcm_batch_samples(2, 48000), 123] {
            let samples: Vec<_> = (0..length)
                .map(|index| {
                    if (index / BLOCK_SAMPLES).is_multiple_of(2) {
                        0.5
                    } else {
                        0.0
                    }
                })
                .collect();
            let block = meter.block(samples);
            for (index, chunk) in block.samples.chunks(BLOCK_SAMPLES).enumerate() {
                expected_levels.extend(std::iter::repeat_n(block.levels[index], chunk.len()));
            }
            expected_samples.extend_from_slice(&block.samples);
            tx.send(block).unwrap();
        }
        drop(tx);
        let mut source = PcmSource {
            receiver: rx,
            block: first.samples.into_iter(),
            block_levels: first.levels.into_iter(),
            level_samples: 0,
            channels: 2,
            sample_rate: 48000,
            progress: progress.clone(),
        };
        assert_eq!(progress.levels.load(Ordering::Relaxed), 0);
        for (sample, level) in expected_samples.iter().zip(expected_levels) {
            assert_eq!(source.next(), Some(*sample));
            assert_eq!(progress.levels.load(Ordering::Relaxed), level);
            assert!(!progress.buffering.load(Ordering::Relaxed));
        }
        assert_eq!(
            progress.samples.load(Ordering::Relaxed),
            expected_samples.len() as u64
        );
        assert_eq!(source.next(), None);
        assert_eq!(progress.levels.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn decoder_batches_preserve_pcm_and_release_the_buffer_on_stop() {
        use std::time::Instant;

        let file =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/seek.flac");
        let cancelled = Arc::new(AtomicBool::new(false));
        let prepared = prepare(Input::File(file.clone()), cancelled.clone()).unwrap();
        let batch_samples =
            pcm_batch_samples(prepared.source.channels, prepared.source.sample_rate);
        assert_eq!(prepared.source.block.len(), BLOCK_SAMPLES);
        let mut actual: Vec<_> = prepared.source.block.collect();
        loop {
            let block = match prepared
                .source
                .receiver
                .recv_timeout(Duration::from_secs(2))
            {
                Ok(block) => block,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(error) => panic!("Decoder stalled: {error}"),
            };
            assert!(block.samples.len() <= batch_samples);
            assert_eq!(block.samples.capacity(), batch_samples);
            assert_eq!(
                block.levels.len(),
                block.samples.len().div_ceil(BLOCK_SAMPLES)
            );
            actual.extend(block.samples);
        }
        let expected: Vec<_> = Decoder::try_from(BufReader::new(File::open(&file).unwrap()))
            .unwrap()
            .collect();
        assert_eq!(actual, expected);

        let prepared = prepare(Input::File(file), cancelled.clone()).unwrap();
        for _ in 0..BUFFER_BLOCKS {
            prepared
                .source
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
        }
        let buffer = prepared.buffer.clone();
        cancelled.store(true, Ordering::Relaxed);
        drop(prepared);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&buffer) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            Arc::strong_count(&buffer),
            1,
            "Stopped decoder retained the audio buffer"
        );
    }

    #[test]
    fn pipe_buffer_starts_early_seeks_locally_replays_offline_and_cancels_helpers() {
        use std::{path::Path, time::Instant};
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/seek.flac");
        let gate = std::env::temp_dir().join(format!("plavno-pipe-gate-{}", std::process::id()));
        let _ = std::fs::remove_file(&gate);
        let code = "read -r request; dd if=\"$1\" bs=131072 count=1 2>/dev/null; while [ ! -e \"$2\" ]; do sleep .01; done; dd if=\"$1\" bs=131072 skip=1 2>/dev/null";
        let buffer = cache_input(Input::Pipe {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                code.into(),
                "fixture".into(),
                fixture.as_os_str().to_string_lossy().into_owned(),
                gate.as_os_str().to_string_lossy().into_owned(),
            ],
            request: serde_json::json!({"path":fixture,"gate":gate}),
            quality: "Synthetic FLAC stream".into(),
        })
        .unwrap();
        let cancel = || Arc::new(AtomicBool::new(false));
        let prepared = super::prepare(buffer.clone(), cancel()).unwrap();
        assert!(prepared.source.channels() > 0);
        assert!(
            buffer.state.length().is_none(),
            "Playback waited for the complete stream"
        );
        assert!(buffer.buffered_bytes() < std::fs::metadata(&fixture).unwrap().len());
        let pid = buffer
            .state
            .stream
            .as_ref()
            .unwrap()
            .child
            .lock()
            .unwrap()
            .id();
        for seconds in [0.5, 0.1, 0.0] {
            let mut seek = super::prepare_at(buffer.clone(), cancel(), seconds).unwrap();
            assert!((seek.position - seconds).abs() < 0.001);
            assert!(seek.source.next().is_some());
        }
        assert_eq!(
            buffer
                .state
                .stream
                .as_ref()
                .unwrap()
                .child
                .lock()
                .unwrap()
                .id(),
            pid,
            "Seeking spawned a new audio request"
        );
        std::fs::write(&gate, b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while buffer.state.length().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut reader = CacheReader {
            audio: buffer.clone(),
            position: 0,
        };
        let mut bytes = vec![];
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, std::fs::read(&fixture).unwrap());
        let mut seek = super::prepare_at(buffer.clone(), cancel(), 30.0).unwrap();
        assert!(seek.source.next().is_some());
        std::fs::remove_file(&gate).unwrap();
        drop(seek);
        drop(prepared);
        drop(reader);
        drop(buffer);

        let buffer = cache_input(Input::Pipe {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "read -r request; printf cached; exec sleep 30".into(),
            ],
            request: serde_json::json!({}),
            quality: "Cancellation fixture".into(),
        })
        .unwrap();
        let stream = buffer.state.stream.as_ref().unwrap();
        let mut reader = CacheReader {
            audio: buffer.clone(),
            position: 0,
        };
        let mut first = [0; 6];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"cached");
        let started = Instant::now();
        buffer.cancel();
        while !stream.progress.lock().unwrap().done {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "Eviction did not terminate the helper"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(buffer.buffered_bytes(), 0);
    }

    #[test]
    fn spotify_counter_and_header_offset_preserve_audio_and_cached_seeks() {
        let audio = include_bytes!("../tests/fixtures/seek.flac");
        let key = [0x39; 16];
        let iv = [
            0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63,
            0x0d, 0x93,
        ];
        let mut encrypted = vec![0x42; 167];
        encrypted.extend_from_slice(audio);
        AesCtr::new(&key.into(), &iv.into()).apply_keystream(&mut encrypted);
        let (url, stop, server) = range_server(encrypted);
        let buffer = cache_input(Input::Stream {
            url,
            key: Some(key),
            iv,
            skip: 167,
            hint: "flac",
            quality: "Spotify transport fixture".into(),
        })
        .unwrap();
        let mut reader = CacheReader {
            audio: buffer.clone(),
            position: 0,
        };
        reader.seek(SeekFrom::Start(257)).unwrap();
        let mut bytes = [0; 37];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, &audio[257..294]);
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut result = vec![];
        reader.read_to_end(&mut result).unwrap();
        assert_eq!(result, audio);
        stop.send(()).unwrap();
        let ranges = server.join().unwrap();
        assert!(ranges.iter().all(|(start, _)| *start >= 167));
        let mut seek = super::prepare_at(buffer, Arc::new(AtomicBool::new(false)), 30.0).unwrap();
        assert!(seek.source.next().is_some());
    }

    type RangeRequests = Vec<(usize, usize)>;

    fn range_server(
        data: Vec<u8>,
    ) -> (
        String,
        mpsc::Sender<()>,
        std::thread::JoinHandle<RangeRequests>,
    ) {
        use std::{
            io::{BufRead, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let (stop, done) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut positions = Vec::new();
            while matches!(done.try_recv(), Err(TryRecvError::Empty)) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut range = None;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        let (start, end) = value.trim().split_once('-').unwrap();
                        range = Some((
                            start.parse::<usize>().unwrap(),
                            end.parse::<usize>().unwrap() + 1,
                        ));
                    }
                }
                let (start, end) = range.unwrap();
                let end = end.min(data.len());
                positions.push((start, end - start));
                assert!(start < end);
                write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", end-1, data.len(), end-start).unwrap();
                stream.write_all(&data[start..end]).unwrap();
            }
            positions
        });
        (url, stop, server)
    }

    struct DelayedRangeServer {
        url: String,
        transferred: Arc<AtomicU64>,
        release: Arc<AtomicBool>,
        blocked: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl DelayedRangeServer {
        fn new(data: Vec<u8>) -> Self {
            use std::{
                io::{BufRead, Write},
                net::TcpListener,
            };
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/audio", listener.local_addr().unwrap());
            let transferred = Arc::new(AtomicU64::new(0));
            let release = Arc::new(AtomicBool::new(false));
            let blocked = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let count = transferred.clone();
            let gate = release.clone();
            let waiting = blocked.clone();
            let done = stop.clone();
            let worker = std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(stream) => stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 {
                            break;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=")
                        {
                            let (start, end) = value.trim().split_once('-').unwrap();
                            range = Some((
                                start.parse::<usize>().unwrap(),
                                end.parse::<usize>().unwrap() + 1,
                            ));
                        }
                    }
                    let (start, end) = range.unwrap();
                    let end = end.min(data.len());
                    assert!(start < end && end - start <= HTTP_CHUNK);
                    // The prefix and tail (MP4 metadata) are available. The middle stays blocked.
                    if start >= HTTP_CHUNK && start < data.len() - HTTP_CHUNK {
                        waiting.store(true, Ordering::SeqCst);
                        while !gate.load(Ordering::SeqCst) && !done.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        if done.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    let sent = (|| -> io::Result<()> {
                        write!(
                            stream,
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            end - 1,
                            data.len(),
                            end - start
                        )?;
                        stream.write_all(&data[start..end])
                    })();
                    if sent.is_ok() {
                        count.fetch_add((end - start) as u64, Ordering::SeqCst);
                    }
                }
            });
            Self {
                url,
                transferred,
                release,
                blocked,
                stop,
                worker: Some(worker),
            }
        }
    }

    impl Drop for DelayedRangeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.release.store(true, Ordering::SeqCst);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn long_flac_mp4(metadata_at_start: bool) -> Vec<u8> {
        let fixture = include_bytes!("../tests/fixtures/tone-flac.m4a");
        let ftyp_len = u32::from_be_bytes(fixture[..4].try_into().unwrap()) as usize;
        let mdat_len =
            u32::from_be_bytes(fixture[ftyp_len..ftyp_len + 4].try_into().unwrap()) as usize;
        let frame = &fixture[ftyp_len + 8..ftyp_len + mdat_len];
        let mut moov = fixture[ftyp_len + mdat_len..].to_vec();
        let packets = 4096u32;
        let samples = packets * 2205;
        let audio_offset = ftyp_len + 8 + if metadata_at_start { moov.len() } else { 0 };
        for (tag, offset, value) in [
            (*b"stts", 12, packets),
            (*b"stsc", 16, packets),
            (*b"stsz", 12, packets),
            (*b"stco", 12, audio_offset as u32),
            (*b"mdhd", 20, samples),
            (*b"mvhd", 20, samples),
            (*b"tkhd", 24, samples),
        ] {
            let position = moov.windows(4).position(|s| s == tag).unwrap() + offset;
            moov[position..position + 4].copy_from_slice(&value.to_be_bytes());
        }
        let info = moov.windows(4).position(|s| s == b"dfLa").unwrap() + 12;
        let format = u64::from_be_bytes(moov[info + 10..info + 18].try_into().unwrap());
        let format = (format & !((1u64 << 36) - 1)) | u64::from(samples);
        moov[info + 10..info + 18].copy_from_slice(&format.to_be_bytes());
        moov[info + 18..info + 34].fill(0); // No checksum for the repeated test waveform.
        let mut data = fixture[..ftyp_len].to_vec();
        if metadata_at_start {
            data.extend_from_slice(&moov);
        }
        data.extend_from_slice(&((frame.len() * packets as usize + 8) as u32).to_be_bytes());
        data.extend_from_slice(b"mdat");
        for _ in 0..packets {
            data.extend_from_slice(frame);
        }
        if !metadata_at_start {
            data.extend_from_slice(&moov);
        }
        data
    }

    #[test]
    fn playback_starts_while_the_rest_of_the_http_file_is_blocked() {
        let mp3 = include_bytes!("../tests/fixtures/tone.mp3").repeat(2048);
        let flac = include_bytes!("../tests/fixtures/tone.flac").to_vec();
        let cases = [
            ("MP3", "mp3", mp3),
            ("FLAC", "flac", flac),
            ("FLAC/MP4 moov first", "m4a", long_flac_mp4(true)),
            ("FLAC/MP4 moov last", "m4a", long_flac_mp4(false)),
        ];
        for (name, hint, data) in cases {
            for encrypted in [false, true] {
                let total = data.len() as u64;
                assert!(total > 2 * HTTP_CHUNK as u64);
                let key = encrypted.then(|| std::array::from_fn(|i| i as u8));
                let mut bytes = data.clone();
                if let Some(key) = key {
                    AesCtr::new(&key.into(), &[0u8; 16].into()).apply_keystream(&mut bytes);
                }
                let server = DelayedRangeServer::new(bytes);
                let input = Input::Http {
                    url: server.url.clone(),
                    aes128_key: key,
                    hint,
                    quality: name.into(),
                };
                let cancel = Arc::new(AtomicBool::new(false));
                let cancelled = cancel.clone();
                let (tx, rx) = mpsc::channel();
                let worker = std::thread::spawn(move || {
                    let _ = tx.send(prepare(input, cancelled));
                });
                let ready = rx.recv_timeout(Duration::from_secs(3));
                if ready.is_err() {
                    // Unblock downloads before joining so a regression fails without hanging.
                    server.release.store(true, Ordering::SeqCst);
                    cancel.store(true, Ordering::SeqCst);
                    worker.join().unwrap();
                    panic!("{name}, encrypted={encrypted}: playback waited for the blocked body");
                }
                let prepared = ready.unwrap().unwrap();
                let progress = prepared.progress.clone();
                let (sink, mut output) = Sink::new();
                sink.append(prepared.source);
                let samples: Vec<_> = output.by_ref().take(BLOCK_SAMPLES).collect();
                assert_eq!(
                    progress.samples.load(Ordering::SeqCst),
                    BLOCK_SAMPLES as u64
                );
                assert!(
                    samples.iter().any(|sample| sample.abs() > 0.01),
                    "{name}: no audible PCM"
                );
                assert!(!server.release.load(Ordering::SeqCst));
                let received = server.transferred.load(Ordering::SeqCst);
                assert!(
                    received < total,
                    "{name}: full file downloaded before playback"
                );
                assert!(
                    received <= 3 * HTTP_CHUNK as u64,
                    "{name}: excessive startup download"
                );
                println!(
                    "{name}, encrypted={encrypted}: {received}/{total} bytes, {} PCM samples before releasing the rest",
                    samples.len()
                );
                cancel.store(true, Ordering::SeqCst);
                drop(output);
                drop(sink);
                worker.join().unwrap();
            }
        }
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "Cache operation timed out"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn full_files_download_without_pcm_consumption_and_replay_offline() {
        let cases = [
            (
                "mp3",
                include_bytes!("../tests/fixtures/tone.mp3").repeat(2048),
            ),
            (
                "flac",
                include_bytes!("../tests/fixtures/tone.flac").to_vec(),
            ),
            ("m4a", long_flac_mp4(true)),
            ("m4a", long_flac_mp4(false)),
        ];
        for (hint, bytes) in cases {
            for encrypted in [false, true] {
                let key = encrypted.then(|| std::array::from_fn(|i| i as u8));
                let mut wire = bytes.clone();
                if let Some(key) = key {
                    AesCtr::new(&key.into(), &[0u8; 16].into()).apply_keystream(&mut wire);
                }
                let (url, stop, server) = range_server(wire);
                let cancel = Arc::new(AtomicBool::new(false));
                let prepared = prepare(
                    Input::Http {
                        url,
                        aes128_key: key,
                        hint,
                        quality: "Test".into(),
                    },
                    cancel.clone(),
                )
                .unwrap();
                let cache = &prepared.buffer;
                wait_until(|| cache.buffered_bytes() == bytes.len() as u64);
                assert_eq!(
                    prepared.progress.samples.load(Ordering::Relaxed),
                    0,
                    "Downloading required playing PCM"
                );
                let buffer = prepared.buffer.clone();
                cancel.store(true, Ordering::Relaxed);
                drop(prepared);
                drop(stop);
                let mut requests = server.join().unwrap();
                requests.sort_unstable();
                let mut end = 0;
                for (start, count) in requests {
                    assert_eq!(start, end, "Downloaded overlapping ranges or left a gap");
                    end += count;
                }
                assert_eq!(end, bytes.len());
                let cache = &buffer;
                let mut reader = CacheReader {
                    audio: cache.clone(),
                    position: 0,
                };
                let mut actual = Vec::new();
                reader.read_to_end(&mut actual).unwrap();
                assert_eq!(
                    actual, bytes,
                    "Cached audio differs from the decrypted file"
                );
                for position in [31.0, 7.0, 0.0] {
                    let cancel = Arc::new(AtomicBool::new(false));
                    let mut replay = prepare_at(buffer.clone(), cancel.clone(), position).unwrap();
                    assert!(
                        replay
                            .source
                            .by_ref()
                            .take(BLOCK_SAMPLES)
                            .any(|s| s.abs() > 0.01),
                        "Offline replay/seek produced no audio"
                    );
                    cancel.store(true, Ordering::Relaxed);
                }
                println!(
                    "{hint}, encrypted={encrypted}: {end} bytes cached without playback, offline replay and seek passed"
                );
            }
        }
    }

    #[test]
    fn buffer_seek_is_local_and_reads_fetch_only_missing_bytes() {
        let bytes: Vec<_> = (0..HTTP_CHUNK * 3 + 123).map(|i| (i % 251) as u8).collect();
        for encrypted in [false, true] {
            let key = encrypted.then(|| std::array::from_fn(|i| i as u8));
            let mut wire = bytes.clone();
            if let Some(key) = key {
                AesCtr::new(&key.into(), &[0u8; 16].into()).apply_keystream(&mut wire);
            }
            let (url, stop, server) = range_server(wire);
            let buffer = cache_input(Input::Http {
                url,
                aes128_key: key,
                hint: "mp3",
                quality: "Test".into(),
            })
            .unwrap();
            let mut reader = CacheReader {
                audio: buffer.clone(),
                position: 0,
            };
            let offset = (HTTP_CHUNK * 2 + 19) as u64;
            reader.seek(SeekFrom::Start(offset)).unwrap();
            assert_eq!(
                buffer.buffered_bytes(),
                HTTP_CHUNK as u64,
                "Seeking fetched audio before a read"
            );
            let mut chunk = [0; 32];
            reader.read_exact(&mut chunk).unwrap();
            assert_eq!(chunk, bytes[offset as usize..offset as usize + 32]);
            assert_eq!(buffer.buffered_bytes(), HTTP_CHUNK as u64 + 32);
            reader.seek(SeekFrom::Start(offset - 8)).unwrap();
            let mut combined = [0; 40];
            reader.read_exact(&mut combined).unwrap();
            assert_eq!(combined, bytes[offset as usize - 8..offset as usize + 32]);
            reader.seek(SeekFrom::End(-16)).unwrap();
            drop(stop);
            assert_eq!(
                server.join().unwrap(),
                [
                    (0, HTTP_CHUNK),
                    (offset as usize, 32),
                    (offset as usize - 8, 8)
                ],
                "Fetched already buffered bytes or fetched on seek"
            );
            // Independent readers share the same buffer, including disjoint cached ranges.
            let mut replay = CacheReader {
                audio: buffer,
                position: 17,
            };
            replay.read_exact(&mut chunk).unwrap();
            assert_eq!(chunk, bytes[17..49]);
            replay.seek(SeekFrom::Start(offset - 8)).unwrap();
            replay.read_exact(&mut combined).unwrap();
            assert_eq!(combined, bytes[offset as usize - 8..offset as usize + 32]);
        }
    }

    #[test]
    fn cached_reads_do_not_wait_for_network_and_eviction_cancels_download() {
        let bytes: Vec<_> = (0..HTTP_CHUNK * 4 + 123).map(|i| (i % 251) as u8).collect();
        let server = DelayedRangeServer::new(bytes.clone());
        let cache = cache_input(Input::Http {
            url: server.url.clone(),
            aes128_key: None,
            hint: "mp3",
            quality: "Test".into(),
        })
        .unwrap();
        cache.start_download().unwrap();
        wait_until(|| server.blocked.load(Ordering::SeqCst));
        let mut reader = CacheReader {
            audio: cache.clone(),
            position: 17,
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 32];
            reader.read_exact(&mut bytes).unwrap();
            tx.send(bytes).unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            bytes[17..49]
        );
        let weak = Arc::downgrade(&cache);
        let state = cache.state.clone();
        cache.cancel();
        assert_eq!(cache.buffered_bytes(), 0);
        drop(cache);
        wait_until(|| weak.upgrade().is_none());
        server.release.store(true, Ordering::SeqCst);
        wait_until(|| Arc::strong_count(&state) == 1);
        assert!(
            state.chunks.lock().unwrap().is_empty(),
            "An evicted request repopulated the cache"
        );
    }

    #[test]
    fn aes_ctr_matches_independent_zero_counter_vector() {
        let key = std::array::from_fn(|i| i as u8);
        let mut cipher = AesCtr::new(&key.into(), &[0u8; 16].into());
        let mut bytes: Vec<_> = (0..33).collect();
        cipher.apply_keystream(&mut bytes);
        assert_eq!(
            bytes,
            [
                198, 160, 57, 52, 131, 138, 93, 133, 103, 70, 139, 105, 173, 197, 214, 118, 99, 87,
                1, 134, 129, 213, 162, 9, 81, 98, 167, 248, 121, 233, 51, 21, 105
            ]
        );
    }

    #[test]
    fn encrypted_http_ranges_support_unaligned_seeks_and_rewinds() {
        let data: Vec<u8> = (0..HTTP_CHUNK * 2 + 123).map(|n| (n % 251) as u8).collect();
        let key = std::array::from_fn(|i| i as u8);
        let mut encrypted = data.clone();
        AesCtr::new(&key.into(), &[0u8; 16].into()).apply_keystream(&mut encrypted);
        let (url, stop, server) = range_server(encrypted);
        let result = (|| -> io::Result<()> {
            let mut reader = HttpReader::new(url, Some(key));
            let mut bytes = [0u8; 37];
            for position in [0, HTTP_CHUNK + 3, 17, HTTP_CHUNK * 2 + 1] {
                reader.seek(SeekFrom::Start(position as u64))?;
                reader.read_exact(&mut bytes)?;
                assert_eq!(bytes, data[position..position + bytes.len()]);
            }
            assert_eq!(reader.seek(SeekFrom::End(-1))?, data.len() as u64 - 1);
            let mut last = [0u8; 1];
            reader.read_exact(&mut last)?;
            assert_eq!(last[0], *data.last().unwrap());
            assert_eq!(reader.read(&mut last)?, 0);
            Ok(())
        })();
        drop(stop);
        server.join().unwrap();
        result.unwrap();
    }

    #[test]
    fn decodes_encrypted_http_flac_mp4_with_metadata_at_the_end() {
        let fixture = include_bytes!("../tests/fixtures/silence-flac.m4a");
        let ftyp_len = u32::from_be_bytes(fixture[..4].try_into().unwrap()) as usize;
        let padding_len = HTTP_CHUNK * 2 + 17;
        let mut bytes = fixture[..ftyp_len].to_vec();
        bytes.extend_from_slice(&(padding_len as u32).to_be_bytes());
        bytes.extend_from_slice(b"free");
        bytes.resize(bytes.len() + padding_len - 8, 0);
        bytes.extend_from_slice(&fixture[ftyp_len..]);
        let stco = bytes.windows(4).position(|s| s == b"stco").unwrap();
        bytes[stco + 12..stco + 16]
            .copy_from_slice(&((ftyp_len + padding_len + 8) as u32).to_be_bytes());
        let key = std::array::from_fn(|i| i as u8);
        AesCtr::new(&key.into(), &[0u8; 16].into()).apply_keystream(&mut bytes);
        let (url, stop, server) = range_server(bytes);
        let result = prepare(
            Input::Http {
                url,
                aes128_key: Some(key),
                hint: "m4a",
                quality: "FLAC · lossless".into(),
            },
            Arc::new(AtomicBool::new(false)),
        );
        drop(stop);
        let positions = server.join().unwrap();
        let prepared = result.unwrap();
        let progress = prepared.progress.clone();
        for _ in prepared.source {}
        assert_eq!(progress.samples.load(Ordering::Relaxed), 2205);
        assert!(progress.error.lock().unwrap().is_none());
        assert!(positions.iter().any(|(p, _)| *p > HTTP_CHUNK));
    }

    #[test]
    fn decodes_flac_and_flac_in_mp4() {
        for name in ["silence.flac", "silence-flac.m4a"] {
            let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            let prepared = prepare(Input::File(file), Arc::new(AtomicBool::new(false))).unwrap();
            let progress = prepared.progress.clone();
            for _ in prepared.source {}
            assert_eq!(progress.samples.load(Ordering::Relaxed), 2205, "{name}");
            assert!(progress.error.lock().unwrap().is_none());
        }
    }

    #[test]
    fn decodes_mp3_without_an_audio_device() {
        let file =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/silence.mp3");
        let prepared = prepare(Input::File(file), Arc::new(AtomicBool::new(false))).unwrap();
        let progress = prepared.progress.clone();
        for _ in prepared.source {}
        assert!(progress.samples.load(Ordering::Relaxed) >= 8820);
        assert!(progress.error.lock().unwrap().is_none());
    }

    #[test]
    fn http_reader_requests_bounded_ranges_and_preserves_bytes() {
        use std::{
            io::{BufRead, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/track", listener.local_addr().unwrap());
        let data: Vec<u8> = (0..HTTP_CHUNK + 123).map(|n| (n % 251) as u8).collect();
        let expected = data.clone();
        let server = std::thread::spawn(move || {
            for start in [0, HTTP_CHUNK] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains(&format!("range: bytes={start}-{}", start + HTTP_CHUNK - 1))
                );
                let end = (start + HTTP_CHUNK).min(data.len());
                write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", end - 1, data.len(), end - start).unwrap();
                stream.write_all(&data[start..end]).unwrap();
            }
        });
        let mut actual = Vec::new();
        HttpReader::new(url, None).read_to_end(&mut actual).unwrap();
        server.join().unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn underrun_is_silence_and_does_not_count_as_listened_audio() {
        let (tx, rx) = mpsc::sync_channel(1);
        let progress = Arc::new(Progress::default());
        let mut source = PcmSource {
            receiver: rx,
            block: vec![].into_iter(),
            block_levels: vec![].into_iter(),
            level_samples: 0,
            channels: 2,
            sample_rate: 48000,
            progress: progress.clone(),
        };
        assert_eq!(source.next(), Some(0.0));
        assert!(progress.buffering.load(Ordering::Relaxed));
        assert_eq!(progress.samples.load(Ordering::Relaxed), 0);
        tx.send(PcmBlock {
            samples: vec![0.5, -0.5],
            levels: vec![
                Levels {
                    rms: 0.5,
                    peak: 0.5,
                    ..Levels::default()
                }
                .pack(),
            ],
        })
        .unwrap();
        assert_eq!(source.next(), Some(0.5));
        assert!(Levels::unpack(progress.levels.load(Ordering::Relaxed)).rms > 0.49);
        assert_eq!(source.next(), Some(-0.5));
        assert_eq!(progress.samples.load(Ordering::Relaxed), 2);
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(progress.levels.load(Ordering::Relaxed), 0);
        drop(tx);
        assert_eq!(source.next(), None);
    }

    #[test]
    fn preloaded_audio_levels_are_published_only_when_samples_are_played() {
        let file =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac");
        let mut prepared = prepare(Input::File(file), Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(prepared.progress.levels.load(Ordering::Relaxed), 0);
        assert_eq!(prepared.progress.samples.load(Ordering::Relaxed), 0);
        assert!(prepared.source.next().is_some());
        let levels = Levels::unpack(prepared.progress.levels.load(Ordering::Relaxed));
        assert!(levels.rms > 0.1 && levels.peak > 0.19);
        assert_eq!(prepared.progress.samples.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn meter_detects_treble_attacks_and_releases_before_the_next_hit() {
        let tone: Vec<f32> = (0..1024)
            .map(|i| (i as f32 * std::f32::consts::TAU * 3000.0 / 44100.0).sin() * 0.2)
            .collect();
        let mut meter = Meter::new(1, 44100);
        let attack = Levels::unpack(meter.analyze(&tone));
        assert!(
            attack.pulse > 0.8,
            "A treble attack did not trigger the wave"
        );
        let silence = [0.0; 1024];
        for _ in 0..5 {
            meter.analyze(&silence);
        }
        assert!(meter.pulse < 0.3, "Pulse release smears fast percussion");
        assert!(Levels::unpack(meter.analyze(&tone)).pulse > 0.8);
        for _ in 0..100 {
            meter.analyze(&tone);
        }
        assert!(
            meter.pulse < 0.01,
            "A steady treble tone creates fake beats"
        );
    }

    #[test]
    fn meter_detects_bass_attacks_without_pulsing_on_a_steady_tone() {
        let tone = |frequency: f32| -> Vec<f32> {
            (0..4410)
                .map(|i| 0.2 * (std::f32::consts::TAU * frequency * i as f32 / 44100.0).sin())
                .collect()
        };
        let bass = tone(80.0);
        let mut meter = Meter::new(1, 44100);
        let onset = Levels::unpack(meter.analyze(&bass));
        assert!(onset.rms > 0.14 && onset.bass > 0.12 && onset.pulse > 0.9);
        for _ in 0..25 {
            meter.analyze(&bass);
        }
        let steady = Levels::unpack(meter.analyze(&bass));
        assert!(steady.pulse < 0.01, "Steady tone creates fake beats");
        for _ in 0..8 {
            meter.analyze(&vec![0.0; 4410]);
        }
        assert!(Levels::unpack(meter.analyze(&bass)).pulse > 0.9);
        let treble = Levels::unpack(Meter::new(1, 44100).analyze(&tone(1000.0)));
        assert!((treble.rms - onset.rms).abs() < 0.001);
        assert!(treble.bass < onset.bass * 0.25);
        let stereo: Vec<f32> = bass
            .into_iter()
            .flat_map(|sample| [sample, -sample])
            .collect();
        let stereo = Levels::unpack(Meter::new(2, 44100).analyze(&stereo));
        assert!(
            (stereo.bass - onset.bass).abs() < 0.001,
            "Opposite channel phases cancel the meter"
        );
    }

    #[test]
    fn seeks_http_audio_forward_and_back_with_shared_ram_cache() {
        let cases = [
            (
                "MP3",
                "mp3",
                include_bytes!("../tests/fixtures/seek.mp3").to_vec(),
            ),
            (
                "FLAC",
                "flac",
                include_bytes!("../tests/fixtures/seek.flac").to_vec(),
            ),
            ("FLAC/MP4 moov first", "m4a", long_flac_mp4(true)),
            ("FLAC/MP4 moov last", "m4a", long_flac_mp4(false)),
        ];
        for (name, hint, bytes) in cases {
            for encrypted in [false, true] {
                let key = encrypted.then(|| std::array::from_fn(|i| i as u8));
                let mut data = bytes.clone();
                if let Some(key) = key {
                    AesCtr::new(&key.into(), &[0u8; 16].into()).apply_keystream(&mut data);
                }
                for position in [31.0, 7.0] {
                    let (url, stop, server) = range_server(data.clone());
                    let mut reference = Decoder::builder()
                        .with_data(io::Cursor::new(bytes.clone()))
                        .with_byte_len(bytes.len() as u64)
                        .with_hint(hint)
                        .build()
                        .unwrap();
                    let offset = (position
                        * f64::from(reference.sample_rate())
                        * f64::from(reference.channels()))
                        as usize;
                    let expected: Vec<f32> = reference
                        .by_ref()
                        .skip(offset)
                        .take(BLOCK_SAMPLES)
                        .collect();
                    let cancel = Arc::new(AtomicBool::new(false));
                    let mut prepared = prepare_at(
                        cache_input(Input::Http {
                            url: url.clone(),
                            aes128_key: key,
                            hint,
                            quality: name.into(),
                        })
                        .unwrap(),
                        cancel.clone(),
                        position,
                    )
                    .unwrap_or_else(|error| {
                        panic!("{name}, encrypted={encrypted}, position={position}: {error:#}")
                    });
                    assert_eq!(prepared.position, position);
                    let actual: Vec<f32> = prepared.source.by_ref().take(BLOCK_SAMPLES).collect();
                    assert_eq!(actual.len(), expected.len());
                    let error = actual
                        .iter()
                        .zip(&expected)
                        .skip(2048)
                        .map(|(a, b)| (a - b).abs())
                        .sum::<f32>()
                        / (BLOCK_SAMPLES - 2048) as f32;
                    if hint == "mp3" {
                        let frequency = |samples: &[f32]| {
                            samples[2048..]
                                .windows(2)
                                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                                .count() as f32
                                * prepared.samples_per_second as f32
                                / 2048.0
                        };
                        assert!(
                            (frequency(&actual) - frequency(&expected)).abs() < 25.0,
                            "MP3 seek landed far from the requested position"
                        );
                    } else {
                        assert!(
                            error < 0.005,
                            "{name}, encrypted={encrypted}, position={position}: PCM mismatch {error}"
                        );
                    }
                    cancel.store(true, Ordering::Relaxed);
                    drop(prepared);
                    drop(stop);
                    let positions = server.join().unwrap();
                    let transferred: usize = positions.iter().map(|(_, count)| count).sum();
                    assert!(positions.iter().any(|(start, _)| *start >= HTTP_CHUNK));
                    assert!(
                        transferred <= bytes.len(),
                        "Downloaded overlapping ranges: {positions:?}"
                    );
                }
            }
        }
    }
}
