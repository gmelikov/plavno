#![cfg(windows)]

use std::{fs, io::Write};

#[test]
fn process_cancellation_terminates_descendants() {
    use plavno_platform::{
        process::Process,
        windows::{
            owned,
            sys::Win32::{Foundation::*, System::Threading::*},
        },
    };
    use std::{
        io::{BufRead, BufReader},
        os::windows::io::AsRawHandle,
        process::{Command, Stdio},
        time::Duration,
    };
    let mut process = Process::spawn(Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", "$child = Start-Process -PassThru -WindowStyle Hidden -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30'; [Console]::WriteLine($child.Id); Start-Sleep -Seconds 30"]).stdout(Stdio::piped()).stderr(Stdio::null())).unwrap();
    let mut output = BufReader::new(process.stdout.take().unwrap());
    assert!(plavno_platform::wait_readable(output.get_ref(), Duration::from_secs(20)).unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let pid: u32 = line.trim().parse().unwrap();
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        assert!(!handle.is_null());
        let descendant = owned(handle);
        assert_eq!(
            WaitForSingleObject(descendant.as_raw_handle(), 0),
            WAIT_TIMEOUT
        );
        process.terminate();
        assert_eq!(
            WaitForSingleObject(descendant.as_raw_handle(), 5_000),
            WAIT_OBJECT_0
        );
    }
}

#[test]
fn credentials_require_private_acls_and_atomic_replacement_preserves_them() {
    let root = std::env::temp_dir().join(format!("plavno ACL музыка {}", std::process::id()));
    plavno_platform::private_directory(&root).unwrap();
    let shared = root.join("inherited.json");
    fs::write(&shared, b"shared").unwrap();
    assert!(plavno_platform::private_file(&shared).unwrap());
    // TEMP can already be private on Windows. Explicitly grant Everyone read
    // access so the rejection does not depend on the runner's inherited ACL.
    let result = std::process::Command::new("icacls.exe")
        .arg(&shared)
        .args(["/grant", "*S-1-1-0:(R)"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!plavno_platform::private_file(&shared).unwrap());
    plavno_platform::private_directory(&root).unwrap();
    let path = root.join("credentials.json");
    for bytes in [b"first".as_slice(), b"second".as_slice()] {
        let temp = root.join("pending.json");
        let mut file = plavno_platform::private_options()
            .create_new(true)
            .write(true)
            .open(&temp)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::rename(temp, &path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(plavno_platform::private_file(&path).unwrap());
    }
    fs::remove_dir_all(root).unwrap();
}
