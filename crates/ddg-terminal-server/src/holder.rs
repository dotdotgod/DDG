//! The Holder process: owns OS PTYs, keeps a 1 MiB output ring buffer (`tail`)
//! per PTY, reaps children, and restarts `ddg-terminal-server core` if it
//! crashes while PTYs are still alive. No VT parsing, no policy.

use crate::paths::{self, Paths};
use crate::pty::{self, PtyHandle};
use ddg_agents_os::ipc::{LocalStream, connect};
use ddg_agents_proto::BUILTIN_AGENT_PRESET_BINS;
use ddg_agents_proto::holder as proto;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const TAIL_BYTES: usize = 1 << 20;
const EXIT_DRAIN: Duration = Duration::from_millis(200);

pub(crate) fn unknown_bins() -> proto::HolderProbeBinsResult {
    proto::HolderProbeBinsResult {
        items: BUILTIN_AGENT_PRESET_BINS
            .iter()
            .map(|bin| proto::HolderBinStatus {
                bin: (*bin).into(),
                status: proto::BinProbeStatus::Unknown,
            })
            .collect(),
    }
}

fn probe_path(path: Option<&OsStr>) -> proto::HolderProbeBinsResult {
    use proto::BinProbeStatus::*;

    let mut result = unknown_bins();
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return result;
    };
    let mut dirs: Vec<_> = std::env::split_paths(path).collect();
    let count = dirs.len();
    dirs.retain(|dir| dir.is_absolute());
    for item in &mut result.items {
        item.status = if dirs.len() == count {
            NotFound
        } else {
            Unknown
        };
        for dir in &dirs {
            match probe_candidate(dir, &item.bin) {
                Found => {
                    item.status = Found;
                    break;
                }
                Unknown => item.status = Unknown,
                NotFound => {}
            }
        }
    }
    result
}

#[cfg(unix)]
fn probe_candidate(dir: &Path, bin: &str) -> proto::BinProbeStatus {
    use proto::BinProbeStatus::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let candidate = dir.join(bin);
    if fs::metadata(&candidate).is_ok_and(|meta| meta.is_file())
        && CString::new(candidate.as_os_str().as_bytes())
            .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 })
    {
        Found
    } else {
        NotFound
    }
}

#[cfg(windows)]
fn probe_candidate(dir: &Path, bin: &str) -> proto::BinProbeStatus {
    use proto::BinProbeStatus::*;

    let is_file = |ext| match fs::metadata(dir.join(format!("{bin}.{ext}"))) {
        Ok(meta) => Ok(meta.is_file()),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    };
    match is_file("exe") {
        Ok(true) => Found,
        Err(_) => Unknown,
        Ok(false)
            if ["cmd", "bat", "ps1"]
                .into_iter()
                .any(|ext| !matches!(is_file(ext), Ok(false))) =>
        {
            Unknown
        }
        Ok(false) => NotFound,
    }
}

struct HeldTerm {
    pid: u32,
    session_id: String,
    run_generation: u64,
    pty: Arc<PtyHandle>,
    input: Sender<Vec<u8>>,
    cols: u16,
    rows: u16,
    tail: VecDeque<u8>,
    raw_offset: u64,
    exit: Option<proto::HolderExit>,
    reader_done: bool,
}

impl HeldTerm {
    fn spawned(&self) -> proto::HolderSpawned {
        proto::HolderSpawned {
            pid: self.pid,
            session_id: self.session_id.clone(),
            run_generation: self.run_generation,
        }
    }
}

struct CoreConn {
    generation: u64,
    frames: Sender<Vec<u8>>,
    stream: LocalStream,
}

struct HolderState {
    ptys: HashMap<u32, HeldTerm>,
    core: Option<CoreConn>,
    generation: u64,
    core_left: Instant,
    intentional_leave: bool,
    restarts: Vec<Instant>,
    probe_running: bool,
}

