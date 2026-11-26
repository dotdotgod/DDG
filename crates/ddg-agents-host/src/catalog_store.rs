//! Atomic catalog commits and preservation of incompatible catalogs.

use crate::paths::{self, write_atomic};
use ddg_agents_proto::host::{
    Catalog, ExecutorSpec, MAX_CATALOG_BYTES, ResetNotice, RoomKind, TabRuntimeState,
};
use ddg_agents_proto::id::sanitize_display_text;
use ddg_agents_proto::terminal::PtyInfo;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};

use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct StoredCatalog<'a> {
    #[serde(flatten)]
    catalog: &'a Catalog,
    #[serde(skip_serializing_if = "Option::is_none")]
    v1_backup_file: Option<&'a str>,
}

pub fn save_catalog(
    path: &Path,
    catalog: &Catalog,
    notice: Option<&ResetNotice>,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&StoredCatalog {
        catalog,
        v1_backup_file: notice
            .filter(|_| !has_visible_room(catalog))
            .map(|notice| notice.backup_file.as_str()),
    })?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(io::Error::other("catalog exceeds storage limit"));
    }
    write_atomic(path, &bytes)
}

fn preserve(path: &Path, bytes: &[u8], kind: &str) -> io::Result<PathBuf> {
    let base = format!("json.{kind}.{}", paths::unix_secs());
    for n in 0.. {
        let backup = path.with_extension(if n == 0 {
            base.clone()
        } else {
            format!("{base}.{n}")
        });
        let mut file = match ddg_agents_os::fs::create_private_file(&backup) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            paths::remove_if_exists(&backup);
            return Err(e);
        }
        return Ok(backup);
    }
    unreachable!()
}

fn is_v1(value: &Value) -> bool {
    value["schema_version"].as_u64() == Some(1)
        || value["rooms"].as_array().into_iter().flatten().any(|room| {
            room["id"].as_i64().is_some()
                || room["id"].as_u64().is_some()
                || room["tabs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|tab| tab["id"].as_i64().is_some() || tab["id"].as_u64().is_some())
        })
}

pub fn load_catalog(path: &Path, host_id: &str) -> io::Result<(Catalog, Option<ResetNotice>)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((Catalog::empty(host_id), None));
        }
        Err(e) => return Err(e),
    };
    let raw = serde_json::from_slice::<Value>(&bytes).ok();
    if raw.as_ref().is_some_and(is_v1) {
        let backup = preserve(path, &bytes, "v1.bak")?;
        let catalog = Catalog::empty(host_id);
        let notice = reset_notice(
            backup
                .file_name()
                .expect("backup basename")
                .to_string_lossy()
                .as_ref(),
        )
        .expect("generated backup basename");
        save_catalog(path, &catalog, Some(&notice))?;
        return Ok((catalog, Some(notice)));
    }
    match Catalog::from_json_bytes(&bytes) {
        Ok(mut catalog) => {
            catalog.host_id = host_id.to_owned();
            for room in &mut catalog.rooms {
                room.name = display_name(room.name.take());
                room.auto_name = sanitize_display_text(&room.auto_name, 80);
                for tab in &mut room.tabs {
                    tab.title = sanitize_display_text(&tab.title, 160);
                    strip_session_env(&mut tab.executor);
                }
            }
            let notice = raw
                .as_ref()
                .filter(|_| !has_visible_room(&catalog))
                .and_then(|value| value["v1_backup_file"].as_str())
                .and_then(reset_notice);
            Ok((catalog, notice))
        }
        Err(_) => {
            preserve(path, &bytes, "corrupt")?;
            fs::remove_file(path)?;
            eprintln!("ddg-agents-host: preserved corrupt catalog");
            Ok((Catalog::empty(host_id), None))
        }
    }
}

fn reset_notice(basename: &str) -> Option<ResetNotice> {
    let stamp = basename.strip_prefix("catalog.json.v1.bak.")?;
    (!stamp.is_empty()
        && stamp
            .split('.')
            .all(|part| part.parse::<u64>().is_ok() && part.bytes().all(|b| b.is_ascii_digit())))
    .then(|| ResetNotice {
        notice_id: basename.into(),
        backup_file: basename.into(),
    })
}

pub fn has_visible_room(catalog: &Catalog) -> bool {
    catalog
        .rooms
        .iter()
        .any(|room| room.kind == RoomKind::Standard)
}

pub const SESSION_ENV_KEYS: [&str; 6] = [
    "DDG_SESSION_ID",
    "DDG_ROOM_ID",
    "DDG_HOST_ID",
    "DDG_HOST_SOCK",
    "DDG_TERMINAL_SOCK",
    "DDG_SESSION_TOKEN",
];

pub fn is_reserved_env(key: &str) -> bool {
    key.as_bytes()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"DDG_"))
}

pub fn strip_session_env(executor: &mut ExecutorSpec) {
    if let ExecutorSpec::Command { env, .. } = executor {
        env.retain(|key, _| !is_reserved_env(key));
    }
}

pub fn reconcile_with_live_ptys(catalog: &mut Catalog, live_ptys: &[PtyInfo]) {
    let live: HashMap<_, _> = live_ptys
        .iter()
        .map(|p| ((p.session_id.as_str(), p.run_generation), p))
        .collect();
    for room in &mut catalog.rooms {
        room.tabs.retain_mut(|tab| {
            if let Some(info) = live.get(&(tab.id.as_str(), tab.run_generation)) {
                tab.pty_id = Some(info.pty_id);
                tab.runtime = TabRuntimeState::Running {
                    pty_id: info.pty_id,
                    pid: info.pid,
                };
                if info.cwd.is_some() {
                    tab.last_cwd.clone_from(&info.cwd);
                }
                if !info.title.is_empty() {
                    tab.title = sanitize_display_text(&info.title, 160);
                }
                return true;
            }
            tab.pty_id = None;
            if !matches!(tab.runtime, TabRuntimeState::Exited { .. }) {
                tab.runtime = TabRuntimeState::StoppedAfterReboot;
            }
            room.kind != RoomKind::Background
        });
    }
}

