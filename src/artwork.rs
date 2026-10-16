use image::{ImageReader, RgbImage, imageops::FilterType};
use ratatui::{buffer::Buffer, layout::Rect, style::Color};
use std::{
    collections::VecDeque,
    io::Cursor,
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver},
    },
    time::Duration,
};

type Cover = Arc<RgbImage>;
type Reply = (String, Option<Cover>);

#[derive(Default)]
struct Pending {
    url: Option<String>,
    closed: bool,
}

#[derive(Default)]
struct Requests {
    pending: Mutex<Pending>,
    ready: Condvar,
}

pub struct Artwork {
    wanted: Option<String>,
    submitted: Option<String>,
    requests: Arc<Requests>,
    replies: Receiver<Reply>,
    cache: VecDeque<(String, Cover)>,
    image: Option<Cover>,
    raster: Option<RgbImage>,
}

impl Artwork {
    pub fn new() -> std::io::Result<Self> {
        let requests = Arc::new(Requests::default());
        let worker_requests = requests.clone();
        let (sender, replies) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("artwork".into())
            .spawn(move || {
                let http: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(8)))
                    .build()
                    .into();
                loop {
                    let url = {
                        let mut pending = worker_requests.pending.lock().unwrap();
                        while pending.url.is_none() && !pending.closed {
                            pending = worker_requests.ready.wait(pending).unwrap();
                        }
                        if pending.closed {
                            break;
                        }
                        pending.url.take().unwrap()
                    };
                    let image = fetch(&http, &url).map(Arc::new);
                    if sender.send((url, image)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            wanted: None,
            submitted: None,
            requests,
            replies,
            cache: VecDeque::new(),
            image: None,
            raster: None,
        })
    }

    pub fn update(&mut self, url: Option<&str>) -> bool {
        let mut changed = false;
        if self.wanted.as_deref() != url {
            self.wanted = url.map(str::to_owned);
            self.submitted = None;
            self.requests.pending.lock().unwrap().url = None;
            self.image = self
                .cache
                .iter()
                .find(|(key, _)| Some(key.as_str()) == url)
                .map(|(_, image)| image.clone());
            self.raster = None;
            changed = true;
        }
        while let Ok((key, image)) = self.replies.try_recv() {
            if let Some(image) = image {
                self.cache.retain(|(cached, _)| *cached != key);
                self.cache.push_back((key.clone(), image.clone()));
                while self.cache.len() > 3 {
                    self.cache.pop_front();
                }
                if self.wanted.as_deref() == Some(&key) {
                    self.image = Some(image);
                    self.raster = None;
                    changed = true;
                }
            }
        }
        if self.image.is_none()
            && self.wanted != self.submitted
            && let Some(url) = &self.wanted
        {
            self.requests.pending.lock().unwrap().url = Some(url.clone());
            self.submitted = self.wanted.clone();
            self.requests.ready.notify_one();
        }
        changed
    }