impl HolderState {
    fn disconnect_deadline(&self, grace: Duration) -> Option<Instant> {
        if self.core.is_some() {
            return None;
        }
        let grace = if self.intentional_leave {
            Duration::from_secs(60)
        } else if self.generation == 0 {
            grace.max(Duration::from_secs(5))
        } else {
            grace
        };
        Some(self.core_left + grace)
    }
}

struct Holder {
    state: Mutex<HolderState>,
    changed: Condvar,
    core_changed: Condvar,
    instance: String,
    grace: Duration,
    bin_path: PathBuf,
    paths: Paths,
    #[cfg(unix)]
    socket: PathBuf,
}

pub fn spawn_detached(
    paths: &Paths,
    bin_path: &Path,
    cmd: &mut Command,
    log_name: &str,
) -> io::Result<()> {
    paths.create_dirs()?;
    cmd.arg("--bin-path").arg(bin_path);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.state_dir.join(log_name))?;
    let log_err = log.try_clone()?;

    cmd.env("DDG_STATE_DIR", &paths.state_dir)
        .env("DDG_RUN_DIR", &paths.run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    ddg_agents_os::process::configure_detached(cmd);
    let _child = cmd.spawn()?;
    Ok(())
}

pub fn run(socket: PathBuf, bin_path: PathBuf) -> io::Result<()> {
    let paths = Paths::from_env()?;
    paths.create_dirs()?;
    if connect(&socket).is_ok() {
        return Err(io::Error::other(format!(
            "holder already listening at {}",
            socket.display()
        )));
    }
    #[cfg(unix)]
    let listener = {
        paths::remove_if_exists(&socket);
        let listener = UnixListener::bind(&socket)?;
        paths::set_socket_permissions(&socket)?;
        listener
    };
    #[cfg(unix)]
    let incoming = listener.incoming();
    #[cfg(windows)]
    let incoming = ddg_agents_os::ipc::LocalListener::bind(&socket)?.incoming()?;

    let grace = std::env::var("DDG_HOLDER_GRACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(1), Duration::from_millis);

    let holder = Arc::new(Holder {
        state: Mutex::new(HolderState {
            ptys: HashMap::new(),
            core: None,
            generation: 0,
            core_left: Instant::now(),
            intentional_leave: false,
            restarts: Vec::new(),
            probe_running: false,
        }),
        changed: Condvar::new(),
        core_changed: Condvar::new(),
        instance: paths::random_hex(),
        grace: grace.clamp(Duration::from_millis(1), Duration::from_secs(60)),
        bin_path,
        paths,
        #[cfg(unix)]
        socket,
    });

    let supervisor = holder.clone();
    std::thread::Builder::new()
        .name("holder-supervise".into())
        .spawn(move || supervisor.supervise())?;

    for stream in incoming {
        let holder = holder.clone();
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(io::stderr(), "holder: accept failed: {e}");
                continue;
            }
        };
        std::thread::spawn(move || {
            if let Err(e) = holder.serve(stream, || probe_path(std::env::var_os("PATH").as_deref()))
            {
                let _ = writeln!(io::stderr(), "holder: core connection error: {e}");
            }
        });
    }
    Ok(())
}

