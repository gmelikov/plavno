#![cfg(all(windows, feature = "test-audio"))]

use plavno_platform::{
    process::Job,
    windows::{
        check, owned,
        sys::Win32::{
            Foundation::*,
            System::{Console::*, Pipes::CreatePipe, Threading::*},
        },
        wide,
    },
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::windows::io::{AsRawHandle, OwnedHandle},
    ptr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[path = "support/terminal.rs"]
mod terminal;

struct Console(HPCON);
impl Drop for Console {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.0);
        }
    }
}

struct Attributes(Vec<usize>);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast());
        }
    }
}

fn pipe() -> (OwnedHandle, OwnedHandle) {
    unsafe {
        let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
        check(CreatePipe(&mut read, &mut write, ptr::null(), 0)).unwrap();
        (owned(read), owned(write))
    }
}

#[test]
fn conpty_plays_local_files_pauses_resizes_and_quits() {
    let root = std::env::temp_dir().join(format!("plavno terminal музыка {}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let track = root.join("local track.flac");
    fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/seek.flac"),
        &track,
    )
    .unwrap();
    // This integration test has its own process; no other tests share these vars.
    unsafe {
        std::env::set_var("PLAVNO_TEST_AUDIO", "1");
        std::env::set_var("XDG_STATE_HOME", &root);
        std::env::set_var("XDG_CONFIG_HOME", &root);
    }
    let (input_read, input_write) = pipe();
    let (output_read, output_write) = pipe();
    let mut input = File::from(input_write);
    let output = Arc::new(Mutex::new(Vec::new()));
    let received = output.clone();
    let reader = std::thread::spawn(move || {
        let mut file = File::from(output_read);
        let mut bytes = [0; 8192];
        while let Ok(size) = file.read(&mut bytes) {
            if size == 0 {
                break;
            }
            let mut data = received.lock().unwrap();
            if data.len() < 4 * 1024 * 1024 {
                data.extend_from_slice(&bytes[..size]);
            }
        }
    });
    let (process, console, job) = unsafe {
        let mut console = 0;
        assert!(
            CreatePseudoConsole(
                COORD { X: 100, Y: 35 },
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                0,
                &mut console
            ) >= 0
        );
        let console = Console(console);
        let mut size = 0;
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        check(InitializeProcThreadAttributeList(
            storage.as_mut_ptr().cast(),
            1,
            0,
            &mut size,
        ))
        .unwrap();
        let mut attributes = Attributes(storage);
        check(UpdateProcThreadAttribute(
            attributes.0.as_mut_ptr().cast(),
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            console.0 as *const _,
            size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null(),
        ))
        .unwrap();
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        // Match Windows Terminal: null standard handles let ConPTY supply the
        // console handles instead of duplicating the test runner's redirected IO.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.lpAttributeList = attributes.0.as_mut_ptr().cast();
        let mut line = wide(format!(
            "\"{}\" --file \"{}\" --no-wave",
            env!("CARGO_BIN_EXE_plavno"),
            track.display()
        ));
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        check(CreateProcessW(
            wide(env!("CARGO_BIN_EXE_plavno")).as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            ptr::null(),
            ptr::null(),
            &startup.StartupInfo,
            &mut info,
        ))
        .unwrap();
        let process = owned(info.hProcess);
        let thread = owned(info.hThread);
        let job = Job::new().unwrap();
        job.assign(&process).unwrap();
        assert_ne!(ResumeThread(thread.as_raw_handle()), u32::MAX);
        (process, console, job)
    };
    drop(input_read);
    drop(output_write);
    let wait_for = |text: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let data = output.lock().unwrap();
            let screen = terminal::screen_text(&data, 100, 48);
            if screen.contains(text) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "No {text:?} in terminal screen: {screen:?}; raw output: {:?}",
                String::from_utf8_lossy(&data)
            );
            drop(data);
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for("Playing");
    wait_for(concat!("plavno ", env!("CARGO_PKG_VERSION")));
    input.write_all(b" ").unwrap();
    input.flush().unwrap();
    wait_for("Paused");
    unsafe {
        assert!(ResizePseudoConsole(console.0, COORD { X: 70, Y: 25 }) >= 0);
    }
    input.write_all(b"q").unwrap();
    input.flush().unwrap();
    unsafe {
        assert_eq!(
            WaitForSingleObject(process.as_raw_handle(), 10_000),
            WAIT_OBJECT_0,
            "Player did not quit"
        );
        let mut exit = 1;
        check(GetExitCodeProcess(process.as_raw_handle(), &mut exit)).unwrap();
        assert_eq!(exit, 0);
    }
    drop(job);
    drop(input);
    drop(console);
    reader.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}
