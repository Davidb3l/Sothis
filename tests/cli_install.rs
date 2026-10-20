//! Black-box tests of the `sothis` binary's mutating commands.
//!
//! SAFETY: every invocation sets `SOTHIS_HOME` to a fresh temp directory and
//! the mutating ones pass `--no-modify-path --no-plugins`, so nothing here can
//! touch the real PATH, registry, `~/.local/bin` or `~/.claude`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Temp {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("sothis-cli-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sothis(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sothis"))
        .args(args)
        .env("SOTHIS_HOME", home)
        // These tests install into temp directories, which on a machine where
        // the shell itself is virtualized (the Claude desktop app) would be
        // refused; the refusal has its own tests.
        .env("SOTHIS_ALLOW_VIRTUALIZED_ROOT", "1")
        .env_remove("SOTHIS_RELEASE_MIRROR")
        .env_remove("SOTHIS_COSIGN")
        .output()
        .unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn env_reports_the_sothis_home_override() {
    let temp = Temp::new("env");
    let home = temp.0.join("home");
    let output = sothis(&home, &["env", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    assert_eq!(value["root"], home.to_string_lossy().as_ref());
    assert_eq!(
        value["installDir"],
        home.join("bin").to_string_lossy().as_ref()
    );
}

#[test]
fn a_relative_sothis_home_is_rejected() {
    let output = sothis(Path::new("relative-home"), &["env", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(json(&output)["error"]
        .as_str()
        .unwrap()
        .contains("SOTHIS_HOME must be an absolute path"));
}

#[test]
fn usage_errors_exit_two() {
    let temp = Temp::new("usage");
    for args in [
        vec!["install", "--bogus"],
        vec!["install", "--profile", "nope", "--no-modify-path"],
        vec!["install", "stray", "--no-modify-path"],
        vec!["install", "--plan", "--mirror", "x"],
        vec!["rollback"],
        vec!["rollback", "not-a-tool"],
        vec!["uninstall", "--mirror", "x"],
    ] {
        let output = sothis(&temp.0, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    assert!(
        !temp.0.join("state").exists(),
        "usage errors must not create state"
    );
}

#[test]
fn plan_mode_is_unchanged_and_mutates_nothing() {
    let temp = Temp::new("plan");
    let output = sothis(
        &temp.0,
        &[
            "install",
            "--profile",
            "full",
            "--platform",
            "windows-x64",
            "--plan",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    assert_eq!(value["dryRun"], true);
    assert_eq!(value["operation"], "install");
    assert_eq!(value["items"].as_array().unwrap().len(), 6);
    let mut keys = value["items"][0]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        ["action", "name", "note", "support", "tool", "version"]
    );
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 0);
}

#[test]
fn an_install_from_an_empty_mirror_fails_closed_and_leaves_no_bin() {
    let temp = Temp::new("empty-mirror");
    let home = temp.0.join("home");
    let mirror = temp.0.join("mirror");
    fs::create_dir_all(&mirror).unwrap();
    let output = sothis(
        &home,
        &[
            "install",
            "--profile",
            "core",
            "--mirror",
            mirror.to_str().unwrap(),
            "--no-modify-path",
            "--no-plugins",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["ok"], false);
    let items = value["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    for item in items {
        assert_eq!(item["result"], "failed");
        assert!(item["detail"]
            .as_str()
            .unwrap()
            .contains("missing from the mirror"));
    }
    assert!(!home.join("bin").exists());
    assert_eq!(
        fs::read_dir(home.join("state").join("receipts"))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
}

#[test]
fn uninstall_and_update_on_an_empty_home_are_harmless() {
    let temp = Temp::new("empty-home");
    let home = temp.0.join("home");
    let uninstall = sothis(
        &home,
        &[
            "uninstall",
            "--profile",
            "core",
            "--no-modify-path",
            "--json",
        ],
    );
    assert_eq!(uninstall.status.code(), Some(0));
    let value = json(&uninstall);
    assert_eq!(value["ok"], true);
    assert!(value["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["result"] == "not-installed"));

    let update = sothis(
        &home,
        &[
            "update",
            "--profile",
            "core",
            "--no-modify-path",
            "--no-plugins",
            "--json",
        ],
    );
    assert_eq!(update.status.code(), Some(0));
    assert!(json(&update)["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["result"] == "not-installed" || item["result"] == "unmanaged"));
}

#[test]
fn rollback_without_history_fails_with_exit_one() {
    let temp = Temp::new("rollback");
    let output = sothis(&temp.0.join("home"), &["rollback", "ametrite", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["ok"], false);
    assert_eq!(value["items"][0]["action"], "rollback");
}

/// Full cycle against the REAL published Windows assets, with real cosign.
///
/// Run manually: set `SOTHIS_E2E_ASSETS` to a directory holding
/// `amt-x86_64-pc-windows-msvc.zip(.sha256)`,
/// `hayvenhurst-0.0.8-windows-x64.tar.gz(.sha256,.sigstore.json)` and
/// `sirius-forester-0.1.7-windows-x64.tar.gz(.sha256,.sigstore.json)`, then
/// `cargo test --test cli_install -- --ignored`.
#[test]
#[ignore = "needs SOTHIS_E2E_ASSETS (real release assets) and cosign"]
fn real_release_assets_install_verify_idempotency_tamper_and_uninstall() {
    let Some(assets) = std::env::var_os("SOTHIS_E2E_ASSETS").map(PathBuf::from) else {
        panic!("SOTHIS_E2E_ASSETS is not set");
    };
    let temp = Temp::new("real-assets");
    let mirror = temp.0.join("mirror");
    for (repo, version, prefix) in [
        ("Ametrite", "0.4.2", "amt-x86_64-pc-windows-msvc.zip"),
        (
            "Hayvenhurst-dev",
            "0.0.8",
            "hayvenhurst-0.0.8-windows-x64.tar.gz",
        ),
        (
            "Sirius-Forester",
            "0.1.7",
            "sirius-forester-0.1.7-windows-x64.tar.gz",
        ),
    ] {
        let dir = mirror
            .join("Davidb3l")
            .join(repo)
            .join(format!("v{version}"));
        fs::create_dir_all(&dir).unwrap();
        for suffix in ["", ".sha256", ".sigstore.json"] {
            let source = assets.join(format!("{prefix}{suffix}"));
            if source.is_file() {
                fs::copy(&source, dir.join(format!("{prefix}{suffix}"))).unwrap();
            }
        }
    }
    let home = temp.0.join("home");
    let install = |extra: &[&str]| {
        let mut args = vec![
            "install",
            "--profile",
            "core",
            "--mirror",
            mirror.to_str().unwrap(),
            "--no-modify-path",
            "--no-plugins",
            "--json",
        ];
        args.extend_from_slice(extra);
        sothis(&home, &args)
    };

    let first = install(&[]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    assert!(json(&first)["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["result"] == "installed"));
    for exe in ["amt.exe", "hayven.exe", "hayven-native.exe", "sirius.exe"] {
        assert!(home.join("bin").join(exe).is_file(), "{exe}");
    }

    let second = install(&[]);
    assert_eq!(second.status.code(), Some(0));
    assert!(json(&second)["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["result"] == "already-installed"));

    let uninstall = sothis(
        &home,
        &[
            "uninstall",
            "--profile",
            "core",
            "--no-modify-path",
            "--json",
        ],
    );
    assert_eq!(uninstall.status.code(), Some(0));
    assert!(!home.join("bin").exists() || fs::read_dir(home.join("bin")).unwrap().count() == 0);

    // Tamper: flip one byte of the mirrored hayven archive.
    let hayven = mirror
        .join("Davidb3l")
        .join("Hayvenhurst-dev")
        .join("v0.0.8")
        .join("hayvenhurst-0.0.8-windows-x64.tar.gz");
    let mut bytes = fs::read(&hayven).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    fs::write(&hayven, bytes).unwrap();
    let tampered = install(&[]);
    assert_eq!(tampered.status.code(), Some(1));
    let value = json(&tampered);
    let failed = value["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["tool"] == "hayvenhurst")
        .unwrap();
    assert!(failed["detail"]
        .as_str()
        .unwrap()
        .contains("sha256 mismatch"));
    assert!(!home.join("bin").join("hayven.exe").exists());
}

// ------------------------------------------------ virtualized roots
//
// The refusal itself is covered with a fake probe in the unit tests (the real
// probe needs a virtualized process). Here the real binary runs the real probe
// against a directory under %USERPROFILE%, which is never virtualized.

#[cfg(windows)]
fn profile_scratch(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(std::env::var_os("USERPROFILE").unwrap()).join(format!(
        ".sothis-cli-{label}-{}-{nonce}",
        std::process::id()
    ))
}

#[cfg(windows)]
fn check<'a>(report: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    report["checks"]
        .as_array()?
        .iter()
        .find(|check| check["name"] == name)
}

#[cfg(windows)]
#[test]
fn doctor_probes_a_profile_root_and_passes_root_not_virtualized() {
    let home = profile_scratch("doctor");
    let output = Command::new(env!("CARGO_BIN_EXE_sothis"))
        .args(["doctor", "--profile", "bell", "--json"])
        .env("SOTHIS_HOME", &home)
        .output()
        .unwrap();
    let existed = home.exists();
    let report = json(&output);
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(
        check(&report, "root_not_virtualized").expect("the probe check")["status"],
        "pass"
    );
    assert!(
        !existed,
        "doctor probes through an ancestor and creates no root"
    );
}

#[cfg(windows)]
#[test]
fn an_install_into_a_profile_root_is_not_refused_without_any_allow_flag() {
    let home = profile_scratch("install");
    let mirror = home.with_extension("mirror");
    fs::create_dir_all(&mirror).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sothis"))
        .args([
            "install",
            "--profile",
            "core",
            "--no-modify-path",
            "--no-plugins",
            "--mirror",
            mirror.to_str().unwrap(),
        ])
        .env("SOTHIS_HOME", &home)
        .env_remove("SOTHIS_ALLOW_VIRTUALIZED_ROOT")
        .env_remove("SOTHIS_RELEASE_MIRROR")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&mirror);
    assert!(!text.contains("refusing to install"), "{text}");
    assert!(text.contains("missing from the mirror"), "{text}");
}

#[cfg(windows)]
#[test]
fn env_defaults_to_dot_sothis_in_the_profile_not_localappdata() {
    let temp = Temp::new("env-default");
    let profile = temp.0.join("profile");
    let local = temp.0.join("profile").join("AppData").join("Local");
    let output = Command::new(env!("CARGO_BIN_EXE_sothis"))
        .args(["env", "--json"])
        .env_remove("SOTHIS_HOME")
        .env("USERPROFILE", &profile)
        .env("HOME", &profile)
        .env("LOCALAPPDATA", &local)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let value = json(&output);
    assert_eq!(
        value["root"],
        profile.join(".sothis").to_string_lossy().as_ref()
    );
    assert_eq!(
        value["installDir"],
        profile
            .join(".sothis")
            .join("bin")
            .to_string_lossy()
            .as_ref()
    );
}

#[test]
fn update_plan_lists_the_plugin_update_steps() {
    let temp = Temp::new("plan-plugins");
    let plugins = temp.0.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"sirius@sothis":[{"scope":"user"}],"sirius@sirius-forester":[{}]}}"#,
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_sothis"))
            .args(args)
            .env("SOTHIS_HOME", temp.0.join("home"))
            .env("CLAUDE_PLUGINS_DIR", &plugins)
            .output()
            .unwrap()
    };
    let output = run(&["update", "--plan", "--profile", "core", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let steps = json(&output)["pluginSteps"].clone();
    assert_eq!(
        steps,
        serde_json::json!([
            "claude plugin marketplace update sothis",
            "claude plugin update sirius@sothis"
        ])
    );

    let human = run(&["update", "--plan", "--profile", "core"]);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("plugin steps:"), "{text}");
    assert!(
        text.contains("claude plugin update sirius@sothis"),
        "{text}"
    );
    assert!(text.contains("no machine state changed"), "{text}");

    let skipped = run(&[
        "update",
        "--plan",
        "--profile",
        "core",
        "--no-plugins",
        "--json",
    ]);
    assert_eq!(json(&skipped)["pluginSteps"], serde_json::json!([]));
    // Install plans do not list plugin updates.
    let install = run(&["install", "--plan", "--profile", "core", "--json"]);
    assert_eq!(json(&install)["pluginSteps"], serde_json::json!([]));
    assert!(!temp.0.join("home").exists(), "a plan writes nothing");
}
