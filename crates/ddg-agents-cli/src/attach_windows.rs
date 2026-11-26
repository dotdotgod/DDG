use std::io::{self, IsTerminal, Read, Write};
use tokio::sync::mpsc;
use windows_sys::Win32::{Foundation::HANDLE, System::Console::*};

struct Mode(HANDLE, u32);
impl Mode {
    fn set(which: STD_HANDLE, enable: u32, disable: u32) -> io::Result<Self> {
        let mut original = 0;
        unsafe {
            let handle = GetStdHandle(which);
            if GetConsoleMode(handle, &mut original) == 0
                || SetConsoleMode(handle, (original & !disable) | enable) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(handle, original))
        }
    }
}
impl Drop for Mode {
    fn drop(&mut self) {
        unsafe { SetConsoleMode(self.0, self.1) };
    }
}

pub(super) struct ConsoleMode {
    _input: Option<Mode>,
    _output: Option<Mode>,
}
impl ConsoleMode {
    pub(super) fn enter(tty: bool) -> io::Result<Self> {
        Ok(Self {
            _input: tty
                .then(|| {
                    Mode::set(
                        STD_INPUT_HANDLE,
                        ENABLE_VIRTUAL_TERMINAL_INPUT,
                        ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT,
                    )
                })
                .transpose()?,
            _output: io::stdout()
                .is_terminal()
                .then(|| {
                    Mode::set(
                        STD_OUTPUT_HANDLE,
                        ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                        0,
                    )
                })
                .transpose()?,
        })
    }
}

pub fn terminal_size() -> (u16, u16) {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(which), &mut info) } != 0 {
            let rect = info.srWindow;
            return (
                (rect.Right - rect.Left + 1) as u16,
                (rect.Bottom - rect.Top + 1) as u16,
            );
        }
    }
    (80, 24)
}

pub(super) fn console_input() -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel(4);
    // shortcut: console reads can block until a key; this one-shot CLI exits after attach.
    // Use cancellable console input if attach ever runs in a long-lived process.
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let mut bytes = vec![0; 4096];
            let read = input.read(&mut bytes);
            if read
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::Interrupted)
            {
                continue;
            }
            let end = !matches!(&read, Ok(n) if *n > 0);
            if tx
                .blocking_send(read.map(|n| {
                    bytes.truncate(n);
                    bytes
                }))
                .is_err()
                || end
            {
                break;
            }
        }
    });
    rx
}

pub(super) async fn read_console(
    input: &mut Option<mpsc::Receiver<io::Result<Vec<u8>>>>,
    bytes: &mut Vec<u8>,
) -> io::Result<usize> {
    let Some(input) = input else {
        return std::future::pending().await;
    };
    *bytes = input.recv().await.transpose()?.unwrap_or_default();
    Ok(bytes.len())
}

#[derive(Default)]
pub(super) struct ConsoleOutput(Vec<u8>);

pub(super) fn write_console(output: &mut ConsoleOutput, bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if stdout.is_terminal() {
        output.0.extend_from_slice(bytes);
        let mut unprinted = output.0.as_slice();
        while !unprinted.is_empty() {
            match std::str::from_utf8(unprinted) {
                Ok(text) => {
                    stdout.write_all(text.as_bytes())?;
                    unprinted = &[];
                }
                Err(error) => {
                    let (valid, rest) = unprinted.split_at(error.valid_up_to());
                    stdout.write_all(valid)?;
                    let Some(invalid) = error.error_len() else {
                        unprinted = rest;
                        break;
                    };
                    stdout.write_all("\u{fffd}".as_bytes())?;
                    unprinted = &rest[invalid..];
                }
            }
        }
        let printed = output.0.len() - unprinted.len();
        output.0.drain(..printed);
    } else {
        stdout.write_all(bytes)?;
    }
    stdout.flush()
}
