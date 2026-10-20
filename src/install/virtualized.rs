//! Detecting Windows file-system virtualization of a would-be Sothis root.
//!
//! Processes started by the Claude desktop app on Windows have their new
//! writes under `%LOCALAPPDATA%` and `%APPDATA%` silently redirected into
//! `%LOCALAPPDATA%\Packages\<package>\LocalCache\...`. That happens even though
//! the process has no package identity (`GetCurrentPackageFullName` reports
//! "no package"), so it cannot be predicted from process properties. Registry
//! writes (the user PATH) are not redirected, which is how a v0.1.0 install
//! ended up invisible to every other program while PATH pointed at it.
//!
//! The only reliable test is empirical: create a uniquely named probe file in
//! the target directory, ask the OS where the open handle really lives
//! (`GetFinalPathNameByHandleW`), and delete it. If the final path differs and
//! lands under `...\Packages\<package>\LocalCache\...`, the directory is
//! virtualized. Writes outside AppData (for example `%USERPROFILE%\.sothis`)
//! are not.

use std::path::{Path, PathBuf};

/// Environment variable equivalent of `--allow-virtualized-root`.
pub const ALLOW_ENV: &str = "SOTHIS_ALLOW_VIRTUALIZED_ROOT";

/// What a probe of one directory found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// The probe file landed exactly where it was created.
    Direct,
    /// The OS redirected the probe file into a package's private folder.
    Virtualized {
        /// The package folder name, for example `Claude_pzs8sxrjxfjjc`.
        package: String,
        /// Where the probe file really landed.
        landed: PathBuf,
    },
    /// The probe could not be completed (not writable, not Windows, ...).
    /// Never treated as virtualized.
    Unknown(String),
}

/// Probes a directory for virtualization. Faked in tests.
pub trait VirtualizationProbe {
    fn probe(&self, dir: &Path) -> Probe;
}

/// The real probe: on Windows it creates and removes a probe file; elsewhere
/// there is nothing to virtualize.
pub struct RealVirtualizationProbe;

impl VirtualizationProbe for RealVirtualizationProbe {
    fn probe(&self, dir: &Path) -> Probe {
        probe_directory(dir)
    }
}

/// A scripted answer per directory, for tests.
#[cfg(test)]
pub struct FakeProbe {
    /// Directories (case-insensitive prefix) that are virtualized, with the
    /// package folder they land in.
    pub virtualized: Vec<(PathBuf, &'static str)>,
    pub unknown: bool,
    pub probed: std::cell::RefCell<Vec<PathBuf>>,
}

#[cfg(test)]
impl FakeProbe {
    pub fn direct() -> Self {
        FakeProbe {
            virtualized: Vec::new(),
            unknown: false,
            probed: std::cell::RefCell::new(Vec::new()),
        }
    }

