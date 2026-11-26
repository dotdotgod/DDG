//! Sandboxed stdio JSON-RPC 2.0 backend plugin host (`ext.<plugin>.*`).
//!
//! Each plugin lives in `<DDG_PLUGINS_DIR>/<dir>/plugin.toml` (or `plugin.json`),
//! runs as an isolated child process, declares its `ext.<name>.*` methods during
//! `plugin.hello`, and can only invoke host reverse-RPCs permitted by its declared
//! `capabilities`. Plugin crashes or timeouts return `PluginUnavailable` without
//! ever affecting `ddg-agents-host` or active PTYs.

use crate::host_service::HostService;
use ddg_agents_proto::host::{
    ExecutorSpec, RoomCreateParams, RoomRenameParams, TabCloseParams, TabCreateParams, TabRecord,
    TabRestartParams,
};
use ddg_agents_proto::plugin::{
    PluginCapability, PluginHelloResult, PluginManifest, PluginMethodSpec, is_valid_ext_method,
};
use ddg_agents_proto::rpc::{ErrorKind, Request, Response, RpcError};
use ddg_agents_proto::terminal::{PtyKillParams, PtyListResult};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

struct PluginInstance {
    manifest: PluginManifest,
    methods: Vec<PluginMethodSpec>,
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>,
    alive: Arc<AtomicBool>,
    next_id: AtomicU64,
}

impl PluginInstance {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(RpcError::new(
                ErrorKind::PluginUnavailable,
                format!("plugin {:?} is unavailable", self.manifest.name),
                None,
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(id, method, params);
        let line = format!(
            "{}\n",
            serde_json::to_string(&req).map_err(|e| RpcError::new(
                ErrorKind::Internal,
                e.to_string(),
                None
            ))?
        );
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        if self.tx.send(line).is_err() {
            self.alive.store(false, Ordering::Relaxed);
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(RpcError::new(
                ErrorKind::PluginUnavailable,
                format!("plugin {:?} stdin closed", self.manifest.name),
                None,
            ));
        }
        let timeout = Duration::from_millis(self.manifest.timeout_ms);
        let resp = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                RpcError::new(
                    ErrorKind::PluginUnavailable,
                    format!(
                        "plugin {:?} timed out on {method} after {}ms",
                        self.manifest.name, self.manifest.timeout_ms
                    ),
                    None,
                )
            })?
            .map_err(|_| {
                self.alive.store(false, Ordering::Relaxed);
                RpcError::new(
                    ErrorKind::PluginUnavailable,
                    format!(
                        "plugin {:?} crashed or exited during {method}",
                        self.manifest.name
                    ),
                    None,
                )
            })?;
        match resp.error {
            Some(err) => Err(err),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }
}

pub struct PluginHost {
    plugins: Mutex<HashMap<String, Arc<PluginInstance>>>,
    children: Mutex<HashSet<u32>>,
}

