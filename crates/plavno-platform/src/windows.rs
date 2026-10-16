use std::{
    ffi::c_void,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr,
};
pub use windows_sys as sys;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, LocalFree},
    Security::{Authorization::*, *},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn check(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn with_user<T>(f: impl FnOnce(PSID) -> io::Result<T>) -> io::Result<T> {
    unsafe {
        let mut token = ptr::null_mut();
        check(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
        let mut size = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
        // usize storage provides the alignment required by TOKEN_USER.
        let mut bytes = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        let result = check(GetTokenInformation(
            token,
            TokenUser,
            bytes.as_mut_ptr().cast(),
            size,
            &mut size,
        ));
        CloseHandle(token);
        result?;
        f((*(bytes.as_ptr().cast::<TOKEN_USER>())).User.Sid)
    }
}

pub fn protect(path: &Path) -> io::Result<()> {
    with_user(|sid| unsafe {
        let mut string = ptr::null_mut();
        check(ConvertSidToStringSidW(sid, &mut string))?;
        let _string = Local(string.cast());
        let mut length = 0;
        while *string.add(length) != 0 {
            length += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(string, length));
        let sddl = wide(format!("D:P(A;OICI;FA;;;{sid})"));
        let mut descriptor = ptr::null_mut();
        check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        ))?;
        let _descriptor = Local(descriptor);
        check(SetFileSecurityW(
            wide(path).as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        ))
    })
}

pub fn private_acl(path: &Path) -> io::Result<bool> {
    with_user(|user| unsafe {
        let mut owner = ptr::null_mut();
        let mut acl: *mut ACL = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let error = GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        );
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let _descriptor = Local(descriptor);
        if owner.is_null() || EqualSid(owner, user) == 0 || acl.is_null() {
            return Ok(false);
        }
        let mut allowed = false;
        for index in 0..(*acl).AceCount {
            let mut ace = ptr::null_mut();
            check(GetAce(acl, index as u32, &mut ace))?;
            let header = &*ace.cast::<ACE_HEADER>();
            if header.AceType
                != windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE as u8
            {
                return Ok(false);
            }
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            let sid = (&ace.SidStart as *const u32).cast_mut().cast();
            if EqualSid(sid, user) != 0 {
                allowed = true;
            } else if IsWellKnownSid(sid, WinLocalSystemSid) == 0
                && IsWellKnownSid(sid, WinBuiltinAdministratorsSid) == 0
            {
                return Ok(false);
            }
        }
        Ok(allowed)
    })
}

/// # Safety
/// `handle` must be a valid, uniquely owned handle returned by Windows.
pub unsafe fn owned(handle: HANDLE) -> OwnedHandle {
    // Only successful API calls returning owned handles may call this helper.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}
