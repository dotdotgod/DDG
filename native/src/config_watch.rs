//! Reload config snapshots after filesystem changes, keeping the last valid values.

use crate::host_bridge::BridgeMessage;
use crate::ui_scale::UiPrefs;
#[cfg(test)]
use crate::ui_scale::UiScale;
use ddg_agents_transport::{
    HostsConfig,
    agent::{AgentsConfig, MAX_AGENTS_BYTES},
    programs::{MAX_PROGRAMS_BYTES, ProgramsConfig},
};
use gpui::private::serde_json;
use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

const DEBOUNCE: Duration = Duration::from_millis(50);

enum Load<T> {
    Missing,
    Invalid,
    Loaded(T),
}

fn strict_load<T>(path: &Path, parse: impl FnOnce(&[u8]) -> Option<T>) -> Load<T> {
    match fs::read(path) {
        Ok(bytes) => parse(&bytes).map_or(Load::Invalid, Load::Loaded),
        Err(err) if err.kind() == ErrorKind::NotFound => Load::Missing,
        Err(_) => Load::Invalid,
    }
}

fn strict_load_hosts(path: &Path) -> Load<HostsConfig> {
    strict_load(path, |bytes| {
        serde_json::from_slice::<HostsConfig>(bytes)
            .ok()
            .filter(|cfg| cfg.validate().is_ok() && !cfg.hosts.is_empty())
    })
}

fn strict_load_ui_prefs(path: &Path) -> Load<UiPrefs> {
    match UiPrefs::load(path) {
        Ok(prefs) => Load::Loaded(prefs),
        Err(err) if err.kind() == ErrorKind::NotFound => Load::Missing,
        Err(_) => Load::Invalid,
    }
}

fn read_config<T: gpui::private::serde::de::DeserializeOwned + Default>(
    path: &Path,
    max: usize,
    no_follow: bool,
) -> Result<T, String> {
    #[cfg(unix)]
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | if no_follow { libc::O_NOFOLLOW } else { 0 })
        .open(path);
    #[cfg(windows)]
    let opened = if no_follow {
        ddg_agents_os::fs::open_regular(path)
    } else {
        fs::File::open(path)
    };
    let file = match opened {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(T::default()),
        Err(_) => return Err("설정 파일을 열 수 없습니다".into()),
    };
    let mut bytes = Vec::new();
    if !file.metadata().is_ok_and(|m| m.is_file())
        || file.take((max + 1) as u64).read_to_end(&mut bytes).is_err()
        || bytes.len() > max
    {
        return Err("설정 파일을 읽을 수 없습니다".into());
    }
    serde_json::from_slice(&bytes).map_err(|err| format!("설정 파일 형식 오류 ({}행)", err.line()))
}

pub fn load_agents_snapshot(path: &Path) -> Result<AgentsConfig, String> {
    // The watcher does not create corruption backups for transient partial writes.
    read_config::<AgentsConfig>(path, MAX_AGENTS_BYTES, true)?;
    AgentsConfig::load(path)
        .map(|(config, _)| config)
        .map_err(|err| err.to_string())
}

fn load_programs_snapshot(path: &Path) -> Result<ProgramsConfig, String> {
    let parsed: ProgramsConfig = read_config(path, MAX_PROGRAMS_BYTES, false)?;
    if parsed == ProgramsConfig::default() {
        return Ok(parsed);
    }
    if ProgramsConfig::load(path) == parsed {
        Ok(parsed)
    } else {
        Err("programs.json 형식 오류".into())
    }
}

pub(crate) struct ConfigReloadState {
    pub hosts_path: Option<PathBuf>,
    pub last_hosts: HostsConfig,
    pub ui_prefs_path: Option<PathBuf>,
    pub last_ui: UiPrefs,
    pub agents_path: Option<PathBuf>,
    pub programs_path: Option<PathBuf>,
}

#[derive(Default)]
struct ReloadDiff {
    hosts: Option<HostsConfig>,
    ui_prefs: Option<UiPrefs>,
    had_invalid: bool,
}