    pub fn virtualizing(dir: PathBuf, package: &'static str) -> Self {
        FakeProbe {
            virtualized: vec![(dir, package)],
            ..FakeProbe::direct()
        }
    }
}

#[cfg(test)]
impl VirtualizationProbe for FakeProbe {
    fn probe(&self, dir: &Path) -> Probe {
        self.probed.borrow_mut().push(dir.to_path_buf());
        if self.unknown {
            return Probe::Unknown("simulated".to_string());
        }
        for (base, package) in &self.virtualized {
            if is_under(dir, base) {
                return Probe::Virtualized {
                    package: (*package).to_string(),
                    landed: PathBuf::from(format!(
                        r"C:\Users\t\AppData\Local\Packages\{package}\LocalCache\Local\probe"
                    )),
                };
            }
        }
        Probe::Direct
    }
}

fn canonical(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Whether `path` is `base` or lies under it (case-insensitive, separator
/// tolerant, on directory boundaries only).
pub fn is_under(path: &Path, base: &Path) -> bool {
    let (path, base) = (canonical(path), canonical(base));
    !base.is_empty()
        && (path == base
            || path
                .strip_prefix(&base)
                .is_some_and(|rest| rest.starts_with('\\')))
}

/// Decide from the requested probe path and the handle's final path.
/// Virtualized exactly when they differ and the final path lies under
/// `...\Packages\<package>\LocalCache\...`; any other difference (a junction,
/// `subst`, an 8.3 name) is not virtualization.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn classify(requested: &Path, final_path: &str) -> Probe {
    let landed = final_path
        .strip_prefix(r"\\?\")
        .unwrap_or(final_path)
        .to_string();
    if canonical(requested) == canonical(Path::new(&landed)) {
        return Probe::Direct;
    }
    let parts = landed.split('\\').collect::<Vec<_>>();
    for window in parts.windows(3) {
        if window[0].eq_ignore_ascii_case("packages")
            && !window[1].is_empty()
            && window[2].eq_ignore_ascii_case("localcache")
        {
            return Probe::Virtualized {
                package: window[1].to_string(),
                landed: PathBuf::from(landed),
            };
        }
    }
    Probe::Direct
}

/// `dir` itself if it exists, else its nearest existing ancestor.
#[cfg_attr(not(windows), allow(dead_code))]
fn nearest_existing_dir(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| candidate.is_dir())
        .map(Path::to_path_buf)
}

#[cfg(not(windows))]
fn probe_directory(_dir: &Path) -> Probe {
    Probe::Unknown("file-system virtualization is a Windows behavior".to_string())
}

#[cfg(windows)]
fn probe_directory(dir: &Path) -> Probe {
    use std::fs::OpenOptions;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };

    let Some(target) = nearest_existing_dir(dir) else {
        return Probe::Unknown(format!("no existing ancestor of {}", dir.display()));
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let path = target.join(format!(".sothis-probe-{}-{nonce}", std::process::id()));
    let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) => {
            return Probe::Unknown(format!(
                "could not create a probe file in {}: {error}",
                target.display()
            ))
        }
    };
    let mut buffer = vec![0u16; 1024];
    // SAFETY: the handle is a live, owned file handle for the duration of the
    // call, and `buffer` is a writable slice of the length passed.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    let result = if length == 0 || length >= buffer.len() {
        Probe::Unknown(format!(
            "could not resolve the final path of the probe file in {}",
            target.display()
        ))
    } else {
        classify(&path, &String::from_utf16_lossy(&buffer[..length]))
    };
    drop(file);
    let _ = std::fs::remove_file(&path);
    result
}

/// The refusal / failure text for a virtualized directory.
pub fn virtualized_message(dir: &Path, package: &str, landed: &Path) -> String {
    format!(
        "Windows redirected a test file written to {dir} into {landed} (package folder {package}): this process is virtualized (for example it was started by the Claude desktop app), so new writes under %LOCALAPPDATA% and %APPDATA% go to a private folder that other programs cannot see, while the PATH registry edit is not redirected. Use a root outside AppData: unset SOTHIS_HOME to get the default (%USERPROFILE%\\.sothis), or point it elsewhere",
        dir = dir.display(),
        landed = landed.display(),
    )
}