impl PluginHost {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            plugins: Mutex::new(HashMap::new()),
            children: Mutex::new(HashSet::new()),
        })
    }

    pub fn owns_pid(&self, pid: u32) -> bool {
        self.children
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&pid)
    }

    /// Scans `plugins_dir` for `<subdir>/plugin.toml` or `<subdir>/plugin.json`
    /// and starts each valid plugin in an isolated subprocess.
    pub async fn load_dir(self: &Arc<Self>, plugins_dir: &Path, host: Weak<HostService>) {
        let Ok(entries) = std::fs::read_dir(plugins_dir) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();

        for dir in dirs {
            let manifest = match load_manifest_from_dir(&dir) {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("ddg-agents-host: skipping plugin in {}: {e}", dir.display());
                    continue;
                }
            };
            match spawn_plugin(manifest, &dir, host.clone()).await {
                Ok(inst) => {
                    let name = inst.manifest.name.clone();
                    self.plugins
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(name, inst);
                }
                Err(e) => {
                    eprintln!(
                        "ddg-agents-host: failed to start plugin in {}: {e}",
                        dir.display()
                    );
                }
            }
        }
    }

    pub fn discover_methods(&self) -> Vec<Value> {
        let guard = self.plugins.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::new();
        let mut names: Vec<&String> = guard.keys().collect();
        names.sort();
        for name in names {
            if let Some(inst) = guard.get(name)
                && inst.alive.load(Ordering::Relaxed)
            {
                for m in &inst.methods {
                    out.push(json!({
                        "name": m.name,
                        "summary": m.description.clone().unwrap_or_else(|| format!("Plugin {} method", inst.manifest.name)),
                        "paramStructure": "by-name",
                        "params": [{
                            "name": "params",
                            "required": false,
                            "schema": m.params_schema.clone().unwrap_or_else(|| json!({"type": "object"})),
                        }],
                        "result": {
                            "name": "result",
                            "schema": m.result_schema.clone().unwrap_or_else(|| json!({})),
                        }
                    }));
                }
            }
        }
        out
    }

    pub async fn call_ext(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let Some(rest) = method.strip_prefix("ext.") else {
            return Err(RpcError::not_found(format!("unknown method {method:?}")));
        };
        let Some((plugin_name, _)) = rest.split_once('.') else {
            return Err(RpcError::invalid_params(format!(
                "invalid extension method {method:?} (expected ext.<plugin>.<action>)"
            )));
        };
        if !is_valid_ext_method(plugin_name, method) {
            return Err(RpcError::invalid_params(format!(
                "invalid extension method name {method:?}"
            )));
        }
        let inst = self
            .plugins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(plugin_name)
            .cloned()
            .ok_or_else(|| {
                RpcError::new(
                    ErrorKind::PluginUnavailable,
                    format!("plugin {plugin_name:?} is not loaded"),
                    None,
                )
            })?;
        inst.call(method, params).await
    }
}

async fn spawn_plugin(
    manifest: PluginManifest,
    plugin_dir: &Path,
    host: Weak<HostService>,
) -> Result<Arc<PluginInstance>, String> {
    manifest.validate()?;
    let mut program = PathBuf::from(&manifest.command[0]);
    if manifest.command[0].starts_with("./") || manifest.command[0].starts_with("../") {
        program = plugin_dir.join(&manifest.command[0]);
    }
    let mut cmd = Command::new(&program);
    cmd.args(&manifest.command[1..])
        .current_dir(plugin_dir)
        .env_remove("DDG_SESSION_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {} failed: {e}", program.display()))?;
    let pid = child.id().ok_or("missing plugin PID")?;
    let owner = host.upgrade().map(|h| h.plugins.clone());
    if let Some(owner) = &owner {
        owner
            .children
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(pid);
    }
    let mut stdin = child.stdin.take().ok_or("missing plugin stdin")?;
    let stdout = child.stdout.take().ok_or("missing plugin stdout")?;

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let capabilities: HashSet<PluginCapability> = manifest.capabilities.iter().copied().collect();

    let alive_w = alive.clone();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
        alive_w.store(false, Ordering::Relaxed);
    });

    let pending_r = pending.clone();
    let alive_r = alive.clone();
    let tx_r = tx.clone();
    let caps_r = capabilities.clone();
    let plugin_name = manifest.name.clone();

    tokio::spawn(async move {
        let mut _child = child;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if val.get("method").is_some() {
                // Reverse-call from plugin → host with strict capability enforcement!
                let Ok(req) = serde_json::from_value::<Request>(val) else {
                    continue;
                };
                let host_opt = host.upgrade();
                let caps = caps_r.clone();
                let out_tx = tx_r.clone();
                let pname = plugin_name.clone();
                tokio::spawn(async move {
                    let resp = handle_plugin_reverse_call(&pname, &caps, host_opt, req).await;
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = out_tx.send(format!("{s}\n"));
                    }
                });
            } else if let Some(id) = val.get("id").and_then(Value::as_u64)
                && let Ok(resp) = serde_json::from_value::<Response>(val)
                && let Some(waiter) = pending_r
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id)
            {
                let _ = waiter.send(resp);
            }
        }
        alive_r.store(false, Ordering::Relaxed);
        pending_r.lock().unwrap_or_else(|p| p.into_inner()).clear();
        let _ = _child.kill().await;
        if let Some(owner) = owner {
            owner
                .children
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&pid);
        }
    });

    // Run `plugin.hello` handshake before exposing methods.
    let (hello_tx, hello_rx) = oneshot::channel();
    pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(1, hello_tx);
    let hello_req = Request::new(
        1,
        "plugin.hello",
        json!({
            "protocol_version": 1,
            "plugin": manifest.name,
            "capabilities": manifest.capabilities,
        }),
    );
    let hello_line = format!(
        "{}\n",
        serde_json::to_string(&hello_req).expect("serializable")
    );
    tx.send(hello_line)
        .map_err(|_| "plugin closed before handshake".to_owned())?;

    let resp = tokio::time::timeout(Duration::from_secs(5), hello_rx)
        .await
        .map_err(|_| format!("plugin {:?} timed out on plugin.hello", manifest.name))?
        .map_err(|_| format!("plugin {:?} exited during plugin.hello", manifest.name))?;
    if let Some(err) = resp.error {
        return Err(format!("plugin.hello rejected: {}", err.message));
    }
    let hello: PluginHelloResult = serde_json::from_value(resp.result.unwrap_or(Value::Null))
        .map_err(|e| format!("invalid plugin.hello result: {e}"))?;

    for m in &hello.methods {
        if !is_valid_ext_method(&manifest.name, &m.name) {
            return Err(format!(
                "plugin {:?} tried to register forbidden method {:?}",
                manifest.name, m.name
            ));
        }
    }

    Ok(Arc::new(PluginInstance {
        manifest,
        methods: hello.methods,
        tx,
        pending,
        alive,
        next_id: AtomicU64::new(2),
    }))
}

