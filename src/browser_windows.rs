use anyhow::{Context, Result};
use plavno_platform::{
    process::Job,
    windows::{
        check, owned,
        sys::Win32::{
            Foundation::*,
            Security::SECURITY_ATTRIBUTES,
            System::{Console::*, Pipes::CreatePipe, Threading::*},
        },
        wide,
    },
};
use std::{
    fs::File,
    io,
    os::windows::io::{AsRawHandle, OwnedHandle},
    path::Path,
    ptr,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct Child {
    process: OwnedHandle,
    job: Job,
    #[cfg(test)]
    pid: u32,
}

impl Child {
    #[cfg(test)]
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn try_wait(&mut self) -> io::Result<Option<()>> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(Some(())),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn wait(&mut self) -> io::Result<()> {
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub fn terminate(&mut self) {
        self.job.terminate();
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.wait();
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    unsafe {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
        check(CreatePipe(&mut read, &mut write, &attributes, 0))?;
        Ok((owned(read), owned(write)))
    }
}

// Quote arguments according to the Windows CRT command-line rules, including
// trailing backslashes in paths. No shell interprets this command line.
fn quote(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    let units = wide(value);
    let mut output = vec![b'"' as u16];
    let mut slashes = 0;
    for &unit in &units[..units.len() - 1] {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        output.extend(std::iter::repeat_n(
            b'\\' as u16,
            slashes * if unit == b'"' as u16 { 2 } else { 1 },
        ));
        if unit == b'"' as u16 {
            output.push(b'\\' as u16);
        }
        slashes = 0;
        output.push(unit);
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
    output
}

struct Attributes(Vec<usize>);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast());
        }
    }
}

pub(super) fn launch(
    program: &Path,
    profile: &Path,
    headless: bool,
) -> Result<(Child, File, File)> {
    unsafe {
        let (read_input, write_input) = pipe()?;
        let (read_output, write_output) = pipe()?;
        check(SetHandleInformation(
            write_input.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            0,
        ))?;
        check(SetHandleInformation(
            read_output.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            0,
        ))?;
        let mut handles = [read_input.as_raw_handle(), write_output.as_raw_handle()];
        let mut bytes = 0;
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes);
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        check(InitializeProcThreadAttributeList(
            storage.as_mut_ptr().cast(),
            1,
            0,
            &mut bytes,
        ))?;
        let mut attributes = Attributes(storage);
        check(UpdateProcThreadAttribute(
            attributes.0.as_mut_ptr().cast(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            size_of_val(&handles),
            ptr::null_mut(),
            ptr::null(),
        ))?;
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes.0.as_mut_ptr().cast();
        let mut args = vec![
            program.as_os_str().to_owned(),
            "--remote-debugging-pipe".into(),
            format!(
                "--remote-debugging-io-pipes={},{}",
                handles[0] as usize, handles[1] as usize
            )
            .into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-sync".into(),
        ];
        let mut profile_arg = std::ffi::OsString::from("--user-data-dir=");
        profile_arg.push(profile);
        args.push(profile_arg);
        if headless {
            args.push("--headless=new".into());
        }
        args.push("about:blank".into());
        let mut line = Vec::new();
        for arg in args {
            if !line.is_empty() {
                line.push(32);
            }
            line.extend(quote(arg));
        }
        line.push(0);
        let job = Job::new()?;
        let mut info: PROCESS_INFORMATION = std::mem::zeroed();
        check(CreateProcessW(
            wide(program).as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            ptr::null(),
            ptr::null(),
            &startup.StartupInfo,
            &mut info,
        ))
        .context("Cannot start Chrome/Chromium; check PLAVNO_BROWSER")?;
        let process = owned(info.hProcess);
        let thread = owned(info.hThread);
        if let Err(error) = job.assign(&process) {
            TerminateProcess(process.as_raw_handle(), 1);
            return Err(error.into());
        }
        let child = Child {
            process,
            job,
            #[cfg(test)]
            pid: info.dwProcessId,
        };
        if ResumeThread(thread.as_raw_handle()) == u32::MAX {
            return Err(io::Error::last_os_error().into());
        }
        Ok((child, write_input.into(), read_output.into()))
    }
}

static CANCELLED: Mutex<Option<Weak<AtomicBool>>> = Mutex::new(None);

unsafe extern "system" fn interrupt(kind: u32) -> i32 {
    if kind != CTRL_C_EVENT && kind != CTRL_BREAK_EVENT {
        return 0;
    }
    if let Some(flag) = CANCELLED
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(Weak::upgrade))
    {
        flag.store(true, Ordering::Relaxed);
        1
    } else {
        0
    }
}

pub(super) struct Interrupts;
impl Interrupts {
    pub fn new(flag: Arc<AtomicBool>) -> io::Result<Self> {
        unsafe {
            check(SetConsoleCtrlHandler(Some(interrupt), 1))?;
        }
        *CANCELLED.lock().unwrap() = Some(Arc::downgrade(&flag));
        Ok(Self)
    }
}
impl Drop for Interrupts {
    fn drop(&mut self) {
        *CANCELLED.lock().unwrap() = None;
        unsafe {
            SetConsoleCtrlHandler(Some(interrupt), 0);
        }
    }
}

#[cfg(test)]
pub(super) fn process_exited(pid: u32) -> bool {
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if handle.is_null() {
            return true;
        }
        let process = owned(handle);
        WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_OBJECT_0
    }
}
