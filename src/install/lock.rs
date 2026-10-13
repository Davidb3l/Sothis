//! Global mutation lock (`<root>/state/lock`), held for a whole mutating command.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;

/// An exclusive advisory OS lock. Released when dropped (or when the process
/// dies, which is why an OS lock is used instead of a pid file).
#[derive(Debug)]
pub struct MutationLock {
    file: File,
}

impl MutationLock {
    /// Take the lock or fail fast when another sothis mutation holds it.
    pub fn acquire(path: &Path) -> Result<MutationLock, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("could not open lock file {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(MutationLock { file }),
            Err(TryLockError::WouldBlock) => Err(format!(
                "another sothis install/update/uninstall is already running (lock held on {}); wait for it to finish and retry",
                path.display()
            )),
            Err(TryLockError::Error(e)) => Err(format!(
                "could not lock {}: {e}",
                path.display()
            )),
        }
    }
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fsx::testutil::TempDir;

    #[test]
    fn a_second_acquisition_fails_fast_until_the_first_is_released() {
        let dir = TempDir::new("lock");
        let path = dir.path().join("state").join("lock");
        let first = MutationLock::acquire(&path).unwrap();
        let error = MutationLock::acquire(&path).unwrap_err();
        assert!(error.contains("already running"), "{error}");
        drop(first);
        MutationLock::acquire(&path).unwrap();
    }
}