fn redact_for_plugin(tabs: &mut [TabRecord]) {
    for tab in tabs {
        if let ExecutorSpec::Command { env, .. } = &mut tab.executor {
            env.clear();
        }
    }
}

async fn handle_plugin_reverse_call(
    plugin_name: &str,
    caps: &HashSet<PluginCapability>,
    host: Option<Arc<HostService>>,
    req: Request,
) -> Response {
    let id = req.id.clone();
    let Some(host) = host else {
        return Response::err(
            id,
            RpcError::new(ErrorKind::Unavailable, "host shutting down", None),
        );
    };
    let Ok(_op) = host.reload_gate.try_read() else {
        return Response::err(id, crate::admin::draining());
    };
    let require = |cap: PluginCapability| -> Result<(), RpcError> {
        if caps.contains(&cap) {
            Ok(())
        } else {
            Err(RpcError::new(
                ErrorKind::PermissionDenied,
                format!(
                    "plugin {plugin_name:?} lacks capability {cap:?} for {}",
                    req.method
                ),
                None,
            ))
        }
    };

    if let Some(target) = req.params.get("host_id")
        && target.as_str() != Some(&host.host_id)
    {
        return Response::err(
            id,
            RpcError::permission_denied("plugin reverse calls are local only"),
        );
    }
    let result: Result<Value, RpcError> = match req.method.as_str() {
        "host.catalog_get" => require(PluginCapability::HostRead).map(|()| {
            let mut catalog = host.plugin_catalog();
            for room in &mut catalog.rooms {
                redact_for_plugin(&mut room.tabs);
            }
            json!(catalog)
        }),
        "host.room_create" => match require(PluginCapability::HostWrite) {
            Err(e) => Err(e),
            Ok(()) => match serde_json::from_value::<RoomCreateParams>(req.params) {
                Ok(p) => host.room_create(p).await.map(|mut r| {
                    redact_for_plugin(&mut r.room.tabs);
                    json!(r)
                }),
                Err(e) => Err(RpcError::invalid_params(e.to_string())),
            },
        },
        "host.room_rename" => match require(PluginCapability::HostWrite) {
            Err(e) => Err(e),
            Ok(()) => serde_json::from_value::<RoomRenameParams>(req.params)
                .map_err(|e| RpcError::invalid_params(e.to_string()))
                .and_then(|p| {
                    host.room_rename(p).map(|mut r| {
                        redact_for_plugin(&mut r.room.tabs);
                        json!(r)
                    })
                }),
        },
        "host.tab_create" => match require(PluginCapability::HostWrite) {
            Err(e) => Err(e),
            Ok(()) => match serde_json::from_value::<TabCreateParams>(req.params) {
                Ok(p) => host.tab_create_with_visibility(p, true).await.map(|mut r| {
                    redact_for_plugin(std::slice::from_mut(&mut r.tab));
                    json!(r)
                }),
                Err(e) => Err(RpcError::invalid_params(e.to_string())),
            },
        },
        "host.tab_close" => match require(PluginCapability::HostWrite) {
            Err(e) => Err(e),
            Ok(()) => match serde_json::from_value::<TabCloseParams>(req.params) {
                Ok(p) => host
                    .tab_close_with_visibility(p, true)
                    .await
                    .map(|r| json!(r)),
                Err(e) => Err(RpcError::invalid_params(e.to_string())),
            },
        },
        "host.tab_restart" => match require(PluginCapability::HostWrite) {
            Err(e) => Err(e),
            Ok(()) => match serde_json::from_value::<TabRestartParams>(req.params) {
                Ok(p) => host
                    .tab_restart_with_visibility(p, true)
                    .await
                    .map(|mut r| {
                        redact_for_plugin(std::slice::from_mut(&mut r.tab));
                        json!(r)
                    }),
                Err(e) => Err(RpcError::invalid_params(e.to_string())),
            },
        },
        "pty.list" => {
            async {
                require(PluginCapability::PtyObserve)?;
                let mut list: PtyListResult = host.term()?.call("pty.list", json!({})).await?;
                list.ptys.retain(|pty| host.plugin_pty_visible(pty));
                Ok(json!(list))
            }
            .await
        }
        "pty.kill" => {
            async {
                require(PluginCapability::PtyControl)?;
                let params: PtyKillParams = serde_json::from_value(req.params)
                    .map_err(|e| RpcError::invalid_params(e.to_string()))?;
                let term = host.term()?;
                let list: PtyListResult = term.call("pty.list", json!({})).await?;
                let pty = list
                    .ptys
                    .iter()
                    .find(|p| p.pty_id == params.pty_id)
                    .ok_or_else(|| RpcError::not_found("PTY not found"))?;
                if !host.plugin_pty_visible(pty) {
                    return Err(RpcError::permission_denied("PTY is not visible to plugins"));
                }
                term.call_value("pty.kill", json!(params)).await
            }
            .await
        }
        _ => Err(RpcError::permission_denied(
            "plugin reverse-call is not permitted",
        )),
    };

    match result {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, e),
    }
}

