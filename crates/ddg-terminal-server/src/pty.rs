//! Native POSIX PTY spawn, resize, process-group signaling, and child reaping.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
pub type Child = std::process::Child;
pub const HANGUP: i32 = libc::SIGHUP;
pub const KILL: i32 = libc::SIGKILL;
pub const TERMINATE: i32 = libc::SIGTERM;

pub struct PtyHandle {
    master: OwnedFd,
}

impl PtyHandle {
    pub fn reader(&self) -> io::Result<File> {
        Ok(self.master.try_clone()?.into())
    }

    pub fn writer(&self) -> io::Result<File> {
        Ok(self.master.try_clone()?.into())
    }

    pub fn foreground_pgrp(&self) -> Option<u32> {
        let pgid = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        (pgid > 0).then_some(pgid as u32)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows.clamp(1, 300),
            ws_col: cols.clamp(1, 500),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn signal_process_group(&self, pid: u32, sig: i32) {
        let pgid = pid as libc::pid_t;
        if pgid <= 0 {
            return;
        }
        unsafe {
            // Signal the child's session/process group first, then the leader directly.
            let _ = libc::killpg(pgid, sig);
            let _ = libc::kill(pgid, sig);
        }
    }
}

pub fn spawn_pty(
    argv: &[String],
    env: &BTreeMap<String, String>,
    cwd: &str,
    cols: u16,
    rows: u16,
) -> io::Result<(PtyHandle, Child)> {
    let mut master_fd: RawFd = -1;
    let mut slave_fd: RawFd = -1;
    let mut ws = libc::winsize {
        ws_row: rows.clamp(1, 300),
        ws_col: cols.clamp(1, 500),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // macOS takes a mutable winsize pointer; Linux takes a const pointer.
    #[allow(clippy::unnecessary_mut_passed)]
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
    set_cloexec(master.as_raw_fd())?;
    set_cloexec(slave.as_raw_fd())?;

    let default_shell;
    let (program, args): (&str, &[String]) = if let Some((first, rest)) = argv.split_first() {
        (first.as_str(), rest)
    } else {
        default_shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        (default_shell.as_str(), &[])
    };

    let mut cmd = Command::new(program);
    cmd.args(args);
    if !cwd.trim().is_empty() {
        cmd.current_dir(cwd);
    }
    for key in [
        "TMUX",
        "TMUX_PANE",
        "ZELLIJ",
        "ZELLIJ_SESSION_NAME",
        "KITTY_WINDOW_ID",
        "GHOSTTY_RESOURCES_DIR",
        "WEZTERM_PANE",
        "ITERM_SESSION_ID",
        "TERM_PROGRAM_VERSION",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("TERM_PROGRAM", "ddg")
        .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
        .envs(env);

    let slave_raw = slave.as_raw_fd();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "macos")]
            let tiocsctty: libc::c_ulong = libc::TIOCSCTTY as libc::c_ulong;
            #[cfg(not(target_os = "macos"))]
            let tiocsctty: libc::c_ulong = libc::TIOCSCTTY as libc::c_ulong;
            if libc::ioctl(slave_raw, tiocsctty, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            for fd in 0..=2 {
                if libc::dup2(slave_raw, fd) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if slave_raw > 2 {
                let _ = libc::close(slave_raw);
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;
    drop(slave);
    Ok((PtyHandle { master }, child))
}

pub fn wait_child(child: &mut Child) -> io::Result<(Option<i32>, Option<i32>)> {
    let status = child.wait()?;
    Ok((status.code(), status.signal()))
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn env_and_foreground_pgrp() {
        // Re-exec with inherited variables instead of mutating the test runner's environment.
        const CHILD: &str = "DDG_TEST_PTY_ENV";
        let external = [
            "TMUX",
            "TMUX_PANE",
            "ZELLIJ",
            "ZELLIJ_SESSION_NAME",
            "KITTY_WINDOW_ID",
            "GHOSTTY_RESOURCES_DIR",
            "WEZTERM_PANE",
            "ITERM_SESSION_ID",
        ];
        if std::env::var_os(CHILD).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pty::tests::env_and_foreground_pgrp",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .envs(external.map(|key| (key, "inherited")))
                .env("TERM_PROGRAM", "ghostty")
                .env("TERM_PROGRAM_VERSION", "inherited")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        for explicit in [false, true] {
            let env = if explicit {
                [
                    ("TMUX".into(), "manual".into()),
                    ("TERM_PROGRAM".into(), "manual".into()),
                    ("TERM_PROGRAM_VERSION".into(), "manual".into()),
                ]
                .into()
            } else {
                BTreeMap::new()
            };
            let argv = [
                "/bin/sh",
                "-c",
                "/usr/bin/env; printf 'ENV_END\\n'; read hold",
            ]
            .map(String::from);
            let (pty, mut child) = spawn_pty(&argv, &env, "", 80, 24).unwrap();
            let mut reader = BufReader::new(pty.reader().unwrap());
            let mut output = Vec::new();
            loop {
                let mut line = Vec::new();
                assert_ne!(reader.read_until(b'\n', &mut line).unwrap(), 0);
                if line.starts_with(b"ENV_END") {
                    break;
                }
                output.extend(line);
            }
            let fg = pty.foreground_pgrp();
            pty.writer().unwrap().write_all(b"\n").unwrap();
            // Drain the echoed newline before waiting for the slave to close.
            reader.read_until(b'\n', &mut Vec::new()).unwrap();
            assert!(child.wait().unwrap().success());
            assert_eq!(fg, Some(child.id()));
            let output = String::from_utf8_lossy(&output);
            let lines: BTreeMap<_, _> = output
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            assert_eq!(
                lines["TERM_PROGRAM"],
                if explicit { "manual" } else { "ddg" }
            );
            assert_eq!(
                lines["TERM_PROGRAM_VERSION"],
                if explicit {
                    "manual"
                } else {
                    env!("CARGO_PKG_VERSION")
                }
            );
            for key in external {
                assert_eq!(
                    lines.get(key).copied(),
                    (explicit && key == "TMUX").then_some("manual")
                );
            }
        }
        let non_tty = PtyHandle {
            master: File::open("/dev/null").unwrap().into(),
        };
        assert_eq!(non_tty.foreground_pgrp(), None);
    }
}
