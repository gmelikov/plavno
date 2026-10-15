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
fn hiding_visualization_discards_the_queued_cover_request() {
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
