use std::{fs, io, path::Path};
pub mod process;

#[cfg(unix)]
use std::os::fd::AsRawFd as PipeHandle;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as PipeHandle;

pub fn wait_readable(pipe: &impl PipeHandle, timeout: std::time::Duration) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let mut poll = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut poll,
                1,
                timeout.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ready > 0)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{Foundation::ERROR_BROKEN_PIPE, System::Pipes::PeekNamedPipe};
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut available = 0;
            let ready = unsafe {
                PeekNamedPipe(
                    pipe.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ready == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    return Ok(true);
                }
                return Err(error);
            }
            if available > 0 {
                return Ok(true);
            }
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            if wait.is_zero() {
                return Ok(false);
            }
            std::thread::sleep(wait.min(std::time::Duration::from_millis(10)));
        }
    }
}

#[cfg(windows)]
pub mod windows;

#[derive(Clone, Copy)]
pub struct Random;

impl Random {
    pub fn new() -> io::Result<Self> {
        let mut probe = [0];
        getrandom::fill(&mut probe).map_err(io::Error::other)?;
        Ok(Self)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(*self)
    }
}

impl io::Read for Random {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        getrandom::fill(bytes).map_err(io::Error::other)?;
        Ok(bytes.len())
    }
}

pub fn private_file(path: &Path) -> io::Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(meta.permissions().mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    windows::private_acl(path)
}

pub fn private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(io::Error::other("Private directory must not be a symlink"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    windows::protect(path)
}

pub fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        // Private parent directories supply the protected DACL at creation.
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
}

pub fn config_home() -> io::Result<std::path::PathBuf> {
    home("XDG_CONFIG_HOME", "APPDATA", ".config")
}

pub fn cache_home() -> io::Result<std::path::PathBuf> {
    home("XDG_CACHE_HOME", "LOCALAPPDATA", ".cache")
}

pub fn state_home() -> io::Result<std::path::PathBuf> {
    home("XDG_STATE_HOME", "LOCALAPPDATA", ".local/state")
}

fn home(xdg: &str, windows: &str, unix: &str) -> io::Result<std::path::PathBuf> {
    let env = |key| {
        std::env::var_os(key)
            .filter(|s| !s.is_empty())
            .map(Into::into)
    };
    if let Some(path) = env(xdg) {
        return Ok(path);
    }
    #[cfg(windows)]
    if let Some(path) = env(windows) {
        return Ok(path);
    }
    let _ = windows;
    env("HOME")
        .map(|path: std::path::PathBuf| path.join(unix))
        .ok_or_else(|| io::Error::other("User data directory is not set"))
}
