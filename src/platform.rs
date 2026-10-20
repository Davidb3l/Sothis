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
}
