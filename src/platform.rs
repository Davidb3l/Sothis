use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Windows,
    Macos,
    Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostArch {
    X64,
    Arm64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPlatform {
    pub os: HostOs,
    pub arch: HostArch,
}

impl HostPlatform {
    pub fn detect() -> Result<Self, String> {
        let os = match std::env::consts::OS {
            "windows" => HostOs::Windows,
            "macos" => HostOs::Macos,
            "linux" => HostOs::Linux,
            other => return Err(format!("unsupported operating system: {other}")),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => HostArch::X64,
            "aarch64" => HostArch::Arm64,
            other => return Err(format!("unsupported architecture: {other}")),
        };
        Ok(Self { os, arch })
    }

    pub fn key(self) -> &'static str {
        match (self.os, self.arch) {
            (HostOs::Windows, HostArch::X64) => "windows-x64",
            (HostOs::Windows, HostArch::Arm64) => "windows-arm64",
            (HostOs::Macos, HostArch::X64) => "macos-x64",
            (HostOs::Macos, HostArch::Arm64) => "macos-arm64",
            (HostOs::Linux, HostArch::X64) => "linux-x64",
            (HostOs::Linux, HostArch::Arm64) => "linux-arm64",
        }
    }

    pub(crate) fn install_dir_from(
        self,
        local_app_data: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Result<PathBuf, String> {
        let (name, base, suffix): (&str, Option<PathBuf>, &[&str]) = match self.os {
            HostOs::Windows => ("LOCALAPPDATA", local_app_data, &["Sothis", "bin"]),
            HostOs::Macos | HostOs::Linux => ("HOME", home, &[".local", "bin"]),
        };
        let base = base.ok_or_else(|| format!("{name} is not set"))?;
        if !base.is_absolute() {
            return Err(format!(
                "{name} must be an absolute path (got {})",
                base.display()
            ));
        }
        Ok(suffix
            .iter()
            .fold(base, |path, component| path.join(component)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_keys_are_stable() {
        assert_eq!(
            HostPlatform {
                os: HostOs::Windows,
                arch: HostArch::X64
            }
            .key(),
            "windows-x64"
        );
        assert_eq!(
            HostPlatform {
                os: HostOs::Linux,
                arch: HostArch::Arm64
            }
            .key(),
            "linux-arm64"
        );
    }

    #[test]
    fn install_roots_must_be_absolute() {
        let windows = HostPlatform {
            os: HostOs::Windows,
            arch: HostArch::X64,
        };
        assert!(windows
            .install_dir_from(Some(PathBuf::from("relative")), None)
            .is_err());

        let absolute = if cfg!(windows) {
            PathBuf::from(r"C:\Users\tester\AppData\Local")
        } else {
            PathBuf::from("/tmp/local-app-data")
        };
        assert!(windows.install_dir_from(Some(absolute), None).is_ok());
    }
}
