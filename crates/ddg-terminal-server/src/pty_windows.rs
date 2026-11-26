//! ConPTY owns the console; its job owns all processes in the session.
use ddg_agents_os::process::{WinChild, anonymous_pipe, environment, spawn_extended};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr::null,
    sync::{Arc, Mutex},
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::{Console::*, JobObjects::*, Threading::*},
};

pub const HANGUP: i32 = 1;
pub const KILL: i32 = 9;
pub const TERMINATE: i32 = 15;

struct Console(HPCON);
impl Drop for Console {
    fn drop(&mut self) {
        unsafe { ClosePseudoConsole(self.0) };
    }
}

struct Session {
    console: Mutex<Option<Console>>,
    job: OwnedHandle,
}

impl Session {
    fn terminate(&self) {
        unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
    }
}

pub struct PtyHandle {
    output: File,
    input: File,
    session: Arc<Session>,
}
impl PtyHandle {
    pub fn reader(&self) -> io::Result<File> {
        self.output.try_clone()
    }
    pub fn writer(&self) -> io::Result<File> {
        self.input.try_clone()
    }
    pub fn foreground_pgrp(&self) -> Option<u32> {
        None
    }
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if let Some(console) = self.session.console.lock().unwrap().as_ref() {
            hr(unsafe { ResizePseudoConsole(console.0, size(cols, rows)) })?;
        }
        Ok(())
    }
    pub fn signal_process_group(&self, _: u32, _: i32) {
        self.session.terminate();
    }
}
impl Drop for PtyHandle {
    fn drop(&mut self) {
        self.session.terminate();
    }
}

pub struct Child {
    process: WinChild,
    session: Arc<Session>,
}
impl Child {
    pub fn id(&self) -> u32 {
        self.process.id()
    }
}

pub fn wait_child(child: &mut Child) -> io::Result<(Option<i32>, Option<i32>)> {
    let exit = child.process.wait();
    // Kill descendants even if the original process already exited. Close ConPTY
    // on the reaper while the independent reader is still draining its output.
    child.session.terminate();
    let console = child.session.console.lock().unwrap().take();
    drop(console);
    exit.map(|code| (Some(code), None))
}

fn size(cols: u16, rows: u16) -> COORD {
    COORD {
        X: cols.clamp(1, 500) as i16,
        Y: rows.clamp(1, 300) as i16,
    }
}
fn hr(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::from_raw_os_error(result))
    } else {
        Ok(())
    }
}

pub fn spawn_pty(
    argv: &[String],
    env: &BTreeMap<String, String>,
    cwd: &str,
    cols: u16,
    rows: u16,
) -> io::Result<(PtyHandle, Child)> {
    let mut overrides = BTreeMap::from([
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "ddg".into()),
        (
            "TERM_PROGRAM_VERSION".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
    ]);
    for (key, val) in env {
        overrides.insert(key.to_uppercase(), val.clone());
    }
    let env = environment(
        &[
            "TMUX",
            "TMUX_PANE",
            "ZELLIJ",
            "ZELLIJ_SESSION_NAME",
            "KITTY_WINDOW_ID",
            "GHOSTTY_RESOURCES_DIR",
            "WEZTERM_PANE",
            "ITERM_SESSION_ID",
            "TERM_PROGRAM_VERSION",
        ],
        &overrides,
    )?;
    let default_shell;
    let args: Vec<_> = if argv.is_empty() {
        default_shell = std::env::var_os("COMSPEC")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cmd.exe".into());
        vec![default_shell.as_os_str()]
    } else {
        argv.iter().map(OsStr::new).collect()
    };

    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let (console_in, input) = anonymous_pipe()?;
    let (output, console_out) = anonymous_pipe()?;
    let mut console = 0;
    hr(unsafe {
        CreatePseudoConsole(
            size(cols, rows),
            console_in.as_raw_handle(),
            console_out.as_raw_handle(),
            0,
            &mut console,
        )
    })?;
    let console = Console(console);
    drop((console_in, console_out));
    let jobs = [job.as_raw_handle()];
    let attrs = [
        (
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            console.0 as *const _,
            size_of::<HPCON>(),
        ),
        (
            PROC_THREAD_ATTRIBUTE_JOB_LIST,
            jobs.as_ptr().cast(),
            size_of::<HANDLE>(),
        ),
    ];
    let mut startup = STARTUPINFOEXW::default();
    // Leave stdio null so ConPTY supplies it instead of the parent's redirected handles.
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    // Job assignment is part of CreateProcessW: the child cannot run outside it.
    let process = unsafe {
        spawn_extended(
            None,
            &args,
            (!cwd.trim().is_empty()).then(|| Path::new(cwd)),
            &env,
            startup,
            &attrs,
            0,
            false,
        )?
    };
    let session = Arc::new(Session {
        console: Mutex::new(Some(console)),
        job,
    });
    Ok((
        PtyHandle {
            output,
            input,
            session: session.clone(),
        },
        Child { process, session },
    ))
}

#[cfg(test)]
#[test]
fn conpty_echo_resize_and_exit_leaves_no_active_processes() -> io::Result<()> {
    use std::{
        io::{Read, Write},
        time::{Duration, Instant},
    };
    let args = [
        "cmd.exe",
        "/d",
        "/q",
        "/c",
        "start /b ping.exe -n 60 127.0.0.1 >NUL & echo ok & set /p TEST=",
    ]
    .map(String::from);
    let (pty, mut child) = spawn_pty(&args, &BTreeMap::new(), "", 80, 24)?;
    pty.resize(100, 32)?;
    let mut output = pty.reader()?;
    let (echo, echoed) = std::sync::mpsc::channel();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    std::thread::spawn(move || {
        // ConPTY can omit newlines; reserve the first byte for split matches.
        let mut buf = [0; 4097];
        while let Ok(n @ 1..) = output.read(&mut buf[1..]) {
            {
                let mut seen = captured.lock().unwrap();
                let len = n.min(1024 - seen.len());
                seen.extend_from_slice(&buf[1..1 + len]);
            }
            if buf[..=n].windows(2).any(|w| w == b"ok") {
                let _ = echo.send(());
                let _ = io::copy(&mut output, &mut io::sink());
                break;
            }
            buf[0] = buf[n];
        }
    });
    let active = || {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    pty.session.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        info.ActiveProcesses
    };
    echoed
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|err| {
            let seen = seen.lock().unwrap();
            panic!(
                "cmd.exe did not echo ok ({err}); active processes: {}; output: {:?}",
                active(),
                String::from_utf8_lossy(&seen)
            );
        });
    assert!(active() >= 2, "descendant never joined the ConPTY job");
    pty.writer()?.write_all(b"continue\r")?;
    let (reaped, exit) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = reaped.send(wait_child(&mut child));
    });
    let (code, signal) = exit
        .recv_timeout(Duration::from_secs(15))
        .expect("ConPTY did not close")?;
    assert_eq!((code, signal), (Some(0), None));
    let deadline = Instant::now() + Duration::from_secs(5);
    while active() != 0 {
        assert!(
            Instant::now() < deadline,
            "processes survived ConPTY shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