pub fn display_name(name: Option<String>) -> Option<String> {
    name.map(|name| sanitize_display_text(&name, 80).trim().to_owned())
        .filter(|name| !name.is_empty())
}

pub fn folder_basename(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let name = Path::new(&path).file_name()?.to_str()?;
    if name.len() == 2 && name.as_bytes()[0].is_ascii_alphabetic() && name.ends_with(':') {
        return None;
    }
    display_name(Some(name.to_owned()))
}

#[cfg(test)]
#[test]
fn folder_basename_accepts_both_separators() {
    assert_eq!(folder_basename(r"C:\work/project\"), Some("project".into()));
    assert_eq!(folder_basename("/work/project/"), Some("project".into()));
    assert_eq!(folder_basename(r"C:\"), None);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ddg_agents_proto::host::{ExecutorSpec, RoomRecord, TabRecord};

    #[test]
    fn v1_reset_preserves_both_backups_and_only_reports_basename() {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(format!("/tmp/ddg-cat-v1-{}", paths::random_hex()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("catalog.json");
        let mut backups = Vec::new();
        for bytes in [
            br#"{"schema_version":1}"#.as_slice(),
            br#"{"rooms":[{"id":1}]} "#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            let (cat, notice) = load_catalog(&path, "host-a").unwrap();
            let notice = notice.unwrap();
            assert_eq!(cat, Catalog::empty("host-a"));
            assert!(notice.backup_file.starts_with("catalog.json.v1.bak."));
            assert!(!notice.backup_file.contains('/'));
            assert_eq!(
                load_catalog(&path, "host-a").unwrap().1.as_ref(),
                Some(&notice)
            );
            let backup = dir.join(notice.backup_file);
            assert_eq!(std::fs::read(&backup).unwrap(), bytes);
            assert_eq!(
                std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
            backups.push(backup);
        }
        assert_ne!(backups[0], backups[1]);
        assert_eq!(
            Catalog::from_json_bytes(&std::fs::read(path).unwrap()).unwrap(),
            Catalog::empty("host-a")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_catalog_is_preserved_and_never_overwritten() {
        let dir = PathBuf::from(format!("/tmp/ddg-cat-corrupt-{}", paths::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("catalog.json");
        std::fs::write(&path, b"{ broken json").unwrap();

        let (cat, notice) = load_catalog(&path, "host-a").unwrap();
        assert!(notice.is_none());
        assert!(cat.rooms.is_empty());
        assert!(!path.exists());
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt."))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), b"{ broken json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_marks_missing_ptys_stopped_after_reboot() {
        let mut cat = Catalog::empty("host-a");
        cat.rooms.push(RoomRecord {
            id: ddg_agents_proto::id::generate_room_id().unwrap(),
            kind: RoomKind::Standard,
            visibility: ddg_agents_proto::host::RoomVisibility::Visible,
            name: None,
            auto_name: "룸 1".into(),
            layout: None,
            tabs: vec![
                TabRecord {
                    id: ddg_agents_proto::id::generate_session_id().unwrap(),
                    run_generation: 1,
                    parent_session_id: None,
                    space_cwd: None,
                    last_cwd: None,
                    title: String::new(),
                    executor: ExecutorSpec::DefaultShell,
                    pty_id: Some(10),
                    runtime: TabRuntimeState::Running {
                        pty_id: 10,
                        pid: 100,
                    },
                    agent_profile: None,
                    parent_host_id: None,
                },
                TabRecord {
                    agent_profile: None,
                    parent_host_id: None,
                    id: ddg_agents_proto::id::generate_session_id().unwrap(),
                    run_generation: 1,
                    parent_session_id: None,
                    space_cwd: None,
                    last_cwd: None,
                    title: String::new(),
                    executor: ExecutorSpec::DefaultShell,
                    pty_id: Some(11),
                    runtime: TabRuntimeState::Running {
                        pty_id: 11,
                        pid: 101,
                    },
                },
            ],
        });
        let mut background = cat.rooms[0].clone();
        background.kind = RoomKind::Background;
        background.visibility = ddg_agents_proto::host::RoomVisibility::Hidden;
        background.id = "rm_bg_hosta".into();
        background.tabs.remove(0);
        background.tabs[0].id = ddg_agents_proto::id::generate_session_id().unwrap();
        cat.rooms.push(background);
        let live = vec![PtyInfo {
            session_id: cat.rooms[0].tabs[0].id.clone(),
            run_generation: 1,
            pty_id: 10,
            pid: 100,
            cols: 80,
            rows: 24,
            cwd: Some("/tmp/proj".into()),
            title: "zsh".into(),
            controller: None,
            agent_status: None,
            foreground: None,
        }];
        reconcile_with_live_ptys(&mut cat, &live);
        assert_eq!(
            cat.rooms[0].tabs[0].runtime,
            TabRuntimeState::Running {
                pty_id: 10,
                pid: 100
            }
        );
        assert_eq!(cat.rooms[0].tabs[0].last_cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(cat.rooms[0].tabs[1].pty_id, None);
        assert!(cat.rooms[1].tabs.is_empty());
        assert_eq!(
            cat.rooms[0].tabs[1].runtime,
            TabRuntimeState::StoppedAfterReboot
        );
    }
}