impl Holder {
    fn lock(&self) -> MutexGuard<'_, HolderState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send_core(st: &HolderState, frame: Vec<u8>) {
        if let Some(core) = &st.core {
            let _ = core.frames.send(frame);
        }
    }

    fn core_gone(&self, st: &mut HolderState, intentional: bool) {
        if let Some(core) = st.core.take()
            && !intentional
        {
            let _ = core.stream.shutdown(std::net::Shutdown::Both);
        }
        st.intentional_leave = intentional;
        st.core_left = Instant::now();
        self.core_changed.notify_one();
    }

    fn write_to_core(&self, generation: u64, mut stream: LocalStream, frames: Receiver<Vec<u8>>) {
        for frame in frames {
            if stream.write_all(&frame).is_err() {
                break;
            }
        }
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|c| c.generation == generation) {
            self.core_gone(&mut st, false);
        }
    }

    fn serve(
        self: &Arc<Self>,
        mut stream: LocalStream,
        probe: impl Fn() -> proto::HolderProbeBinsResult + Clone + Send + 'static,
    ) -> io::Result<()> {
        let hello = match proto::read_frame(&mut stream)? {
            Some((proto::HELLO, _, payload)) => proto::parse_json::<proto::HolderHello>(&payload)?,
            _ => return Ok(()),
        };
        if hello.version != proto::HOLDER_PROTO_VERSION {
            return Err(io::Error::other(format!(
                "unsupported holder protocol version {}",
                hello.version
            )));
        }

        let generation = {
            let mut st = self.lock();
            if st.core.is_some() {
                self.core_gone(&mut st, false);
            }
            st.generation += 1;
            st.intentional_leave = false;
            let mut ids: Vec<u32> = st.ptys.keys().copied().collect();
            ids.sort_unstable();

            let mut features = Vec::new();
            if cfg!(unix) {
                features.push("fg_pgrp_v1".into());
            }
            features.push(proto::FEATURE_PROBE_BINS_V1.into());
            let welcome = proto::HolderWelcome {
                version: proto::HOLDER_PROTO_VERSION,
                instance: self.instance.clone(),
                pid: std::process::id(),
                features,
                ptys: ids
                    .iter()
                    .map(|id| {
                        let t = &st.ptys[id];
                        proto::HeldPty {
                            pty_id: *id,
                            session_id: t.session_id.clone(),
                            run_generation: t.run_generation,
                            pid: t.pid,
                            cols: t.cols,
                            rows: t.rows,
                            exit: t.exit,
                            raw_offset: t.raw_offset,
                        }
                    })
                    .collect(),
            };
            let mut initial = proto::encode_json(proto::WELCOME, 0, &welcome);
            for id in &ids {
                let t = &st.ptys[id];
                let (a, b) = t.tail.as_slices();
                let mut bytes = Vec::with_capacity(a.len() + b.len());
                bytes.extend_from_slice(a);
                bytes.extend_from_slice(b);
                initial.extend_from_slice(&proto::encode_frame(proto::TAIL, *id, &bytes));
            }

            let (frames, rx) = mpsc::channel();
            let writer_stream = stream.try_clone()?;
            let kept_stream = stream.try_clone()?;
            let holder = self.clone();
            let generation = st.generation;
            std::thread::spawn(move || holder.write_to_core(generation, writer_stream, rx));
            st.core = Some(CoreConn {
                generation,
                frames,
                stream: kept_stream,
            });
            Self::send_core(&st, initial);
            self.core_changed.notify_one();
            generation
        };

        let res = self.commands(&mut stream, generation, probe);
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|c| c.generation == generation) {
            self.core_gone(&mut st, false);
        }
        res
    }

    fn query_bins(
        self: &Arc<Self>,
        generation: u64,
        request_id: u32,
        probe: impl FnOnce() -> proto::HolderProbeBinsResult + Send + 'static,
    ) {
        let mut st = self.lock();
        if st.core.as_ref().is_none_or(|c| c.generation != generation) {
            return;
        }
        let reply = move |st: &HolderState, result| {
            Self::send_core(
                st,
                proto::encode_json(proto::BINS_RESULT, request_id, &result),
            );
        };
        if st.probe_running {
            reply(&st, unknown_bins());
            return;
        }

        st.probe_running = true;
        let holder = self.clone();
        if std::thread::Builder::new()
            .name("holder-probe-bins".into())
            .spawn(move || {
                let result = probe();
                let mut st = holder.lock();
                st.probe_running = false;
                // Request IDs belong to the Core connection that issued them.
                if st.core.as_ref().is_some_and(|c| c.generation == generation) {
                    reply(&st, result);
                }
            })
            .is_err()
        {
            st.probe_running = false;
            reply(&st, unknown_bins());
        }
    }

    fn commands(
        self: &Arc<Self>,
        stream: &mut LocalStream,
        generation: u64,
        probe: impl Fn() -> proto::HolderProbeBinsResult + Clone + Send + 'static,
    ) -> io::Result<()> {
        while let Some((kind, pty_id, payload)) = proto::read_frame(stream)? {
            match kind {
                proto::SPAWN => {
                    let spec: proto::HolderSpawn = proto::parse_json(&payload)?;
                    self.spawn(pty_id, spec);
                }
                proto::INPUT => {
                    if let Some(t) = self.lock().ptys.get(&pty_id) {
                        let _ = t.input.send(payload);
                    }
                }
                proto::RESIZE => {
                    let sz: proto::HolderSize = proto::parse_json(&payload)?;
                    let mut st = self.lock();
                    if let Some(t) = st.ptys.get_mut(&pty_id) {
                        t.cols = sz.cols;
                        t.rows = sz.rows;
                        let _ = t.pty.resize(sz.cols, sz.rows);
                    }
                }
                proto::SIGNAL => {
                    let sig: proto::HolderSignal = proto::parse_json(&payload)?;
                    let st = self.lock();
                    if let Some(t) = st.ptys.get(&pty_id).filter(|t| t.exit.is_none()) {
                        t.pty.signal_process_group(t.pid, sig.signal);
                    }
                }
                proto::RELEASE => {
                    let mut st = self.lock();
                    if let Some(t) = st.ptys.remove(&pty_id)
                        && t.exit.is_none()
                    {
                        t.pty.signal_process_group(t.pid, pty::HANGUP);
                        if st.core.is_none() {
                            self.core_changed.notify_one();
                        }
                    }
                }
                proto::QUERY_FG_PGRP => {
                    let st = self.lock();
                    let entries = if pty_id == 0 {
                        st.ptys
                            .iter()
                            .filter(|(_, t)| t.exit.is_none())
                            .map(|(&pty_id, t)| proto::HolderFgPgrp {
                                pty_id,
                                pgrp: t.pty.foreground_pgrp(),
                            })
                            .collect()
                    } else {
                        let pgrp = st
                            .ptys
                            .get(&pty_id)
                            .filter(|t| t.exit.is_none())
                            .and_then(|t| t.pty.foreground_pgrp());
                        vec![proto::HolderFgPgrp { pty_id, pgrp }]
                    };
                    Self::send_core(
                        &st,
                        proto::encode_json(
                            proto::FG_PGRP,
                            pty_id,
                            &proto::HolderFgPgrpBatch { entries },
                        ),
                    );
                }
                proto::QUERY_BINS => {
                    let _: proto::HolderProbeBins = proto::parse_json(&payload)?;
                    self.query_bins(generation, pty_id, probe.clone());
                }
                proto::GOODBYE => {
                    let mode: proto::HolderGoodbye = proto::parse_json(&payload)?;
                    let mut st = self.lock();
                    if st.core.as_ref().is_none_or(|c| c.generation != generation) {
                        return Ok(());
                    }
                    if mode == proto::HolderGoodbye::Shutdown {
                        for t in st.ptys.values().filter(|t| t.exit.is_none()) {
                            t.pty.signal_process_group(t.pid, pty::KILL);
                        }
                        Self::send_core(&st, proto::encode_frame(proto::BYE, 0, &[]));
                        drop(st);
                        std::thread::sleep(Duration::from_millis(50));
                        #[cfg(unix)]
                        paths::remove_if_exists(&self.socket);
                        std::process::exit(0);
                    }
                    Self::send_core(&st, proto::encode_frame(proto::BYE, 0, &[]));
                    self.core_gone(&mut st, true);
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn spawn(self: &Arc<Self>, pty_id: u32, spec: proto::HolderSpawn) {
        let mut st = self.lock();
        if let Some(t) = st.ptys.get(&pty_id) {
            Self::send_core(
                &st,
                proto::encode_json(proto::SPAWNED, pty_id, &t.spawned()),
            );
            return;
        }
        let started = pty::spawn_pty(&spec.argv, &spec.env, &spec.cwd, spec.cols, spec.rows)
            .and_then(|(pty, child)| Ok((pty.reader()?, pty.writer()?, pty, child)));
        let reply = match started {
            Err(e) => proto::encode_json(
                proto::SPAWN_FAILED,
                pty_id,
                &proto::HolderSpawnFailed {
                    error: format!("cannot start PTY process: {e}"),
                },
            ),
            Ok((reader, mut writer, pty, child)) => {
                let pid = child.id();
                let (input, rx) = mpsc::channel::<Vec<u8>>();
                std::thread::spawn(move || {
                    for bytes in rx {
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                });
                let held = HeldTerm {
                    pid,
                    session_id: spec.session_id,
                    run_generation: spec.run_generation,
                    pty: Arc::new(pty),
                    input,
                    cols: spec.cols,
                    rows: spec.rows,
                    tail: VecDeque::new(),
                    raw_offset: 0,
                    exit: None,
                    reader_done: false,
                };
                let reply = proto::encode_json(proto::SPAWNED, pty_id, &held.spawned());
                st.ptys.insert(pty_id, held);
                let pumper = self.clone();
                let reaper = self.clone();
                std::thread::spawn(move || pumper.pump(pty_id, reader));
                std::thread::spawn(move || reaper.reap(pty_id, child));
                reply
            }
        };
        Self::send_core(&st, reply);
    }

    fn pump(&self, pty_id: u32, mut reader: impl Read) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut st = self.lock();
            let Some(t) = st.ptys.get_mut(&pty_id) else {
                break;
            };
            t.tail.extend(&buf[..n]);
            let excess = t.tail.len().saturating_sub(TAIL_BYTES);
            if excess > 0 {
                t.tail.drain(..excess);
                t.raw_offset += excess as u64;
            }
            let frame = proto::encode_frame(proto::OUTPUT, pty_id, &buf[..n]);
            Self::send_core(&st, frame);
        }
        if let Some(t) = self.lock().ptys.get_mut(&pty_id) {
            t.reader_done = true;
        }
        self.changed.notify_all();
    }

    fn reap(self: &Arc<Self>, pty_id: u32, mut child: pty::Child) {
        let (code, signal) = match pty::wait_child(&mut child) {
            Ok(ended) => ended,
            Err(e) => {
                let _ = writeln!(io::stderr(), "holder: wait pty {pty_id}: {e}");
                return;
            }
        };
        let exit = proto::HolderExit { code, signal };
        let deadline = Instant::now() + EXIT_DRAIN;
        let mut st = self.lock();
        while st.ptys.get(&pty_id).is_some_and(|t| !t.reader_done) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        if let Some(t) = st.ptys.get_mut(&pty_id) {
            t.exit = Some(exit);
            let frame = proto::encode_json(proto::EXITED, pty_id, &exit);
            Self::send_core(&st, frame);
            if st.core.is_none() {
                self.core_changed.notify_one();
            }
        }
    }

    fn wait_for_core_change_until<'a>(
        &self,
        st: MutexGuard<'a, HolderState>,
        deadline: Instant,
    ) -> MutexGuard<'a, HolderState> {
        self.core_changed
            .wait_timeout(st, deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|p| p.into_inner())
            .0
    }

    fn supervise(&self) {
        let start_lock = self.paths.state_dir.join("terminal-start.lock");
        let mut st = self.lock();
        loop {
            let Some(deadline) = st.disconnect_deadline(self.grace) else {
                st = self
                    .core_changed
                    .wait(st)
                    .unwrap_or_else(|p| p.into_inner());
                continue;
            };
            let now = Instant::now();
            if now < deadline {
                st = self.wait_for_core_change_until(st, deadline);
                continue;
            }
            if !st.ptys.values().any(|t| t.exit.is_none()) {
                #[cfg(unix)]
                paths::remove_if_exists(&self.socket);
                std::process::exit(0);
            }
            let history = Duration::from_secs(300);
            st.restarts.retain(|t| now.duration_since(*t) < history);
            let backoff = (self.grace * 2u32.pow(st.restarts.len().min(5) as u32))
                .min(Duration::from_secs(30));
            if let Some((first, last)) = st.restarts.first().zip(st.restarts.last())
                && now < *last + backoff
            {
                let retry = (*last + backoff).min(*first + history);
                st = self.wait_for_core_change_until(st, retry);
                continue;
            }
            let Ok(start) = ddg_agents_os::lock::FileLock::try_lock_exclusive(&start_lock) else {
                // Another process owns startup; the file lock has no notification channel.
                st = self.wait_for_core_change_until(
                    st,
                    now + self.grace.min(Duration::from_millis(50)),
                );
                continue;
            };
            st.restarts.push(now);
            st.core_left = now;
            drop(st);
            if let Err(e) = spawn_detached(
                &self.paths,
                &self.bin_path,
                Command::new(&self.bin_path).arg("core"),
                "core.log",
            ) {
                let _ = writeln!(io::stderr(), "holder: failed to restart core: {e}");
            }
            drop(start);
            st = self.lock();
        }
    }
}

