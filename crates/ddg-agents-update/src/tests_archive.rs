use super::*;
use std::io::{Cursor, Write};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

fn zip_files(names: &[&str], bytes: &[u8]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for name in names {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn archive_streaming_limit_and_forged_size() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("extracted");
    let mut bytes = zip_files(&["binary"], &[0; 128]);
    extract_zip_bounded(Cursor::new(&bytes), &dest, 128).unwrap();
    assert_eq!(fs::read(dest.join("binary")).unwrap(), [0; 128]);
    fs::remove_dir_all(&dest).unwrap();
    let err = extract_zip_bounded(Cursor::new(&bytes), &dest, 127).unwrap_err();
    assert_eq!(err.code, Code::ArchiveUnsafe);
    assert!(err.message().contains("uncompressed byte limit"), "{err}");
    assert!(!dest.exists());
    // Deliberately tiny fixture: inaccurate metadata must not set the actual-byte budget.
    let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    for offset in [22, central + 24] {
        bytes[offset..offset + 4].copy_from_slice(&1u32.to_le_bytes());
    }
    let error = extract_zip_bounded(Cursor::new(bytes), &dest, 64).unwrap_err();
    assert_eq!(error.code, Code::ArchiveUnsafe);
    assert!(error.message().contains("uncompressed"), "{error}");
    assert!(!dest.exists());
    assert_eq!(MAX_UNCOMPRESSED_BYTES, 512 * 1024 * 1024);
}

#[test]
fn archive_rejects_unsafe_paths_duplicates_and_entry_count() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("extracted");
    for names in [
        vec!["a", "./a"],
        vec!["__MACOSX/file"],
        vec!["nested/._name"],
        vec!["../relative"],
        vec!["/absolute"],
        vec!["C:/drive"],
        vec!["a:stream"],
        vec!["NUL.txt"],
        vec!["CON"],
        vec!["LPT9.log"],
        vec!["COM1 .txt"],
        vec!["trailing."],
        vec!["trailing "],
        vec![r"\\server\share"],
        vec!["a\0b"],
    ] {
        assert_eq!(
            extract_zip(Cursor::new(zip_files(&names, b"x")), &dest)
                .unwrap_err()
                .code,
            Code::ArchiveUnsafe,
            "{names:?}"
        );
        assert!(!dest.exists());
    }
    let many: Vec<_> = (0..=MAX_ENTRIES).map(|i| format!("file{i}")).collect();
    let names: Vec<_> = many.iter().map(String::as_str).collect();
    assert_eq!(
        extract_zip(Cursor::new(zip_files(&names, b"")), &dest)
            .unwrap_err()
            .code,
        Code::ArchiveUnsafe
    );
    assert!(!dest.exists());
    // Verify detection before zip's central-directory name deduplication can hide a record.
    let mut bytes = zip_files(&["alpha", "bravo"], b"");
    let entry = bytes.windows(5).rposition(|w| w == b"bravo").unwrap();
    bytes[entry..entry + 5].copy_from_slice(b"alpha");
    assert_eq!(
        extract_zip(Cursor::new(bytes), &dest).unwrap_err().code,
        Code::ArchiveUnsafe
    );
}

fn zip_links(links: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    zip.start_file("Example.app/Versions/A/binary", options)
        .unwrap();
    zip.write_all(b"binary").unwrap();
    zip.start_file("not-in-bundle", options).unwrap();
    for (name, target) in links {
        zip.add_symlink(*name, *target, options).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn archive_rejects_symlink_chain_escape_and_children() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("extracted");
    for links in [
        vec![
            ("Example.app/Versions/Current", "A"),
            (
                "Example.app/link",
                "Versions/Current/../../../not-in-bundle",
            ),
        ],
        vec![("Example.app/a", "b"), ("Example.app/b", "a")],
        vec![("Example.app/Versions", "link")],
        vec![("other-link", "not-in-bundle")],
    ] {
        assert_eq!(
            extract_zip(Cursor::new(zip_links(&links)), &dest)
                .unwrap_err()
                .code,
            Code::ArchiveUnsafe
        );
        assert!(!dest.exists());
    }
    #[cfg(unix)]
    {
        let links = [
            ("Example.app/Versions/Current", "A"),
            ("Example.app/link", "Versions/Current/binary"),
        ];
        extract_zip(Cursor::new(zip_links(&links)), &dest).unwrap();
        assert_eq!(fs::read(dest.join("Example.app/link")).unwrap(), b"binary");
    }
}

fn tar_gz(kind: tar::EntryType) -> Vec<u8> {
    use flate2::{Compression, write::GzEncoder};
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(kind);
    header.set_path("entry").unwrap();
    header.set_mode(0o755);
    let body: &[u8] = if kind.is_file() { b"tar" } else { b"" };
    header.set_size(body.len() as u64);
    if kind.is_hard_link() {
        header.set_link_name("elsewhere").unwrap();
    }
    header.set_cksum();
    builder.append(&header, body).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

#[test]
fn archive_tar_and_disk_space() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("extracted");
    for kind in [
        tar::EntryType::Link,
        tar::EntryType::Fifo,
        tar::EntryType::Char,
        tar::EntryType::Block,
    ] {
        assert_eq!(
            extract_tar_gz(&tar_gz(kind)[..], &dest).unwrap_err().code,
            Code::ArchiveUnsafe
        );
        assert!(!dest.exists());
    }
    extract_tar_gz(&tar_gz(tar::EntryType::Regular)[..], &dest).unwrap();
    assert_eq!(fs::read(dest.join("entry")).unwrap(), b"tar");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(dest.join("entry"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );
    }
    assert!(check_disk_space(dir.path(), 0).is_ok());
    assert_eq!(
        check_disk_space(dir.path(), u64::MAX).unwrap_err().code,
        Code::InsufficientDiskSpace
    );
}