fn reload<T: Default + PartialEq>(loaded: Load<T>, last: &T, invalid: &mut bool) -> Option<T> {
    let value = match loaded {
        Load::Missing => T::default(),
        Load::Loaded(value) => value,
        Load::Invalid => {
            *invalid = true;
            return None;
        }
    };
    (value != *last).then_some(value)
}

impl ConfigReloadState {
    fn reload_changed(&mut self) -> ReloadDiff {
        let mut diff = ReloadDiff::default();
        if let Some(path) = &self.hosts_path {
            diff.hosts = reload(
                strict_load_hosts(path),
                &self.last_hosts,
                &mut diff.had_invalid,
            )
            .inspect(|value| self.last_hosts.clone_from(value));
        }
        if let Some(path) = &self.ui_prefs_path {
            diff.ui_prefs = reload(
                strict_load_ui_prefs(path),
                &self.last_ui,
                &mut diff.had_invalid,
            )
            .inspect(|value| self.last_ui.clone_from(value));
        }
        diff
    }
}

fn refresh_watches(
    watcher: &mut RecommendedWatcher,
    parents: &[PathBuf],
    watched: &mut HashSet<PathBuf>,
    stale: &mut HashSet<PathBuf>,
) {
    let mut wanted = HashSet::new();
    for parent in parents {
        if let Some(dir) = parent.ancestors().find(|dir| dir.is_dir()) {
            // Also watch the containing directory: not all backends report deletion of a watch root.
            wanted.extend(
                [Some(dir), dir.parent()]
                    .into_iter()
                    .flatten()
                    .filter_map(|dir| dir.canonicalize().ok()),
            );
        }
    }
    if *watched == wanted && stale.is_empty() {
        return;
    }
    let mut paths = watcher.paths_mut();
    watched.retain(|dir| {
        if wanted.contains(dir) && !stale.contains(dir) {
            return true;
        }
        let _ = paths.remove(dir);
        false
    });
    stale.clear();
    for dir in wanted {
        if !watched.contains(&dir) {
            match paths.add(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    watched.insert(dir);
                }
                Err(err) => eprintln!("could not watch {}: {err}", dir.display()),
            }
        }
    }
    if let Err(err) = paths.commit() {
        eprintln!("could not update config watches: {err}");
    }
}