    pub fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        let Some(image) = &self.image else { return };
        if area.is_empty() {
            return;
        }
        // Two vertical pixels per cell compensate for the terminal's tall glyphs.
        let size = (u32::from(area.width), u32::from(area.height) * 2);
        if self.raster.as_ref().map(RgbImage::dimensions) != Some(size) {
            let (width, height) = image.dimensions();
            let (crop_width, crop_height) = if width * size.1 > height * size.0 {
                ((height * size.0 / size.1).max(1), height)
            } else {
                (width, (width * size.1 / size.0).max(1))
            };
            let cropped = image::imageops::crop_imm(
                &**image,
                (width - crop_width) / 2,
                (height - crop_height) / 2,
                crop_width,
                crop_height,
            )
            .to_image();
            let mut raster =
                image::imageops::resize(&cropped, size.0, size.1, FilterType::Triangle);
            for pixel in raster.pixels_mut() {
                for channel in &mut pixel.0 {
                    *channel = (u16::from(*channel) * 22 / 100) as u8;
                }
            }
            self.raster = Some(raster);
        }
        let raster = self.raster.as_ref().unwrap();
        for y in 0..area.height {
            for x in 0..area.width {
                let top = raster.get_pixel(u32::from(x), u32::from(y) * 2).0;
                let bottom = raster.get_pixel(u32::from(x), u32::from(y) * 2 + 1).0;
                buffer[(area.x + x, area.y + y)]
                    .set_char('▀')
                    .set_fg(Color::Rgb(top[0], top[1], top[2]))
                    .set_bg(Color::Rgb(bottom[0], bottom[1], bottom[2]));
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub fn image(&self, url: Option<&str>) -> Option<&Arc<RgbImage>> {
        // The engine may have changed tracks before the artwork worker was polled.
        (self.wanted.as_deref() == url)
            .then_some(self.image.as_ref())
            .flatten()
    }
}

impl Drop for Artwork {
    fn drop(&mut self) {
        let mut pending = self.requests.pending.lock().unwrap();
        pending.closed = true;
        pending.url = None;
        self.requests.ready.notify_one();
    }
}

fn fetch(http: &ureq::Agent, url: &str) -> Option<RgbImage> {
    if let Some(file_url) = url.strip_prefix(crate::local_metadata::COVER_PREFIX) {
        let path = url::Url::parse(file_url).ok()?.to_file_path().ok()?;
        let metadata = crate::local_metadata::read(&path).ok()?;
        return decode(metadata.cover.as_deref()?);
    }
    let url = url::Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let bytes = http
        .get(url.as_str())
        .call()
        .ok()?
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_vec()
        .ok()?;
    decode(&bytes)
}

fn decode(bytes: &[u8]) -> Option<RgbImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    Some(reader.decode().ok()?.thumbnail(192, 192).to_rgb8())
}

#[cfg(test)]
#[path = "artwork_regression.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wave::Wave;
    use ratatui::widgets::Widget;

    fn artwork(image: RgbImage) -> Artwork {
        let requests = Arc::new(Requests::default());
        let (_, replies) = mpsc::sync_channel(1);
        Artwork {
            wanted: Some("cover".into()),
            submitted: Some("cover".into()),
            requests,
            replies,
            cache: VecDeque::new(),
            image: Some(Arc::new(image)),
            raster: None,
        }
    }

