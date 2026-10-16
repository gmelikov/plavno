use std::{
    io,
    ops::{Deref, DerefMut},
    process::{Child, Command},
};

pub struct Process {
    child: Child,
    #[cfg(windows)]
    _job: Job,
}

impl Process {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        }
        let mut child = command.spawn()?;
        #[cfg(windows)]
        let job = {
            match Job::new().and_then(|job| {
                job.assign(&child)?;
                resume(child.id())?;
                Ok(job)
            }) {
                Ok(job) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
        };
        let _ = &mut child;
        Ok(Self {
            child,
            #[cfg(windows)]
            _job: job,
        })
    }

    pub fn terminate(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        self._job.terminate();
        let _ = self.child.kill();
    }
}

#[cfg(windows)]
fn resume(pid: u32) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };
    unsafe {
        // std::process does not expose its main thread handle on stable Rust.
        // The process is suspended, so it has exactly one thread at this point.
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = crate::windows::owned(snapshot);
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        crate::windows::check(Thread32First(snapshot.as_raw_handle(), &mut entry))?;
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = crate::windows::owned(thread);
                if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            if Thread32Next(snapshot.as_raw_handle(), &mut entry) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
    }
}

impl Deref for Process {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl DerefMut for Process {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

#[cfg(windows)]
pub struct Job(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl Job {
    pub fn new() -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(crate::windows::owned(handle));
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            crate::windows::check(SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&info) as u32,
            ))?;
            Ok(job)
        }
    }
    pub fn assign(&self, process: &impl std::os::windows::io::AsHandle) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            crate::windows::check(
                windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                    self.0.as_raw_handle(),
                    process.as_handle().as_raw_handle(),
                ),
            )
        }
    }
    pub fn terminate(&self) {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}
