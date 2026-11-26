//! `ddg-agents-host connect` (stdin/stdout ↔ host.sock bridge) and `health`.

use crate::paths::Paths;
use ddg_agents_os::ipc::{LocalStream, connect};
use ddg_agents_proto::rpc::{Request, Response};
use serde_json::json;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn connect_or_start(paths: &Paths) -> io::Result<LocalStream> {
    if let Ok(stream) = connect(&paths.host_sock) {
        return Ok(stream);
    }
    paths.create_dirs()?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .env("DDG_STATE_DIR", &paths.state_dir)
        .env("DDG_RUN_DIR", &paths.run_dir)
        .env("DDG_PLUGINS_DIR", &paths.plugins_dir)
        .env_remove("DDG_SESSION_TOKEN")
        .env_remove("DDG_HUB_ENABLED")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ddg_agents_os::process::configure_detached(&mut cmd);
    let _child = cmd.spawn()?;

    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if let Ok(stream) = connect(&paths.host_sock) {
            return Ok(stream);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::other(format!(
        "ddg-agents-host did not listen within 6s at {}",
        paths.host_sock.display()
    )))
}

pub fn run_connect() -> io::Result<()> {
    let paths = Paths::from_env()?;
    let mut read_stream = connect_or_start(&paths)?;
    let mut write_stream = read_stream.try_clone()?;

    let stdin_thread = std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if write_stream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = write_stream.shutdown(std::net::Shutdown::Write);
    });

    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match read_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    #[cfg(unix)]
    let _ = stdin_thread.join();
    #[cfg(windows)]
    drop(stdin_thread);
    Ok(())
}

pub fn run_health() -> io::Result<()> {
    let paths = Paths::from_env()?;
    let mut stream = connect(&paths.host_sock)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let req = Request::new(1, "ping", json!({}));
    let line = format!("{}\n", serde_json::to_string(&req).expect("serializable"));
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let resp: Response =
        serde_json::from_str(&reply).map_err(|e| io::Error::other(e.to_string()))?;
    if resp.result.and_then(|v| v["ok"].as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(io::Error::other("ddg-agents-host health check failed"))
    }
}
