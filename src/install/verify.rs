//! Release verification: sha256 sidecar parsing and the cosign (Sigstore) check.

use crate::install::fsx;
use crate::install::proc::{self, describe_spawn_error};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";
const COSIGN_TIMEOUT: Duration = Duration::from_secs(300);

pub const COSIGN_GUIDANCE: &str = "cosign was not found, so the Sigstore signature cannot be verified. Install it (winget install Sigstore.Cosign, brew install cosign, or a release from https://github.com/sigstore/cosign/releases) or set SOTHIS_COSIGN to its absolute path; pass --allow-missing-cosign only to accept checksum-only verification";

/// What the receipt records about the signature check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureStatus {
    #[serde(rename = "verified")]
    Verified,
    #[serde(rename = "skipped-no-cosign")]
    SkippedNoCosign,
    #[serde(rename = "not-published")]
    NotPublished,
}

/// Parse a `.sha256` sidecar: one line, `<64 hex> *<asset>` (binary marker) or
/// `<64 hex>  <asset>`. The filename must equal `asset_name`. Returns the
/// digest lowercased.
pub fn parse_sha256_file(body: &str, asset_name: &str) -> Result<String, String> {
    let mut lines = body.lines().map(str::trim).filter(|line| !line.is_empty());
    let line = lines
        .next()
        .ok_or_else(|| format!("checksum file for {asset_name} is empty"))?;
    if lines.next().is_some() {
        return Err(format!(
            "checksum file for {asset_name} must contain exactly one line"
        ));
    }
    let (digest, rest) = line
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("checksum file for {asset_name} has no filename: {line:?}"))?;
    if !fsx::is_sha256_hex(digest) {
        return Err(format!(
            "checksum file for {asset_name} does not start with a 64-digit hex sha256: {digest:?}"
        ));
    }
    // `sha256sum` marks binary mode with `*`; text mode uses a second space.
    let name = rest.trim_start();
    let name = name.strip_prefix('*').unwrap_or(name);
    if name != asset_name {
        return Err(format!(
            "checksum file names {name:?} but the asset is {asset_name:?}"
        ));
    }
    Ok(digest.to_ascii_lowercase())
}

/// Compare the computed digest with the published one (case-insensitive).
pub fn verify_checksum(actual: &str, expected: &str, asset_name: &str) -> Result<(), String> {
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "sha256 mismatch for {asset_name}: published {expected}, downloaded {actual}; the download is corrupt or has been tampered with"
        ))
    }
}

/// Substitute `{version}` in the manifest's Sigstore identity template.
pub fn sigstore_identity(template: &str, version: &str) -> String {
    template.replace("{version}", version)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The verifier could not run at all (cosign not installed).
    Unavailable(String),
    /// The verifier ran and rejected the artifact. Always fatal.
    Failed(String),
}

pub trait SignatureVerifier {
    fn verify(&self, asset: &Path, bundle: &Path, identity: &str) -> Result<(), VerifyError>;
}

/// Arguments for `cosign verify-blob`, exactly as validated on real releases.
pub fn cosign_args(asset: &Path, bundle: &Path, identity: &str) -> Vec<OsString> {
    vec![
        OsString::from("verify-blob"),
        asset.as_os_str().to_owned(),
        OsString::from("--bundle"),
        bundle.as_os_str().to_owned(),
        OsString::from("--certificate-identity"),
        OsString::from(identity),
        OsString::from("--certificate-oidc-issuer"),
        OsString::from(OIDC_ISSUER),
    ]
}

/// Inputs for locating cosign, injectable for tests.
pub struct CosignSearch<'a> {
    /// Value of `SOTHIS_COSIGN`, if set.
    pub env_override: Option<PathBuf>,
    /// PATH lookup (normally `doctor::find_command`).
    pub find_command: &'a dyn Fn(&str) -> Option<PathBuf>,
    pub windows: bool,
    pub local_app_data: Option<PathBuf>,
}