pub(crate) fn spawn_config_watcher(
    rt: &Handle,
    mut state: ConfigReloadState,
    out: mpsc::UnboundedSender<BridgeMessage>,
) -> JoinHandle<()> {
    rt.spawn(async move {
        let agents_path = state.agents_path.take();
        let programs_path = state.programs_path.take();
        let mut last_agents = None;
        let mut last_programs = None;
        let parents: Vec<_> = [&state.hosts_path, &state.ui_prefs_path, &agents_path, &programs_path]
            .into_iter()
            .flatten()
            .filter_map(|path| std::path::absolute(path).ok())
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect();
        if parents.is_empty() {
            return;
        }
        let (events, mut rx) = mpsc::unbounded_channel();
        let mut watcher = match notify::recommended_watcher(move |event: notify::Result<Event>| {
            // Reading the configs must not trigger another reload.
            if !matches!(&event, Ok(e) if e.kind.is_access() && e.kind != EventKind::Access(AccessKind::Close(AccessMode::Write))) {
                let _ = events.send(event);
            }
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                eprintln!("could not start config watcher: {err}");
                return;
            }
        };
        let mut watched = HashSet::new();
        let mut stale = HashSet::new();
        // Reload once after registering watches to cover edits between startup load and registration.
        let mut deadline = Some(Instant::now());
        let mut retry = false;
        loop {
            tokio::select! {
                biased;
                () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    refresh_watches(&mut watcher, &parents, &mut watched, &mut stale);
                    let diff = state.reload_changed();
                    if let Some(cfg) = diff.hosts
                        && out.send(BridgeMessage::HostsConfigReloaded(cfg)).is_err() {
                        break;
                    }
                    if let Some(prefs) = diff.ui_prefs
                        && out.send(BridgeMessage::UiPrefsReloaded(prefs)).is_err() {
                        break;
                    }
                    if let Some(path) = &agents_path {
                        let value = load_agents_snapshot(path);
                        if last_agents.as_ref() != Some(&value) {
                            if out.send(BridgeMessage::AgentsReloaded(value.clone())).is_err() { break; }
                            last_agents = Some(value);
                        }
                    }
                    if let Some(path) = &programs_path {
                        let value = load_programs_snapshot(path);
                        if last_programs.as_ref() != Some(&value) {
                            if out.send(BridgeMessage::ProgramsReloaded(value.clone())).is_err() { break; }
                            last_programs = Some(value);
                        }
                    }
                    retry = !retry && (diff.had_invalid
                        || last_agents.as_ref().is_some_and(Result::is_err)
                        || last_programs.as_ref().is_some_and(Result::is_err));
                    deadline = retry.then(|| Instant::now() + DEBOUNCE);
                }
                event = rx.recv() => {
                    let Some(event) = event else { break; };
                    match event {
                        Ok(event) if event.need_rescan() => stale.extend(watched.iter().cloned()),
                        Ok(event) if matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))) => {
                            // A recreated directory can have the same path but an obsolete watch.
                            stale.extend(watched.iter().filter(|dir| event.paths.iter().any(|path| dir.starts_with(path))).cloned());
                        }
                        Ok(_) => {},
                        Err(err) => eprintln!("config watch event: {err}"),
                    }
                    deadline = Some(Instant::now() + DEBOUNCE);
                    retry = false;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::time::timeout;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "ddg-config-watch-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed),
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                    Err(err) => panic!("create config test directory: {err}"),
                }
            }
        }

        fn state(&self, dir: &str) -> ConfigReloadState {
            ConfigReloadState {
                hosts_path: Some(self.0.join(dir).join("hosts.json")),
                last_hosts: HostsConfig::default(),
                ui_prefs_path: Some(self.0.join(dir).join("ui.json")),
                last_ui: UiPrefs::default(),
                agents_path: None,
                programs_path: None,
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn hosts(label: &str) -> HostsConfig {
        let mut cfg = HostsConfig::default();
        cfg.hosts[0].label = label.into();
        cfg
    }

    fn assert_no_diff(diff: ReloadDiff, invalid: bool) {
        assert!(diff.hosts.is_none());
        assert!(diff.ui_prefs.is_none());
        assert_eq!(diff.had_invalid, invalid);
    }

    #[test]
    fn launcher_modal_and_ui_prefs_preserve_v2_fields() {
        let root = Scratch::new();
        let mut state = root.state("");
        let path = state.ui_prefs_path.clone().unwrap();
        fs::write(&path, br#"{"schema_version":2,"ui_scale_pct":120,"agent_sort":"manual","pinned_agents":["local:t1"],"manual_agent_order":["local:t1"],"collapsed_sections":["section:rooms"]}"#).unwrap();
        let prefs = UiPrefs::load_or_default(&path);
        let json = serde_json::to_value(&prefs).unwrap();
        assert_eq!(json["ui_scale_pct"], 120);
        assert_eq!(json["collapsed_sections"][0], "section:rooms");
        assert_eq!(json["pinned_agents"][0], "local:t1");
        assert_eq!(json["manual_agent_order"][0], "local:t1");
        assert_eq!(json["agent_sort"], "manual");
        assert_eq!(state.reload_changed().ui_prefs, Some(prefs));
        assert_no_diff(state.reload_changed(), false);
    }

    #[test]
    fn reloads_only_valid_changes_and_resets_deleted_files() {
        let root = Scratch::new();
        let mut state = root.state("");
        let path = state.hosts_path.clone().unwrap();
        let ui_path = state.ui_prefs_path.clone().unwrap();
        assert_no_diff(state.reload_changed(), false);

        let cfg = hosts("updated");
        cfg.save(&path).unwrap();
        fs::write(&ui_path, br#"{"schema_version":2,"ui_scale_pct":135}"#).unwrap();
        let diff = state.reload_changed();
        assert_eq!(diff.hosts, Some(cfg.clone()));
        assert_eq!(diff.ui_prefs, Some(UiScale::from_percent(140).into()));
        assert!(!diff.had_invalid);
        assert_no_diff(state.reload_changed(), false);

        fs::write(&path, b"{").unwrap();
        let next_ui = UiPrefs::from(UiScale::from_percent(160));
        next_ui.save(&ui_path).unwrap();
        let diff = state.reload_changed();
        assert!(diff.hosts.is_none());
        assert_eq!(diff.ui_prefs, Some(next_ui));
        assert!(diff.had_invalid);

        cfg.save(&path).unwrap();
        fs::write(&ui_path, br#"{"schema_version":1,"ui_scale_pct":90}"#).unwrap();
        assert_no_diff(state.reload_changed(), true);
        UiPrefs::from(UiScale::from_percent(160))
            .save(&ui_path)
            .unwrap();
        assert_no_diff(state.reload_changed(), false);

        let mut invalid = cfg.clone();
        invalid.hosts.clear();
        invalid.save(&path).unwrap();
        assert_no_diff(state.reload_changed(), true);
        invalid.schema_version += 1;
        invalid.hosts = cfg.hosts;
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert_no_diff(state.reload_changed(), true);

        fs::remove_file(&path).unwrap();
        fs::remove_file(&ui_path).unwrap();
        let diff = state.reload_changed();
        assert_eq!(diff.hosts, Some(HostsConfig::default()));
        assert_eq!(diff.ui_prefs, Some(UiPrefs::default()));
        assert!(!diff.had_invalid);
        assert_no_diff(state.reload_changed(), false);
    }

    async fn next_reload(rx: &mut mpsc::UnboundedReceiver<BridgeMessage>) -> BridgeMessage {
        timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("config reload event timed out")
            .expect("watcher stopped")
    }

    async fn expect_hosts(rx: &mut mpsc::UnboundedReceiver<BridgeMessage>, expected: HostsConfig) {
        let BridgeMessage::HostsConfigReloaded(cfg) = next_reload(rx).await else {
            panic!("expected hosts reload");
        };
        assert_eq!(cfg, expected);
    }

    #[tokio::test]
    async fn live_notify_promotes_and_falls_back_for_atomic_and_in_place_updates() {
        let root = Scratch::new();
        let mut state = root.state("nested/config");
        let path = state.hosts_path.clone().unwrap();
        let ui_path = state.ui_prefs_path.clone().unwrap();
        // Synchronize on the initial reload so the ancestor watch is installed before the write.
        state.last_hosts = hosts("initial");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let watch = spawn_config_watcher(&Handle::current(), state, tx);
        expect_hosts(&mut rx, HostsConfig::default()).await;
        for label in ["created nested directory", "atomic replacement"] {
            let cfg = hosts(label);
            cfg.save(&path).unwrap();
            expect_hosts(&mut rx, cfg).await;
        }

        let cfg = hosts("in place");
        fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        expect_hosts(&mut rx, cfg).await;
        fs::write(&path, b"{").unwrap();
        assert!(
            timeout(Duration::from_millis(250), rx.recv())
                .await
                .is_err()
        );
        let cfg = hosts("valid again");
        fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        expect_hosts(&mut rx, cfg).await;

        fs::remove_dir_all(root.0.join("nested")).unwrap();
        expect_hosts(&mut rx, HostsConfig::default()).await;
        let cfg = hosts("recreated");
        cfg.save(&path).unwrap();
        expect_hosts(&mut rx, cfg).await;

        let expected = UiPrefs::from(UiScale::from_percent(150));
        expected.save(&ui_path).unwrap();
        let BridgeMessage::UiPrefsReloaded(prefs) = next_reload(&mut rx).await else {
            panic!("expected UI prefs reload");
        };
        assert_eq!(prefs, expected);
        watch.abort();
        assert!(watch.await.unwrap_err().is_cancelled());
    }
}
