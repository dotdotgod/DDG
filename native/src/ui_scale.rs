// GPUI 0.2.2 supplies Serde via its macro reexports, keeping the locked dependency graph intact.
#[cfg(windows)]
use ddg_agents_os::fs::ensure_private_dir;
use ddg_agents_os::{fs::write_atomic, lock::FileLock};
use gpui::private::{serde, serde_json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[cfg(any(test, unix))]
use std::fs;
#[cfg(unix)]
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

pub const UI_SCALE_MIN: u16 = 50;
pub const UI_SCALE_MAX: u16 = 200;
pub const UI_SCALE_STEP: u16 = 10;
pub const UI_SCALE_DEFAULT: u16 = 100;
pub const BASE_REM_PX: f32 = 14.0;
pub const BASE_TERM_FONT_PX: f32 = 12.0;
pub const UI_PREFS_SCHEMA_VERSION: u32 = 2;
const UI_PREFS_MAX_BYTES: usize = 64 * 1024;
pub(crate) const UI_PREFS_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiScale(u16);

impl Default for UiScale {
    fn default() -> Self {
        Self(UI_SCALE_DEFAULT)
    }
}

// Each fixed design length becomes a read-only pixel table at compile time.
macro_rules! fixed_px {
    ($scale:expr, $length:expr) => {{
        const PIXELS: [f32; $crate::ui_scale::ZOOM_LEVELS] =
            $crate::ui_scale::zoom_table($length, true, 1.0);
        $scale.lookup(&PIXELS)
    }};
}

pub const ZOOM_LEVELS: usize = ((UI_SCALE_MAX - UI_SCALE_MIN) / UI_SCALE_STEP + 1) as usize;
pub const fn zoom_table(base: f32, snap: bool, minimum: f32) -> [f32; ZOOM_LEVELS] {
    let mut values = [0.; ZOOM_LEVELS];
    let mut i = 0;
    while i < ZOOM_LEVELS {
        let percent = UI_SCALE_MIN + i as u16 * UI_SCALE_STEP;
        let value = base * (1.1 * (percent as f32 / 100.));
        values[i] = (if snap { value.round() } else { value }).max(minimum);
        i += 1;
    }
    values
}

impl UiScale {
    pub fn lookup(self, values: &[f32; ZOOM_LEVELS]) -> f32 {
        values[((self.0 - UI_SCALE_MIN) / UI_SCALE_STEP) as usize]
    }
    pub fn from_percent(percent: u16) -> Self {
        let percent = percent.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        Self((percent + UI_SCALE_STEP / 2) / UI_SCALE_STEP * UI_SCALE_STEP)
    }

    pub fn percent(self) -> u16 {
        self.0
    }

    pub fn factor(self) -> f32 {
        self.lookup(&const { zoom_table(1., false, 0.) })
    }

    pub fn px(self, base: f32) -> f32 {
        // Shared lookup for fixed sizes, including panel widths and tree indentation.
        macro_rules! lengths {
            ($($n:literal),+) => { match base {
                $($n => fixed_px!(self, $n),)+
                // Arbitrary widths from dragging the sidebar have no fixed table.
                _ => (base * self.factor()).round().max(1.0),
            } };
        }
        lengths!(1., 1.5, 2., 3., 3.5, 4.5, 5., 6., 7., 7.5, 9., 10., 10.5, 11., 12.,
                 13., 14., 15., 17., 19., 20., 27., 28., 49., 95., 111., 136., 157.,
                 170., 190., 204., 234., 306., 357., 391., 408., 429., 442., 544.)
    }

    pub fn rem_px(self) -> f32 {
        self.lookup(&const { zoom_table(BASE_REM_PX, false, 0.) })
    }

    pub fn term_font_px(self) -> f32 {
        self.lookup(&const { zoom_table(BASE_TERM_FONT_PX, false, 7.) })
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
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub seeded_preset_ids: BTreeSet<String>,
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
            seeded_preset_ids: BTreeSet::new(),
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
        let mut bytes = Vec::new();
        #[cfg(unix)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)?;
        #[cfg(windows)]
        let file = ddg_agents_os::fs::open_regular(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("UI preferences are not a regular file"));
        }
        file.take((UI_PREFS_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > UI_PREFS_MAX_BYTES {
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

    pub(crate) fn load_or_missing(path: &Path) -> io::Result<Self> {
        match Self::load(path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            result => result,
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let _lock = Self::lock(path, Duration::ZERO)?;
        let latest = Self::load_or_missing(path)?;
        let mut next = self.clone();
        next.seeded_preset_ids.extend(latest.seeded_preset_ids);
        next.write_locked(path)
    }

    /// Append under the same lock as normal settings writes, retaining the
    /// latest other prefs. The caller owns only IDs newly written here.
    pub(crate) fn reserve_preset_ids(
        path: &Path,
        timeout: Duration,
        propose: impl FnOnce(&mut BTreeSet<String>),
    ) -> io::Result<BTreeSet<String>> {
        let _lock = Self::lock(path, timeout)?;
        let mut latest = Self::load_or_missing(path)?;
        let mut proposed = latest.seeded_preset_ids.clone();
        propose(&mut proposed);
        let added: BTreeSet<_> = proposed
            .difference(&latest.seeded_preset_ids)
            .cloned()
            .collect();
        if !added.is_empty() {
            latest.seeded_preset_ids.extend(proposed);
            latest.write_locked(path)?;
        }
        Ok(added)
    }

    fn lock(path: &Path, timeout: Duration) -> io::Result<FileLock> {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        let lock_path = path.with_extension("lock");
        #[cfg(unix)]
        {
            let parent = parent.unwrap_or_else(|| Path::new("."));
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
            let dir = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(parent)?;
            let meta = dir.metadata()?;
            // SAFETY: geteuid takes no arguments and has no preconditions.
            let owner = unsafe { libc::geteuid() };
            if meta.uid() != owner || meta.mode() & 0o022 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe UI settings directory",
                ));
            }
            match fs::symlink_metadata(&lock_path) {
                Ok(meta) if !meta.is_file() || meta.uid() != owner || meta.mode() & 0o077 != 0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unsafe UI settings lock",
                    ));
                }
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
                _ => {}
            }
        }
        #[cfg(windows)]
        if let Some(parent) = parent {
            ensure_private_dir(parent)?;
        }
        if timeout.is_zero() {
            return FileLock::try_lock_exclusive(&lock_path);
        }
        let start = Instant::now();
        loop {
            match FileLock::try_lock_exclusive(&lock_path) {
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                result => return result,
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for UI settings lock",
                ));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }

    fn write_locked(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        if bytes.len() > UI_PREFS_MAX_BYTES {
            return Err(io::Error::other("UI preferences exceed 64 KiB"));
        }
        write_atomic(path, &bytes)
    }
}

