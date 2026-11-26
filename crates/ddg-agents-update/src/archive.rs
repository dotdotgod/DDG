use crate::{UpdateError, UpdateErrorCode as Code};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, Read, Seek},
    path::{Path, PathBuf},
};

pub const MAX_UNCOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 2_048;

fn unsafe_archive(message: impl AsRef<str>) -> UpdateError {
    UpdateError::new(Code::ArchiveUnsafe, message)
}

fn archive_error(e: impl std::fmt::Display) -> UpdateError {
    unsafe_archive(e.to_string())
}

/// 'archive_size' is compressed size in bytes.
pub fn check_disk_space(dir: &Path, archive_size: u64) -> Result<(), UpdateError> {
    let available = available_bytes(dir)
        .map_err(|e| UpdateError::new(Code::InsufficientDiskSpace, e.to_string()))?;
    if available < u128::from(archive_size) * 3 {
        return Err(UpdateError::new(
            Code::InsufficientDiskSpace,
            "Need free space for three times the compressed archive size",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn available_bytes(dir: &Path) -> io::Result<u128> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let dir = CString::new(dir.as_os_str().as_bytes())?;
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: dir is terminated, and stats is writable and read only on success.
    if unsafe { libc::statvfs(dir.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    Ok(u128::from(stats.f_bavail) * u128::from(stats.f_frsize))
}

#[cfg(windows)]
fn available_bytes(dir: &Path) -> io::Result<u128> {
    use std::{os::windows::ffi::OsStrExt, ptr::null_mut};
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let dir = fs::canonicalize(dir)?;
    let dir: Vec<_> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut available = 0;
    // SAFETY: dir is terminated, available is writable; other outputs are optional.
    if unsafe { GetDiskFreeSpaceExW(dir.as_ptr(), &mut available, null_mut(), null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u128::from(available))
}

fn valid_component(name: &str) -> bool {
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let device_number = base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"));
    !name.is_empty()
        && name != ".."
        && !name.contains(['\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
        && !name.ends_with(['.', ' '])
        && !name.eq_ignore_ascii_case("__MACOSX")
        && !name.starts_with("._")
        && !matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !device_number.is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn archive_path(name: &str) -> Result<PathBuf, UpdateError> {
    if name.is_empty() || name.starts_with('/') {
        return Err(unsafe_archive("Empty or absolute archive entry path"));
    }
    let mut path = PathBuf::new();
    for part in name.split('/').filter(|p| !matches!(*p, "" | ".")) {
        if !valid_component(part) {
            return Err(unsafe_archive("Disallowed archive path component"));
        }
        path.push(part);
    }
    Ok(path)
}

fn link_parts(target: &str) -> Result<VecDeque<&str>, UpdateError> {
    if target.is_empty() || target.starts_with('/') {
        return Err(unsafe_archive("Symlink target must be relative"));
    }
    target
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|part| {
            if matches!(part, "." | "..") || valid_component(part) {
                Ok(part)
            } else {
                Err(unsafe_archive("Disallowed symlink target component"))
            }
        })
        .collect()
}

enum Node {
    Directory,
    File,
    Link(String),
}

// Links are validated against the graph and created only after every regular
// entry is written, so archive paths never traverse a filesystem symlink.
struct Staging<'a> {
    dir: &'a Path,
    explicit: HashSet<PathBuf>,
    nodes: HashMap<PathBuf, Node>,
    complete: bool,
}

impl<'a> Staging<'a> {
    fn new(dir: &'a Path) -> Result<Self, UpdateError> {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(dir)
            .map_err(|e| UpdateError::new(Code::InstallDirNotWritable, e.to_string()))?;
        Ok(Self {
            dir,
            explicit: HashSet::new(),
            nodes: HashMap::new(),
            complete: false,
        })
    }

    fn add(&mut self, path: &Path, node: Node) -> Result<(), UpdateError> {
        if self.explicit.len() >= MAX_ENTRIES || !self.explicit.insert(path.to_owned()) {
            return Err(unsafe_archive(
                "Too many archive entries or duplicate normalized path",
            ));
        }
        let Some(parent) = path.parent() else {
            return if matches!(node, Node::Directory) {
                Ok(())
            } else {
                Err(unsafe_archive("Archive entry cannot replace its root"))
            };
        };
        let mut prefix = PathBuf::new();
        for part in parent {
            prefix.push(part);
            self.directory(&prefix)?;
        }
        if matches!(node, Node::Directory) {
            self.directory(path)?;
        } else if self.nodes.insert(path.to_owned(), node).is_some() {
            return Err(unsafe_archive("Conflicting archive paths"));
        }
        Ok(())
    }

    fn directory(&mut self, path: &Path) -> Result<(), UpdateError> {
        match self.nodes.get(path) {
            Some(Node::Directory) => Ok(()),
            Some(_) => Err(unsafe_archive("Archive entry traverses a file or symlink")),
            None => {
                fs::create_dir(self.dir.join(path)).map_err(archive_error)?;
                self.nodes.insert(path.to_owned(), Node::Directory);
                Ok(())
            }
        }
    }

    fn file(&mut self, path: &Path, mode: u32, mut reader: impl Read) -> Result<u64, UpdateError> {
        self.add(path, Node::File)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(self.dir.join(path)).map_err(archive_error)?;
        let written = io::copy(&mut reader, &mut file).map_err(archive_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Preserve executable bits, never special bits or group/world write.
            file.set_permissions(fs::Permissions::from_mode(0o600 | (mode & 0o155)))
                .map_err(archive_error)?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(written)
    }

    fn validate_link(&self, path: &Path, target: &str) -> Result<(), UpdateError> {
        let mut resolved = path.parent().unwrap().to_owned();
        let Some(bundle_depth) = resolved
            .iter()
            .position(|part| part.to_string_lossy().ends_with(".app"))
        else {
            return Err(unsafe_archive(
                "Symlinks are only allowed inside an app bundle",
            ));
        };
        let bundle: PathBuf = resolved.iter().take(bundle_depth + 1).collect();
        let mut pending = link_parts(target)?;
        let mut hops = 0;
        while let Some(part) = pending.pop_front() {
            match part {
                ".." => {
                    if resolved == bundle {
                        return Err(unsafe_archive("Symlink escapes its app bundle"));
                    }
                    resolved.pop();
                }
                "." => {}
                name => {
                    resolved.push(name);
                    match self.nodes.get(&resolved) {
                        Some(Node::Directory) => {}
                        Some(Node::File) if pending.is_empty() => {}
                        Some(Node::Link(next)) if hops < 40 => {
                            hops += 1;
                            resolved.pop();
                            for part in link_parts(next)?.into_iter().rev() {
                                pending.push_front(part);
                            }
                        }
                        _ => return Err(unsafe_archive("Unresolvable archive symlink")),
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), UpdateError> {
        for (path, node) in &self.nodes {
            if let Node::Link(target) = node {
                self.validate_link(path, target)?;
            }
        }
        for (path, node) in &self.nodes {
            if let Node::Link(target) = node {
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, self.dir.join(path)).map_err(archive_error)?;
                #[cfg(not(unix))]
                return Err(unsafe_archive(
                    "Archive symlinks are not supported on this platform",
                ));
            }
        }
        self.complete = true;
        Ok(())
    }
}

impl Drop for Staging<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let _ = fs::remove_dir_all(self.dir);
        }
    }
}

struct Bounded<R> {
    reader: R,
    remaining: u64,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = self.remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let len = self.reader.read(&mut buffer[..limit])?;
        self.remaining = self
            .remaining
            .checked_sub(len as u64)
            .ok_or_else(|| io::Error::other("Archive exceeds the uncompressed byte limit"))?;
        Ok(len)
    }
}

/// Extract into a nonexistent directory under a caller-owned private parent.
pub fn extract_zip(reader: impl Read + Seek, dir: &Path) -> Result<(), UpdateError> {
    extract_zip_bounded(reader, dir, MAX_UNCOMPRESSED_BYTES)
}

fn extract_zip_bounded(
    reader: impl Read + Seek,
    dir: &Path,
    mut remaining: u64,
) -> Result<(), UpdateError> {
    let mut archive = zip::ZipArchive::new(reader).map_err(archive_error)?;
    if archive.len() > MAX_ENTRIES || archive.metadata().has_duplicate_names() {
        return Err(unsafe_archive(
            "Too many archive entries or duplicate ZIP names",
        ));
    }
    archive
        .validate_header_consistency()
        .map_err(archive_error)?;
    let mut staging = Staging::new(dir)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(archive_error)?;
        let name = entry.name().map_err(archive_error)?;
        let path = archive_path(&name)?;
        let mode = entry.unix_mode().unwrap_or_default();
        let is_dir = entry.is_dir();
        let mut input = Bounded {
            reader: &mut entry,
            remaining,
        };
        match mode & 0o170000 {
            0o120000 if !is_dir => {
                let mut target = String::new();
                (&mut input)
                    .take(4097)
                    .read_to_string(&mut target)
                    .map_err(archive_error)?;
                if target.len() > 4096 {
                    return Err(unsafe_archive("Archive symlink target is too long"));
                }
                staging.add(&path, Node::Link(target))?;
            }
            0 | 0o040000 if is_dir || mode & 0o170000 == 0o040000 => {
                if input.read(&mut [0]).map_err(archive_error)? != 0 {
                    return Err(unsafe_archive("Archive directory contains file data"));
                }
                staging.add(&path, Node::Directory)?;
            }
            0 | 0o100000 if !is_dir => {
                staging.file(&path, mode, &mut input)?;
            }
            _ => return Err(unsafe_archive("Disallowed ZIP entry type")),
        }
        let actual = remaining - input.remaining;
        remaining = input.remaining;
        if actual != entry.size() {
            return Err(unsafe_archive("ZIP entry length does not match its header"));
        }
    }
    staging.finish()
}

/// Extract into a nonexistent directory under a caller-owned private parent.
pub fn extract_tar_gz(reader: impl Read, dir: &Path) -> Result<(), UpdateError> {
    let mut staging = Staging::new(dir)?;
    let gz = flate2::read::MultiGzDecoder::new(reader);
    let mut archive = tar::Archive::new(Bounded {
        reader: gz,
        remaining: MAX_UNCOMPRESSED_BYTES,
    });
    // Read raw members so metadata entries cannot hide extra members from the cap.
    // shortcut: only direct tar headers; add bounded PAX/GNU extensions if release paths need them.
    for entry in archive.entries().map_err(archive_error)?.raw(true) {
        let mut entry = entry.map_err(archive_error)?;
        let name = entry.path_bytes();
        let path = archive_path(std::str::from_utf8(&name).map_err(archive_error)?)?;
        let kind = entry.header().entry_type();
        if kind.is_file() {
            let size = entry.size();
            let mode = entry.header().mode().map_err(archive_error)?;
            if staging.file(&path, mode, &mut entry)? != size {
                return Err(unsafe_archive("Truncated tar member"));
            }
        } else if kind.is_dir() && entry.size() == 0 {
            staging.add(&path, Node::Directory)?;
        } else if kind.is_symlink() && entry.size() == 0 {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| unsafe_archive("Missing symlink target"))?;
            let target = std::str::from_utf8(&target)
                .map_err(archive_error)?
                .to_owned();
            staging.add(&path, Node::Link(target))?;
        } else {
            return Err(unsafe_archive("Disallowed tar entry type"));
        }
    }
    // Drain through the same budget for CRC validation; accept only tar zero padding.
    let mut remaining = archive.into_inner();
    let mut buffer = [0; 8192];
    loop {
        let n = remaining.read(&mut buffer).map_err(archive_error)?;
        if n == 0 {
            break;
        }
        if buffer[..n].iter().any(|&byte| byte != 0) {
            return Err(unsafe_archive("Nonzero data after tar terminator"));
        }
    }
    staging.finish()
}

#[cfg(test)]
#[path = "tests_archive.rs"]
mod tests;
