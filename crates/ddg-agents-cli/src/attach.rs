use crate::output::{Error, Output, Result, cell};
use ddg_agents_proto::{id::encode_base64, terminal::ControlMode};
use ddg_agents_transport::{ConnectionState, HostClient, HostEvent, TabStreamHandle};
use serde_json::json;
use std::io::{self, IsTerminal};
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
#[cfg(unix)]
use unix::*;
#[cfg(windows)]
#[path = "attach_windows.rs"]
mod windows;
#[cfg(windows)]
pub use windows::terminal_size;
#[cfg(windows)]
use windows::{ConsoleMode, console_input, read_console, write_console};

#[cfg(unix)]
pub use unix::terminal_size;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{future::pending, os::fd::RawFd};
    use tokio::io::{Interest, unix::AsyncFd};

    pub fn terminal_size() -> (u16, u16) {
        for fd in [libc::STDOUT_FILENO, libc::STDIN_FILENO, libc::STDERR_FILENO] {
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0
                && size.ws_col != 0
                && size.ws_row != 0
            {
                return (size.ws_col, size.ws_row);
            }
        }
        (80, 24)
    }

    fn syscall(result: libc::c_int) -> io::Result<libc::c_int> {
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    pub(super) struct RawMode(RawFd, libc::termios);

    impl RawMode {
        pub(super) fn enter(fd: RawFd) -> io::Result<Self> {
            let mut original: libc::termios = unsafe { std::mem::zeroed() };
            syscall(unsafe { libc::tcgetattr(fd, &mut original) })?;
            let mut raw = original;
            unsafe { libc::cfmakeraw(&mut raw) };
            syscall(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) })?;
            Ok(Self(fd, original))
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            // Retry if an unrelated signal interrupted terminal restoration.
            loop {
                match syscall(unsafe { libc::tcsetattr(self.0, libc::TCSANOW, &self.1) }) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    _ => break,
                }
            }
        }
    }

    pub(super) struct Nonblocking(RawFd, libc::c_int);

    impl Nonblocking {
        pub(super) fn set(fd: RawFd) -> io::Result<Self> {
            let flags = syscall(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
            syscall(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
            Ok(Self(fd, flags))
        }
    }

    impl Drop for Nonblocking {
        fn drop(&mut self) {
            unsafe { libc::fcntl(self.0, libc::F_SETFL, self.1) };
        }
    }

    fn byte_count(result: libc::ssize_t) -> io::Result<usize> {
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }

    pub(super) fn poll_stdio(fd: RawFd, interest: Interest) -> io::Result<AsyncFd<RawFd>> {
        // SAFETY: the caller registers a standard descriptor that stays open
        // while the registration exists, and has already made it nonblocking.
        unsafe { AsyncFd::register_with_interest(fd, interest) }.map_err(|e| e.into_parts().1)
    }

    pub(super) async fn read_tty(
        input: Option<&AsyncFd<RawFd>>,
        bytes: &mut [u8],
    ) -> io::Result<usize> {
        let Some(input) = input else {
            return pending().await;
        };
        loop {
            let mut ready = input.readable().await?;
            match ready.try_io(|fd| {
                byte_count(unsafe {
                    libc::read(*fd.get_ref(), bytes.as_mut_ptr().cast(), bytes.len())
                })
            }) {
                Err(_) => continue,
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(result) => return result,
            }
        }
    }

    pub(super) async fn write_terminal(
        poll: &mut Option<AsyncFd<RawFd>>,
        mut bytes: &[u8],
    ) -> io::Result<()> {
        let fd = libc::STDOUT_FILENO;
        while !bytes.is_empty() {
            let write =
                || byte_count(unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) });
            let result = if let Some(poll) = poll {
                match poll.writable().await?.try_io(|_| write()) {
                    Ok(result) => result,
                    Err(_) => continue,
                }
            } else {
                match write() {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Regular-file stdout may not support readiness registration.
                        *poll = Some(poll_stdio(fd, Interest::WRITABLE)?);
                        continue;
                    }
                    result => result,
                }
            };
            match result {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                result => bytes = &bytes[result?..],
            }
        }
        Ok(())
    }
}

pub async fn run(
    client: &HostClient,
    events: mpsc::UnboundedReceiver<HostEvent>,
    id: &str,
    readonly: bool,
    force: bool,
    history: u32,
    out: Output,
) -> Result {
    let (cols, rows) = terminal_size();
    let handle = client
        .tab_attach(id.into(), !readonly, cols, rows, history)
        .await?;
    let channel = handle.channel;
    let result = attached(client, events, &handle, readonly, force, out).await;
    client.detach_channel(channel);
    if result.is_ok() && client.is_connected() {
        // Drain queued input and the detach frame before the one-shot runtime exits.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), client.ping()).await;
    }
    result
}