/// The first virtualized directory among `dirs`, as a message.
pub fn virtualized_problem(dirs: &[&Path], probe: &dyn VirtualizationProbe) -> Option<String> {
    dirs.iter().find_map(|dir| match probe.probe(dir) {
        Probe::Virtualized { package, landed } => Some(virtualized_message(dir, &package, &landed)),
        Probe::Direct | Probe::Unknown(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", path.replace('/', r"\")))
        } else {
            PathBuf::from(format!("/{path}"))
        }
    }

    #[test]
    fn under_is_case_insensitive_and_respects_directory_boundaries() {
        let base = abs("Users/t/AppData/Local");
        assert!(is_under(&abs("Users/t/AppData/Local"), &base));
        assert!(is_under(&abs("users/T/appdata/local/Sothis/bin"), &base));
        assert!(!is_under(&abs("Users/t/AppData/LocalLow/x"), &base));
        assert!(!is_under(&abs("Users/t/.sothis"), &base));
        assert!(!is_under(&abs("Users/t/.sothis"), Path::new("")));
    }

    #[test]
    fn a_final_path_equal_to_the_requested_path_is_direct() {
        let requested = PathBuf::from(r"C:\Users\David\.sothis\.sothis-probe-1");
        for landed in [
            r"\\?\C:\Users\David\.sothis\.sothis-probe-1",
            r"\\?\c:\users\david\.SOTHIS\.sothis-probe-1",
            r"C:\Users\David\.sothis\.sothis-probe-1",
        ] {
            assert_eq!(classify(&requested, landed), Probe::Direct, "{landed}");
        }
    }

    #[test]
    fn a_final_path_under_the_package_local_cache_is_virtualized() {
        let requested = PathBuf::from(r"C:\Users\David\AppData\Local\Sothis\.sothis-probe-1");
        let landed = r"\\?\C:\Users\David\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Local\Sothis\.sothis-probe-1";
        match classify(&requested, landed) {
            Probe::Virtualized { package, landed } => {
                assert_eq!(package, "Claude_pzs8sxrjxfjjc");
                assert!(landed.to_string_lossy().starts_with(r"C:\Users\David"));
                assert!(!landed.to_string_lossy().starts_with(r"\\?\"));
            }
            other => panic!("{other:?}"),
        }
        // Roaming goes to the same place under LocalCache\Roaming.
        let roaming =
            r"\\?\C:\Users\David\AppData\Local\Packages\Claude_x\LocalCache\Roaming\Sothis\p";
        assert!(matches!(
            classify(
                Path::new(r"C:\Users\David\AppData\Roaming\Sothis\p"),
                roaming
            ),
            Probe::Virtualized { .. }
        ));
    }

    #[test]
    fn other_differences_are_not_virtualization() {
        // A junction or subst drive changes the path without any package.
        let requested = PathBuf::from(r"D:\work\.sothis-probe-1");
        assert_eq!(
            classify(&requested, r"\\?\C:\real\work\.sothis-probe-1"),
            Probe::Direct
        );
        // Packages without LocalCache is not the virtualization store.
        assert_eq!(
            classify(&requested, r"\\?\C:\Packages\x\other\.sothis-probe-1"),
            Probe::Direct
        );
    }

    #[test]
    fn the_problem_text_names_the_directory_the_landing_place_and_the_package() {
        let probe = FakeProbe::virtualizing(abs("Users/t/AppData/Local"), "Claude_abc");
        let dir = abs("Users/t/AppData/Local/Sothis");
        let text = virtualized_problem(&[dir.as_path()], &probe).unwrap();
        assert!(text.contains("Claude_abc"), "{text}");
        assert!(text.contains(&dir.display().to_string()), "{text}");
        assert!(text.contains("unset SOTHIS_HOME"), "{text}");
        assert!(virtualized_problem(&[abs("Users/t/.sothis").as_path()], &probe).is_none());
        let unknown = FakeProbe {
            unknown: true,
            ..FakeProbe::direct()
        };
        assert!(virtualized_problem(&[dir.as_path()], &unknown).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn the_real_probe_reports_a_profile_directory_as_direct_and_leaves_nothing() {
        let profile = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = profile.join(format!(".sothis-probe-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = RealVirtualizationProbe.probe(&dir);
        // A directory that does not exist yet is probed through its ancestor.
        let missing = RealVirtualizationProbe.probe(&dir.join("not").join("yet"));
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(existing, Probe::Direct);
        assert_eq!(missing, Probe::Direct);
        assert_eq!(leftovers, 0, "the probe file is removed");
    }

    /// Manual: probes a uniquely named scratch directory under
    /// `%LOCALAPPDATA%` (created and deleted here) and prints the verdict.
    /// Inside the Claude desktop app's container it reports `Virtualized`.
    #[cfg(windows)]
    #[test]
    #[ignore = "probes a scratch directory under %LOCALAPPDATA%; run manually and read the output"]
    fn probes_a_scratch_directory_under_localappdata_and_prints_the_verdict() {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = local.join(format!("sothis-virt-probe-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let verdict = RealVirtualizationProbe.probe(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        println!("probed {}", dir.display());
        println!("verdict: {verdict:?}");
    }
}
