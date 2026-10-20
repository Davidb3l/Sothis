//! Detecting a packaged (MSIX) process, and why that matters for the root.
//!
//! The Claude desktop app on Windows is an MSIX-packaged app. Processes it
//! launches run inside the package container, where file-system virtualization
//! silently redirects *new writes* under `%LOCALAPPDATA%` and `%APPDATA%` to
//! `%LOCALAPPDATA%\Packages\<family>\LocalCache\...`. Registry writes (the user
//! PATH) are not redirected, so an install under AppData lands where only the
//! container can see it while PATH points at a directory that does not exist
//! for everyone else. Writes outside AppData (for example `%USERPROFILE%\.sothis`)
//! are not virtualized.

use std::path::{Path, PathBuf};

/// Environment variable that makes [`RealPackageContext`] report a package, so
/// the guard can be exercised end to end on machines that are not packaged.
/// It can only make the checks stricter, never looser.
pub const ASSUME_PACKAGE_ENV: &str = "SOTHIS_ASSUME_PACKAGE";

/// Whether the current process runs inside an MSIX package container.
pub trait PackageContext {
    /// The package full name (`Name_Version_Arch__PublisherId`) when packaged.
    fn package_full_name(&self) -> Option<String>;
}

pub struct RealPackageContext;

impl PackageContext for RealPackageContext {
    fn package_full_name(&self) -> Option<String> {
        if let Some(fake) = std::env::var(ASSUME_PACKAGE_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            return Some(fake);
        }
        current_package_full_name()
    }
}

/// A fixed answer, for tests.
#[cfg(test)]
pub struct FakePackage(pub Option<&'static str>);

#[cfg(test)]
impl PackageContext for FakePackage {
    fn package_full_name(&self) -> Option<String> {
        self.0.map(str::to_string)
    }
}

#[cfg(not(windows))]
fn current_package_full_name() -> Option<String> {
    None
}

/// `GetCurrentPackageFullName` returns `ERROR_SUCCESS` or
/// `ERROR_INSUFFICIENT_BUFFER` (the length is then in `*length`) when the
/// process has package identity, and `APPMODEL_ERROR_NO_PACKAGE` (15700) when
/// it does not. Anything else is treated as "not packaged".
#[cfg(windows)]
fn current_package_full_name() -> Option<String> {
    use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

    const ERROR_SUCCESS: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    let mut length: u32 = 0;
    // SAFETY: a zero length with a null buffer is the documented way to ask for
    // the required length; `length` is a valid out-pointer.
    let status = unsafe { GetCurrentPackageFullName(&mut length, std::ptr::null_mut()) };
    if status != ERROR_INSUFFICIENT_BUFFER && status != ERROR_SUCCESS {
        return None;
    }
    if length == 0 {
        return Some(String::new());
    }
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: `buffer` holds `length` UTF-16 units and `length` is a valid
    // in/out pointer for its size.
    let status = unsafe { GetCurrentPackageFullName(&mut length, buffer.as_mut_ptr()) };
    if status != ERROR_SUCCESS {
        return None;
    }
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
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

/// When packaged, the reason `root` would be silently virtualized; `None` when
/// it is safe (or the process is not packaged). `app_dirs` are the
/// `%LOCALAPPDATA%` / `%APPDATA%` values.
pub fn virtualized_root_problem(
    root: &Path,
    app_dirs: &[PathBuf],
    package: &dyn PackageContext,
) -> Option<String> {
    let name = package.package_full_name()?;
    let dir = app_dirs.iter().find(|dir| is_under(root, dir))?;
    Some(format!(
        "the Sothis root {} is under {}, and this process runs inside the Windows app package {name}, which silently redirects writes under %LOCALAPPDATA% and %APPDATA% into the package's private folder; whatever is installed there is invisible to other programs. Use a root outside AppData: unset SOTHIS_HOME to get the default (%USERPROFILE%\\.sothis), or point it elsewhere",
        root.display(),
        dir.display()
    ))
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

    fn app_dirs() -> Vec<PathBuf> {
        vec![abs("Users/t/AppData/Local"), abs("Users/t/AppData/Roaming")]
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
    fn an_appdata_root_is_a_problem_only_when_packaged() {
        let root = abs("Users/t/AppData/Local/Sothis");
        let reason =
            virtualized_root_problem(&root, &app_dirs(), &FakePackage(Some("Claude_1_x64__abc")))
                .unwrap();
        assert!(reason.contains("Claude_1_x64__abc"), "{reason}");
        assert!(reason.contains("unset SOTHIS_HOME"), "{reason}");
        assert!(virtualized_root_problem(&root, &app_dirs(), &FakePackage(None)).is_none());
    }

    #[test]
    fn roaming_appdata_counts_and_the_profile_dot_directory_does_not() {
        let package = FakePackage(Some("Claude_1_x64__abc"));
        assert!(virtualized_root_problem(
            &abs("Users/t/AppData/Roaming/sothis"),
            &app_dirs(),
            &package
        )
        .is_some());
        assert!(virtualized_root_problem(&abs("Users/t/.sothis"), &app_dirs(), &package).is_none());
    }

    #[test]
    fn the_real_context_is_not_packaged_in_a_plain_process() {
        // Skipped when the override is set in the surrounding environment.
        if std::env::var_os(ASSUME_PACKAGE_ENV).is_none() && !cfg!(windows) {
            assert!(RealPackageContext.package_full_name().is_none());
        }
    }
}
