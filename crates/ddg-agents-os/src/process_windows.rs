use crate::windows::{check, owned, wide};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, c_void},
    fs::File,
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType},
    System::{Pipes::CreatePipe, Threading::*},
};

pub const DETACHED_FLAGS: u32 = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;

pub fn anonymous_pipe() -> io::Result<(File, File)> {
    let mut read = null_mut();
    let mut write = null_mut();
    unsafe {
        check(CreatePipe(&mut read, &mut write, null(), 0))?;
        Ok((owned(read)?.into(), owned(write)?.into()))
    }
}

pub fn stdio_is_pipe() -> bool {
    [&io::stdin() as &dyn AsRawHandle, &io::stdout()]
        .iter()
        .all(|h| unsafe { GetFileType(h.as_raw_handle()) } == FILE_TYPE_PIPE)
}

struct Attributes(Vec<usize>);
impl Attributes {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0; bytes.div_ceil(size_of::<usize>())];
        unsafe {
            check(InitializeProcThreadAttributeList(
                buf.as_mut_ptr().cast(),
                count,
                0,
                &mut bytes,
            ))?;
        }
        Ok(Self(buf))
    }
    fn raw(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.raw()) };
    }
}

pub struct WinChild {
    handle: OwnedHandle,
    pid: u32,
}
impl WinChild {
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn kill(&self) -> io::Result<()> {
        check(unsafe { TerminateProcess(self.handle.as_raw_handle(), 1) })
    }
    pub fn wait(&self) -> io::Result<i32> {
        if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), INFINITE) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0;
        check(unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) })?;
        Ok(code as i32)
    }
}

pub fn terminate_process(pid: u32) -> io::Result<()> {
    let handle = unsafe { owned(OpenProcess(PROCESS_TERMINATE, 0, pid))? };
    check(unsafe { TerminateProcess(handle.as_raw_handle(), 1) })
}

pub fn spawn_handoff(path: &Path, args: &[&str]) -> io::Result<(WinChild, File, File)> {
    let (child_in, input) = anonymous_pipe()?;
    let (output, child_out) = anonymous_pipe()?;
    let null_out = std::fs::OpenOptions::new().write(true).open(r"\\.\NUL")?;
    let handles = [
        child_in.as_raw_handle(),
        child_out.as_raw_handle(),
        null_out.as_raw_handle(),
    ];
    for h in handles {
        unsafe {
            check(SetHandleInformation(
                h,
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            ))?;
        }
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    // Do not send daemon diagnostics over the token protocol.
    startup.StartupInfo.hStdError = handles[2];
    let args: Vec<_> = std::iter::once(path.as_os_str())
        .chain(args.iter().map(OsStr::new))
        .collect();
    let env = environment(&["DDG_SESSION_TOKEN"], &BTreeMap::new())?;
    let list = [(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        handles.as_ptr().cast(),
        std::mem::size_of_val(&handles),
    )];
    let child = unsafe {
        spawn_extended(
            Some(path),
            &args,
            None,
            &env,
            startup,
            &list,
            DETACHED_FLAGS,
            true,
        )?
    };
    Ok((child, input, output))
}

pub fn environment(remove: &[&str], overrides: &BTreeMap<String, String>) -> io::Result<Vec<u16>> {
    let mut entries = BTreeMap::new();
    for (key, val) in std::env::vars_os() {
        let folded = key.to_string_lossy().to_uppercase();
        if !remove.iter().any(|x| x.eq_ignore_ascii_case(&folded)) {
            entries.insert(folded, (key, val));
        }
    }
    for (key, val) in overrides {
        if key.is_empty() || key.contains('=') {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        entries.insert(key.to_uppercase(), (key.into(), val.into()));
    }
    let mut block = Vec::new();
    for (_, (mut key, value)) in entries {
        key.push("=");
        key.push(value);
        block.extend(wide(&key)?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn command_line(args: &[&OsStr]) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let arg: Vec<_> = arg.encode_wide().collect();
        if arg.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if index != 0 {
            line.push(b' ' as u16);
        }
        let quote = arg.is_empty() || arg.iter().any(|&u| matches!(u, 9 | 32));
        if quote {
            line.push(b'"' as u16);
        }
        let mut slashes = 0;
        for u in arg {
            if u == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            let extra = if u == b'"' as u16 { slashes + 1 } else { 0 };
            line.extend(std::iter::repeat_n(b'\\' as u16, slashes + extra));
            slashes = 0;
            line.push(u);
        }
        line.extend(std::iter::repeat_n(
            b'\\' as u16,
            if quote { 2 * slashes } else { slashes },
        ));
        if quote {
            line.push(b'"' as u16);
        }
    }
    line.push(0);
    Ok(line)
}

/// # Safety
/// Each attribute value must meet Win32's type, size and lifetime requirements.
/// If `inherit` is true, the attributes must include an explicit handle list.
/// `env` must be a double-NUL-terminated Windows environment block.
#[allow(clippy::too_many_arguments)]
pub unsafe fn spawn_extended(
    path: Option<&Path>,
    args: &[&OsStr],
    cwd: Option<&Path>,
    env: &[u16],
    mut startup: STARTUPINFOEXW,
    values: &[(u32, *const c_void, usize)],
    flags: u32,
    inherit: bool,
) -> io::Result<WinChild> {
    let app = path.map(|p| wide(p.as_os_str())).transpose()?;
    let cwd = cwd.map(|p| wide(p.as_os_str())).transpose()?;
    let mut args = command_line(args)?;
    let mut attrs = Attributes::new(values.len() as u32)?;
    for &(kind, value, size) in values {
        unsafe {
            check(UpdateProcThreadAttribute(
                attrs.raw(),
                0,
                kind as usize,
                value,
                size,
                null_mut(),
                null_mut(),
            ))?;
        }
    }
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attrs.raw();
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        check(CreateProcessW(
            app.as_ref().map_or(null(), |p| p.as_ptr()),
            args.as_mut_ptr(),
            null::<SECURITY_ATTRIBUTES>(),
            null(),
            i32::from(inherit),
            flags | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
            cwd.as_ref().map_or(null(), |p| p.as_ptr()),
            &startup.StartupInfo,
            &mut process,
        ))?;
        let _thread = owned(process.hThread)?;
        Ok(WinChild {
            handle: owned(process.hProcess)?,
            pid: process.dwProcessId,
        })
    }
}
