use image::{ImageFormat, ImageReader, Rgb, RgbImage, imageops::FilterType};
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

const COVER_SIZE: u32 = 192;
const MAX_IMAGE_SIZE: u32 = 2048;
const MAX_DECODE_BYTES: usize = 32 * 1024 * 1024;

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

    #[cfg(any(target_os = "macos", windows))]
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
    if reader.format() == Some(ImageFormat::Png) {
        let mut decoder = png::Decoder::new_with_limits(
            Cursor::new(bytes),
            png::Limits {
                bytes: MAX_DECODE_BYTES,
            },
        );
        decoder.set_ignore_text_chunk(true);
        decoder.set_ignore_iccp_chunk(true);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let info = decoder.read_header_info().ok()?;
        let longest = info.width.max(info.height);
        if longest > MAX_IMAGE_SIZE {
            return None;
        }
        if longest > COVER_SIZE {
            return png_thumbnail(decoder.read_info().ok()?);
        }
    }
    let scaled = match reader.format() {
        Some(ImageFormat::Jpeg) => jpeg_thumbnail(bytes),
        Some(ImageFormat::WebP) => webp_thumbnail(bytes),
        _ => None,
    };
    if scaled.is_some() {
        return scaled;
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIZE);
    limits.max_image_height = Some(MAX_IMAGE_SIZE);
    limits.max_alloc = Some(MAX_DECODE_BYTES as u64);
    reader.limits(limits);
    Some(
        reader
            .decode()
            .ok()?
            .thumbnail(COVER_SIZE, COVER_SIZE)
            .into_rgb8(),
    )
}

fn thumbnail_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    let scaled = |size: u32| ((size * COVER_SIZE + longest / 2) / longest).max(1);
    (scaled(width), scaled(height))
}

fn jpeg_thumbnail(bytes: &[u8]) -> Option<RgbImage> {
    use jpeg_decoder::PixelFormat;

    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    decoder.set_max_decoding_buffer_size(MAX_DECODE_BYTES);
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    if !matches!(info.pixel_format, PixelFormat::RGB24 | PixelFormat::L8) {
        return None;
    }
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if !(COVER_SIZE + 1..=MAX_IMAGE_SIZE).contains(&width.max(height)) {
        return None;
    }
    let (width, height) = thumbnail_size(width, height);
    let (scaled_width, scaled_height) = decoder.scale(width as u16, height as u16).ok()?;
    let (scaled_width, scaled_height) = (u32::from(scaled_width), u32::from(scaled_height));
    let pixels = decoder.decode().ok()?;
    let image = match info.pixel_format {
        PixelFormat::RGB24 => RgbImage::from_raw(scaled_width, scaled_height, pixels)?,
        PixelFormat::L8 => {
            let image = image::GrayImage::from_raw(scaled_width, scaled_height, pixels)?;
            image::DynamicImage::ImageLuma8(image).into_rgb8()
        }
        _ => return None,
    };
    if (scaled_width, scaled_height) == (width, height) {
        return Some(image);
    }
    Some(image::imageops::thumbnail(&image, width, height))
}

fn webp_thumbnail(bytes: &[u8]) -> Option<RgbImage> {
    use libwebp_sys::{
        VP8StatusCode::VP8_STATUS_OK, WEBP_CSP_MODE::MODE_RGB, WebPDecode, WebPDecoderConfig,
        WebPFreeDecBuffer, WebPGetFeatures, WebPRGBABuffer,
    };

    let mut config = WebPDecoderConfig::new().ok()?;
    // The input slice and the initialized feature structure are valid for this call.
    if unsafe { WebPGetFeatures(bytes.as_ptr(), bytes.len(), &mut config.input) } != VP8_STATUS_OK
        || config.input.has_animation != 0
    {
        return None;
    }
    let (width, height) = (
        u32::try_from(config.input.width).ok()?,
        u32::try_from(config.input.height).ok()?,
    );
    if !(COVER_SIZE + 1..=MAX_IMAGE_SIZE).contains(&width.max(height)) {
        return None;
    }
    let (width, height) = thumbnail_size(width, height);
    let stride = width as usize * 3;
    let mut pixels = vec![0; stride * height as usize];
    config.options.use_scaling = 1;
    config.options.scaled_width = width as i32;
    config.options.scaled_height = height as i32;
    config.output.colorspace = MODE_RGB;
    config.output.is_external_memory = 1;
    config.output.u.RGBA = WebPRGBABuffer {
        rgba: pixels.as_mut_ptr(),
        stride: stride as i32,
        size: pixels.len(),
    };
    // libwebp writes RGB rows into the supplied buffer and leaves ownership with Rust.
    let status = unsafe {
        let status = WebPDecode(bytes.as_ptr(), bytes.len(), &mut config);
        WebPFreeDecBuffer(&mut config.output);
        status
    };
    if status != VP8_STATUS_OK {
        return None;
    }
    RgbImage::from_raw(width, height, pixels)
}

fn png_thumbnail(mut reader: png::Reader<Cursor<&[u8]>>) -> Option<RgbImage> {
    let (width, height) = reader.info().size();
    let (thumb_width, thumb_height) = thumbnail_size(width, height);
    let channels = reader.output_color_type().0.samples();
    let columns: Vec<_> = (0..width)
        .map(|x| (x * thumb_width / width) as usize)
        .collect();
    let mut sums = vec![[0u32; 3]; (thumb_width * thumb_height) as usize];

    // The decoder yields rows in pass order. Each tuple gives the first
    // source column and row, followed by their sampling steps.
    let passes: &[(u32, u32, usize, usize)] = if reader.info().interlaced {
        &[
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ]
    } else {
        &[(0, 0, 1, 1)]
    };
    for &(first_x, first_y, step_x, step_y) in passes {
        if first_x >= width {
            continue;
        }
        for y in (first_y..height).step_by(step_y) {
            let row = reader.next_row().ok()??;
            let offset = (y * thumb_height / height * thumb_width) as usize;
            let columns = columns[first_x as usize..].iter().step_by(step_x);
            for (&column, pixel) in columns.zip(row.data().chunks_exact(channels)) {
                let rgb = if channels < 3 {
                    [pixel[0]; 3]
                } else {
                    [pixel[0], pixel[1], pixel[2]]
                };
                for (sum, value) in sums[offset + column].iter_mut().zip(rgb) {
                    *sum += u32::from(value);
                }
            }
        }
    }
    if reader.next_row().ok()?.is_some() {
        return None;
    }

    let span = |index: u32, source: u32, target: u32| {
        ((index + 1) * source).div_ceil(target) - (index * source).div_ceil(target)
    };
    Some(RgbImage::from_fn(thumb_width, thumb_height, |x, y| {
        let count = span(x, width, thumb_width) * span(y, height, thumb_height);
        let rgb = sums[(y * thumb_width + x) as usize];
        Rgb(rgb.map(|sum| ((sum + count / 2) / count) as u8))
    }))
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
    #[cfg(any(target_os = "macos", windows))]
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