#[cfg(all(test, windows))]
#[test]
fn ut_term_01_windows_exe_and_shims() {
    use proto::BinProbeStatus::*;

    let dir = std::env::temp_dir().join(format!("ddg-probe-{}", paths::random_hex()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("pi.exe"), b"").unwrap();
    std::fs::write(dir.join("agy.cmd"), b"").unwrap();
    std::fs::write(dir.join("codex.bat"), b"").unwrap();
    std::fs::write(dir.join("opencode.ps1"), b"").unwrap();
    std::fs::create_dir(dir.join("claude.exe")).unwrap();
    let result = probe_path(Some(dir.as_os_str()));
    let relative = std::env::join_paths([dir.as_path(), Path::new(".")]).unwrap();
    let relative = probe_path(Some(&relative));
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(
        result
            .items
            .iter()
            .map(|item| item.status)
            .collect::<Vec<_>>(),
        [Found, Unknown, Unknown, Unknown, NotFound]
    );
    assert_eq!(relative.items.last().unwrap().status, Unknown);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn ut_term_01_unix_path_metadata_and_permissions() {
        use proto::BinProbeStatus::*;
        use std::ffi::OsStr;
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = std::env::temp_dir().join(format!("ddg-probe-{}", paths::random_hex()));
        std::fs::create_dir(&dir).unwrap();
        let marker = dir.join("executed");
        let pi = dir.join("pi");
        std::fs::write(&pi, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o700)).unwrap();
        let agy = dir.join("agy");
        std::fs::write(&agy, b"").unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::create_dir(dir.join("codex")).unwrap();
        symlink(dir.join("missing"), dir.join("opencode")).unwrap();
        symlink(&pi, dir.join("claude")).unwrap();

        let inspect = |path| {
            probe_path(path)
                .items
                .into_iter()
                .map(|item| item.status)
                .collect::<Vec<_>>()
        };
        let statuses = inspect(Some(dir.as_os_str()));
        let relative = std::env::join_paths([dir.as_path(), Path::new(".")]).unwrap();
        let empty_element = std::env::join_paths([dir.as_path(), Path::new("")]).unwrap();
        assert_eq!(statuses, [Found, NotFound, NotFound, NotFound, Found]);
        for path in [&relative, &empty_element] {
            assert_eq!(
                inspect(Some(path)),
                [Found, Unknown, Unknown, Unknown, Found]
            );
        }
        assert_eq!(inspect(None), [Unknown; 5]);
        assert_eq!(inspect(Some(OsStr::new(""))), [Unknown; 5]);
        let executed = marker.exists();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(!executed);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ut_term_01_execute_bit_does_not_override_acl() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ddg-probe-{}", paths::random_hex()));
        fs::create_dir(&dir).unwrap();
        let pi = dir.join("pi");
        fs::write(&pi, b"").unwrap();
        fs::set_permissions(&pi, fs::Permissions::from_mode(0o700)).unwrap();
        let user = Command::new("/usr/bin/id").arg("-un").output().unwrap();
        assert!(user.status.success());
        let user = std::str::from_utf8(&user.stdout).unwrap().trim_end();
        let acl = Command::new("/bin/chmod")
            .args(["+a", &format!("user:{user} deny execute")])
            .arg(&pi)
            .status()
            .unwrap();
        let status = probe_candidate(&dir, "pi");
        fs::remove_dir_all(dir).unwrap();
        assert!(acl.success());
        assert_eq!(status, proto::BinProbeStatus::NotFound);
    }

    #[test]
    fn supervision_arms_deadlines_only_while_disconnected() {
        let left = Instant::now();
        let grace = Duration::from_millis(100);
        let mut st = HolderState {
            ptys: HashMap::new(),
            core: None,
            generation: 0,
            core_left: left,
            intentional_leave: false,
            restarts: Vec::new(),
            probe_running: false,
        };
        assert_eq!(
            st.disconnect_deadline(grace),
            Some(left + Duration::from_secs(5))
        );
        st.generation = 1;
        let (stream, _peer) = LocalStream::pair().unwrap();
        st.core = Some(CoreConn {
            generation: 1,
            frames: mpsc::channel().0,
            stream,
        });
        assert_eq!(st.disconnect_deadline(grace), None);
        st.core = None;
        assert_eq!(st.disconnect_deadline(grace), Some(left + grace));
        st.intentional_leave = true;
        assert_eq!(
            st.disconnect_deadline(grace),
            Some(left + Duration::from_secs(60))
        );
    }

    #[test]
    fn foreground_group_frames_for_live_exited_and_missing_pty() {
        let (mut client, stream) = LocalStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let holder = Arc::new(Holder {
            state: Mutex::new(HolderState {
                ptys: HashMap::new(),
                core: None,
                generation: 0,
                core_left: Instant::now(),
                intentional_leave: false,
                restarts: Vec::new(),
                probe_running: false,
            }),
            changed: Condvar::new(),
            core_changed: Condvar::new(),
            instance: "test".into(),
            grace: Duration::ZERO,
            bin_path: PathBuf::new(),
            paths: Paths::from_env().unwrap(),
            socket: PathBuf::new(),
        });
        let (started, running) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Arc::new(Mutex::new(blocked));
        let service = std::thread::spawn(move || {
            holder
                .serve(stream, move || {
                    started.send(()).unwrap();
                    blocked.lock().unwrap().recv().unwrap();
                    unknown_bins()
                })
                .unwrap()
        });
        client
            .write_all(&proto::encode_json(
                proto::HELLO,
                0,
                &proto::HolderHello {
                    version: proto::HOLDER_PROTO_VERSION,
                },
            ))
            .unwrap();
        let (kind, _, payload) = proto::read_frame(&mut client).unwrap().unwrap();
        assert_eq!(kind, proto::WELCOME);
        assert_eq!(
            proto::parse_json::<proto::HolderWelcome>(&payload)
                .unwrap()
                .features,
            ["fg_pgrp_v1", proto::FEATURE_PROBE_BINS_V1]
        );

        let id = 3;
        client
            .write_all(&proto::encode_json(
                proto::SPAWN,
                id,
                &proto::HolderSpawn {
                    session_id: String::new(),
                    run_generation: 0,
                    argv: ["/bin/sh", "-c", "read hold"].map(String::from).into(),
                    env: Default::default(),
                    cwd: String::new(),
                    cols: 80,
                    rows: 24,
                },
            ))
            .unwrap();
        let (kind, _, payload) = proto::read_frame(&mut client).unwrap().unwrap();
        assert_eq!(kind, proto::SPAWNED);
        let pid = proto::parse_json::<proto::HolderSpawned>(&payload)
            .unwrap()
            .pid;
        assert!(pid > 0);
        fn query(client: &mut LocalStream, id: u32) -> Vec<proto::HolderFgPgrp> {
            client.write_all(&proto::encode_frame(8, id, &[])).unwrap();
            let (kind, pty_id, payload) = proto::read_frame(client).unwrap().unwrap();
            assert_eq!((kind, pty_id), (108, id));
            proto::parse_json::<proto::HolderFgPgrpBatch>(&payload)
                .unwrap()
                .entries
        }
        assert_eq!(
            query(&mut client, 0),
            [proto::HolderFgPgrp {
                pty_id: id,
                pgrp: Some(pid)
            }]
        );
        assert_eq!(
            query(&mut client, id),
            [proto::HolderFgPgrp {
                pty_id: id,
                pgrp: Some(pid)
            }]
        );
        assert_eq!(
            query(&mut client, 99),
            [proto::HolderFgPgrp {
                pty_id: 99,
                pgrp: None
            }]
        );

        let query_bins = |client: &mut LocalStream, id| {
            client
                .write_all(&proto::encode_json(
                    proto::QUERY_BINS,
                    id,
                    &proto::HolderProbeBins {},
                ))
                .unwrap();
        };
        query_bins(&mut client, 99);
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        query_bins(&mut client, 100);
        let (kind, request_id, payload) = proto::read_frame(&mut client).unwrap().unwrap();
        assert_eq!((kind, request_id), (proto::BINS_RESULT, 100));
        assert_eq!(
            proto::parse_json::<proto::HolderProbeBinsResult>(&payload).unwrap(),
            unknown_bins()
        );
        assert!(running.try_recv().is_err());

        client
            .write_all(&proto::encode_frame(proto::INPUT, id, b"\n"))
            .unwrap();
        loop {
            let (kind, _, _) = proto::read_frame(&mut client).unwrap().unwrap();
            if kind == proto::EXITED {
                break;
            }
            assert_eq!(kind, proto::OUTPUT);
        }
        assert!(query(&mut client, 0).is_empty());
        assert_eq!(
            query(&mut client, id),
            [proto::HolderFgPgrp {
                pty_id: id,
                pgrp: None
            }]
        );
        release.send(()).unwrap();
        let (kind, request_id, _) = proto::read_frame(&mut client).unwrap().unwrap();
        assert_eq!((kind, request_id), (proto::BINS_RESULT, 99));
        drop(client);
        service.join().unwrap();
    }

    #[test]
    fn holder_v1_wire_metadata_is_backward_compatible() {
        fn check<T: serde::de::DeserializeOwned + serde::Serialize>(
            kind: u8,
            mut old: Value,
            path: &str,
        ) {
            let value =
                serde_json::to_value(serde_json::from_value::<T>(old.clone()).unwrap()).unwrap();
            let fields = value.pointer(path).unwrap();
            assert_eq!(fields["session_id"], "");
            assert_eq!(fields["run_generation"], 0);

            let fields = old.pointer_mut(path).unwrap();
            fields["session_id"] = json!("ses_00000000000000000000000001");
            fields["run_generation"] = json!(7);
            let bytes =
                proto::encode_json(kind, 1, &serde_json::from_value::<T>(old.clone()).unwrap());
            let (actual_kind, id, payload) =
                proto::read_frame(&mut bytes.as_slice()).unwrap().unwrap();
            assert_eq!((actual_kind, id), (kind, 1));
            let value = serde_json::to_value(proto::parse_json::<T>(&payload).unwrap()).unwrap();
            assert_eq!(
                value.pointer(path).unwrap()["session_id"],
                "ses_00000000000000000000000001"
            );
            assert_eq!(value.pointer(path).unwrap()["run_generation"], 7);
        }
        assert_eq!(proto::HOLDER_PROTO_VERSION, 1);
        check::<proto::HolderSpawn>(
            proto::SPAWN,
            json!({"argv": [], "env": {}, "cwd": "/", "cols": 80, "rows": 24}),
            "",
        );
        check::<proto::HolderSpawned>(proto::SPAWNED, json!({"pid": 1}), "");
        check::<proto::HolderWelcome>(
            proto::WELCOME,
            json!({
                "version": 1, "instance": "test", "pid": 1,
                "ptys": [{"pty_id": 1, "pid": 2, "cols": 80, "rows": 24, "raw_offset": 0}],
            }),
            "/ptys/0",
        );
    }
}