    #[test]
    fn wave_overlays_artwork_without_erasing_its_background() {
        let area = Rect::new(0, 0, 20, 8);
        let mut buffer = Buffer::empty(area);
        let mut artwork = artwork(RgbImage::from_pixel(12, 12, image::Rgb([200, 100, 50])));
        let inner = Rect::new(1, 1, 18, 6);
        artwork.render(inner, &mut buffer);
        (&Wave::default()).render(inner, &mut buffer);
        let cells = buffer
            .content
            .iter()
            .filter(|cell| cell.bg == Color::Rgb(44, 22, 11));
        assert_eq!(cells.clone().count(), 108);
        assert!(cells.clone().any(|cell| cell.symbol() == "▀"));
        assert!(cells.clone().any(|cell| {
            cell.symbol()
                .chars()
                .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
        }));
        let raster = artwork.raster.as_ref().unwrap().as_ptr();
        artwork.render(inner, &mut buffer);
        assert_eq!(artwork.raster.as_ref().unwrap().as_ptr(), raster);
        artwork.render(Rect::new(1, 1, 10, 3), &mut buffer);
        assert_eq!(artwork.raster.as_ref().unwrap().dimensions(), (10, 6));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn now_playing_does_not_receive_the_previous_tracks_cover() {
        let artwork = artwork(RgbImage::new(2, 2));
        assert!(artwork.image(Some("cover")).is_some());
        assert!(artwork.image(Some("next-cover")).is_none());
        assert!(artwork.image(None).is_none());
    }

    #[test]
    fn late_previous_cover_does_not_replace_the_current_album() {
        let mut artwork = artwork(RgbImage::new(2, 2));
        let (sender, receiver) = mpsc::sync_channel(1);
        artwork.replies = receiver;
        sender
            .send(("old-cover".into(), Some(Arc::new(RgbImage::new(3, 3)))))
            .unwrap();
        artwork.update(Some("new-cover"));
        assert!(artwork.image.is_none());
        assert!(artwork.raster.is_none());
        artwork.update(Some("old-cover"));
        assert_eq!(artwork.image.as_ref().unwrap().dimensions(), (3, 3));
        artwork.update(None);
        assert!(artwork.image.is_none());
    }

    #[test]
    fn decode_rejects_invalid_and_oversized_images_and_keeps_small_thumbnails() {
        assert!(decode(b"not an image").is_none());
        for (width, expected) in [(600, Some((192, 1))), (2049, None)] {
            let image = RgbImage::new(width, 1);
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(image)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            assert_eq!(
                decode(bytes.get_ref()).map(|image| image.dimensions()),
                expected
            );
        }
    }

    #[test]
    fn worker_loads_embedded_front_covers_and_reuses_them_after_hiding() {
        use std::time::Instant;
        for extension in ["flac", "mp3", "m4a", "ogg"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/tagged.{extension}"));
            let url = format!(
                "{}{}",
                crate::local_metadata::COVER_PREFIX,
                url::Url::from_file_path(path).unwrap()
            );
            let mut artwork = Artwork::new().unwrap();
            let start = Instant::now();
            while artwork.image.is_none() {
                artwork.update(Some(&url));
                assert!(start.elapsed() < Duration::from_secs(3), "{extension}");
                std::thread::sleep(Duration::from_millis(5));
            }
            let image = artwork.image.as_ref().unwrap().clone();
            assert_eq!(image.dimensions(), (192, 192), "{extension}");
            assert_eq!(image.get_pixel(0, 0).0, [20, 40, 60], "{extension}");
            artwork.update(None);
            artwork.update(Some(&url));
            assert!(Arc::ptr_eq(&image, artwork.image.as_ref().unwrap()));
            assert!(artwork.submitted.is_none());
        }
    }

    #[test]
    fn invalid_local_cover_references_and_files_without_covers_do_not_produce_artwork() {
        let http = ureq::agent();
        for url in [
            "embedded:not a URL",
            "embedded:file:///nonexistent/plavno.flac",
        ] {
            assert!(fetch(&http, url).is_none());
        }
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/silence.flac");
        let url = format!(
            "{}{}",
            crate::local_metadata::COVER_PREFIX,
            url::Url::from_file_path(path).unwrap()
        );
        assert!(fetch(&http, &url).is_none());
    }

    #[test]
    fn worker_loads_http_artwork_once_and_reuses_it_after_hiding() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
            time::Instant,
        };
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(RgbImage::from_pixel(4, 4, image::Rgb([20, 40, 60])))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/cover.png", server.local_addr().unwrap());
        let serving = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            loop {
                let start = request.len();
                assert!(reader.read_line(&mut request).unwrap() > 0);
                assert!(request.len() < 4096);
                if &request[start..] == "\r\n" {
                    break;
                }
            }
            assert!(request.starts_with("GET /cover.png HTTP/1.1\r\n"));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                encoded.get_ref().len()
            )
            .unwrap();
            stream.write_all(encoded.get_ref()).unwrap();
        });
        let mut artwork = Artwork::new().unwrap();
        let start = Instant::now();
        while artwork.image.is_none() {
            artwork.update(Some(&url));
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "Cover loading timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        serving.join().unwrap();
        let image = artwork.image.as_ref().unwrap().clone();
        artwork.update(None);
        artwork.update(Some(&url));
        assert!(Arc::ptr_eq(&image, artwork.image.as_ref().unwrap()));
        assert!(
            artwork.submitted.is_none(),
            "A cached cover triggered another HTTP request"
        );
        assert_eq!(artwork.cache.len(), 1);
    }
}
