//! Filesystem primitives for staged replacement and rollback.

use crate::{UpdateError, UpdateErrorCode as Code, integrity::verify_sha256};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

pub(crate) fn storage_error(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::new(Code::InstallDirNotWritable, error.to_string())
}

pub(crate) fn is_safe_segment(value: &str) -> bool {
    !matches!(value, "" | "." | "..")
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && !value.ends_with('.')
}

pub(crate) fn is_relative_path(value: &str) -> bool {
    !value.contains(['\\', ':'])
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.ends_with(['.', ' ']))
}

pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    if !path.symlink_metadata()?.is_file() {
        return Err(io::Error::other("Expected a regular non-symlink file"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("Expected a regular file"));
    }
    Ok(file)
}

pub(crate) fn plain_dir(path: &Path) -> io::Result<()> {
    if !path.symlink_metadata()?.is_dir() {
        return Err(io::Error::other("Expected a non-symlink directory"));
    }
    Ok(())
}

/// Resolve a serialized regular-file path below a trusted directory, without
/// accepting symlinks in any component below the root.
pub(crate) fn regular_path(root: &Path, relative: &str) -> io::Result<PathBuf> {
    if !is_relative_path(relative) {
        return Err(io::Error::other("Invalid relative file path"));
    }
    plain_dir(root)?;
    let mut path = root.to_owned();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        path.push(part);
        if parts.peek().is_some() {
            plain_dir(&path)?;
        }
    }
    Ok(path)
}

pub(crate) fn verify_file(path: &Path, expected: &str) -> Result<(), UpdateError> {
    let file =
        open_regular(path).map_err(|e| UpdateError::new(Code::ChecksumMismatch, e.to_string()))?;
    verify_sha256(file, expected)
}

pub(crate) fn verify_files<'a>(
    root: &Path,
    expected: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), UpdateError> {
    for (path, digest) in expected {
        let path = regular_path(root, path)
            .map_err(|e| UpdateError::new(Code::ChecksumMismatch, e.to_string()))?;
        verify_file(&path, digest)?;
    }
    Ok(())
}

fn absent(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Backup destination already exists",
        )),
    }
}

pub(crate) fn nonce() -> io::Result<String> {
    let mut bytes = [0; 16];
    ddg_agents_proto::id::fill_csprng(&mut bytes).map_err(io::Error::other)?;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn children(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn tree_digest(path: &Path) -> io::Result<[u8; 32]> {
    fn walk(path: &Path, hash: &mut Sha256, buffer: &mut [u8]) -> io::Result<()> {
        let metadata = path.symlink_metadata()?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            hash.update(b"link");
            let target = fs::read_link(path)?;
            let bytes = target.as_os_str().as_encoded_bytes();
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update(metadata.permissions().mode().to_le_bytes());
        }
        if kind.is_dir() {
            hash.update(b"dir");
            let entries = children(path)?;
            hash.update((entries.len() as u64).to_le_bytes());
            for child in entries {
                let bytes = child.file_name().unwrap().as_encoded_bytes();
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
                walk(&child, hash, buffer)?;
            }
        } else if kind.is_file() {
            hash.update(b"file");
            let mut digest = Sha256::new();
            let mut file = open_regular(path)?;
            loop {
                let n = file.read(buffer)?;
                if n == 0 {
                    break;
                }
                digest.update(&buffer[..n]);
            }
            hash.update(digest.finalize());
        } else {
            return Err(io::Error::other(
                "Unsupported file type in replacement tree",
            ));
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    walk(path, &mut hash, &mut [0; 64 * 1024])?;
    Ok(hash.finalize().into())
}

fn copy_file(source: &Path, target: &Path) -> io::Result<()> {
    let mut input = open_regular(source)?;
    let permissions = input.metadata()?.permissions();
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    io::copy(&mut input, &mut output)?;
    output.set_permissions(permissions)?;
    output.sync_all()
}

#[cfg(not(target_os = "macos"))]
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = source.symlink_metadata()?;
    if metadata.is_dir() {
        fs::create_dir(target)?;
        for child in children(source)? {
            copy_tree(&child, &target.join(child.file_name().unwrap()))?;
        }
        fs::set_permissions(target, metadata.permissions())?;
        sync_dir(target)
    } else if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(source)?, target);
        #[cfg(not(unix))]
        return Err(io::Error::other("Cannot copy a symlink on this platform"));
    } else {
        copy_file(source, target)
    }
}

