//! User PATH management.
//!
//! Windows: the per-user `Environment\Path` registry value, edited through the
//! [`PathStore`] trait so tests never touch the real registry. macOS/Linux:
//! shell rc files are never edited; the exact line to add is printed instead.

use std::path::{Path, PathBuf};

pub const NEW_PROCESS_NOTE: &str = "PATH changes are visible only to new processes";

/// The user PATH value and whether it is `REG_EXPAND_SZ` (preserved on write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathValue {
    pub value: String,
    pub expandable: bool,
}

pub trait PathStore {
    fn read(&self) -> Result<Option<PathValue>, String>;
    fn write(&mut self, value: &PathValue) -> Result<(), String>;
    /// Tell running applications the environment changed (best effort).
    fn broadcast_change(&mut self) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange {
    Added,
    Removed,
    Unchanged,
}

const SEPARATOR: char = ';';

fn normalize(entry: &str, expand: &dyn Fn(&str) -> Option<String>) -> String {
    let mut text = entry.trim().to_string();
    // Expand %VAR% references so `%LOCALAPPDATA%\Sothis\bin` equals its
    // expansion.
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        match rest[start + 1..].find('%') {
            Some(len) => {
                let name = &rest[start + 1..start + 1 + len];
                match expand(name) {
                    Some(value) if !name.is_empty() => out.push_str(&value),
                    _ => out.push_str(&rest[start..start + len + 2]),
                }
                rest = &rest[start + len + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    text = out;
    let trimmed = text.trim_end_matches(['\\', '/']);
    trimmed.replace('/', "\\").to_lowercase()
}

fn contains_entry(value: &str, bin: &str, expand: &dyn Fn(&str) -> Option<String>) -> bool {
    let wanted = normalize(bin, expand);
    value
        .split(SEPARATOR)
        .filter(|part| !part.trim().is_empty())
        .any(|part| normalize(part, expand) == wanted)
}

/// Prepend `bin` to the user PATH when absent (idempotent, case-insensitive,
/// trailing-slash tolerant). Preserves the registry value type.
pub fn ensure_present(
    store: &mut dyn PathStore,
    bin: &Path,
    expand: &dyn Fn(&str) -> Option<String>,
) -> Result<PathChange, String> {
    let bin = bin.to_string_lossy().into_owned();
    let existing = store.read()?;
    let (current, expandable) = match &existing {
        Some(value) => (value.value.clone(), value.expandable),
        None => (String::new(), true),
    };
    if contains_entry(&current, &bin, expand) {
        return Ok(PathChange::Unchanged);
    }
    let value = if current.trim().is_empty() {
        bin
    } else {
        format!("{bin}{SEPARATOR}{current}")
    };
    store.write(&PathValue { value, expandable })?;
    store.broadcast_change().ok(); // best effort; new processes read the registry anyway
    Ok(PathChange::Added)
}

/// Remove every entry equal to `bin` from the user PATH.
pub fn ensure_absent(
    store: &mut dyn PathStore,
    bin: &Path,
    expand: &dyn Fn(&str) -> Option<String>,
) -> Result<PathChange, String> {
    let bin = bin.to_string_lossy().into_owned();
    let Some(existing) = store.read()? else {
        return Ok(PathChange::Unchanged);
    };
    if !contains_entry(&existing.value, &bin, expand) {
        return Ok(PathChange::Unchanged);
    }
    let wanted = normalize(&bin, expand);
    let kept = existing
        .value
        .split(SEPARATOR)
        .filter(|part| normalize(part, expand) != wanted)
        .collect::<Vec<_>>()
        .join(&SEPARATOR.to_string());
    store.write(&PathValue {
        value: kept,
        expandable: existing.expandable,
    })?;
    store.broadcast_change().ok();
    Ok(PathChange::Removed)
}

/// Directories of a `PATH`-style variable, in order.
pub fn split_path_var(value: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(value).collect()
}

pub fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let text = p.to_string_lossy().replace('/', "\\");
        let trimmed = text.trim_end_matches('\\').to_string();
        if cfg!(windows) {
            trimmed.to_lowercase()
        } else {
            p.to_string_lossy().trim_end_matches('/').to_string()
        }
    };
    norm(a) == norm(b)
}

/// Whether `bin` appears in `path_dirs`.
pub fn dir_on_path(bin: &Path, path_dirs: &[PathBuf]) -> bool {
    path_dirs.iter().any(|dir| same_dir(dir, bin))
}

/// What a shell would resolve in each directory of `dirs`, in order: the first
/// of `names` accepted by `is_match` there. One hit per distinct directory, so
/// the same directory listed twice on PATH is one copy, not two.
pub fn find_in_dirs(
    names: &[String],
    dirs: &[PathBuf],
    is_match: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut seen: Vec<&PathBuf> = Vec::new();
    let mut found = Vec::new();
    for dir in dirs {
        if seen.iter().any(|earlier| same_dir(earlier, dir)) {
            continue;
        }
        seen.push(dir);
        if let Some(hit) = names
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| is_match(candidate))
        {
            found.push(hit);
        }
    }
    found
}

/// Directories that would resolve `file_name` *before* `bin` does. When `bin`
/// is not on the path at all, every directory containing the command counts
/// (the installed copy is not what a shell would run).
pub fn find_shadowing(file_name: &str, bin: &Path, path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let before = path_dirs
        .iter()
        .position(|dir| same_dir(dir, bin))
        .unwrap_or(path_dirs.len());
    find_in_dirs(
        &[file_name.to_string()],
        &path_dirs[..before],
        &|candidate| candidate.is_file(),
    )
}

/// The line a macOS/Linux user should add to their shell profile.
pub fn unix_path_line(bin: &Path) -> String {
    format!("export PATH=\"{}:$PATH\"", bin.display())
}

#[cfg(windows)]
pub use windows_store::RegistryPathStore;

#[cfg(windows)]
mod windows_store {
    use super::{PathStore, PathValue};
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ};
    use winreg::{RegKey, RegValue};

