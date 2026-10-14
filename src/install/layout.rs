//! Where Sothis keeps its state, per OS, with a hermetic `SOTHIS_HOME` override.

use crate::platform::{HostOs, HostPlatform};
use std::path::{Path, PathBuf};

/// The process environment values the layout depends on, injectable for tests.
#[derive(Debug, Clone, Default)]
pub struct EnvVars {
    pub sothis_home: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl EnvVars {
    pub fn from_process() -> Self {
        fn var(name: &str) -> Option<PathBuf> {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        }
        EnvVars {
            sothis_home: var("SOTHIS_HOME"),
            local_app_data: var("LOCALAPPDATA"),
            xdg_data_home: var("XDG_DATA_HOME"),
            home: var("HOME").or_else(|| var("USERPROFILE")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// State root: tools/, state/, staging/, rollback/, backups/.
    pub root: PathBuf,
    /// Where tool entry points are placed.
    pub bin: PathBuf,
}

/// Collapse mixed separators and `.` segments so the same directory always
/// renders the same way (the bin directory ends up in the user PATH).
fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

impl Layout {
    pub fn resolve(host: HostPlatform) -> Result<Layout, String> {
        Layout::resolve_from(host, &EnvVars::from_process())
    }

    pub fn resolve_from(host: HostPlatform, env: &EnvVars) -> Result<Layout, String> {
        if let Some(home) = &env.sothis_home {
            if !home.is_absolute() {
                return Err(format!(
                    "SOTHIS_HOME must be an absolute path (got {})",
                    home.display()
                ));
            }
            let root = normalize(home);
            return Ok(Layout {
                bin: root.join("bin"),
                root,
            });
        }
        let bin = host.install_dir_from(env.local_app_data.clone(), env.home.clone())?;
        let root = match host.os {
            HostOs::Windows => bin
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| "could not derive the Sothis root directory".to_string())?,
            HostOs::Macos | HostOs::Linux => {
                match env.xdg_data_home.as_ref().filter(|path| path.is_absolute()) {
                    Some(xdg) => xdg.join("sothis"),
                    None => env
                        .home
                        .clone()
                        .ok_or_else(|| "HOME is not set".to_string())?
                        .join(".local")
                        .join("share")
                        .join("sothis"),
                }
            }
        };
        Ok(Layout {
            root: normalize(&root),
            bin: normalize(&bin),
        })
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.root.join("tools")
    }

    pub fn tool_dir(&self, id: &str) -> PathBuf {
        self.tools_dir().join(id)
    }

    pub fn version_dir(&self, id: &str, version: &str) -> PathBuf {
        self.tool_dir(id).join(version)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    pub fn receipts_dir(&self) -> PathBuf {
        self.state_dir().join("receipts")
    }

    pub fn receipt_path(&self, id: &str) -> PathBuf {
        self.receipts_dir().join(format!("{id}.json"))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.state_dir().join("lock")
    }

    pub fn staging_root(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn staging_dir(&self, txn: &str) -> PathBuf {
        self.staging_root().join(txn)
    }

    pub fn rollback_root(&self) -> PathBuf {
        self.root.join("rollback")
    }

    pub fn rollback_dir(&self, txn: &str) -> PathBuf {
        self.rollback_root().join(txn)
    }

    pub fn backups_dir(&self, id: &str) -> PathBuf {
        self.root.join("backups").join(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::HostArch;

    fn host(os: HostOs) -> HostPlatform {
        HostPlatform {
            os,
            arch: HostArch::X64,
        }
    }

    fn abs(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", path.replace('/', r"\")))
        } else {
            PathBuf::from(format!("/{path}"))
        }
    }

    #[test]
    fn windows_default_is_localappdata_sothis() {
        let env = EnvVars {
            local_app_data: Some(abs("Users/t/AppData/Local")),
            ..EnvVars::default()
        };
        let layout = Layout::resolve_from(host(HostOs::Windows), &env).unwrap();
        assert_eq!(layout.root, abs("Users/t/AppData/Local").join("Sothis"));
        assert_eq!(layout.bin, layout.root.join("bin"));
    }

    #[test]
    fn unix_default_uses_xdg_then_home_with_local_bin() {
        let env = EnvVars {
            home: Some(abs("home/t")),
            ..EnvVars::default()
        };
        let layout = Layout::resolve_from(host(HostOs::Linux), &env).unwrap();
        assert_eq!(
            layout.root,
            abs("home/t").join(".local").join("share").join("sothis")
        );
        assert_eq!(layout.bin, abs("home/t").join(".local").join("bin"));

        let env = EnvVars {
            home: Some(abs("home/t")),
            xdg_data_home: Some(abs("xdg")),
            ..EnvVars::default()
        };
        let layout = Layout::resolve_from(host(HostOs::Macos), &env).unwrap();
        assert_eq!(layout.root, abs("xdg").join("sothis"));
        assert_eq!(layout.bin, abs("home/t").join(".local").join("bin"));

        // A relative XDG_DATA_HOME is ignored per the XDG spec.
        let env = EnvVars {
            home: Some(abs("home/t")),
            xdg_data_home: Some(PathBuf::from("relative")),
            ..EnvVars::default()
        };
        let layout = Layout::resolve_from(host(HostOs::Linux), &env).unwrap();
        assert!(layout.root.ends_with("sothis"));
        assert!(layout.root.starts_with(abs("home/t")));
    }

    #[test]
    fn sothis_home_overrides_everything_on_every_os() {
        for os in [HostOs::Windows, HostOs::Linux, HostOs::Macos] {
            let env = EnvVars {
                sothis_home: Some(abs("scratch/home")),
                local_app_data: Some(abs("ignored")),
                home: Some(abs("ignored")),
                xdg_data_home: Some(abs("ignored")),
            };
            let layout = Layout::resolve_from(host(os), &env).unwrap();
            assert_eq!(layout.root, abs("scratch/home"));
            assert_eq!(layout.bin, abs("scratch/home").join("bin"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn mixed_separators_in_sothis_home_are_normalized() {
        let env = EnvVars {
            sothis_home: Some(PathBuf::from("C:/scratch/home/")),
            ..EnvVars::default()
        };
        let layout = Layout::resolve_from(host(HostOs::Windows), &env).unwrap();
        assert_eq!(layout.root, PathBuf::from(r"C:\scratch\home"));
        assert_eq!(layout.bin, PathBuf::from(r"C:\scratch\home\bin"));
    }

    #[test]
    fn sothis_home_must_be_absolute() {
        let env = EnvVars {
            sothis_home: Some(PathBuf::from("relative/home")),
            ..EnvVars::default()
        };
        let error = Layout::resolve_from(host(HostOs::Linux), &env).unwrap_err();
        assert!(error.contains("SOTHIS_HOME must be an absolute path"));
    }

    #[test]
    fn missing_base_variables_are_errors() {
        assert!(Layout::resolve_from(host(HostOs::Windows), &EnvVars::default()).is_err());
        assert!(Layout::resolve_from(host(HostOs::Linux), &EnvVars::default()).is_err());
    }

    #[test]
    fn derived_paths_sit_under_the_root() {
        let layout = Layout {
            root: abs("r"),
            bin: abs("r").join("bin"),
        };
        assert_eq!(
            layout.receipt_path("amt"),
            abs("r").join("state").join("receipts").join("amt.json")
        );
        assert_eq!(layout.lock_path(), abs("r").join("state").join("lock"));
        assert_eq!(
            layout.version_dir("amt", "1.0.0"),
            abs("r").join("tools").join("amt").join("1.0.0")
        );
        assert_eq!(
            layout.backups_dir("amt"),
            abs("r").join("backups").join("amt")
        );
        assert_eq!(layout.staging_dir("t"), abs("r").join("staging").join("t"));
        assert_eq!(
            layout.rollback_dir("t"),
            abs("r").join("rollback").join("t")
        );
    }
}