/// Find cosign, in order: `SOTHIS_COSIGN`; `cosign` on PATH; on Windows
/// `cosign-windows-amd64` on PATH, then winget's versioned package directory.
/// `Ok(None)` means "not installed"; `Err` means a configured location is bad.
pub fn locate_cosign(search: &CosignSearch<'_>) -> Result<Option<PathBuf>, String> {
    if let Some(path) = &search.env_override {
        if !path.is_absolute() {
            return Err(format!(
                "SOTHIS_COSIGN must be an absolute path (got {})",
                path.display()
            ));
        }
        if !path.is_file() {
            return Err(format!(
                "SOTHIS_COSIGN points at {}, which is not a file",
                path.display()
            ));
        }
        return Ok(Some(path.clone()));
    }
    if let Some(found) = (search.find_command)("cosign") {
        return Ok(Some(found));
    }
    if search.windows {
        if let Some(found) = (search.find_command)("cosign-windows-amd64") {
            return Ok(Some(found));
        }
        if let Some(local) = &search.local_app_data {
            if let Some(found) =
                find_winget_cosign(&local.join("Microsoft").join("WinGet").join("Packages"))
            {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// `<packages>/Sigstore.Cosign_*/cosign*.exe`, deterministic (sorted) order.
fn find_winget_cosign(packages: &Path) -> Option<PathBuf> {
    let mut candidates = std::fs::read_dir(packages)
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("Sigstore.Cosign_")
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    candidates.sort();
    for dir in candidates {
        let mut exes = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                name.starts_with("cosign") && name.ends_with(".exe") && path.is_file()
            })
            .collect::<Vec<_>>();
        exes.sort();
        if let Some(first) = exes.into_iter().next() {
            return Some(first);
        }
    }
    None
}

/// The real verifier: shells out to cosign without a shell.
pub struct CosignVerifier;

impl SignatureVerifier for CosignVerifier {
    fn verify(&self, asset: &Path, bundle: &Path, identity: &str) -> Result<(), VerifyError> {
        let find = |name: &str| crate::doctor::find_command(name);
        let search = CosignSearch {
            env_override: std::env::var_os("SOTHIS_COSIGN")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            find_command: &find,
            windows: cfg!(windows),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        };
        let cosign = match locate_cosign(&search) {
            Ok(Some(path)) => path,
            Ok(None) => return Err(VerifyError::Unavailable(COSIGN_GUIDANCE.to_string())),
            Err(error) => return Err(VerifyError::Failed(error)),
        };
        let mut command = Command::new(&cosign);
        command.args(cosign_args(asset, bundle, identity));
        match proc::run_capture(command, COSIGN_TIMEOUT) {
            Ok(output) if output.success => Ok(()),
            Ok(output) => Err(VerifyError::Failed(format!(
                "cosign rejected the signature (exit {}): {}",
                output
                    .code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
                proc::tail(&output.stderr, 6)
            ))),
            Err(error) => Err(VerifyError::Failed(describe_spawn_error(&cosign, &error))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub signature: SignatureStatus,
    pub warnings: Vec<String>,
}

pub struct ReleaseCheck<'a> {
    pub asset_name: &'a str,
    pub asset_path: &'a Path,
    /// sha256 computed while downloading.
    pub actual_sha256: &'a str,
    /// Body of the `.sha256` sidecar.
    pub checksum_body: &'a str,
    /// Manifest verification mode: `sha256` or `sha256+sigstore`.
    pub verification: &'a str,
    pub bundle_path: Option<&'a Path>,
    /// Sigstore identity with `{version}` already substituted.
    pub identity: Option<&'a str>,
}

/// Checksum first, then (when the manifest asks for it) the Sigstore bundle.
/// Fails closed; the only softening is `allow_missing_cosign` when cosign
/// itself is not installed. A cosign that runs and fails is always fatal.
pub fn verify_release(
    check: &ReleaseCheck<'_>,
    verifier: &dyn SignatureVerifier,
    allow_missing_cosign: bool,
) -> Result<VerifyOutcome, String> {
    let expected = parse_sha256_file(check.checksum_body, check.asset_name)?;
    verify_checksum(check.actual_sha256, &expected, check.asset_name)?;
    match check.verification {
        "sha256" => Ok(VerifyOutcome {
            signature: SignatureStatus::NotPublished,
            warnings: Vec::new(),
        }),
        "sha256+sigstore" => {
            let bundle = check.bundle_path.ok_or_else(|| {
                format!(
                    "{} requires a Sigstore bundle but none was fetched",
                    check.asset_name
                )
            })?;
            let identity = check.identity.ok_or_else(|| {
                format!(
                    "{} requires a Sigstore identity but none is declared",
                    check.asset_name
                )
            })?;
            match verifier.verify(check.asset_path, bundle, identity) {
                Ok(()) => Ok(VerifyOutcome {
                    signature: SignatureStatus::Verified,
                    warnings: Vec::new(),
                }),
                Err(VerifyError::Failed(message)) => Err(format!(
                    "signature verification failed for {}: {message}",
                    check.asset_name
                )),
                Err(VerifyError::Unavailable(message)) if allow_missing_cosign => {
                    Ok(VerifyOutcome {
                        signature: SignatureStatus::SkippedNoCosign,
                        warnings: vec![format!(
                            "WARNING: {} was NOT signature-verified (--allow-missing-cosign): {message}",
                            check.asset_name
                        )],
                    })
                }
                Err(VerifyError::Unavailable(message)) => Err(format!(
                    "cannot verify the signature of {}: {message}",
                    check.asset_name
                )),
            }
        }
        other => Err(format!("unsupported verification mode {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fsx::testutil::TempDir;
    use std::cell::RefCell;

    const HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha256_files_parse_in_both_formats() {
        let binary = format!("{HEX} *a.tar.gz\n");
        let text = format!("{}  a.tar.gz", HEX.to_ascii_uppercase());
        assert_eq!(parse_sha256_file(&binary, "a.tar.gz").unwrap(), HEX);
        assert_eq!(parse_sha256_file(&text, "a.tar.gz").unwrap(), HEX);
        let crlf = format!("{HEX} *a.tar.gz\r\n");
        assert_eq!(parse_sha256_file(&crlf, "a.tar.gz").unwrap(), HEX);
    }

    #[test]
    fn sha256_files_fail_closed() {
        let wrong_name = format!("{HEX} *other.tar.gz");
        assert!(parse_sha256_file(&wrong_name, "a.tar.gz")
            .unwrap_err()
            .contains("names"));
        assert!(parse_sha256_file("", "a").unwrap_err().contains("empty"));
        assert!(parse_sha256_file(HEX, "a")
            .unwrap_err()
            .contains("no filename"));
        assert!(parse_sha256_file("zz *a", "a")
            .unwrap_err()
            .contains("64-digit"));
        let short = format!("{} *a", &HEX[..63]);
        assert!(parse_sha256_file(&short, "a").is_err());
        let non_hex = format!("{}g *a", &HEX[..63]);
        assert!(parse_sha256_file(&non_hex, "a").is_err());
        let two = format!("{HEX} *a\n{HEX} *a\n");
        assert!(parse_sha256_file(&two, "a")
            .unwrap_err()
            .contains("exactly one"));
    }

    #[test]
    fn checksum_comparison_is_case_insensitive_and_mismatch_is_fatal() {
        assert!(verify_checksum(HEX, &HEX.to_ascii_uppercase(), "a").is_ok());
        let error = verify_checksum(HEX, &"0".repeat(64), "a").unwrap_err();
        assert!(error.contains("sha256 mismatch"));
    }

    #[test]
    fn identity_substitutes_the_version() {
        assert_eq!(
            sigstore_identity("https://x/release.yml@refs/tags/v{version}", "0.0.8"),
            "https://x/release.yml@refs/tags/v0.0.8"
        );
    }

    #[test]
    fn cosign_arguments_match_the_validated_command() {
        let args = cosign_args(Path::new("a.tgz"), Path::new("a.tgz.sigstore.json"), "ID");
        let args = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "verify-blob",
                "a.tgz",
                "--bundle",
                "a.tgz.sigstore.json",
                "--certificate-identity",
                "ID",
                "--certificate-oidc-issuer",
                "https://token.actions.githubusercontent.com"
            ]
        );
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn abs(dir: &TempDir, rel: &str) -> PathBuf {
        dir.path().join(rel)
    }

    #[test]
    fn cosign_locator_order() {
        let dir = TempDir::new("cosign");
        let env_cosign = abs(&dir, "env/cosign");
        touch(&env_cosign);
        let path_cosign = abs(&dir, "path/cosign");
        let alt_cosign = abs(&dir, "path/cosign-windows-amd64");
        let winget = dir
            .path()
            .join("Microsoft")
            .join("WinGet")
            .join("Packages")
            .join("Sigstore.Cosign_Microsoft.Winget.Source_8wekyb3d8bbwe")
            .join("cosign-windows-amd64.exe");
        touch(&winget);

        let none = |_: &str| None;
        let path_first = |name: &str| (name == "cosign").then(|| path_cosign.clone());
        let alt_only = |name: &str| (name == "cosign-windows-amd64").then(|| alt_cosign.clone());
        let search = |env: Option<PathBuf>, find: &dyn Fn(&str) -> Option<PathBuf>, windows| {
            locate_cosign(&CosignSearch {
                env_override: env,
                find_command: find,
                windows,
                local_app_data: Some(dir.path().to_path_buf()),
            })
        };

        // 1. The environment override wins over everything.
        assert_eq!(
            search(Some(env_cosign.clone()), &path_first, true).unwrap(),
            Some(env_cosign.clone())
        );
        // 2. Then `cosign` on PATH.
        assert_eq!(
            search(None, &path_first, true).unwrap(),
            Some(path_cosign.clone())
        );
        // 3. Then the versioned Windows name on PATH, but only on Windows.
        assert_eq!(
            search(None, &alt_only, true).unwrap(),
            Some(alt_cosign.clone())
        );
        assert_eq!(search(None, &alt_only, false).unwrap(), None);
        // 4. Then winget's package directory, only on Windows.
        assert_eq!(search(None, &none, true).unwrap(), Some(winget));
        assert_eq!(search(None, &none, false).unwrap(), None);
    }

    #[test]
    fn cosign_override_must_be_absolute_and_exist() {
        let none = |_: &str| None;
        let make = |path: PathBuf| CosignSearch {
            env_override: Some(path),
            find_command: &none,
            windows: false,
            local_app_data: None,
        };
        assert!(locate_cosign(&make(PathBuf::from("relative/cosign")))
            .unwrap_err()
            .contains("absolute"));
        let dir = TempDir::new("cosign-env");
        assert!(locate_cosign(&make(dir.path().join("missing")))
            .unwrap_err()
            .contains("not a file"));
    }

    struct FakeVerifier {
        result: Result<(), VerifyError>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeVerifier {
        fn new(result: Result<(), VerifyError>) -> Self {
            FakeVerifier {
                result,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl SignatureVerifier for FakeVerifier {
        fn verify(&self, asset: &Path, bundle: &Path, identity: &str) -> Result<(), VerifyError> {
            self.calls.borrow_mut().push(format!(
                "{}|{}|{identity}",
                asset.display(),
                bundle.display()
            ));
            self.result.clone()
        }
    }

    fn check<'a>(
        body: &'a str,
        verification: &'a str,
        bundle: Option<&'a Path>,
    ) -> ReleaseCheck<'a> {
        ReleaseCheck {
            asset_name: "a.tgz",
            asset_path: Path::new("a.tgz"),
            actual_sha256: HEX,
            checksum_body: body,
            verification,
            bundle_path: bundle,
            identity: Some("ID"),
        }
    }

    fn body() -> String {
        format!("{HEX} *a.tgz\n")
    }

    #[test]
    fn checksum_only_assets_record_not_published_and_never_call_cosign() {
        let verifier = FakeVerifier::new(Err(VerifyError::Failed("must not be called".into())));
        let body = body();
        let outcome = verify_release(&check(&body, "sha256", None), &verifier, false).unwrap();
        assert_eq!(outcome.signature, SignatureStatus::NotPublished);
        assert!(verifier.calls.borrow().is_empty());
    }

    #[test]
    fn a_checksum_mismatch_is_fatal_before_any_signature_check() {
        let verifier = FakeVerifier::new(Ok(()));
        let bad = format!("{} *a.tgz\n", "0".repeat(64));
        let error = verify_release(
            &check(&bad, "sha256+sigstore", Some(Path::new("b"))),
            &verifier,
            true,
        )
        .unwrap_err();
        assert!(error.contains("sha256 mismatch"));
        assert!(verifier.calls.borrow().is_empty());
    }

    #[test]
    fn a_valid_signature_is_recorded_as_verified() {
        let verifier = FakeVerifier::new(Ok(()));
        let body = body();
        let bundle = Path::new("a.tgz.sigstore.json");
        let outcome = verify_release(
            &check(&body, "sha256+sigstore", Some(bundle)),
            &verifier,
            false,
        )
        .unwrap();
        assert_eq!(outcome.signature, SignatureStatus::Verified);
        assert_eq!(verifier.calls.borrow().len(), 1);
        assert!(verifier.calls.borrow()[0].ends_with("|ID"));
    }

    #[test]
    fn missing_cosign_fails_closed_with_install_guidance() {
        let verifier = FakeVerifier::new(Err(VerifyError::Unavailable(COSIGN_GUIDANCE.into())));
        let body = body();
        let bundle = Path::new("b");
        let error = verify_release(
            &check(&body, "sha256+sigstore", Some(bundle)),
            &verifier,
            false,
        )
        .unwrap_err();
        assert!(error.contains("winget install Sigstore.Cosign"), "{error}");
        assert!(error.contains("brew install cosign"));
    }

    #[test]
    fn allow_missing_cosign_proceeds_loudly_and_records_the_skip() {
        let verifier = FakeVerifier::new(Err(VerifyError::Unavailable(COSIGN_GUIDANCE.into())));
        let body = body();
        let bundle = Path::new("b");
        let outcome = verify_release(
            &check(&body, "sha256+sigstore", Some(bundle)),
            &verifier,
            true,
        )
        .unwrap();
        assert_eq!(outcome.signature, SignatureStatus::SkippedNoCosign);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("NOT signature-verified"));
    }

    #[test]
    fn a_failing_cosign_is_always_fatal_even_with_the_allow_flag() {
        let verifier = FakeVerifier::new(Err(VerifyError::Failed("bad identity".into())));
        let body = body();
        let bundle = Path::new("b");
        for allow in [false, true] {
            let error = verify_release(
                &check(&body, "sha256+sigstore", Some(bundle)),
                &verifier,
                allow,
            )
            .unwrap_err();
            assert!(error.contains("signature verification failed"), "{error}");
            assert!(error.contains("bad identity"));
        }
    }

    #[test]
    fn sigstore_mode_without_a_bundle_or_identity_is_an_error() {
        let verifier = FakeVerifier::new(Ok(()));
        let body = body();
        assert!(verify_release(&check(&body, "sha256+sigstore", None), &verifier, false).is_err());
        let mut no_identity = check(&body, "sha256+sigstore", Some(Path::new("b")));
        no_identity.identity = None;
        assert!(verify_release(&no_identity, &verifier, false).is_err());
        assert!(verify_release(&check(&body, "bogus", None), &verifier, false).is_err());
    }

    #[test]
    fn signature_status_serializes_to_the_receipt_strings() {
        assert_eq!(
            serde_json::to_string(&SignatureStatus::SkippedNoCosign).unwrap(),
            "\"skipped-no-cosign\""
        );
        assert_eq!(
            serde_json::from_str::<SignatureStatus>("\"not-published\"").unwrap(),
            SignatureStatus::NotPublished
        );
        assert!(serde_json::from_str::<SignatureStatus>("\"maybe\"").is_err());
    }
}
