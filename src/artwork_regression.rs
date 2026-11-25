use super::*;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    time::Instant,
};

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn record(size: usize) {
    if MEASURING.try_with(Cell::get).unwrap_or(false) {
        let _ = LARGEST.try_with(|value| value.set(value.get().max(size)));
    }
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[test]
fn large_png_cover_is_downsampled_without_a_full_size_buffer() {
    let mut bytes = Cursor::new(Vec::new());
    let cover = image::RgbaImage::from_fn(2048, 2048, |x, y| {
        image::Rgba([if x < 1024 { 20 } else { 200 }, 40, 60, y as u8])
    });
    image::DynamicImage::ImageRgba8(cover)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();

    LARGEST.set(0);
    MEASURING.set(true);
    let thumbnail = decode(bytes.get_ref());
    MEASURING.set(false);

    let thumbnail = thumbnail.unwrap();
    assert_eq!(thumbnail.dimensions(), (192, 192));
    assert_eq!(thumbnail.get_pixel(10, 90).0, [20, 40, 60]);
    assert_eq!(thumbnail.get_pixel(182, 90).0, [200, 40, 60]);
    let largest = LARGEST.get();
    assert!(largest < 1024 * 1024, "Decoder allocated {largest} bytes");
}

#[test]
fn interlaced_indexed_png_matches_the_full_image_thumbnail() {
    let bytes = include_bytes!("../tests/fixtures/cover-adam7.png");
    let expected = image::load_from_memory(bytes)
        .unwrap()
        .thumbnail(192, 192)
        .into_rgb8();

    let thumbnail = decode(bytes).unwrap();
    assert_eq!(thumbnail.dimensions(), (192, 128));
    assert_eq!(thumbnail, expected);
}

#[test]
fn large_jpeg_and_webp_covers_scale_into_small_output_buffers() {
    let image = image::DynamicImage::ImageRgb8(RgbImage::from_fn(1536, 1024, |x, _| {
        image::Rgb(if x < 768 { [20, 40, 60] } else { [200, 80, 30] })
    }));
    for format in [image::ImageFormat::Jpeg, image::ImageFormat::WebP] {
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();

        LARGEST.set(0);
        MEASURING.set(true);
        let thumbnail = decode(bytes.get_ref());
        MEASURING.set(false);

        let thumbnail = thumbnail.unwrap();
        assert_eq!(thumbnail.dimensions(), (192, 128), "{format:?}");
        assert!(thumbnail.get_pixel(10, 60)[0] < 25, "{format:?}");
        assert!(thumbnail.get_pixel(182, 60)[0] > 195, "{format:?}");
        let largest = LARGEST.get();
        assert!(
            largest < 1024 * 1024,
            "{format:?} allocated {largest} bytes"
        );
    }
}

#[test]
fn narrow_covers_keep_render_allocations_bounded_by_the_viewport() {
    for (dimensions, area) in [
        ((1, 2048), Rect::new(0, 0, 200, 20)),
        ((2048, 1), Rect::new(0, 0, 20, 100)),
    ] {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(RgbImage::from_pixel(
            dimensions.0,
            dimensions.1,
            image::Rgb([200, 100, 50]),
        ))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
        let mut artwork = Artwork::new().unwrap();
        artwork.image = Some(Arc::new(decode(bytes.get_ref()).unwrap()));
        let mut buffer = Buffer::empty(area);
        LARGEST.set(0);
        MEASURING.set(true);
        artwork.render(area, &mut buffer);
        MEASURING.set(false);
        let largest = LARGEST.get();
        eprintln!("Largest render allocation for {dimensions:?}: {largest} bytes");
        assert!(
            largest < 2 * 1024 * 1024,
            "Render allocated {largest} bytes for {dimensions:?}"
        );
        assert_eq!(
            artwork.raster.as_ref().unwrap().dimensions(),
            (u32::from(area.width), u32::from(area.height) * 2)
        );
    }
}

#[test]
fn cropping_preserves_the_center_for_wide_and_tall_viewports() {
    for area in [Rect::new(0, 0, 40, 4), Rect::new(0, 0, 4, 20)] {
        let image = RgbImage::from_fn(192, 192, |x, y| {
            let center = if area.width > area.height * 2 {
                (75..117).contains(&y)
            } else {
                (84..108).contains(&x)
            };
            image::Rgb(if center { [0, 255, 0] } else { [255, 0, 255] })
        });
        let mut artwork = Artwork::new().unwrap();
        artwork.image = Some(Arc::new(image));
        let mut buffer = Buffer::empty(area);
        artwork.render(area, &mut buffer);
        assert!(buffer.content.iter().all(|cell| {
            cell.fg == ratatui::style::Color::Rgb(0, 56, 0)
                && cell.bg == ratatui::style::Color::Rgb(0, 56, 0)
        }));
    }
}

fn read_path(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    let path = request.split_whitespace().nth(1).unwrap().to_owned();
    loop {
        let start = request.len();
        assert!(reader.read_line(&mut request).unwrap() > 0);
        assert!(request.len() < 4096);
        if &request[start..] == "\r\n" {
            break;
        }
    }
    path
}

fn queued_request_after_change(next: Option<&str>, cached: bool) -> Option<String> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (started, observed) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let serving = std::thread::spawn(move || {
        for index in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(2);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("Fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            started.send(read_path(&mut stream)).unwrap();
            if index == 0 {
                released.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        }
    });
    let mut artwork = Artwork::new().unwrap();
    artwork.update(Some(&format!("{base}/a")));
    assert_eq!(observed.recv_timeout(Duration::from_secs(3)).unwrap(), "/a");
    artwork.update(Some(&format!("{base}/b")));
    let next = next.map(|path| format!("{base}{path}"));
    if cached {
        artwork
            .cache
            .push_back((next.clone().unwrap(), Arc::new(RgbImage::new(2, 2))));
    }
    artwork.update(next.as_deref());
    release.send(()).unwrap();
    let second = observed.recv_timeout(Duration::from_secs(3)).ok();
    drop(artwork);
    serving.join().unwrap();
    second
}

#[test]
fn clearing_the_cover_discards_the_queued_cover_request() {
    assert_eq!(queued_request_after_change(None, false), None);
}

#[test]
fn latest_cover_replaces_the_obsolete_queued_request() {
    assert_eq!(
        queued_request_after_change(Some("/c"), false).as_deref(),
        Some("/c")
    );
}

#[test]
fn selecting_a_cached_cover_discards_the_obsolete_queued_request() {
    assert_eq!(queued_request_after_change(Some("/c"), true), None);
}
