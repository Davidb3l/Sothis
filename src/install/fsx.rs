//! Small filesystem and hashing helpers shared by the install engine.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Whether `value` is exactly 64 hexadecimal digits.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path)
        .map_err(|e| format!("could not open {} for hashing: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Why a capped copy stopped.
#[derive(Debug)]
pub enum CopyError {
    Io(io::Error),
    TooLarge(u64),
}

/// Copy `reader` to `writer`, hashing as it goes, failing once more than `cap`
/// bytes have been read. Returns the byte count and the sha256 hex digest.
pub fn copy_hashing<R: Read + ?Sized, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cap: u64,
) -> Result<(u64, String), CopyError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer).map_err(CopyError::Io)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > cap {
            return Err(CopyError::TooLarge(cap));
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read]).map_err(CopyError::Io)?;
    }
    Ok((total, hex(&hasher.finalize())))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Renamed,
    CopiedAcrossDevices,
}

/// Move a file or directory. Plain rename first (works for running
/// executables on Windows); only when the rename crosses devices fall back to
/// copy + remove and report that it happened.
pub fn move_path(from: &Path, to: &Path) -> io::Result<MoveKind> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(MoveKind::Renamed),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            copy_recursive(from, to)?;
            remove_any(from)?;
            Ok(MoveKind::CopiedAcrossDevices)
        }
        Err(error) => Err(error),
    }
}

/// True when something (including a dangling symlink) exists at `path`.
pub fn exists_nofollow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Remove a file or a directory tree. A missing path is not an error.
pub fn remove_any(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            clear_readonly_tree(path);
            if metadata.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        }
        other => other,
    }
}

#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly_tree(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
        if metadata.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    clear_readonly_tree(&entry.path());
                }
            }
        }
    }
}

/// Recursively copy a file or directory. Symlinks are refused: nothing the
/// installer places may point outside its own tree.
pub fn copy_recursive(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to copy symlink {}", from.display()),
        ));
    }
    if metadata.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to).map(|_| ())
    }
}

/// Hash of a path as recorded in a receipt: sha256 for files, `None` for
/// directories. Symlinks are an error.
pub fn entry_hash(path: &Path) -> Result<Option<String>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("could not inspect {}: {e}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{} is a symlink", path.display()));
    }
    if metadata.is_dir() {
        Ok(None)
    } else {
        sha256_file(path).map(Some)
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format seconds since the Unix epoch as an RFC 3339 UTC timestamp.
pub fn rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn now_rfc3339() -> String {
    rfc3339_utc(unix_now())
}

/// A transaction id unique per process start: `<unix-millis>-<pid>-<counter>`.
pub fn new_txn_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "{millis}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
pub mod testutil {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A temp directory removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "sothis-test-{label}-{}-{nonce}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = super::remove_any(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TempDir;
    use super::*;

    #[test]
    fn hex_and_digest_helpers() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(!is_sha256_hex(&"g".repeat(64)));
        assert!(!is_sha256_hex(&"a".repeat(63)));
        let dir = TempDir::new("hash");
        let file = dir.path().join("f");
        fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn capped_copy_hashes_and_enforces_the_cap() {
        let mut out = Vec::new();
        let (bytes, digest) = copy_hashing(&mut &b"abc"[..], &mut out, 3).unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(out, b"abc");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut out = Vec::new();
        assert!(matches!(
            copy_hashing(&mut &b"abcd"[..], &mut out, 3),
            Err(CopyError::TooLarge(3))
        ));
    }

    #[test]
    fn rfc3339_formatting_is_correct() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_800_000_000), "2027-01-15T08:00:00Z");
        assert_eq!(rfc3339_utc(1_781_222_399), "2026-06-11T23:59:59Z");
    }

    #[test]
    fn txn_ids_are_unique() {
        assert_ne!(new_txn_id(), new_txn_id());
    }

    #[test]
    fn move_and_copy_handle_files_and_directories() {
        let dir = TempDir::new("move");
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested").join("a.txt"), b"a").unwrap();
        let copied = dir.path().join("copied");
        copy_recursive(&src, &copied).unwrap();
        assert_eq!(fs::read(copied.join("nested").join("a.txt")).unwrap(), b"a");

        let moved = dir.path().join("deep").join("moved");
        assert_eq!(move_path(&src, &moved).unwrap(), MoveKind::Renamed);
        assert!(!exists_nofollow(&src));
        assert!(moved.join("nested").join("a.txt").is_file());

        remove_any(&moved).unwrap();
        remove_any(&moved).unwrap(); // missing is fine
        assert!(!exists_nofollow(&moved));
    }

    #[test]
    fn entry_hash_is_none_for_directories() {
        let dir = TempDir::new("entryhash");
        assert_eq!(entry_hash(dir.path()).unwrap(), None);
        let file = dir.path().join("f");
        fs::write(&file, b"abc").unwrap();
        assert!(entry_hash(&file).unwrap().is_some());
        assert!(entry_hash(&dir.path().join("missing")).is_err());
    }
}
