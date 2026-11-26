//! Private directories and atomic file replacement.

use ddg_agents_proto::id::fill_csprng;
use std::fs::{self, File};
#[cfg(unix)]
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Creates or tightens a caller-owned application directory. Ancestors must
/// be trusted; the directory itself must not be a symlink.
#[cfg(unix)]
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = dir.metadata()?;
    // SAFETY: geteuid takes no arguments and has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o7777 != 0o700 {
        dir.set_permissions(Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(any(windows, test))]
pub(crate) fn private_sddl(sid: &str, directory: bool) -> String {
    let inherit = if directory { "OICI" } else { "" };
    // P is the creation-time equivalent of PROTECTED_DACL_SECURITY_INFORMATION.
    format!("O:{sid}D:P(A;{inherit};GA;;;{sid})")
}

#[cfg(test)]
#[test]
fn protected_user_only_sddl() {
    assert_eq!(
        private_sddl("S-1-5-21-42", false),
        "O:S-1-5-21-42D:P(A;;GA;;;S-1-5-21-42)"
    );
    assert_eq!(
        private_sddl("S-1-5-21-42", true),
        "O:S-1-5-21-42D:P(A;OICI;GA;;;S-1-5-21-42)"
    );
}

#[cfg(all(windows, test))]
mod windows_tests {
    use super::{create_private_file, ensure_private_dir};
    use crate::windows::{LocalAlloc, check, owner_sid, sid_string, user_sid};
    use std::{
        fs::{self, File},
        io,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        path::Path,
        ptr::null_mut,
    };
    use windows_sys::Win32::{
        Security::{Authorization::*, *},
        Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
    };

    const DIR_INHERIT: u8 = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;

    fn open_dir(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    fn verify(file: &File, inherit: u8, expected_owner: &str) -> io::Result<()> {
        unsafe {
            let (mut sd, mut owner, mut dacl) = (null_mut(), null_mut(), null_mut());
            let error = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut sd,
            );
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            let _sd = LocalAlloc(sd);
            let mut control = 0;
            let mut revision = 0;
            check(GetSecurityDescriptorControl(
                sd,
                &mut control,
                &mut revision,
            ))?;
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            assert!(!dacl.is_null());
            assert_eq!((*dacl).AceCount, 1);
            let mut ace = null_mut();
            check(GetAce(dacl, 0, &mut ace))?;
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
            assert_eq!(ace.Header.AceFlags, inherit);
            assert_eq!(
                sid_string((&raw const ace.SidStart).cast_mut().cast())?,
                user_sid()?
            );
            assert_eq!(sid_string(owner)?, expected_owner);
        }
        Ok(())
    }

    #[test]
    fn windows_private_file_and_directory_use_a_protected_user_dacl() -> io::Result<()> {
        let dir = std::env::temp_dir()
            .join(ddg_agents_proto::id::generate_message_id().map_err(io::Error::other)?);
        ensure_private_dir(&dir)?;
        let result = (|| {
            let sid = user_sid()?;
            verify(&open_dir(&dir)?, DIR_INHERIT, &sid)?;
            let file = create_private_file(&dir.join("private.txt"))?;
            verify(&file, 0, &sid)
        })();
        fs::remove_dir_all(&dir)?;
        result
    }

    #[test]
    fn windows_ensure_private_dir_tightens_existing_directory() -> io::Result<()> {
        let dir = std::env::temp_dir()
            .join(ddg_agents_proto::id::generate_message_id().map_err(io::Error::other)?);
        fs::create_dir(&dir)?;
        let result = (|| {
            let owner = owner_sid(open_dir(&dir)?.as_raw_handle(), SE_FILE_OBJECT)?;
            ensure_private_dir(&dir)?;
            verify(&open_dir(&dir)?, DIR_INHERIT, &owner)
        })();
        fs::remove_dir(&dir)?;
        result
    }
}

#[cfg(windows)]
fn secure_existing(
    handle: &std::os::windows::io::OwnedHandle,
    attrs: &windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
) -> io::Result<()> {
    use crate::windows::{check, is_current_owner, owner_sid};
    use std::{os::windows::io::AsRawHandle, ptr::null_mut};
    use windows_sys::Win32::Security::{Authorization::*, *};
    unsafe {
        let h = handle.as_raw_handle();
        if !is_current_owner(&owner_sid(h, SE_FILE_OBJECT)?)? {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut dacl = null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        check(GetSecurityDescriptorDacl(
            attrs.lpSecurityDescriptor,
            &mut present,
            &mut dacl,
            &mut defaulted,
        ))?;
        let err = SetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        );
        if err != 0 {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        Ok(())
    }
}

#[cfg(windows)]
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    use crate::windows::{owned, private_security, wide};
    use std::{os::windows::fs::MetadataExt, ptr::null_mut};
    use windows_sys::Win32::Storage::FileSystem::*;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && !parent.is_dir()
    {
        ensure_private_dir(parent)?;
    }
    let name = wide(path.as_os_str())?;
    let (_sd, attrs) = private_security(true)?;
    if unsafe { CreateDirectoryW(name.as_ptr(), &attrs) } == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        let handle = unsafe {
            owned(CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            ))?
        };
        let file = File::from(handle);
        let meta = file.metadata()?;
        if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        secure_existing(&file.into(), &attrs)?;
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn open_private(path: &Path, new: bool) -> io::Result<File> {
    use crate::windows::{owned, private_security, wide};
    use std::{os::windows::fs::MetadataExt, ptr::null_mut};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::*,
    };
    let name = wide(path.as_os_str())?;
    let (_sd, attrs) = private_security(false)?;
    let handle = unsafe {
        owned(CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attrs,
            if new { CREATE_NEW } else { OPEN_ALWAYS },
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        ))?
    };
    let file = File::from(handle);
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    if new {
        return Ok(file);
    }
    let handle = file.into();
    secure_existing(&handle, &attrs)?;
    Ok(File::from(handle))
}

/// Opens a new private regular file without replacing an existing entry.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    return open_private(path, true);
    #[cfg(unix)]
    {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        if let Err(error) = file.set_permissions(Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(path);
            return Err(error);
        }
        Ok(file)
    }
}

#[cfg(windows)]
pub fn open_regular(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub fn is_current_user_file(path: &Path) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    let file = open_regular(path)?;
    let owner = crate::windows::owner_sid(
        file.as_raw_handle(),
        windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT,
    )?;
    crate::windows::is_current_owner(&owner)
}

#[cfg(windows)]
pub fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    crate::windows::check(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) })?;
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Writes and syncs a private temporary file before replacing the destination.
/// The destination's parent must already exist and be trusted.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut nonce = [0; 16];
    fill_csprng(&mut nonce).map_err(io::Error::other)?;
    let tmp = path.with_file_name(format!(".ddg-tmp-{:032x}", u128::from_be_bytes(nonce)));
    let mut file = create_private_file(&tmp)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