#[cfg(target_os = "macos")]
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    fn sync(path: &Path) -> io::Result<()> {
        let ty = path.symlink_metadata()?.file_type();
        if ty.is_dir() {
            for child in children(path)? {
                sync(&child)?;
            }
            sync_dir(path)
        } else if ty.is_file() {
            open_regular(path)?.sync_all()
        } else {
            Ok(())
        }
    }
    // Local filesystem copy only (not archive extraction): retain bundle metadata
    // such as extended attributes and stapled notarization data.
    let source = source.canonicalize()?;
    let target = target
        .parent()
        .unwrap()
        .canonicalize()?
        .join(target.file_name().unwrap());
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg(source)
        .arg(&target)
        .output()?
        .status;
    if !status.success() {
        return Err(io::Error::other(
            "Could not copy app bundle with its metadata",
        ));
    }
    sync(&target)
}

/// Publish a verified sibling copy; a failed copy or check leaves the live
/// destination and the source unchanged. The caller holds the update lock.
pub(crate) fn copy_verified(
    source: &Path,
    dest: &Path,
    replace_file: bool,
    verify: impl FnOnce(&Path) -> Result<(), UpdateError>,
) -> Result<(), UpdateError> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    plain_dir(parent).map_err(storage_error)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let scratch = parent.join(format!(".ddg-copy-{}", nonce().map_err(storage_error)?));
    builder.create(&scratch).map_err(storage_error)?;
    let result = (|| {
        let name = dest
            .file_name()
            .ok_or_else(|| storage_error("Destination has no filename"))?;
        let tmp = scratch.join(name);
        let metadata = source.symlink_metadata().map_err(storage_error)?;
        let expected = tree_digest(source).map_err(storage_error)?;
        if metadata.is_dir() {
            copy_tree(source, &tmp).map_err(storage_error)?;
        } else {
            copy_file(source, &tmp).map_err(storage_error)?;
        }
        let actual = tree_digest(&tmp).map_err(storage_error)?;
        crate::integrity::verify_digest(&actual, expected)?;
        verify(&tmp)?;
        if replace_file {
            match dest.symlink_metadata() {
                Ok(metadata) if metadata.is_file() => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                _ => {
                    return Err(storage_error(
                        "Replacement destination is not a regular file",
                    ));
                }
            }
        } else {
            absent(dest).map_err(storage_error)?;
        }
        fs::rename(&tmp, dest).map_err(storage_error)?;
        sync_dir(parent).map_err(storage_error)
    })();
    let _ = fs::remove_dir_all(scratch);
    result
}

/// Move a backup without replacing another backup. On EXDEV, fsync and hash
/// the complete temp copy before publishing it; leave source intact on error.
pub fn copy_or_move_preserving_backup(source: &Path, dest: &Path) -> Result<(), UpdateError> {
    let metadata = source.symlink_metadata().map_err(storage_error)?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(storage_error(
            "Backup source must be a regular file or directory",
        ));
    }
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    plain_dir(parent).map_err(storage_error)?;
    absent(dest).map_err(storage_error)?;
    match fs::rename(source, dest) {
        Ok(()) => sync_dir(parent).map_err(storage_error),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            copy_verified(source, dest, false, |_| Ok(()))?;
            // A failed cleanup must not invalidate the already committed backup.
            if metadata.is_dir() {
                let _ = fs::remove_dir_all(source);
            } else {
                let _ = fs::remove_file(source);
            }
            Ok(())
        }
        Err(err) => Err(storage_error(err)),
    }
}

fn valid_nonce(nonce: &str) -> bool {
    (1..=64).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

pub fn make_tombstone_name(path: &Path, nonce: &str) -> Result<PathBuf, UpdateError> {
    if !valid_nonce(nonce) {
        return Err(storage_error("Invalid tombstone nonce"));
    }
    let mut name = path
        .file_name()
        .ok_or_else(|| storage_error("Missing binary filename"))?
        .to_os_string();
    name.push(format!(".old.{nonce}"));
    Ok(path.with_file_name(name))
}

/// Run at startup under the update lock. Best effort: leave locked files for a later run.
pub fn gc_stale_tombstones(dir: &Path) -> Result<usize, UpdateError> {
    let mut removed = 0;
    for entry in fs::read_dir(dir).map_err(storage_error)?.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .and_then(|n| n.rsplit_once(".old."))
            .is_some_and(|(base, n)| !base.is_empty() && valid_nonce(n))
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let path = entry.path();
        #[cfg(unix)]
        let _lock = {
            use std::os::fd::AsRawFd;
            let Ok(file) = open_regular(&path) else {
                continue;
            };
            // SAFETY: this descriptor stays open until after deletion.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                continue;
            }
            file
        };
        if fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}
