//! The Holder process: owns OS PTYs, keeps a 1 MiB output ring buffer (`tail`)
//! per PTY, reaps children, and restarts `ddg-terminal-server core` if it
//! crashes while PTYs are still alive. No VT parsing, no policy.

use crate::paths::{self, Paths};
use crate::pty::{self, PtyHandle};
use ddg_agents_os::ipc::{LocalStream, connect};
use ddg_agents_proto::holder as proto;
use std::collections::{HashMap, VecDeque};
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
}

struct Holder {
    state: Mutex<HolderState>,
    changed: Condvar,
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
        }),
        changed: Condvar::new(),
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
            if let Err(e) = holder.serve(stream) {
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

    fn serve(self: &Arc<Self>, mut stream: LocalStream) -> io::Result<()> {
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

            let welcome = proto::HolderWelcome {
                version: proto::HOLDER_PROTO_VERSION,
                instance: self.instance.clone(),
                pid: std::process::id(),
                features: if cfg!(unix) {
                    vec!["fg_pgrp_v1".into()]
                } else {
                    vec![]
                },
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
            generation
        };

        let res = self.commands(&mut stream, generation);
        let mut st = self.lock();
        if st.core.as_ref().is_some_and(|c| c.generation == generation) {
            self.core_gone(&mut st, false);
        }
        res
    }

    fn commands(self: &Arc<Self>, stream: &mut LocalStream, generation: u64) -> io::Result<()> {
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
                    if let Some(t) = self.lock().ptys.remove(&pty_id)
                        && t.exit.is_none()
                    {
                        t.pty.signal_process_group(t.pid, pty::HANGUP);
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
        }
    }

    fn supervise(&self) {
        loop {
            std::thread::sleep(self.grace.min(Duration::from_millis(50)));
            let mut st = self.lock();
            let grace = if st.intentional_leave {
                Duration::from_secs(60)
            } else if st.generation == 0 {
                self.grace.max(Duration::from_secs(5))
            } else {
                self.grace
            };
            if st.core.is_some() || st.core_left.elapsed() < grace {
                continue;
            }
            let has_live = st.ptys.values().any(|t| t.exit.is_none());
            if !has_live {
                #[cfg(unix)]
                paths::remove_if_exists(&self.socket);
                std::process::exit(0);
            }
            let now = Instant::now();
            st.restarts
                .retain(|t| now.duration_since(*t) < Duration::from_secs(300));
            let backoff = self.grace * 2u32.pow(st.restarts.len().min(5) as u32);
            if st
                .restarts
                .last()
                .is_some_and(|t| now.duration_since(*t) < backoff.min(Duration::from_secs(30)))
            {
                continue;
            }
            let Ok(_start) = ddg_agents_os::lock::FileLock::try_lock_exclusive(
                &self.paths.state_dir.join("terminal-start.lock"),
            ) else {
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
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{Value, json};

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
            }),
            changed: Condvar::new(),
            instance: "test".into(),
            grace: Duration::ZERO,
            bin_path: PathBuf::new(),
            paths: Paths::from_env().unwrap(),
            socket: PathBuf::new(),
        });
        let service = std::thread::spawn(move || holder.serve(stream).unwrap());
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
            ["fg_pgrp_v1"]
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
