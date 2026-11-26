mod connect;
mod control;
mod core;
mod holder;
mod mailbox;
mod outbox;
mod paths;
mod proc_info;
#[cfg(unix)]
mod pty;
#[cfg(windows)]
#[path = "pty_windows.rs"]
mod pty;
mod server;

use std::io::{self, Write};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg_path = |name: &str| {
        args.windows(2)
            .find(|w| w[0] == name)
            .map(|w| PathBuf::from(&w[1]))
    };
    let bin_path = || match arg_path("--bin-path") {
        Some(path) if path.canonicalize()? == path && path.is_file() => Ok(path),
        Some(_) => Err(io::Error::other(
            "--bin-path must name a canonical executable file",
        )),
        None => std::env::current_exe()?.canonicalize(),
    };
    let result = match args.first().map(String::as_str) {
        Some("--version") => {
            println!("ddg-terminal-server {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("core") => bin_path().and_then(server::serve),
        Some("holder") => match (arg_path("--socket"), arg_path("--bin-path")) {
            (Some(socket), Some(_)) => bin_path().and_then(|bin| holder::run(socket, bin)),
            _ => Err(io::Error::other(
                "usage: ddg-terminal-server holder --socket <path> --bin-path <path>",
            )),
        },
        Some("connect") => connect::run_connect(),
        Some("health") => connect::run_health(),
        _ => Err(io::Error::other(
            "usage: ddg-terminal-server <core|holder --socket <path>|connect|health>",
        )),
    };
    if let Err(e) = result {
        let _ = writeln!(io::stderr(), "ddg-terminal-server: {e}");
        std::process::exit(1);
    }
}
