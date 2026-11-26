// GPUI 0.2.2 supplies Serde via its macro reexports, keeping the locked dependency graph intact.
use gpui::private::{serde, serde_json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[cfg(any(unix, test))]
use std::fs;
#[cfg(unix)]
use std::fs::DirBuilder;
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(any(unix, test))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

pub const UI_SCALE_MIN: u16 = 50;
pub const UI_SCALE_MAX: u16 = 200;
pub const UI_SCALE_STEP: u16 = 10;
pub const UI_SCALE_DEFAULT: u16 = 100;
pub const BASE_UI_SCALE: f32 = 0.9;
pub const BASE_REM_PX: f32 = 16.0;
pub const BASE_TERM_FONT_PX: f32 = 14.0;
pub const UI_PREFS_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiScale(u16);

impl Default for UiScale {
    fn default() -> Self {
        Self(UI_SCALE_DEFAULT)
    }
}

impl UiScale {
    pub fn from_percent(percent: u16) -> Self {
        let percent = percent.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        Self((percent + UI_SCALE_STEP / 2) / UI_SCALE_STEP * UI_SCALE_STEP)
    }

    pub fn percent(self) -> u16 {
        self.0
    }

    pub fn factor(self) -> f32 {
        (f32::from(self.0) / 100.0) * BASE_UI_SCALE
    }

    pub fn px(self, base: f32) -> f32 {
        (base * self.factor()).round().max(1.0)
    }

    pub fn rem_px(self) -> f32 {
        BASE_REM_PX * self.factor()
    }

    pub fn term_font_px(self) -> f32 {
        (BASE_TERM_FONT_PX * self.factor()).max(7.0)
    }

    pub fn zoom_in(&mut self) -> bool {
        self.set(Self::from_percent(self.0 + UI_SCALE_STEP))
    }

    pub fn zoom_out(&mut self) -> bool {
        self.set(Self::from_percent(self.0 - UI_SCALE_STEP))
    }

    pub fn reset(&mut self) -> bool {
        self.set(Self::default())
    }

    fn set(&mut self, scale: Self) -> bool {
        std::mem::replace(self, scale) != scale
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "serde", rename_all = "snake_case")]
pub enum AgentSortMode {
    #[default]
    Priority,
    Grouped,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "serde")]
pub struct UiPrefs {
    pub schema_version: u32,
    pub ui_scale_pct: u16,
    #[serde(default)]
    pub agent_sort: AgentSortMode,
    #[serde(default)]
    pub pinned_agents: Vec<String>,
    #[serde(default)]
    pub manual_agent_order: Vec<String>,
    #[serde(default)]
    pub collapsed_sections: BTreeSet<String>,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self::from(UiScale::default())
    }
}

impl From<UiScale> for UiPrefs {
    fn from(scale: UiScale) -> Self {
        Self {
            schema_version: UI_PREFS_SCHEMA_VERSION,
            ui_scale_pct: scale.percent(),
            agent_sort: AgentSortMode::default(),
            pinned_agents: Vec::new(),
            manual_agent_order: Vec::new(),
            collapsed_sections: BTreeSet::new(),
        }
    }
}

impl UiPrefs {
    pub fn default_path() -> Option<PathBuf> {
        ddg_agents_os::paths::config_dir()
            .ok()
            .map(|dir| dir.join("ui.json"))
    }

