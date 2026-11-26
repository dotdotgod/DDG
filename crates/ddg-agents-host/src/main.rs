mod admin;
mod catalog_store;
mod cli;
mod connect;
mod daemon_swap;
mod host_service;
mod hub;
mod paths;
mod plugin_host;
mod server;
mod term_conn;
mod token_store;

use std::io::{self, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--version"] {
        println!("ddg-agents-host {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if matches!(
        args.first().map(String::as_str),
        Some("__apply-daemon-swap" | "__rollback-daemon-swap")
    ) {
        daemon_swap::run(&args);
        return;
    }
    let res = match args.first().map(String::as_str) {
        Some("serve") if args.len() == 1 => server::serve(false),
        Some("serve") if args.as_slice() == ["serve", "--restore-tokens-pipe"] => {
            server::serve(true)
        }
        Some("connect") if args.len() == 1 => connect::run_connect(),
        Some("health") if args.len() == 1 => connect::run_health(),
        _ => cli::run(&args),
    };
    if let Err(e) = res {
        let _ = writeln!(io::stderr(), "ddg-agents-host: {e}");
        std::process::exit(1);
    }
}
