use rodio::{Decoder, Source};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct Server {
    url: String,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(files: HashMap<String, Vec<u8>>, hls: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let files = Arc::new(files);
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let gate = release.clone();
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let (files, gate, stop) = (files.clone(), gate.clone(), stopped.clone());
                        requests.push(thread::spawn(move || {
                            serve(socket, &files, &gate, &stop, hls)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("Fixture server failed: {error}"),
                }
            }
            for request in requests {
                request.join().unwrap();
            }
        });
        Self {
            url,
            release,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.release.store(true, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(
    mut socket: TcpStream,
    files: &HashMap<String, Vec<u8>>,
    gate: &AtomicBool,
    stop: &AtomicBool,
    hls: bool,
) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .trim_start_matches('/')
        .split('?')
        .next()
        .unwrap();
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
            range = Some(value.trim().to_owned());
        }
    }
    let Some(bytes) = files.get(path) else {
        let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    let (start, end) =
        range
            .as_ref()
            .and_then(|r| r.split_once('-'))
            .map_or((0, bytes.len() - 1), |(a, b)| {
                (
                    a.parse::<usize>().unwrap(),
                    b.parse::<usize>()
                        .unwrap_or(bytes.len() - 1)
                        .min(bytes.len() - 1),
                )
            });
    let code = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let content = if path.ends_with("m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "application/octet-stream"
    };
    let header = format!(
        "HTTP/1.1 {code}\r\nContent-Type: {content}\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        end + 1 - start,
        bytes.len()
    );
    if socket.write_all(header.as_bytes()).is_err() {
        return;
    }
    let prefix = if hls && path == "seg004.ts" {
        start
    } else if !hls {
        (128 * 1024).clamp(start, end + 1)
    } else {
        end + 1
    };
    if socket.write_all(&bytes[start..prefix]).is_err() {
        return;
    }
    if prefix < end + 1 {
        while !gate.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(2));
        }
        let _ = socket.write_all(&bytes[prefix..=end]);
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGTERM);
            }
            let _ = self.0.wait();
        }
    }
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn check(hls: bool, encrypted: bool) {
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "plavno-native-stream-{}-{hls}-{encrypted}",
        std::process::id()
    )));
    fs::create_dir_all(&scratch.0).unwrap();
    let runtime = scratch.0.join("runtime");
    fs::create_dir(&runtime).unwrap();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/seek.flac");
    let mut files = HashMap::new();
    if hls {
        let ffmpeg = std::env::var_os("PLAVNO_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
        let mut command = Command::new(ffmpeg);
        command
            .current_dir(&scratch.0)
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&fixture)
            .args([
                "-t",
                "12",
                "-c:a",
                "aac",
                "-b:a",
                "256k",
                "-hls_time",
                "2",
                "-hls_list_size",
                "0",
                "-hls_segment_filename",
                "seg%03d.ts",
            ]);
        if encrypted {
            fs::write(scratch.0.join("key.bin"), [0x39; 16]).unwrap();
            fs::write(
                scratch.0.join("keyinfo"),
                "key.bin\nkey.bin\n00000000000000000000000000000000\n",
            )
            .unwrap();
            command.args(["-hls_key_info_file", "keyinfo"]);
        }
        assert!(
            command
                .args(["-f", "hls", "list.m3u8"])
                .status()
                .unwrap()
                .success()
        );
        for file in fs::read_dir(&scratch.0)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_file())
        {
            files.insert(
                file.file_name().to_string_lossy().into_owned(),
                fs::read(file.path()).unwrap(),
            );
        }
    } else {
        files.insert("audio.flac".into(), fs::read(&fixture).unwrap());
    }
    let server = Server::new(files, hls);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_plavno"))
            .arg("--stream-audio")
            .current_dir(&runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let request = serde_json::json!({"url":format!("{}/{}",server.url,if hls {"list.m3u8"} else {"audio.flac"})});
    writeln!(process.0.stdin.take().unwrap(), "{request}").unwrap();
    let mut stdout = process.0.stdout.take().unwrap();
    let (ready, started) = mpsc::channel();
    let output = thread::spawn(move || {
        let mut output = Vec::new();
        let mut ready = Some(ready);
        loop {
            let mut chunk = [0; 8192];
            let count = stdout.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            output.extend_from_slice(&chunk[..count]);
            if output.len() > 8192
                && let Some(ready) = ready.take()
            {
                ready.send(()).unwrap();
            }
        }
        output
    });
    assert!(
        started.recv_timeout(Duration::from_secs(8)).is_ok(),
        "Audio did not start before the remaining HTTP data was released"
    );
    assert!(
        process.0.try_wait().unwrap().is_none(),
        "Stream finished while the remaining bytes were blocked"
    );
    server.release.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "Native stream did not finish");
        thread::sleep(Duration::from_millis(5));
    };
    let bytes = output.join().unwrap();
    assert!(status.success(), "Native stream failed");
    assert_eq!(&bytes[..4], b"fLaC");
    let decoder = Decoder::try_from(BufReader::new(Cursor::new(bytes))).unwrap();
    let rate = decoder.sample_rate() as usize * usize::from(decoder.channels());
    let samples = decoder.count();
    assert!(samples > 10 * rate);
    if hls {
        assert!(samples < 13 * rate);
    }
    assert_eq!(
        fs::read_dir(&runtime).unwrap().count(),
        0,
        "Streaming wrote audio or fragment files to disk"
    );
}

#[test]
#[ignore = "Requires ffmpeg on PATH or PLAVNO_FFMPEG"]
fn native_http_audio_starts_before_full_download() {
    check(false, false);
}

#[test]
#[ignore = "Requires ffmpeg on PATH or PLAVNO_FFMPEG"]
fn native_hls_audio_starts_before_full_download() {
    check(true, false);
}

#[test]
#[ignore = "Requires ffmpeg on PATH or PLAVNO_FFMPEG"]
fn native_aes_hls_audio_starts_before_full_download() {
    check(true, true);
}