    pub fn load_or_default(path: &Path) -> Self {
        Self::load(path).unwrap_or_default()
    }

    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        const LIMIT: usize = 64 * 1024;
        let mut bytes = Vec::new();
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("UI preferences are not a regular file"));
        }
        file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > LIMIT {
            return Err(io::Error::other("UI preferences exceed 64 KiB"));
        }
        let mut prefs: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if prefs.schema_version != UI_PREFS_SCHEMA_VERSION {
            return Err(io::Error::other("unsupported UI preferences version"));
        }
        prefs.ui_scale_pct = UiScale::from_percent(prefs.ui_scale_pct).percent();
        prefs.pinned_agents.truncate(256);
        prefs.manual_agent_order.truncate(512);
        Ok(prefs)
    }

    #[cfg(windows)]
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            ddg_agents_os::fs::ensure_private_dir(parent)?;
        }
        ddg_agents_os::fs::write_atomic(path, &serde_json::to_vec(self)?)
    }

    #[cfg(unix)]
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut dirs = DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        dirs.mode(0o700);
        dirs.create(parent)?;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let (temp, mut file) = loop {
            let temp = parent.join(unique_name(".ddg-ui.tmp"));
            match options.open(&temp) {
                Ok(file) => break (temp, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let result = serde_json::to_writer(&mut file, self)
            .map_err(io::Error::other)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all());
        drop(file);
        let result = result.and_then(|()| fs::rename(&temp, path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(any(unix, test))]
fn unique_name(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_snaps_percent() {
        for (input, expected) in [
            (0, 50),
            (54, 50),
            (55, 60),
            (144, 140),
            (145, 150),
            (199, 200),
            (u16::MAX, 200),
        ] {
            assert_eq!(UiScale::from_percent(input).percent(), expected);
        }
    }

    #[test]
    fn steps_without_drift_and_reports_limit_and_reset() {
        let mut scale = UiScale::default();
        for _ in 0..100 {
            for expected in (110..=200).step_by(10) {
                assert!(scale.zoom_in());
                assert_eq!(scale.percent(), expected);
            }
            assert!(!scale.zoom_in());
            for expected in (50..200).step_by(10).rev() {
                assert!(scale.zoom_out());
                assert_eq!(scale.percent(), expected);
            }
            assert!(!scale.zoom_out());
            for _ in 0..5 {
                assert!(scale.zoom_in());
            }
            assert_eq!(scale.percent(), 100);
            assert!(!scale.reset());
        }
        assert!(scale.zoom_out());
        assert!(scale.reset());
        assert_eq!(scale, UiScale::default());
    }

    #[test]
    fn scales_logical_pixels_and_type() {
        for (pct, factor, pixels, rem, font) in [
            (50, 0.45, 1., 7.2, 7.),
            (100, 0.9, 3., 14.4, 12.6),
            (150, 1.35, 4., 21.6, 18.9),
            (200, 1.8, 5., 28.8, 25.2),
        ] {
            let scale = UiScale::from_percent(pct);
            assert!((scale.factor() - factor).abs() < 1e-5);
            assert_eq!(scale.px(3.), pixels);
            assert_eq!(scale.px(0.1), 1.);
            assert!((scale.rem_px() - rem).abs() < 1e-5);
            assert!((scale.term_font_px() - font).abs() < 1e-5);
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir();
            loop {
                let path = root.join(unique_name("ddg-ui-test"));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create scratch directory: {e}"),
                }
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prefs_round_trip_replaces_file_with_private_permissions() {
        let root = Scratch::new();
        let dir = root.0.join("config/ddg");
        let path = dir.join("ui.json");
        for pct in [130, 70] {
            let prefs = UiPrefs::from(UiScale::from_percent(pct));
            prefs.save(&path).unwrap();
            assert_eq!(UiPrefs::load_or_default(&path), prefs);
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for (path, mode) in [(&path, 0o600), (&dir, 0o700)] {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    mode
                );
            }
        }
    }

    #[test]
    fn prefs_rejects_bad_files_and_normalizes_percent() {
        let root = Scratch::new();
        let path = root.0.join("ui.json");
        assert_eq!(UiPrefs::load_or_default(&path), UiPrefs::default());
        assert_eq!(UiPrefs::load_or_default(&root.0), UiPrefs::default());
        for json in [
            "{",
            r#"{"schema_version":1,"ui_scale_pct":90}"#,
            r#"{"schema_version":3,"ui_scale_pct":160}"#,
            r#"{"ui_scale_pct":160}"#,
        ] {
            fs::write(&path, json).unwrap();
            assert_eq!(UiPrefs::load_or_default(&path), UiPrefs::default());
        }
        for (raw, expected) in [(1, 50), (125, 130), (65535, 200)] {
            fs::write(
                &path,
                format!(r#"{{"schema_version":2,"ui_scale_pct":{raw}}}"#),
            )
            .unwrap();
            let prefs = UiPrefs::load_or_default(&path);
            assert_eq!(prefs.schema_version, 2);
            assert_eq!(prefs.ui_scale_pct, expected);
        }
    }

    #[test]
    fn prefs_cleans_temp_if_rename_fails() {
        let root = Scratch::new();
        let path = root.0.join("ui.json");
        fs::create_dir(&path).unwrap();
        assert!(UiPrefs::default().save(&path).is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    }
}