#[cfg(test)]
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
            (50, 0.55, 2., 7.7, 7.),
            (90, 0.99, 3., 13.86, 11.88),
            (100, 1.1, 3., 15.4, 13.2),
            (150, 1.65, 5., 23.1, 19.8),
            (200, 2.2, 7., 30.8, 26.4),
        ] {
            let scale = UiScale::from_percent(pct);
            assert!((scale.factor() - factor).abs() < 1e-5);
            assert_eq!(fixed_px!(scale, 3.), pixels);
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

    // UT-NATIVE-01: legacy v2 defaults, then two writers using stale in-memory prefs.
    #[test]
    fn seeded_pref_history_survives_stale_and_concurrent_writers() {
        let root = Scratch::new();
        let path = root.0.join("ui.json");
        fs::write(&path, br#"{"schema_version":2,"ui_scale_pct":90}"#).unwrap();
        let mut stale = UiPrefs::load(&path).unwrap();
        assert!(stale.seeded_preset_ids.is_empty());
        let mut current = UiPrefs::from(UiScale::from_percent(140));
        current.collapsed_sections.insert("rooms".into());
        current.save(&path).unwrap();
        let owned = UiPrefs::reserve_preset_ids(&path, Duration::ZERO, |ids| {
            ids.insert("pi".into());
        })
        .unwrap();
        assert_eq!(owned, ["pi".into()].into());
        let loaded = UiPrefs::load(&path).unwrap();
        assert_eq!(loaded.ui_scale_pct, 140);
        assert_eq!(loaded.collapsed_sections, current.collapsed_sections);
        assert_eq!(loaded.seeded_preset_ids, owned);
        stale.seeded_preset_ids.insert("codex".into());
        let barrier = std::sync::Barrier::new(3);
        let saved = std::thread::scope(|scope| {
            let save = scope.spawn(|| {
                barrier.wait();
                stale.save(&path)
            });
            scope.spawn(|| {
                barrier.wait();
                UiPrefs::reserve_preset_ids(&path, UI_PREFS_LOCK_TIMEOUT, |ids| {
                    ids.insert("antigravity".into());
                })
                .unwrap();
            });
            barrier.wait();
            save.join().unwrap()
        });
        if let Err(err) = saved {
            assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
            stale.save(&path).unwrap();
        }
        assert_eq!(
            UiPrefs::load(&path).unwrap().seeded_preset_ids,
            ["pi".into(), "codex".into(), "antigravity".into()].into()
        );
    }

    #[test]
    fn prefs_foreground_save_does_not_wait_on_busy_ledger() {
        let root = Scratch::new();
        let path = root.0.join("ui.json");
        let lock = UiPrefs::lock(&path, Duration::ZERO).unwrap();
        let err = UiPrefs::default().save(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(!path.exists());
        drop(lock);
        UiPrefs::default().save(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prefs_do_not_change_existing_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = Scratch::new();
        let path = root.0.join("ui.json");
        let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o777;
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
        UiPrefs::default().save(&path).unwrap();
        assert_eq!(mode(&root.0), 0o755);
        let bytes = fs::read(&path).unwrap();
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o775)).unwrap();
        let err = UiPrefs::from(UiScale::from_percent(90))
            .save(&path)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(mode(&root.0), 0o775);
        assert_eq!(fs::read(&path).unwrap(), bytes);
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
        assert!(path.with_extension("lock").is_file());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
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
    fn prefs_accepts_exactly_64_kib_and_rejects_one_more_byte() {
        let root = Scratch::new();
        let path = root.0.join("ui.json");
        let mut json = br#"{"schema_version":2,"ui_scale_pct":90}"#.to_vec();
        json.resize(65_536, b' ');
        fs::write(&path, &json).unwrap();
        assert_eq!(UiPrefs::load(&path).unwrap().ui_scale_pct, 90);
        json.push(b' ');
        fs::write(&path, &json).unwrap();
        assert!(UiPrefs::load(&path).is_err());
    }

    #[test]
    fn prefs_save_rejects_nonregular_existing_file() {
        let root = Scratch::new();
        let path = root.0.join("ui.json");
        fs::create_dir(&path).unwrap();
        assert!(UiPrefs::default().save(&path).is_err());
        assert!(path.is_dir());
        assert!(path.with_extension("lock").is_file());
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
    }
}
