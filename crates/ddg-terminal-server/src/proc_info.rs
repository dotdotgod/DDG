//! Bounded process metadata for the PTY's foreground group leader.

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use ddg_agents_proto::id::sanitize_display_text;
use ddg_agents_proto::terminal::ForegroundHint;

#[cfg(any(test, target_os = "macos"))]
const MAX_ARGC: usize = 64;
#[cfg(any(test, target_os = "linux"))]
const MAX_CMDLINE_BYTES: usize = 4096;

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn interpreter(comm: &str) -> bool {
    matches!(
        comm,
        "node"
            | "nodejs"
            | "bun"
            | "deno"
            | "python"
            | "python3"
            | "ruby"
            | "perl"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "npx"
            | "tsx"
            | "uv"
    )
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn classify_argv<'a>(comm: &str, argv: impl IntoIterator<Item = &'a [u8]>) -> Option<String> {
    if !interpreter(comm) {
        return None;
    }
    let arg = std::str::from_utf8(argv.into_iter().nth(1)?).ok()?;
    // No options are allowlisted: eval flags and any unknown flag stop classification.
    if arg.starts_with('-')
        || arg.contains(['=', '@'])
        || arg.contains("://")
        || arg.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    display_basename(arg)
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn display_basename(name: &str) -> Option<String> {
    let basename = std::path::Path::new(name).file_name()?.to_str()?;
    let text = sanitize_display_text(basename, 64);
    (!text.is_empty()).then_some(text)
}

#[cfg(any(test, target_os = "macos"))]
fn parse_procargs2(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    let (argc, bytes) = bytes.split_first_chunk::<4>()?;
    let argc = usize::try_from(i32::from_ne_bytes(*argc))
        .ok()?
        .min(MAX_ARGC);
    if argc == 0 {
        return Some(Vec::new());
    }
    let rest = &bytes[bytes.iter().position(|&b| b == 0)? + 1..];
    // XNU inserts alignment NULs between the executable path and argv[0].
    let mut rest = &rest[rest.iter().position(|&b| b != 0)?..];
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = rest.iter().position(|&b| b == 0)?;
        argv.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    Some(argv)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_cmdline(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes[..bytes.len().min(MAX_CMDLINE_BYTES)]
        .split_inclusive(|&b| b == 0)
        .map_while(|arg| arg.strip_suffix(b"\0"))
}

#[cfg(any(test, target_os = "linux"))]
fn linux_stat_ids(bytes: &[u8]) -> Option<(u32, u32)> {
    // comm is parenthesized and can itself contain spaces and parentheses.
    let tail = bytes.rsplitn(2, |&b| b == b')').next()?;
    let mut fields = tail.strip_prefix(b" ")?.split(|&b| b == b' ');
    let pgrp = std::str::from_utf8(fields.nth(2)?).ok()?.parse().ok()?;
    let sid = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    Some((pgrp, sid))
}

#[cfg(any(test, target_os = "linux"))]
fn linux_status_euid(bytes: &[u8]) -> Option<u32> {
    let uids = bytes
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"Uid:"))?;
    std::str::from_utf8(uids)
        .ok()?
        .split_ascii_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(any(test, target_os = "linux"))]
fn linux_identity_matches(stat: &[u8], status: &[u8], pgid: u32, sid: u32, euid: u32) -> bool {
    linux_stat_ids(stat) == Some((pgid, sid)) && linux_status_euid(status) == Some(euid)
}

#[cfg(target_os = "linux")]
pub fn inspect_foreground(pgid: u32, pty_pid: u32) -> Option<ForegroundHint> {
    use std::ffi::CStr;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};

    // Relative opens keep all reads tied to the same proc entry if the PID is recycled.
    let proc = File::open(format!("/proc/{pgid}")).ok()?;
    let read = |name: &CStr, limit: usize| -> Option<Vec<u8>> {
        let fd = unsafe {
            libc::openat(
                proc.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        let mut bytes = Vec::new();
        let file = unsafe { File::from_raw_fd(fd) };
        file.take(limit as u64).read_to_end(&mut bytes).ok()?;
        Some(bytes)
    };
    let stat = read(c"stat", 4096)?;
    let status = read(c"status", 4096)?;
    if !linux_identity_matches(&stat, &status, pgid, pty_pid, unsafe { libc::geteuid() }) {
        return None;
    }
    let bytes = read(c"comm", 256)?;
    let name = std::str::from_utf8(bytes.strip_suffix(b"\n")?).ok()?;
    let script_hint = if interpreter(name) {
        read(c"cmdline", MAX_CMDLINE_BYTES)
            .and_then(|bytes| classify_argv(name, parse_cmdline(&bytes)))
    } else {
        None
    };
    Some(ForegroundHint {
        pid: pgid,
        comm: display_basename(name)?,
        script_hint,
    })
}

#[cfg(target_os = "macos")]
pub fn inspect_foreground(pgid: u32, _pty_pid: u32) -> Option<ForegroundHint> {
    use std::mem::{MaybeUninit, size_of};

    let pid = i32::try_from(pgid).ok()?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let info_size = size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            info_size,
        )
    };
    if read != info_size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pgid != pgid || info.pbi_uid != unsafe { libc::geteuid() } {
        return None;
    }

    let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if len <= 0 {
        return None;
    }
    let path = std::ffi::CStr::from_bytes_until_nul(&path)
        .ok()?
        .to_str()
        .ok()?;
    let name = std::path::Path::new(path).file_name()?.to_str()?;
    let script_hint = if interpreter(name) {
        macos_script_hint(pid, name)
    } else {
        None
    };
    Some(ForegroundHint {
        pid: pgid,
        comm: display_basename(name)?,
        script_hint,
    })
}