    /// `HKCU\Environment\Path`.
    pub struct RegistryPathStore;

    fn decode(bytes: &[u8]) -> String {
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect::<Vec<_>>();
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }

    fn encode(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    impl PathStore for RegistryPathStore {
        fn read(&self) -> Result<Option<PathValue>, String> {
            let key = RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags("Environment", KEY_READ)
                .map_err(|e| format!("could not open HKCU\\Environment: {e}"))?;
            match key.get_raw_value("Path") {
                Ok(raw) => {
                    let expandable = if raw.vtype == REG_EXPAND_SZ {
                        true
                    } else if raw.vtype == REG_SZ {
                        false
                    } else {
                        return Err(format!(
                            "HKCU\\Environment\\Path has unsupported registry type {:?}",
                            raw.vtype
                        ));
                    };
                    Ok(Some(PathValue {
                        value: decode(&raw.bytes),
                        expandable,
                    }))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!("could not read HKCU\\Environment\\Path: {error}")),
            }
        }

        fn write(&mut self, value: &PathValue) -> Result<(), String> {
            let key = RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
                .map_err(|e| format!("could not open HKCU\\Environment for writing: {e}"))?;
            let raw = RegValue {
                bytes: encode(&value.value),
                vtype: if value.expandable {
                    REG_EXPAND_SZ
                } else {
                    REG_SZ
                },
            };
            key.set_raw_value("Path", &raw)
                .map_err(|e| format!("could not write HKCU\\Environment\\Path: {e}"))
        }

        fn broadcast_change(&mut self) -> Result<(), String> {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
            };
            let environment: Vec<u16> = "Environment\0".encode_utf16().collect();
            let mut result = 0usize;
            // SAFETY: `environment` is a NUL-terminated UTF-16 string that
            // outlives the call; `result` is a valid out-pointer.
            let status = unsafe {
                SendMessageTimeoutW(
                    HWND_BROADCAST,
                    WM_SETTINGCHANGE,
                    0,
                    environment.as_ptr() as isize,
                    SMTO_ABORTIFHUNG,
                    5000,
                    &mut result,
                )
            };
            if status == 0 {
                Err("WM_SETTINGCHANGE broadcast timed out or failed; new terminals will still see the change".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Read-only smoke test of the registry decode path. Never writes.
        #[test]
        #[ignore = "reads the real HKCU\\Environment\\Path (read-only); run manually"]
        fn reads_the_real_user_path_without_writing() {
            let value = RegistryPathStore
                .read()
                .unwrap()
                .expect("a user PATH exists");
            assert!(!value.value.is_empty());
            println!(
                "expandable={} entries={}",
                value.expandable,
                value.value.split(';').count()
            );
        }

        #[test]
        fn utf16_round_trips_with_a_terminator() {
            let bytes = encode(r"C:\Users\ü\bin;%X%");
            assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
            assert_eq!(decode(&bytes), r"C:\Users\ü\bin;%X%");
        }
    }
}
#[cfg(test)]
pub mod testutil {
    use super::{PathStore, PathValue};

    #[derive(Default)]
    pub struct FakeStore {
        pub value: Option<PathValue>,
        pub writes: usize,
        pub broadcasts: usize,
    }

    impl PathStore for FakeStore {
        fn read(&self) -> Result<Option<PathValue>, String> {
            Ok(self.value.clone())
        }
        fn write(&mut self, value: &PathValue) -> Result<(), String> {
            self.value = Some(value.clone());
            self.writes += 1;
            Ok(())
        }
        fn broadcast_change(&mut self) -> Result<(), String> {
            self.broadcasts += 1;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::FakeStore;
    use super::*;
    use crate::install::fsx::testutil::TempDir;
    use std::fs;

    fn store(value: &str, expandable: bool) -> FakeStore {
        FakeStore {
            value: Some(PathValue {
                value: value.to_string(),
                expandable,
            }),
            ..FakeStore::default()
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    const BIN: &str = r"C:\Users\t\AppData\Local\Sothis\bin";

    #[test]
    fn prepend_is_idempotent() {
        let mut fake = store(r"C:\Windows;C:\Tools", true);
        assert_eq!(
            ensure_present(&mut fake, Path::new(BIN), &no_env).unwrap(),
            PathChange::Added
        );
        assert_eq!(
            fake.value.as_ref().unwrap().value,
            format!(r"{BIN};C:\Windows;C:\Tools")
        );
        assert_eq!(fake.broadcasts, 1);
        assert_eq!(
            ensure_present(&mut fake, Path::new(BIN), &no_env).unwrap(),
            PathChange::Unchanged
        );
        assert_eq!(fake.writes, 1, "the second call must not write");
    }

    #[test]
    fn dedupe_ignores_case_and_trailing_separators() {
        for existing in [
            r"C:\x;c:\users\T\appdata\local\sothis\BIN",
            &format!(r"C:\x;{BIN}\"),
            &format!(r"C:\x;{BIN}/"),
            &format!(r" {BIN} ;C:\x"),
        ] {
            let mut fake = store(existing, false);
            assert_eq!(
                ensure_present(&mut fake, Path::new(BIN), &no_env).unwrap(),
                PathChange::Unchanged,
                "{existing}"
            );
            assert_eq!(fake.writes, 0);
        }
    }

    #[test]
    fn dedupe_expands_environment_references() {
        let expand = |name: &str| {
            name.eq_ignore_ascii_case("LOCALAPPDATA")
                .then(|| r"C:\Users\t\AppData\Local".to_string())
        };
        let mut fake = store(r"%LOCALAPPDATA%\Sothis\bin;C:\x", true);
        assert_eq!(
            ensure_present(&mut fake, Path::new(BIN), &expand).unwrap(),
            PathChange::Unchanged
        );
        // An unknown variable is compared literally and never matches.
        let mut other = store(r"%NOPE%\Sothis\bin", true);
        assert_eq!(
            ensure_present(&mut other, Path::new(BIN), &expand).unwrap(),
            PathChange::Added
        );
    }

    #[test]
    fn the_registry_value_type_is_preserved() {
        for expandable in [true, false] {
            let mut fake = store(r"C:\x", expandable);
            ensure_present(&mut fake, Path::new(BIN), &no_env).unwrap();
            assert_eq!(fake.value.as_ref().unwrap().expandable, expandable);
        }
        // A missing value is created as REG_EXPAND_SZ holding just the bin dir.
        let mut empty = FakeStore::default();
        ensure_present(&mut empty, Path::new(BIN), &no_env).unwrap();
        assert_eq!(
            empty.value,
            Some(PathValue {
                value: BIN.to_string(),
                expandable: true
            })
        );
    }

    #[test]
    fn removal_drops_only_the_bin_entry_and_keeps_the_type() {
        let mut fake = store(&format!(r"{BIN};C:\keep;{BIN}\;D:\also"), false);
        assert_eq!(
            ensure_absent(&mut fake, Path::new(BIN), &no_env).unwrap(),
            PathChange::Removed
        );
        assert_eq!(
            fake.value,
            Some(PathValue {
                value: r"C:\keep;D:\also".to_string(),
                expandable: false
            })
        );
        assert_eq!(
            ensure_absent(&mut fake, Path::new(BIN), &no_env).unwrap(),
            PathChange::Unchanged
        );
        let mut missing = FakeStore::default();
        assert_eq!(
            ensure_absent(&mut missing, Path::new(BIN), &no_env).unwrap(),
            PathChange::Unchanged
        );
    }

    #[test]
    fn shadowing_finds_earlier_directories_with_the_same_command() {
        let root = TempDir::new("shadow");
        let first = root.path().join("cargo-bin");
        let bin = root.path().join("sothis-bin");
        let late = root.path().join("late");
        for dir in [&first, &bin, &late] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("amt.exe"), "x").unwrap();
        }
        let dirs = vec![first.clone(), bin.clone(), late.clone()];
        assert_eq!(
            find_shadowing("amt.exe", &bin, &dirs),
            vec![first.join("amt.exe")]
        );
        // Directories after bin are not shadowing.
        let dirs = vec![bin.clone(), first.clone()];
        assert!(find_shadowing("amt.exe", &bin, &dirs).is_empty());
        // When bin is not on PATH at all, every hit shadows the new install.
        let dirs = vec![first.clone(), late.clone()];
        assert_eq!(find_shadowing("amt.exe", &bin, &dirs).len(), 2);
        assert!(find_shadowing("nope.exe", &bin, &dirs).is_empty());
        assert!(dir_on_path(&bin, &[late, bin.clone()]));
        assert!(!dir_on_path(&bin, &[first]));
    }

    #[test]
    fn the_unix_advice_is_an_exact_line() {
        assert_eq!(
            unix_path_line(Path::new("/home/t/.local/bin")),
            "export PATH=\"/home/t/.local/bin:$PATH\""
        );
    }
}
