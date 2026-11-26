use std::{
    ffi::{OsStr, c_void},
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_OBJECT_TYPE,
        },
        GetTokenInformation, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_OWNER,
        TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    let mut units: Vec<_> = s.encode_wide().collect();
    if units.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    units.push(0);
    Ok(units)
}

pub fn check(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub unsafe fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub struct LocalAlloc(pub *mut c_void);
impl Drop for LocalAlloc {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

pub fn user_sid() -> io::Result<String> {
    token_sid(false)
}

fn token_owner_sid() -> io::Result<String> {
    token_sid(true)
}

// An elevated token can default to the Administrators group as owner.
pub fn is_current_owner(owner: &str) -> io::Result<bool> {
    Ok(owner == user_sid()? || owner == token_owner_sid()?)
}

fn token_sid(owner: bool) -> io::Result<String> {
    let class = if owner { TokenOwner } else { TokenUser };
    unsafe {
        let mut token = null_mut();
        check(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
        let token = owned(token)?;
        let mut len = 0;
        GetTokenInformation(token.as_raw_handle(), class, null_mut(), 0, &mut len);
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        check(GetTokenInformation(
            token.as_raw_handle(),
            class,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        ))?;
        let sid = if owner {
            (*buf.as_ptr().cast::<TOKEN_OWNER>()).Owner
        } else {
            (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid
        };
        sid_string(sid)
    }
}

pub unsafe fn sid_string(sid: *mut c_void) -> io::Result<String> {
    let mut text = null_mut();
    unsafe {
        check(ConvertSidToStringSidW(sid, &mut text))?;
        let _text = LocalAlloc(text.cast());
        let len = (0..).find(|&i| *text.add(i) == 0).unwrap();
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}

pub fn private_security(directory: bool) -> io::Result<(LocalAlloc, SECURITY_ATTRIBUTES)> {
    let sid = user_sid()?;
    let sddl = wide(OsStr::new(&crate::fs::private_sddl(&sid, directory)))?;
    let mut descriptor = null_mut();
    unsafe {
        check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        ))?;
    }
    Ok((
        LocalAlloc(descriptor),
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        },
    ))
}

pub fn owner_sid(handle: HANDLE, kind: SE_OBJECT_TYPE) -> io::Result<String> {
    unsafe {
        let mut descriptor = null_mut();
        let mut owner = null_mut();
        let err = GetSecurityInfo(
            handle,
            kind,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        );
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let _descriptor = LocalAlloc(descriptor);
        sid_string(owner)
    }
}