#[cfg(target_os = "macos")]
fn macos_script_hint(pid: i32, name: &str) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut bytes = [0u8; 16 * 1024];
    let mut len = bytes.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    classify_argv(name, parse_procargs2(bytes.get(..len)?)?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn inspect_foreground(_pgid: u32, _pty_pid: u32) -> Option<ForegroundHint> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_reads_only_bounded_argc_and_never_envp() {
        fn procargs(argc: i32, body: &[u8]) -> Vec<u8> {
            [
                argc.to_ne_bytes().as_slice(),
                b"/usr/bin/python3\0\0\0",
                body,
            ]
            .concat()
        }
        let data = procargs(3, b"python3\0/work/app.py\0\0OPENAI_API_KEY=sk-secret\0");
        assert_eq!(
            parse_procargs2(&data).unwrap(),
            [b"python3".as_slice(), b"/work/app.py", b""]
        );
        assert_eq!(
            classify_argv("python3", parse_procargs2(&data).unwrap()),
            Some("app.py".into())
        );
        let data = procargs(1, b"python3\0not-an-argument.py\0");
        assert_eq!(
            classify_argv("python3", parse_procargs2(&data).unwrap()),
            None
        );

        for argc in [MAX_ARGC, MAX_ARGC + 1] {
            let mut body = b"python3\0".to_vec();
            body.extend(b"x\0".repeat(MAX_ARGC - 1));
            // The cap must not even look for a terminator after MAX_ARGC.
            body.extend_from_slice(b"unterminated");
            assert_eq!(
                parse_procargs2(&procargs(argc as i32, &body))
                    .unwrap()
                    .len(),
                MAX_ARGC
            );
        }
        for bad in [
            procargs(-1, b"python3\0"),
            procargs(2, b"python3\0unfinished"),
            vec![1, 2],
        ] {
            assert!(parse_procargs2(&bad).is_none());
        }
    }

    #[test]
    fn linux_parsers_check_identity_and_cap_cmdline() {
        let stat = b"42 (name ) with spaces) S 1 42 7 3 42";
        let status = b"Name:\tpython3\nUid:\t2000\t1000\t2000\t1000\n";
        assert!(linux_identity_matches(stat, status, 42, 7, 1000));
        assert!(!linux_identity_matches(stat, status, 42, 8, 1000));
        assert!(!linux_identity_matches(stat, status, 43, 7, 1000));
        assert!(!linux_identity_matches(stat, status, 42, 7, 2000));
        assert!(!linux_identity_matches(b"invalid", status, 42, 7, 1000));
        assert!(!linux_identity_matches(stat, b"Uid:\t1000", 42, 7, 1000));
        assert_eq!(
            classify_argv("python3", parse_cmdline(b"python3\0/opt/app.py\0")),
            Some("app.py".into())
        );

        let mut data = b"python3\0".to_vec();
        data.resize(MAX_CMDLINE_BYTES, b'x');
        assert_eq!(classify_argv("python3", parse_cmdline(&data)), None);
        data.push(0);
        assert_eq!(classify_argv("python3", parse_cmdline(&data)), None);
        data[MAX_CMDLINE_BYTES - 1] = 0;
        assert_eq!(
            classify_argv("python3", parse_cmdline(&data)),
            Some("x".repeat(64))
        );
    }

    #[test]
    fn classification_stops_before_unsafe_arguments() {
        for bad in [
            "-c",
            "-e",
            "--eval",
            "--eval=secret",
            "-p",
            "-m",
            "-",
            "--unknown",
            "TOKEN=abc",
            "https://user:pass@host",
            "user@host",
            "two words",
            "two\nlines",
            "say\x1bthat",
            "wide\u{2028}space",
        ] {
            assert_eq!(
                classify_argv("node", [b"node".as_slice(), bad.as_bytes(), b"valid.js"]),
                None,
                "{bad:?}"
            );
        }
        for argv in [
            [b"env".as_slice(), b"SECRET=1", b"python3", b"app.py"],
            [
                b"/usr/bin/env".as_slice(),
                b"API_KEY=sk-1",
                b"node",
                b"index.js",
            ],
        ] {
            assert_eq!(classify_argv("env", argv), None);
        }
        assert_eq!(
            classify_argv("not-an-interpreter", [b"node".as_slice(), b"app.js"]),
            None
        );
        assert_eq!(
            classify_argv(
                "node",
                [b"node".as_slice(), "/work/a\u{202e}pp.js".as_bytes()]
            ),
            Some("app.js".into())
        );
    }
}