fn load_manifest_from_dir(dir: &Path) -> Result<Option<PluginManifest>, String> {
    let toml_path = dir.join("plugin.toml");
    if toml_path.is_file() {
        let text = std::fs::read_to_string(&toml_path)
            .map_err(|e| format!("read {}: {e}", toml_path.display()))?;
        return parse_plugin_toml(&text).map(Some);
    }
    let json_path = dir.join("plugin.json");
    if json_path.is_file() {
        let bytes =
            std::fs::read(&json_path).map_err(|e| format!("read {}: {e}", json_path.display()))?;
        let manifest: PluginManifest = serde_json::from_slice(&bytes)
            .map_err(|e| format!("parse {}: {e}", json_path.display()))?;
        manifest.validate()?;
        return Ok(Some(manifest));
    }
    Ok(None)
}

/// Minimal strict parser for `plugin.toml` (`name`, `version`, `command`,
/// `capabilities`, `timeout_ms`) with zero extra external crate dependencies.
pub fn parse_plugin_toml(text: &str) -> Result<PluginManifest, String> {
    let mut obj = serde_json::Map::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = strip_toml_comment(raw).trim();
        if line.is_empty() || (line.starts_with('[') && line.ends_with(']')) {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!(
                "plugin.toml line {}: expected key = value",
                idx + 1
            ));
        };
        let key = k.trim();
        let val_str = v.trim();
        // TOML strings, integers, and string arrays are valid JSON values.
        let val: Value = serde_json::from_str(val_str).map_err(|e| {
            format!(
                "plugin.toml line {}: invalid value for {key:?}: {e}",
                idx + 1
            )
        })?;
        obj.insert(key.to_owned(), val);
    }
    let manifest: PluginManifest = serde_json::from_value(Value::Object(obj))
        .map_err(|e| format!("invalid plugin.toml: {e}"))?;
    manifest.validate()?;
    Ok(manifest)
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else if ch == '"' {
            in_str = true;
        } else if ch == '#' {
            return &line[..i];
        }
    }
    line
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::host_service::tests::{Fixture, fixture_catalog};
    use ddg_agents_proto::terminal::PtyInfo;

    #[tokio::test]
    async fn reverse_rpc_never_exposes_or_mutates_hidden_sessions() {
        let catalog = fixture_catalog();
        let room_id = catalog.rooms[1].id.clone();
        let child = catalog.rooms[1].tabs[0].id.clone();
        let ptys: Vec<_> = catalog
            .rooms
            .iter()
            .flat_map(|room| &room.tabs)
            .map(|tab| PtyInfo {
                pty_id: tab.pty_id.unwrap(),
                session_id: tab.id.clone(),
                run_generation: 1,
                pid: 100,
                cols: 80,
                rows: 24,
                cwd: None,
                title: String::new(),
                controller: None,
                agent_status: None,
                foreground: Some(ddg_agents_proto::terminal::ForegroundHint {
                    pid: tab.pty_id.unwrap(),
                    comm: if tab.pty_id == Some(11) {
                        "hidden-agent"
                    } else {
                        "visible-agent"
                    }
                    .into(),
                    script_hint: None,
                }),
            })
            .collect();
        let mut f = Fixture::new(catalog).await;
        let caps = HashSet::from([
            PluginCapability::HostRead,
            PluginCapability::HostWrite,
            PluginCapability::PtyControl,
            PluginCapability::PtyObserve,
        ]);
        let call = |method: &str, params| {
            let host = f.host.clone();
            let caps = caps.clone();
            let req = Request::new(1, method, params);
            tokio::spawn(
                async move { handle_plugin_reverse_call("test", &caps, Some(host), req).await },
            )
        };
        let result = call("host.catalog_get", json!({}))
            .await
            .unwrap()
            .result
            .unwrap();
        assert_eq!(result["rooms"].as_array().unwrap().len(), 1);
        assert_eq!(result["rooms"][0]["visibility"], "visible");
        for (method, params) in [
            (
                "host.room_rename",
                json!({"room_id": room_id, "name": "mutated"}),
            ),
            (
                "host.tab_create",
                json!({"room_id": room_id, "cols": 80, "rows": 24}),
            ),
            ("host.tab_create", json!({"cols": 80, "rows": 24})),
            (
                "host.tab_create",
                json!({"background": true, "cols": 80, "rows": 24}),
            ),
            ("host.tab_screen", json!({"tab_id": child})),
            (
                "host.tab_write",
                json!({"tab_id": child, "data_b64": "eA=="}),
            ),
            (
                "host.tab_move",
                json!({"tab_id": child, "target": {"kind": "new_room"}}),
            ),
            ("host.tab_close", json!({"tab_id": child})),
            (
                "host.tab_restart",
                json!({"tab_id": child, "cols": 80, "rows": 24}),
            ),
            (
                "host.tab_attach",
                json!({"tab_id": child, "cols": 80, "rows": 24}),
            ),
            ("host.catalog_get", json!({"host_id": "remote"})),
            ("host.session_list", json!({})),
            ("host.msg_send", json!({})),
            ("host.relay_attach", json!({})),
            ("host.list_hosts", json!({})),
        ] {
            let denied = call(method, params).await.unwrap().error.unwrap();
            assert_eq!(
                denied.data.code.as_deref(),
                Some("permission_denied"),
                "{method}"
            );
        }
        let list = call("pty.list", json!({}));
        let kill = call("pty.kill", json!({"pty_id": 11}));
        // Both reverse calls may only send pty.list upstream; hidden pty.kill is denied.
        for _ in 0..2 {
            let req = f.expect("pty.list").await;
            f.reply(req, json!({ "ptys": ptys })).await;
        }
        let listed: PtyListResult =
            serde_json::from_value(list.await.unwrap().result.unwrap()).unwrap();
        assert_eq!(listed.ptys, ptys[..1]);
        let denied = kill.await.unwrap().error.unwrap();
        assert_eq!(denied.data.code.as_deref(), Some("permission_denied"));
    }

    #[tokio::test]
    async fn reverse_rpc_cannot_message_or_replay_an_operator_close() {
        let mut catalog = fixture_catalog();
        let tab = &mut catalog.rooms[1].tabs[0];
        tab.pty_id = None;
        tab.parent_session_id = None;
        tab.runtime = ddg_agents_proto::host::TabRuntimeState::StoppedAfterReboot;
        let tab_id = tab.id.clone();
        let f = Fixture::new(catalog).await;
        let caps = HashSet::from([PluginCapability::HostWrite]);
        let message = handle_plugin_reverse_call(
            "test",
            &caps,
            Some(f.host.clone()),
            Request::new(
                1,
                "host.tab_msg_send",
                json!({"to_session_id": tab_id, "data_b64": "eA=="}),
            ),
        )
        .await;
        assert_eq!(
            message.error.unwrap().data.kind,
            ErrorKind::PermissionDenied
        );

        let close = TabCloseParams {
            tab_id,
            request_id: Some("close-hidden".into()),
        };
        f.host
            .tab_close_with_visibility(close.clone(), false)
            .await
            .unwrap();
        let replay = handle_plugin_reverse_call(
            "test",
            &caps,
            Some(f.host.clone()),
            Request::new(2, "host.tab_close", json!(close)),
        )
        .await;
        assert_eq!(replay.error.unwrap().data.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn plugin_redaction_and_hub_boundary() {
        let mut catalog = fixture_catalog();
        let visible = catalog.rooms[0].id.clone();
        let hidden = catalog.rooms[1].tabs[0].id.clone();
        let command = json!({"kind":"command","argv":["cat"],"env":{"PRIVATE":"secret"}});
        catalog.rooms[0].tabs[0].executor = serde_json::from_value(command.clone()).unwrap();
        catalog.rooms[1].tabs[0].executor = serde_json::from_value(command.clone()).unwrap();
        let mut f = Fixture::new(catalog).await;
        let caps = HashSet::from([
            PluginCapability::HostRead,
            PluginCapability::HostWrite,
            PluginCapability::PtyObserve,
        ]);
        let host = f.host.clone();
        let call = |method: &str, params| {
            let req = Request::new(1, method, params);
            let host = host.clone();
            let caps = caps.clone();
            tokio::spawn(
                async move { handle_plugin_reverse_call("test", &caps, Some(host), req).await },
            )
        };
        let assert_redacted = |result: &Value| {
            let data = result.to_string();
            for forbidden in [
                "secret",
                "PRIVATE",
                "OPERATOR_ONLY",
                "rm_bg_",
                "activities",
                hidden.as_str(),
            ] {
                assert!(!data.contains(forbidden), "{forbidden}: {result}");
            }
            let assert_tab = |tab: &Value| {
                let executor: ExecutorSpec =
                    serde_json::from_value(tab["executor"].clone()).unwrap();
                assert!(matches!(executor, ExecutorSpec::Command { env, .. } if env.is_empty()));
            };
            let assert_room = |room: &Value| {
                assert_eq!(room["visibility"], "visible");
                for tab in room["tabs"].as_array().unwrap() {
                    assert_tab(tab);
                }
            };
            if let Some(rooms) = result["rooms"].as_array() {
                rooms.iter().for_each(assert_room);
            } else if let Some(room) = result.get("room") {
                assert_room(room);
            } else {
                assert_tab(&result["tab"]);
            }
        };
        let get = call("host.catalog_get", json!({}))
            .await
            .unwrap()
            .result
            .unwrap();
        assert_eq!(get["rooms"].as_array().unwrap().len(), 1);
        assert_redacted(&get);
        let rename = call(
            "host.room_rename",
            json!({"room_id":visible,"name":"renamed"}),
        )
        .await
        .unwrap()
        .result
        .unwrap();
        assert_redacted(&rename);

        let room_params = json!({"cols":80,"rows":24,"executor":command,"request_id":"room"});
        let create_room = call("host.room_create", room_params.clone());
        let spawn = f.expect("pty.spawn").await;
        assert_eq!(spawn.params["env"]["PRIVATE"], "secret");
        f.reply(spawn, json!({"pty_id":20,"pid":120})).await;
        let room = create_room.await.unwrap().result.unwrap();
        assert_redacted(&room);
        assert_eq!(
            call("host.room_create", room_params)
                .await
                .unwrap()
                .result
                .unwrap(),
            room
        );

        let operator_host = host.clone();
        let operator = tokio::spawn(async move {
            let params = serde_json::from_value(json!({"cols":80,"rows":24,"executor":{"kind":"command","argv":["cat"],"env":{"OPERATOR_ONLY":"hidden"}},"request_id":"req-1"})).unwrap();
            operator_host.tab_create(params).await.unwrap()
        });
        let spawn = f.expect("pty.spawn").await;
        f.reply(spawn, json!({"pty_id":21,"pid":121})).await;
        let operator_id = operator.await.unwrap().tab.id;
        let params =
            json!({"room_id":visible,"cols":80,"rows":24,"executor":command,"request_id":"req-1"});
        let create_tab = call("host.tab_create", params.clone());
        let spawn = f.expect("pty.spawn").await;
        assert_eq!(spawn.params["env"]["PRIVATE"], "secret");
        f.reply(spawn, json!({"pty_id":22,"pid":122})).await;
        let tab = create_tab.await.unwrap().result.unwrap();
        assert_ne!(tab["tab"]["id"], operator_id);
        assert_redacted(&tab);
        assert_eq!(
            call("host.tab_create", params.clone())
                .await
                .unwrap()
                .result
                .unwrap(),
            tab
        );

        let restart = call(
            "host.tab_restart",
            json!({"tab_id":tab["tab"]["id"],"cols":80,"rows":24}),
        );
        let kill = f.expect("pty.kill").await;
        f.reply(kill, json!({"exit_code":0})).await;
        let spawn = f.expect("pty.spawn").await;
        f.reply(spawn, json!({"pty_id":23,"pid":123})).await;
        assert_redacted(&restart.await.unwrap().result.unwrap());

        for key in ["parent_session_id", "parent_host_id"] {
            let mut params = params.clone();
            params[key] = json!("forged");
            let error = call("host.tab_create", params)
                .await
                .unwrap()
                .error
                .unwrap();
            assert_eq!(error.data.kind, ErrorKind::InvalidParams);
        }
        for method in [
            "host.tab_mark_seen",
            "host.catalog_subscribe",
            "host.tab_activity_changed",
            "host.tab_seen_changed",
        ] {
            assert!(!crate::hub::allowed(method));
            assert_eq!(
                call(method, json!({}))
                    .await
                    .unwrap()
                    .error
                    .unwrap()
                    .data
                    .kind,
                ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn parses_plugin_toml_manifest() {
        let toml = r#"
            # Sample backend plugin
            name = "demo"
            version = "0.1.0"
            command = ["python3", "./demo_plugin.py"]
            capabilities = ["host_read"]
            timeout_ms = 5000
        "#;
        let m = parse_plugin_toml(toml).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.command, vec!["python3", "./demo_plugin.py"]);
        assert_eq!(m.capabilities, vec![PluginCapability::HostRead]);
        assert_eq!(m.timeout_ms, 5000);
    }
}
