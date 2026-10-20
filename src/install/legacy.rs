//! The v0.1.0 default root, `%LOCALAPPDATA%\Sothis` (Windows only).
//!
//! v0.1.0 installed there, which an MSIX-packaged parent (the Claude desktop
//! app) silently redirects into the package's private folder; see
//! [`crate::install::packaged`]. v0.1.1 moved the default to
//! `%USERPROFILE%\.sothis`. A v0.1.0 install is never adopted or deleted; it is
//! reported with the commands that remove it, and its PATH entry is migrated.

use crate::install::layout::EnvVars;
use crate::platform::{HostOs, HostPlatform};
use std::path::{Path, PathBuf};

/// The unexpanded spelling v0.1.0's documentation could have led to.
const UNEXPANDED_BIN: &str = r"%LOCALAPPDATA%\Sothis\bin";

/// `%LOCALAPPDATA%\Sothis`, on Windows with an absolute `LOCALAPPDATA`.
pub fn legacy_root(host: HostPlatform, env: &EnvVars) -> Option<PathBuf> {
    if host.os != HostOs::Windows {
        return None;
    }
    env.local_app_data
        .as_ref()
        .filter(|path| path.is_absolute())
        .map(|path| path.join("Sothis"))
}

/// Every spelling of the v0.1.0 bin directory to remove from the user PATH:
/// the expanded entry and the `%LOCALAPPDATA%` form. Empty off Windows.
pub fn legacy_path_entries(host: HostPlatform, env: &EnvVars) -> Vec<String> {
    if host.os != HostOs::Windows {
        return Vec::new();
    }
    let mut entries = vec![UNEXPANDED_BIN.to_string()];
    if let Some(root) = legacy_root(host, env) {
        entries.push(root.join("bin").to_string_lossy().into_owned());
    }
    entries
}

/// Whether a v0.1.0 install left receipts at `root`.
pub fn receipts_present(root: &Path) -> bool {
    std::fs::read_dir(root.join("state").join("receipts"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// The note printed by `install`/`update` and reported by `doctor`.
pub fn note(legacy_root: &Path, new_root: &Path) -> String {
    format!(
        "a v0.1.0 install exists at the old root {old} (it was not adopted or modified). \
Sothis now installs to {new}. To remove the old one: \
$env:SOTHIS_HOME = \"$env:LOCALAPPDATA\\Sothis\"; sothis uninstall --profile <the profile you installed> --no-modify-path; \
then delete the folder {old}",
        old = legacy_root.display(),
        new = new_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fsx::testutil::TempDir;
    use crate::platform::HostArch;

    fn host(os: HostOs) -> HostPlatform {
        HostPlatform {
            os,
            arch: HostArch::X64,
        }
    }

    #[test]
    fn the_legacy_root_is_localappdata_sothis_on_windows_only() {
        let dir = TempDir::new("legacy-root");
        let env = EnvVars {
            local_app_data: Some(dir.path().to_path_buf()),
            ..EnvVars::default()
        };
        assert_eq!(
            legacy_root(host(HostOs::Windows), &env),
            Some(dir.path().join("Sothis"))
        );
        assert_eq!(legacy_root(host(HostOs::Linux), &env), None);
        assert!(legacy_path_entries(host(HostOs::Macos), &env).is_empty());
        let entries = legacy_path_entries(host(HostOs::Windows), &env);
        assert_eq!(entries[0], r"%LOCALAPPDATA%\Sothis\bin");
        assert_eq!(
            entries[1],
            dir.path()
                .join("Sothis")
                .join("bin")
                .to_string_lossy()
                .into_owned()
        );
        assert!(legacy_root(host(HostOs::Windows), &EnvVars::default()).is_none());
    }

    #[test]
    fn receipts_present_needs_a_non_empty_receipts_directory() {
        let dir = TempDir::new("legacy-receipts");
        assert!(!receipts_present(dir.path()));
        let receipts = dir.path().join("state").join("receipts");
        std::fs::create_dir_all(&receipts).unwrap();
        assert!(!receipts_present(dir.path()));
        std::fs::write(receipts.join("amt.json"), "{}").unwrap();
        assert!(receipts_present(dir.path()));
        assert!(receipts.join("amt.json").is_file(), "nothing is deleted");
    }

    #[test]
    fn the_note_names_both_roots_and_the_removal_commands() {
        let text = note(Path::new(r"C:\L\Sothis"), Path::new(r"C:\U\.sothis"));
        assert!(text.contains(r"C:\L\Sothis"));
        assert!(text.contains(r"C:\U\.sothis"));
        assert!(text.contains("sothis uninstall --profile"));
        assert!(text.contains("--no-modify-path"));
        assert!(text.contains(r#"$env:SOTHIS_HOME = "$env:LOCALAPPDATA\Sothis""#));
    }
}
