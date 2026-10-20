//! End-to-end tests of the bootstrap scripts (`scripts/install.sh`,
//! `scripts/install.ps1`) against a locally packaged fake release.
//!
//! The "release" is the freshly built `sothis` binary packaged exactly as
//! `release.yml` does (a top directory holding the binary, LICENSE and
//! README.md; `.tar.gz` or `.zip`; a `<hex> *<asset>` sidecar), served through
//! `SOTHIS_BOOTSTRAP_MIRROR`. cosign is a fake that records its arguments and
//! accepts or rejects, selected through `SOTHIS_COSIGN`, so the signature
//! path is deterministic on every machine whether or not a real cosign exists.
//!
//! SAFETY: every run sets `SOTHIS_HOME` to a fresh temp directory and the
//! scripts are driven with `--no-install` (or `--no-modify-path --no-plugins`
//! for the hand-off tests), so nothing here can touch the real PATH, registry,
//! `~/.local/bin` or `~/.claude`. The `sh` tests skip themselves when no `sh`
//! is installed.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const IDENTITY_PREFIX: &str =
    "https://github.com/Davidb3l/Sothis/.github/workflows/release.yml@refs/tags/v";
const ISSUER: &str = "https://token.actions.githubusercontent.com";

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Temp {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sothis-bootstrap-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The asset platform the scripts would pick on this machine (and the one the
/// tests pin through `SOTHIS_BOOTSTRAP_PLATFORM`), with its archive type.
fn platform() -> (&'static str, &'static str) {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", _) => ("windows-x64", "zip"),
        ("macos", "aarch64") => ("macos-arm64", "tar.gz"),
        ("macos", _) => ("macos-x64", "tar.gz"),
        (_, "aarch64") => ("linux-arm64", "tar.gz"),
        _ => ("linux-x64-glibc", "tar.gz"),
    }
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "sothis.exe"
    } else {
        "sothis"
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Release {
    mirror: PathBuf,
    asset: String,
    /// sha256 of the packaged sothis binary.
    binary_sha256: String,
}

impl Release {
    fn dir(&self) -> PathBuf {
        self.mirror
            .join("Davidb3l")
            .join("Sothis")
            .join(format!("v{VERSION}"))
    }

    fn asset_path(&self) -> PathBuf {
        self.dir().join(&self.asset)
    }

    /// A placeholder Sigstore bundle; the fake cosign never reads it, but the
    /// scripts must fetch it whenever a cosign is present.
    fn add_bundle(&self) {
        fs::write(
            self.dir().join(format!("{}.sigstore.json", self.asset)),
            "{}",
        )
        .unwrap();
    }
}

/// Package the built binary like release.yml does and serve it from a mirror.
fn make_release(root: &Path) -> Release {
    let (platform, archive) = platform();
    let top = format!("sothis-{VERSION}-{platform}");
    let asset = format!("{top}.{archive}");
    let binary = fs::read(env!("CARGO_BIN_EXE_sothis")).unwrap();
    let mirror = root.join("mirror");
    let release = Release {
        mirror,
        asset: asset.clone(),
        binary_sha256: sha256_hex(&binary),
    };
    fs::create_dir_all(release.dir()).unwrap();
    let files: [(&str, Vec<u8>, u32); 3] = [
        (exe_name(), binary, 0o755),
        ("LICENSE", b"MIT\n".to_vec(), 0o644),
        ("README.md", b"# sothis\n".to_vec(), 0o644),
    ];
    let archive_path = release.asset_path();
    if archive == "zip" {
        let mut writer = zip::ZipWriter::new(fs::File::create(&archive_path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes, _) in &files {
            writer.start_file(format!("{top}/{name}"), options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    } else {
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(encoder);
        for (name, bytes, mode) in &files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{top}/{name}"), &bytes[..])
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }
    let digest = sha256_hex(&fs::read(&archive_path).unwrap());
    fs::write(
        release.dir().join(format!("{asset}.sha256")),
        format!("{digest} *{asset}\n"),
    )
    .unwrap();
    release
}

/// Forward slashes: accepted by Rust, PowerShell and MSYS `sh` alike.
fn slashed(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A fake cosign recording its arguments next to itself (`cosign.args`).
/// Returns the path to hand to `SOTHIS_COSIGN`.
fn fake_cosign(dir: &Path, accept: bool, for_powershell: bool) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let code = if accept { 0 } else { 1 };
    if for_powershell {
        let path = dir.join("cosign.cmd");
        fs::write(
            &path,
            format!("@echo off\r\necho %* > \"%~dp0cosign.args\"\r\nexit /b {code}\r\n"),
        )
        .unwrap();
        path
    } else {
        let path = dir.join("cosign");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/cosign.args\"\nexit {code}\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

fn recorded_cosign_args(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("cosign.args")).ok()
}

fn sh_available() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

struct Run {
    output: Output,
}

impl Run {
    fn code(&self) -> i32 {
        self.output.status.code().unwrap_or(-1)
    }

    fn text(&self) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&self.output.stdout),
            String::from_utf8_lossy(&self.output.stderr)
        )
    }
}

struct Setup<'a> {
    release: &'a Release,
    home: &'a Path,
    cosign: Option<&'a Path>,
    release_mirror: Option<&'a Path>,
}

fn env_for(command: &mut Command, setup: &Setup<'_>, slash: bool) {
    let text = |path: &Path| {
        if slash {
            slashed(path)
        } else {
            path.to_string_lossy().into_owned()
        }
    };
    let (platform, _) = platform();
    command
        .env("SOTHIS_HOME", text(setup.home))
        // The temp homes below can sit under AppData, which is virtualized when
        // the shell was started by the Claude desktop app; these scenarios are
        // not about that (the refusal has its own test).
        .env("SOTHIS_ALLOW_VIRTUALIZED_ROOT", "1")
        .env("SOTHIS_BOOTSTRAP_MIRROR", text(&setup.release.mirror))
        .env("SOTHIS_BOOTSTRAP_PLATFORM", platform)
        .env_remove("SOTHIS_RELEASE_MIRROR");
    match setup.cosign {
        Some(path) => {
            command.env("SOTHIS_COSIGN", text(path));
        }
        None => {
            command.env_remove("SOTHIS_COSIGN");
        }
    }
    if let Some(mirror) = setup.release_mirror {
        command.env("SOTHIS_RELEASE_MIRROR", text(mirror));
    }
}

fn run_sh(setup: &Setup<'_>, args: &[&str]) -> Run {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("install.sh");
    let mut command = Command::new("sh");
    command.arg(slashed(&script)).args(args);
    env_for(&mut command, setup, true);
    Run {
        output: command.output().unwrap(),
    }
}

#[cfg(windows)]
fn run_ps1(setup: &Setup<'_>, args: &[&str]) -> Run {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("install.ps1");
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .args(args);
    env_for(&mut command, setup, false);
    Run {
        output: command.output().unwrap(),
    }
}

/// Runs install.ps1 the way a user who logs the install does
/// (`& install.ps1 ... 2>&1 | Out-String -Stream`). Under that redirection,
/// Windows PowerShell turns a native program's stderr lines into error
/// records, which the script's `$ErrorActionPreference = 'Stop'` must not
/// treat as failures.
#[cfg(windows)]
fn run_ps1_redirected(setup: &Setup<'_>, args: &[&str]) -> Run {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("install.ps1");
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    // Parameter names stay bare (a quoted '-Profile' is a positional string);
    // values are single-quoted.
    let arg = |s: &&str| {
        if s.starts_with('-') {
            s.to_string()
        } else {
            quote(s)
        }
    };
    let invocation = format!(
        "& {} {} 2>&1 | Out-String -Stream; exit $LASTEXITCODE",
        quote(&script.to_string_lossy()),
        args.iter().map(arg).collect::<Vec<_>>().join(" ")
    );
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
        .arg(invocation);
    env_for(&mut command, setup, false);
    Run {
        output: command.output().unwrap(),
    }
}

fn placed(home: &Path) -> PathBuf {
    home.join("bin").join(exe_name())
}

fn bin_entries(home: &Path) -> Vec<String> {
    match fs::read_dir(home.join("bin")) {
        Ok(entries) => {
            let mut names = entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            names.sort();
            names
        }
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The scenarios, written once and run against whichever front end is given.

type Runner = fn(&Setup<'_>, &[&str]) -> Run;

/// (flag spelling, runs on PowerShell?) differences between the two scripts.
struct Flavor {
    version: &'static str,
    no_install: &'static str,
    allow_missing: &'static str,
    profile: &'static str,
    no_modify_path: &'static str,
    no_plugins: &'static str,
    powershell: bool,
}

const SH: Flavor = Flavor {
    version: "--version",
    no_install: "--no-install",
    allow_missing: "--allow-missing-cosign",
    profile: "--profile",
    no_modify_path: "--no-modify-path",
    no_plugins: "--no-plugins",
    powershell: false,
};

#[cfg(windows)]
const PS1: Flavor = Flavor {
    version: "-Version",
    no_install: "-NoInstall",
    allow_missing: "-AllowMissingCosign",
    profile: "-Profile",
    no_modify_path: "-NoModifyPath",
    no_plugins: "-NoPlugins",
    powershell: true,
};

fn scenario_places_a_verified_release(run: Runner, flavor: &Flavor) {
    let temp = Temp::new("happy");
    let release = make_release(&temp.0);
    release.add_bundle();
    let cosign_dir = temp.0.join("cosign");
    let cosign = fake_cosign(&cosign_dir, true, flavor.powershell);
    let home = temp.0.join("home");
    let setup = Setup {
        release: &release,
        home: &home,
        cosign: Some(&cosign),
        release_mirror: None,
    };
    let result = run(&setup, &[flavor.version, VERSION, flavor.no_install]);
    assert_eq!(result.code(), 0, "{}", result.text());
    // Placed, byte for byte.
    assert_eq!(
        sha256_hex(&fs::read(placed(&home)).unwrap()),
        release.binary_sha256
    );
    assert_eq!(bin_entries(&home), [exe_name()], "no staging leftovers");
    // cosign was asked the exact question the Rust engine asks.
    let args = recorded_cosign_args(&cosign_dir).expect("cosign was never run");
    let normalized = args.replace(['\r', '\n'], " ");
    assert!(normalized.contains("verify-blob"), "{args}");
    assert!(normalized.contains(&release.asset), "{args}");
    assert!(normalized.contains("--bundle"), "{args}");
    assert!(normalized.contains(".sigstore.json"), "{args}");
    assert!(
        normalized.contains(&format!(
            "--certificate-identity {IDENTITY_PREFIX}{VERSION}"
        )),
        "{args}"
    );
    assert!(
        normalized.contains(&format!("--certificate-oidc-issuer {ISSUER}")),
        "{args}"
    );

    // Replacing an existing copy works and leaves nothing behind.
    let again = run(
        &setup,
        &[flavor.version, &format!("v{VERSION}"), flavor.no_install],
    );
    assert_eq!(again.code(), 0, "{}", again.text());
    assert_eq!(bin_entries(&home), [exe_name()]);
    assert_eq!(
        sha256_hex(&fs::read(placed(&home)).unwrap()),
        release.binary_sha256
    );
}

fn scenario_a_rejecting_cosign_is_always_fatal(run: Runner, flavor: &Flavor) {
    let temp = Temp::new("reject");
    let release = make_release(&temp.0);
    release.add_bundle();
    let cosign_dir = temp.0.join("cosign");
    let cosign = fake_cosign(&cosign_dir, false, flavor.powershell);
    let home = temp.0.join("home");
    let setup = Setup {
        release: &release,
        home: &home,
        cosign: Some(&cosign),
        release_mirror: None,
    };
    // Even with the opt-out: it only excuses a cosign that is not installed.
    let result = run(
        &setup,
        &[
            flavor.version,
            VERSION,
            flavor.no_install,
            flavor.allow_missing,
        ],
    );
    assert_eq!(result.code(), 1, "{}", result.text());
    assert!(
        result.text().contains("cosign rejected"),
        "{}",
        result.text()
    );
    assert!(!home.join("bin").exists(), "nothing may be placed");
}

fn scenario_a_tampered_archive_is_fatal_before_cosign_runs(run: Runner, flavor: &Flavor) {
    let temp = Temp::new("tamper");
    let release = make_release(&temp.0);
    release.add_bundle();
    let mut bytes = fs::read(release.asset_path()).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    fs::write(release.asset_path(), bytes).unwrap();
    let cosign_dir = temp.0.join("cosign");
    let cosign = fake_cosign(&cosign_dir, true, flavor.powershell);
    let home = temp.0.join("home");
    let setup = Setup {
        release: &release,
        home: &home,
        cosign: Some(&cosign),
        release_mirror: None,
    };
    let result = run(&setup, &[flavor.version, VERSION, flavor.no_install]);
    assert_eq!(result.code(), 1, "{}", result.text());
    assert!(
        result.text().contains("sha256 mismatch"),
        "{}",
        result.text()
    );
    assert!(!home.join("bin").exists());
    assert!(
        recorded_cosign_args(&cosign_dir).is_none(),
        "the checksum must be checked before the signature"
    );
}

fn scenario_a_sidecar_for_another_file_is_fatal(run: Runner, flavor: &Flavor) {
    let temp = Temp::new("sidecar");
    let release = make_release(&temp.0);
    release.add_bundle();
    let sidecar = release.dir().join(format!("{}.sha256", release.asset));
    let body = fs::read_to_string(&sidecar).unwrap();
    fs::write(&sidecar, body.replace("sothis-", "other-")).unwrap();
    let cosign = fake_cosign(&temp.0.join("cosign"), true, flavor.powershell);
    let home = temp.0.join("home");
    let setup = Setup {
        release: &release,
        home: &home,
        cosign: Some(&cosign),
        release_mirror: None,
    };
    let result = run(&setup, &[flavor.version, VERSION, flavor.no_install]);
    assert_eq!(result.code(), 1, "{}", result.text());
    assert!(result.text().contains("names"), "{}", result.text());
    assert!(!home.join("bin").exists());
}

fn scenario_a_cosign_that_is_present_needs_the_bundle(run: Runner, flavor: &Flavor) {
    let temp = Temp::new("no-bundle");
    let release = make_release(&temp.0); // deliberately no .sigstore.json
    let cosign = fake_cosign(&temp.0.join("cosign"), true, flavor.powershell);
    let home = temp.0.join("home");
    let setup = Setup {
        release: &release,
        home: &home,
        cosign: Some(&cosign),
        release_mirror: None,
    };
    let result = run(
        &setup,
        &[
            flavor.version,
            VERSION,
            flavor.no_install,
            flavor.allow_missing,
        ],
    );
    assert_eq!(result.code(), 1, "{}", result.text());
    assert!(result.text().contains("sigstore.json"), "{}", result.text());
    assert!(!home.join("bin").exists());
}

fn scenario_hand_off_runs_sothis_install_and_propagates_its_exit_code(
    run: Runner,
    flavor: &Flavor,
) {
    let temp = Temp::new("handoff");
    let release = make_release(&temp.0);
    release.add_bundle();
    let cosign = fake_cosign(&temp.0.join("cosign"), true, flavor.powershell);
    let empty = temp.0.join("empty-release-mirror");
    fs::create_dir_all(&empty).unwrap();

    // `bell` needs no downloads: success propagates as 0.
    let ok_home = temp.0.join("ok-home");
    let setup = Setup {
        release: &release,
        home: &ok_home,
        cosign: Some(&cosign),
        release_mirror: Some(&empty),
    };
    let ok = run(
        &setup,
        &[
            flavor.version,
            VERSION,
            flavor.profile,
            "bell",
            flavor.no_modify_path,
            flavor.no_plugins,
        ],
    );
    assert_eq!(ok.code(), 0, "{}", ok.text());
    assert!(
        ok.text().contains("sothis install --profile bell"),
        "{}",
        ok.text()
    );
    assert!(ok.text().contains("--no-modify-path"), "{}", ok.text());
    assert!(ok.text().contains("--no-plugins"), "{}", ok.text());
    assert!(placed(&ok_home).is_file());

    // `core` against an empty release mirror fails closed: exit 1 propagates.
    let bad_home = temp.0.join("bad-home");
    let setup = Setup {
        release: &release,
        home: &bad_home,
        cosign: Some(&cosign),
        release_mirror: Some(&empty),
    };
    let bad = run(
        &setup,
        &[
            flavor.version,
            VERSION,
            flavor.no_modify_path,
            flavor.no_plugins,
        ],
    );
    assert_eq!(bad.code(), 1, "{}", bad.text());
    assert!(
        bad.text().contains("missing from the mirror"),
        "{}",
        bad.text()
    );
    assert!(placed(&bad_home).is_file(), "sothis itself stays placed");
}

fn scenario_usage_errors(run: Runner, flavor: &Flavor, expected_usage_code: i32) {
    let temp = Temp::new("usage");
    let release = make_release(&temp.0);
    let home = temp.0.join("home");
    let setup = Setup {
        release: &release,
        home: &home,
        cosign: None,
        release_mirror: None,
    };
    for args in [
        vec![flavor.version, "1.2", flavor.no_install],
        vec![flavor.version, VERSION, flavor.profile, "Bad Profile"],
        vec![flavor.no_install], // a mirror needs an explicit version
    ] {
        let result = run(&setup, &args);
        assert_eq!(
            result.code(),
            expected_usage_code,
            "{args:?}: {}",
            result.text()
        );
        assert!(!home.join("bin").exists(), "{args:?}");
    }
}

// ----------------------------------------------------------------------- sh

macro_rules! sh_tests {
    ($($name:ident => $scenario:expr;)*) => {$(
        #[test]
        fn $name() {
            if !sh_available() {
                eprintln!("skipping: no `sh` on PATH");
                return;
            }
            $scenario(run_sh, &SH);
        }
    )*};
}

sh_tests! {
    sh_places_a_verified_release_and_replaces_it_cleanly => scenario_places_a_verified_release;
    sh_a_rejecting_cosign_is_always_fatal => scenario_a_rejecting_cosign_is_always_fatal;
    sh_a_tampered_archive_is_fatal_before_cosign_runs => scenario_a_tampered_archive_is_fatal_before_cosign_runs;
    sh_a_sidecar_for_another_file_is_fatal => scenario_a_sidecar_for_another_file_is_fatal;
    sh_a_present_cosign_needs_the_bundle => scenario_a_cosign_that_is_present_needs_the_bundle;
    sh_hand_off_propagates_the_exit_code => scenario_hand_off_runs_sothis_install_and_propagates_its_exit_code;
}

#[test]
fn sh_usage_errors_exit_two() {
    if !sh_available() {
        eprintln!("skipping: no `sh` on PATH");
        return;
    }
    scenario_usage_errors(run_sh, &SH, 2);
}

#[test]
fn sh_help_and_unknown_options() {
    if !sh_available() {
        eprintln!("skipping: no `sh` on PATH");
        return;
    }
    let temp = Temp::new("sh-help");
    let release = make_release(&temp.0);
    let home = temp.0.join("home");
    let setup = Setup {
        release: &release,
        home: &home,
        cosign: None,
        release_mirror: None,
    };
    let help = run_sh(&setup, &["--help"]);
    assert_eq!(help.code(), 0);
    assert!(help.text().contains("--allow-missing-cosign"));
    let unknown = run_sh(&setup, &["--bogus"]);
    assert_eq!(unknown.code(), 2);
    assert!(unknown.text().contains("unknown option"));
    assert!(!home.exists());
}

// --------------------------------------------------------------- PowerShell

#[cfg(windows)]
macro_rules! ps1_tests {
    ($($name:ident => $scenario:expr;)*) => {$(
        #[test]
        fn $name() {
            $scenario(run_ps1, &PS1);
        }
    )*};
}

#[cfg(windows)]
ps1_tests! {
    ps1_places_a_verified_release_and_replaces_it_cleanly => scenario_places_a_verified_release;
    ps1_a_rejecting_cosign_is_always_fatal => scenario_a_rejecting_cosign_is_always_fatal;
    ps1_a_tampered_archive_is_fatal_before_cosign_runs => scenario_a_tampered_archive_is_fatal_before_cosign_runs;
    ps1_a_sidecar_for_another_file_is_fatal => scenario_a_sidecar_for_another_file_is_fatal;
    ps1_a_present_cosign_needs_the_bundle => scenario_a_cosign_that_is_present_needs_the_bundle;
    ps1_hand_off_propagates_the_exit_code => scenario_hand_off_runs_sothis_install_and_propagates_its_exit_code;
}

#[cfg(windows)]
#[test]
fn ps1_hand_off_survives_a_caller_that_redirects_stderr() {
    // `sothis install` prints its header on stderr; a caller logging with
    // `2>&1` must still get the real outcome, not a spurious failure.
    scenario_hand_off_runs_sothis_install_and_propagates_its_exit_code(run_ps1_redirected, &PS1);
}

#[cfg(windows)]
#[test]
fn ps1_usage_errors_exit_one_with_a_message() {
    // `-File` turns a failed bootstrap into exit 1 (the script owns its process).
    scenario_usage_errors(run_ps1, &PS1, 1);
}

// ---------------------------------------------------------------------------
// Default root (no SOTHIS_HOME): %USERPROFILE%\.sothis on Windows, ~/.local/bin
// elsewhere. Never %LOCALAPPDATA%: processes started by the Claude desktop
// app have their writes there silently redirected.
//
// SAFETY: USERPROFILE/HOME/LOCALAPPDATA/APPDATA all point into a temp dir.

struct DefaultRoot {
    profile: PathBuf,
    local: PathBuf,
}

fn default_root_env(
    command: &mut Command,
    release: &Release,
    cosign: &Path,
    dirs: &DefaultRoot,
    slash: bool,
) {
    let text = |path: &Path| {
        if slash {
            slashed(path)
        } else {
            path.to_string_lossy().into_owned()
        }
    };
    let (platform, _) = platform();
    command
        .env_remove("SOTHIS_HOME")
        .env_remove("SOTHIS_ALLOW_VIRTUALIZED_ROOT")
        .env_remove("SOTHIS_RELEASE_MIRROR")
        .env("USERPROFILE", text(&dirs.profile))
        .env("HOME", text(&dirs.profile))
        .env("LOCALAPPDATA", text(&dirs.local))
        .env("APPDATA", text(&dirs.local.join("Roaming")))
        .env("SOTHIS_BOOTSTRAP_MIRROR", text(&release.mirror))
        .env("SOTHIS_BOOTSTRAP_PLATFORM", platform)
        .env("SOTHIS_COSIGN", text(cosign));
}

/// Where the script should have put sothis with no SOTHIS_HOME.
fn default_bin(profile: &Path) -> PathBuf {
    if cfg!(windows) {
        profile.join(".sothis").join("bin")
    } else {
        profile.join(".local").join("bin")
    }
}

#[test]
fn sh_default_root_is_the_profile_not_appdata() {
    if !sh_available() {
        eprintln!("skipping: no `sh` on PATH");
        return;
    }
    let temp = Temp::new("sh-default");
    let release = make_release(&temp.0);
    release.add_bundle();
    let cosign = fake_cosign(&temp.0.join("cosign"), true, false);
    let dirs = DefaultRoot {
        profile: temp.0.join("profile"),
        local: temp.0.join("profile").join("AppData").join("Local"),
    };
    fs::create_dir_all(&dirs.profile).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("install.sh");
    let mut command = Command::new("sh");
    command
        .arg(slashed(&script))
        .args(["--version", VERSION, "--no-install"]);
    default_root_env(&mut command, &release, &cosign, &dirs, true);
    let run = Run {
        output: command.output().unwrap(),
    };
    assert_eq!(run.code(), 0, "{}", run.text());
    assert!(
        default_bin(&dirs.profile).join(exe_name()).is_file(),
        "{}",
        run.text()
    );
    assert!(
        !dirs.local.join("Sothis").exists(),
        "nothing may be written under LOCALAPPDATA"
    );
}

#[cfg(windows)]
fn ps1_command(release: &Release, cosign: &Path, dirs: &DefaultRoot, args: &[&str]) -> Command {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("install.ps1");
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .args(args);
    default_root_env(&mut command, release, cosign, dirs, false);
    command
}

#[cfg(windows)]
#[test]
fn ps1_default_root_is_the_profile_not_appdata() {
    let temp = Temp::new("ps1-default");
    let release = make_release(&temp.0);
    release.add_bundle();
    let cosign = fake_cosign(&temp.0.join("cosign"), true, true);
    let dirs = DefaultRoot {
        profile: temp.0.join("profile"),
        local: temp.0.join("profile").join("AppData").join("Local"),
    };
    fs::create_dir_all(&dirs.local).unwrap();
    let mut command = ps1_command(
        &release,
        &cosign,
        &dirs,
        &["-Version", VERSION, "-NoInstall"],
    );
    let run = Run {
        output: command.output().unwrap(),
    };
    assert_eq!(run.code(), 0, "{}", run.text());
    assert!(
        default_bin(&dirs.profile).join(exe_name()).is_file(),
        "{}",
        run.text()
    );
    assert!(
        !dirs.local.join("Sothis").exists(),
        "nothing may be written under LOCALAPPDATA"
    );
}

/// The real probe, end to end: a root under the real `%LOCALAPPDATA%` is
/// placed when that directory is not virtualized, and refused (nothing
/// installed) when it is, e.g. in a shell started by the Claude desktop app.
/// `-AllowVirtualizedRoot` always places it. The scratch directory is uniquely
/// named and deleted.
#[cfg(windows)]
#[test]
fn ps1_probes_an_appdata_root_and_refuses_only_a_virtualized_one() {
    let temp = Temp::new("ps1-probe");
    let release = make_release(&temp.0);
    release.add_bundle();
    let cosign = fake_cosign(&temp.0.join("cosign"), true, true);
    let dirs = DefaultRoot {
        profile: temp.0.join("profile"),
        local: temp.0.join("profile").join("AppData").join("Local"),
    };
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
    let home = local.join(format!(
        "sothis-bootstrap-probe-{}-{}",
        std::process::id(),
        temp.0.file_name().unwrap().to_string_lossy()
    ));
    let probe_run = |args: &[&str]| {
        let mut command = ps1_command(&release, &cosign, &dirs, args);
        command
            .env("SOTHIS_HOME", &home)
            .env("LOCALAPPDATA", &local)
            .env("APPDATA", local.parent().unwrap().join("Roaming"));
        Run {
            output: command.output().unwrap(),
        }
    };

    let first = probe_run(&["-Version", VERSION, "-NoInstall"]);
    if first.code() == 0 {
        assert!(
            home.join("bin").join(exe_name()).is_file(),
            "{}",
            first.text()
        );
    } else {
        assert_eq!(first.code(), 1, "{}", first.text());
        assert!(
            first.text().contains("refusing to place sothis"),
            "{}",
            first.text()
        );
        assert!(first.text().contains("Packages"), "{}", first.text());
        assert!(
            !home.join("bin").join(exe_name()).exists(),
            "{}",
            first.text()
        );
    }
    let _ = fs::remove_dir_all(&home);

    let allowed = probe_run(&["-Version", VERSION, "-NoInstall", "-AllowVirtualizedRoot"]);
    assert_eq!(allowed.code(), 0, "{}", allowed.text());
    assert!(home.join("bin").join(exe_name()).is_file());
    let _ = fs::remove_dir_all(&home);
}