async fn attached(
    client: &HostClient,
    events: mpsc::UnboundedReceiver<HostEvent>,
    handle: &TabStreamHandle,
    readonly: bool,
    force: bool,
    out: Output,
) -> Result {
    let mut mode = handle.mode;
    if force && !readonly && mode != ControlMode::Control {
        client
            .tab_claim_control(handle.tab_id.clone(), true)
            .await?;
        mode = ControlMode::Control;
    }
    if io::stderr().is_terminal() {
        let label = if mode == ControlMode::Control {
            "control"
        } else {
            "readonly"
        };
        eprintln!(
            "[ddg] attached to {} ({label}, detach: Ctrl-])",
            cell(&handle.tab_id)
        );
    }
    #[cfg(windows)]
    {
        let shutdown = ddg_agents_os::process::shutdown_signal()?;
        tokio::select! {
            result = stream(client, events, handle, readonly, mode, out) => result,
            _ = shutdown => Ok(()),
        }
    }
    #[cfg(unix)]
    {
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut quit = signal(SignalKind::quit())?;
        tokio::select! {
            result = stream(client, events, handle, readonly, mode, out) => result,
            _ = interrupt.recv() => Ok(()),
            _ = terminate.recv() => Ok(()),
            _ = hangup.recv() => Ok(()),
            _ = quit.recv() => Ok(()),
        }
    }
}

async fn stream(
    client: &HostClient,
    mut events: mpsc::UnboundedReceiver<HostEvent>,
    handle: &TabStreamHandle,
    readonly: bool,
    mut mode: ControlMode,
    out: Output,
) -> Result {
    let channel = handle.channel;

    let tty = io::stdin().is_terminal();
    #[cfg(unix)]
    let (_raw, _stdin_flags, _stdout_flags, input, mut output, mut resize) = {
        use tokio::io::Interest;
        let stdin = libc::STDIN_FILENO;
        let raw = tty.then(|| RawMode::enter(stdin)).transpose()?;
        let stdin_flags = tty.then(|| Nonblocking::set(stdin)).transpose()?;
        let input = tty
            .then(|| poll_stdio(stdin, Interest::READABLE))
            .transpose()?;
        (
            raw,
            stdin_flags,
            Nonblocking::set(libc::STDOUT_FILENO)?,
            input,
            None,
            signal(SignalKind::window_change())?,
        )
    };
    #[cfg(windows)]
    let (_modes, mut input, mut output, mut resize, mut last_size) = (
        ConsoleMode::enter(tty)?,
        tty.then(console_input),
        windows::ConsoleOutput::default(),
        tokio::time::interval(std::time::Duration::from_millis(150)),
        terminal_size(),
    );
    #[cfg(unix)]
    let mut bytes = [0u8; 4096];
    #[cfg(windows)]
    let mut bytes = Vec::new();

    loop {
        #[cfg(unix)]
        let next_input = read_tty(input.as_ref(), &mut bytes);
        #[cfg(windows)]
        let next_input = read_console(&mut input, &mut bytes);
        #[cfg(unix)]
        let resized = resize.recv();
        #[cfg(windows)]
        let resized = resize.tick();
        tokio::select! {
            count = next_input => {
                let count = count?;
                if count == 0 { return Ok(()); }
                let data = &bytes[..count];
                let detach = data.iter().position(|&byte| byte == 0x1d);
                if !readonly && mode == ControlMode::Control
                    && !client.send_input(channel, &data[..detach.unwrap_or(data.len())])
                {
                    return Err(Error::new("unavailable", "terminal connection closed"));
                }
                if detach.is_some() { return Ok(()); }
            }
            _ = resized, if tty => {
                if !readonly && mode == ControlMode::Control {
                    let (cols, rows) = terminal_size();
                    #[cfg(windows)]
                    if std::mem::replace(&mut last_size, (cols, rows)) == (cols, rows) { continue; }
                    if !client.send_resize(channel, cols, rows) {
                        return Err(Error::new("unavailable", "terminal connection closed"));
                    }
                }
            }
            event = events.recv() => match event {
                Some(HostEvent::StreamData { channel: ch, bytes, .. }) if ch == channel => {
                    if out.0 {
                        let mut record = serde_json::to_vec(&json!({
                            "session_id": handle.tab_id,
                            "data_b64": encode_base64(&bytes),
                        }))?;
                        record.push(b'\n');
                        #[cfg(unix)]
                        write_terminal(&mut output, &record).await?;
                        #[cfg(windows)]
                        write_console(&mut output, &record)?;
                    } else {
                        #[cfg(unix)]
                        write_terminal(&mut output, &bytes).await?;
                        #[cfg(windows)]
                        write_console(&mut output, &bytes)?;
                    }
                }
                Some(HostEvent::StreamClose { channel: ch, .. }) if ch == channel => return Ok(()),
                Some(HostEvent::TabControlChanged { tab_id, mode: new_mode, .. }) if tab_id == handle.tab_id => {
                    mode = new_mode;
                }
                Some(HostEvent::StateChanged(ConnectionState::Disconnected(_))) | None => {
                    return Err(Error::new("unavailable", "terminal connection closed"));
                }
                _ => {}
            }
        }
    }
}
