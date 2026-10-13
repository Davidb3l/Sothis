//! Hermetic engine tests: a generated release mirror, fake verifier/probe/
//! claude/PATH store, temp `SOTHIS_HOME`-style layout. No network, no registry,
//! no real PATH, no real `claude`.

use crate::install::engine::{
    self, Context, Operation, Options, PathMode, Services, Summary, VersionProbe,
};
use crate::install::extract::testutil::{tar_gz, zip_bytes, Item};
use crate::install::fsx::{self, testutil::TempDir};
use crate::install::layout::Layout;
use crate::install::lock::MutationLock;
use crate::install::pathenv::testutil::FakeStore;
use crate::install::pathenv::PathValue;
use crate::install::plugins::testutil::FakeClaude;
use crate::install::receipt;
use crate::install::source::MirrorFetcher;
use crate::install::verify::{SignatureStatus, SignatureVerifier, VerifyError};
use crate::manifest::{self, Manifest};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const PLATFORM: &str = "windows-x64";

struct FakeVerifier {
    /// Fail when the asset path contains this text.
    fail_for: RefCell<Option<(String, VerifyError)>>,
    /// Behave as if cosign were not installed.
    unavailable: RefCell<bool>,
    calls: RefCell<Vec<String>>,
}

impl SignatureVerifier for FakeVerifier {
    fn verify(&self, asset: &Path, bundle: &Path, identity: &str) -> Result<(), VerifyError> {
        assert!(bundle.is_file(), "the bundle must have been fetched");
        self.calls.borrow_mut().push(format!(
            "{}|{identity}",
            asset.file_name().unwrap().to_string_lossy()
        ));
        if *self.unavailable.borrow() {
            return Err(VerifyError::Unavailable("cosign was not found".to_string()));
        }
        if let Some((needle, error)) = &*self.fail_for.borrow() {
            if asset.to_string_lossy().contains(needle.as_str()) {
                return Err(error.clone());
            }
        }
        Ok(())
    }
}

/// Reports whatever text the placed "executable" contains, unless a lie is set.
struct FakeProbe {
    lie: RefCell<Option<(String, String)>>,
}

impl VersionProbe for FakeProbe {
    fn version_output(&self, exe: &Path) -> Result<String, String> {
        if let Some((name, output)) = &*self.lie.borrow() {
            if exe.to_string_lossy().contains(name.as_str()) {
                return Ok(output.clone());
            }
        }
        fs::read_to_string(exe).map_err(|e| format!("could not run {}: {e}", exe.display()))
    }
}

struct Harness {
    _dir: TempDir,
    layout: Layout,
    mirror: PathBuf,
    plugin_dir: PathBuf,
    manifest: Manifest,
    verifier: FakeVerifier,
    probe: FakeProbe,
    claude: FakeClaude,
    store: FakeStore,
    path_dirs: Vec<PathBuf>,
    on_path: BTreeMap<String, PathBuf>,
}

fn opts(operation: Operation, profile: &str) -> Options {
    Options {
        operation,
        profile: profile.to_string(),
        allow_missing_cosign: false,
        allow_downgrade: false,
        modify_path: true,
        plugins: false,
        rollback_tool: None,
    }
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new("engine");
        let layout = Layout {
            root: dir.path().join("home"),
            bin: dir.path().join("home").join("bin"),
        };
        let mirror = dir.path().join("mirror");
        let plugin_dir = dir.path().join("claude-plugins");
        fs::create_dir_all(&mirror).unwrap();
        fs::create_dir_all(&plugin_dir).unwrap();
        let harness = Harness {
            _dir: dir,
            layout,
            mirror,
            plugin_dir,
            manifest: manifest::embedded().unwrap(),
            verifier: FakeVerifier {
                fail_for: RefCell::new(None),
                unavailable: RefCell::new(false),
                calls: RefCell::new(Vec::new()),
            },
            probe: FakeProbe {
                lie: RefCell::new(None),
            },
            claude: FakeClaude::new(true),
            store: FakeStore {
                value: Some(PathValue {
                    value: r"C:\Windows;C:\Tools".to_string(),
                    expandable: true,
                }),
                ..FakeStore::default()
            },
            path_dirs: Vec::new(),
            on_path: BTreeMap::new(),
        };
        harness.publish_recommended();
        harness
    }

    fn set_version(&mut self, id: &str, version: &str) {
        let tool = self.manifest.tools.iter_mut().find(|t| t.id == id).unwrap();
        tool.recommended_version = Some(version.to_string());
    }

    fn recommended(&self, id: &str) -> String {
        self.manifest
            .tools
            .iter()
            .find(|t| t.id == id)
            .unwrap()
            .recommended_version
            .clone()
            .unwrap()
    }

    fn publish_recommended(&self) {
        for id in ["ametrite", "hayvenhurst", "sirius-forester"] {
            self.publish(id, &self.recommended(id));
        }
    }

    fn asset_path(&self, id: &str, version: &str) -> PathBuf {
        let tool = self.manifest.tools.iter().find(|t| t.id == id).unwrap();
        let asset = tool.distribution.assets[PLATFORM].replace("{version}", version);
        let mut dir = self.mirror.clone();
        for part in tool.repository.split('/') {
            dir.push(part);
        }
        dir.join(format!("v{version}")).join(asset)
    }

    /// Write the asset, its `.sha256` and (for signed tools) a dummy bundle.
    fn publish(&self, id: &str, version: &str) {
        let path = self.asset_path(id, version);
        let asset = path.file_name().unwrap().to_string_lossy().into_owned();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = match id {
            "ametrite" => zip_bytes(&[
                ("amt.exe", format!("amt {version}").as_bytes()),
                ("LICENSE", b"license"),
                ("README.md", b"readme"),
            ]),
            "hayvenhurst" => {
                let top = format!("hayvenhurst-{version}-windows-x64");
                let names = [
                    format!("{top}/"),
                    format!("{top}/hayven.exe"),
                    format!("{top}/hayven-native.exe"),
                    format!("{top}/skill/"),
                    format!("{top}/skill/hayvenhurst.md"),
                    format!("{top}/viewer/"),
                    format!("{top}/viewer/dist/"),
                    format!("{top}/viewer/dist/index.html"),
                    format!("{top}/LICENSE"),
                ];
                let hayven = format!("hayven {version}");
                let native = format!("native {version}");
                tar_gz(&[
                    Item::Dir(&names[0]),
                    Item::File(&names[1], hayven.as_bytes(), 0o755),
                    Item::File(&names[2], native.as_bytes(), 0o755),
                    Item::Dir(&names[3]),
                    Item::File(&names[4], b"skill", 0o644),
                    Item::Dir(&names[5]),
                    Item::Dir(&names[6]),
                    Item::File(&names[7], b"<html>", 0o644),
                    Item::File(&names[8], b"license", 0o644),
                ])
            }
            "sirius-forester" => {
                let top = format!("sirius-forester-{version}-windows-x64");
                let dir = format!("{top}/");
                let exe = format!("{top}/sirius.exe");
                let sirius = format!("sirius {version}");
                tar_gz(&[Item::Dir(&dir), Item::File(&exe, sirius.as_bytes(), 0o755)])
            }
            other => panic!("no fake release for {other}"),
        };
        fs::write(&path, bytes).unwrap();
        let digest = fsx::sha256_file(&path).unwrap();
        fs::write(
            format!("{}.sha256", path.display()),
            format!("{digest} *{asset}\n"),
        )
        .unwrap();
        let tool = self.manifest.tools.iter().find(|t| t.id == id).unwrap();
        if tool.distribution.verification == "sha256+sigstore" {
            fs::write(
                format!("{}.sigstore.json", path.display()),
                "{\"bundle\":true}",
            )
            .unwrap();
        }
    }

    fn run(&mut self, options: &Options) -> Result<Summary, String> {
        let fetcher = MirrorFetcher {
            dir: self.mirror.clone(),
        };
        let expand = |_: &str| None;
        let on_path = self.on_path.clone();
        let find = move |name: &str| on_path.get(name).cloned();
        let ctx = Context {
            manifest: &self.manifest,
            layout: &self.layout,
            platform: PLATFORM,
            exe_suffix: ".exe",
        };
        let services = Services {
            fetcher: &fetcher,
            verifier: &self.verifier,
            probe: &self.probe,
            claude: &self.claude,
            path: PathMode::Store(&mut self.store),
            plugin_state_dir: Some(self.plugin_dir.clone()),
            path_dirs: self.path_dirs.clone(),
            expand_env: &expand,
            find_command: &find,
        };
        engine::run(&ctx, options, services)
    }

    fn install(&mut self, profile: &str) -> Summary {
        self.run(&opts(Operation::Install, profile)).unwrap()
    }

    fn bin_names(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.layout.bin) else {
            return Vec::new();
        };
        let mut names = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn bin_text(&self, name: &str) -> String {
        fs::read_to_string(self.layout.bin.join(name)).unwrap()
    }

    fn receipt(&self, id: &str) -> Option<receipt::Receipt> {
        receipt::read(&self.layout, id).unwrap()
    }

    fn path_value(&self) -> String {
        self.store.value.as_ref().unwrap().value.clone()
    }

    /// No scratch data may survive a finished command.
    fn assert_no_scratch(&self) {
        for dir in [self.layout.staging_root(), self.layout.rollback_root()] {
            if let Ok(entries) = fs::read_dir(&dir) {
                let left = entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                assert!(left.is_empty(), "{} still has {left:?}", dir.display());
            }
        }
    }
}

fn item<'a>(summary: &'a Summary, tool: &str, action: &str) -> &'a engine::ItemResult {
    summary
        .items
        .iter()
        .find(|i| i.tool == tool && i.action == action)
        .unwrap_or_else(|| panic!("no {tool}/{action} item in {:#?}", summary.items))
}

fn result<'a>(summary: &'a Summary, tool: &str) -> &'a str {
    &summary
        .items
        .iter()
        .find(|i| i.tool == tool && !i.action.contains("plugin"))
        .unwrap_or_else(|| panic!("no item for {tool}"))
        .result
}

// ------------------------------------------------------------------ install

#[test]
fn a_core_install_places_verifies_and_receipts_every_tool() {
    let mut h = Harness::new();
    let summary = h.install("core");
    assert!(summary.ok, "{}", summary.render_human());
    assert_eq!(summary.operation, "install");
    assert_eq!(summary.platform, PLATFORM);
    for id in ["ametrite", "hayvenhurst", "sirius-forester"] {
        assert_eq!(result(&summary, id), "installed");
        assert_eq!(item(&summary, id, "install").action, "install");
    }
    assert_eq!(
        h.bin_names(),
        [
            "amt.exe",
            "hayven-native.exe",
            "hayven.exe",
            "sirius.exe",
            "skill",
            "viewer"
        ]
    );
    assert_eq!(h.bin_text("amt.exe"), "amt 0.4.2");
    assert_eq!(h.bin_text("hayven.exe"), "hayven 0.0.8");
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.7");
    assert!(h
        .layout
        .bin
        .join("viewer")
        .join("dist")
        .join("index.html")
        .is_file());

    let amt = h.receipt("ametrite").unwrap();
    assert_eq!(amt.signature, SignatureStatus::NotPublished);
    assert_eq!(amt.asset, "amt-x86_64-pc-windows-msvc.zip");
    let hayven = h.receipt("hayvenhurst").unwrap();
    assert_eq!(hayven.signature, SignatureStatus::Verified);
    assert_eq!(hayven.platform, PLATFORM);
    assert_eq!(hayven.previous_version, None);
    assert_eq!(
        hayven
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["hayven.exe", "hayven-native.exe", "skill", "viewer"]
    );
    assert_eq!(hayven.bin_dir, h.layout.bin);
    assert!(h
        .layout
        .version_dir("hayvenhurst", "0.0.8")
        .join("LICENSE")
        .is_file());
    assert!(receipt::read_meta(&h.layout.version_dir("hayvenhurst", "0.0.8")).is_ok());

    // Sigstore was checked for the signed tools only, with the substituted identity.
    let calls = h.verifier.calls.borrow().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls
        .iter()
        .any(|c| c.contains("hayvenhurst-0.0.8-windows-x64.tar.gz")
            && c.ends_with("Hayvenhurst-dev/.github/workflows/release.yml@refs/tags/v0.0.8")));

    assert!(summary.path_changed);
    assert!(h
        .path_value()
        .starts_with(&format!("{};", h.layout.bin.display())));
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.contains("PATH changes are visible only to new processes")));
    h.assert_no_scratch();
}

#[test]
fn a_second_install_of_the_same_versions_is_a_no_op() {
    let mut h = Harness::new();
    h.install("core");
    let writes = h.store.writes;
    let before = h.bin_names();
    let summary = h.install("core");
    assert!(summary.ok);
    for id in ["ametrite", "hayvenhurst", "sirius-forester"] {
        assert_eq!(result(&summary, id), "already-installed");
    }
    assert_eq!(h.bin_names(), before);
    assert_eq!(h.store.writes, writes, "PATH is not rewritten");
    assert!(!summary.path_changed);
    assert_eq!(
        h.verifier.calls.borrow().len(),
        2,
        "no re-verification, no re-download"
    );
    h.assert_no_scratch();
}

#[test]
fn modified_installed_files_are_repaired_by_a_reinstall() {
    let mut h = Harness::new();
    h.install("core");
    fs::write(h.layout.bin.join("amt.exe"), "amt 0.4.2").unwrap(); // same content: still intact
    assert_eq!(result(&h.install("core"), "ametrite"), "already-installed");
    fs::write(h.layout.bin.join("sirius.exe"), "corrupted").unwrap();
    let summary = h.install("core");
    assert_eq!(result(&summary, "sirius-forester"), "repaired");
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.7");
    assert_eq!(h.receipt("sirius-forester").unwrap().previous_version, None);
}

#[test]
fn a_tampered_download_fails_on_sha256_and_leaves_the_bin_untouched() {
    let mut h = Harness::new();
    let path = h.asset_path("hayvenhurst", "0.0.8");
    let mut bytes = fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(&path, bytes).unwrap();
    let summary = h.install("core");
    assert!(!summary.ok);
    let failed = item(&summary, "hayvenhurst", "install");
    assert_eq!(failed.result, "failed");
    assert!(failed.failed);
    assert!(
        failed.detail.contains("sha256 mismatch"),
        "{}",
        failed.detail
    );
    // Other tools are unaffected and stay installed.
    assert_eq!(result(&summary, "ametrite"), "installed");
    assert_eq!(result(&summary, "sirius-forester"), "installed");
    assert_eq!(h.bin_names(), ["amt.exe", "sirius.exe"]);
    assert!(h.receipt("hayvenhurst").is_none());
    assert!(!h.layout.tool_dir("hayvenhurst").exists());
    assert_eq!(
        h.verifier
            .calls
            .borrow()
            .iter()
            .filter(|c| c.contains("hayvenhurst"))
            .count(),
        0,
        "the signature is never consulted for a corrupt download"
    );
    h.assert_no_scratch();
}

#[test]
fn a_failing_signature_is_fatal_and_leaves_no_trace() {
    let mut h = Harness::new();
    *h.verifier.fail_for.borrow_mut() = Some((
        "hayvenhurst".to_string(),
        VerifyError::Failed("none of the expected identities matched".to_string()),
    ));
    let summary = h.install("core");
    assert!(!summary.ok);
    let failed = item(&summary, "hayvenhurst", "install");
    assert!(
        failed.detail.contains("signature verification failed"),
        "{}",
        failed.detail
    );
    assert!(failed.detail.contains("expected identities"));
    assert_eq!(h.bin_names(), ["amt.exe", "sirius.exe"]);
    assert!(h.receipt("hayvenhurst").is_none());
    assert!(!h.layout.tool_dir("hayvenhurst").exists());
    h.assert_no_scratch();

    // The allow flag never softens a cosign that ran and failed.
    let mut allow = opts(Operation::Install, "core");
    allow.allow_missing_cosign = true;
    let summary = h.run(&allow).unwrap();
    assert!(!summary.ok);
    assert!(h.receipt("hayvenhurst").is_none());
}

#[test]
fn missing_cosign_fails_closed_unless_explicitly_allowed() {
    let mut h = Harness::new();
    *h.verifier.unavailable.borrow_mut() = true;
    let summary = h.install("core");
    assert!(!summary.ok);
    let failed = item(&summary, "hayvenhurst", "install");
    assert!(
        failed.detail.contains("cannot verify the signature"),
        "{}",
        failed.detail
    );
    assert_eq!(
        result(&summary, "ametrite"),
        "installed",
        "checksum-only tools do not need cosign"
    );
    assert!(h.receipt("sirius-forester").is_none());

    let mut allow = opts(Operation::Install, "core");
    allow.allow_missing_cosign = true;
    let summary = h.run(&allow).unwrap();
    assert!(summary.ok, "{}", summary.render_human());
    assert_eq!(
        h.receipt("hayvenhurst").unwrap().signature,
        SignatureStatus::SkippedNoCosign
    );
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.contains("NOT signature-verified")));
    assert_eq!(
        h.receipt("ametrite").unwrap().signature,
        SignatureStatus::NotPublished
    );
}

#[test]
fn a_bad_version_check_rolls_back_to_the_exact_previous_state() {
    let mut h = Harness::new();
    h.set_version("sirius-forester", "0.1.6");
    h.publish("sirius-forester", "0.1.6");
    h.install("core");
    let before_receipt = h.receipt("sirius-forester").unwrap();
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.6");

    h.set_version("sirius-forester", "0.1.7");
    h.publish("sirius-forester", "0.1.7");
    *h.probe.lie.borrow_mut() = Some(("sirius".to_string(), "sirius 9.9.9".to_string()));
    let summary = h.install("core");
    assert!(!summary.ok);
    let failed = item(&summary, "sirius-forester", "install");
    assert!(
        failed.detail.contains("reports version 9.9.9"),
        "{}",
        failed.detail
    );
    assert!(failed.detail.contains("rolled back"));
    assert_eq!(
        h.bin_text("sirius.exe"),
        "sirius 0.1.6",
        "the old binary is back"
    );
    assert_eq!(h.receipt("sirius-forester").unwrap(), before_receipt);
    assert!(!h.layout.version_dir("sirius-forester", "0.1.7").exists());
    assert!(h.layout.version_dir("sirius-forester", "0.1.6").is_dir());
    assert_eq!(
        h.bin_names().len(),
        6,
        "no stage debris: {:?}",
        h.bin_names()
    );
    h.assert_no_scratch();
}

#[test]
fn an_archive_missing_a_declared_file_fails_before_the_bin_is_touched() {
    let mut h = Harness::new();
    let tool = h
        .manifest
        .tools
        .iter_mut()
        .find(|t| t.id == "ametrite")
        .unwrap();
    tool.distribution.files = Some(vec!["amt{exe}".to_string(), "extra-tool{exe}".to_string()]);
    fs::create_dir_all(&h.layout.bin).unwrap();
    fs::write(h.layout.bin.join("amt.exe"), "user amt").unwrap();
    let summary = h.install("core");
    let failed = item(&summary, "ametrite", "install");
    assert!(
        failed
            .detail
            .contains("does not contain the expected entry"),
        "{}",
        failed.detail
    );
    assert_eq!(h.bin_text("amt.exe"), "user amt");
    assert!(h.receipt("ametrite").is_none());
    assert!(!h.layout.tool_dir("ametrite").exists());
}

#[test]
fn an_existing_unmanaged_binary_is_adopted_and_restored_on_uninstall() {
    let mut h = Harness::new();
    fs::create_dir_all(&h.layout.bin).unwrap();
    fs::write(h.layout.bin.join("amt.exe"), "amt 0.2.0 stale").unwrap();
    fs::write(h.layout.bin.join("unrelated.txt"), "keep").unwrap();
    let summary = h.install("core");
    assert!(summary.ok);
    assert!(item(&summary, "ametrite", "install")
        .detail
        .contains("adopted"));
    assert_eq!(h.bin_text("amt.exe"), "amt 0.4.2");
    let receipt = h.receipt("ametrite").unwrap();
    assert_eq!(receipt.adopted.len(), 1);
    assert_eq!(
        fs::read_to_string(&receipt.adopted[0].backup).unwrap(),
        "amt 0.2.0 stale"
    );

    let summary = h.run(&opts(Operation::Uninstall, "core")).unwrap();
    assert!(summary.ok, "{}", summary.render_human());
    assert_eq!(h.bin_text("amt.exe"), "amt 0.2.0 stale");
    assert_eq!(h.bin_names(), ["amt.exe", "unrelated.txt"]);
    assert!(h.receipt("ametrite").is_none());
    assert!(!h.layout.backups_dir("ametrite").exists());
    h.assert_no_scratch();
}

#[test]
fn files_owned_by_another_tool_are_never_replaced() {
    let mut h = Harness::new();
    h.install("core");
    let tool = h
        .manifest
        .tools
        .iter_mut()
        .find(|t| t.id == "ametrite")
        .unwrap();
    tool.distribution.files = Some(vec!["sirius{exe}".to_string()]);
    h.set_version("ametrite", "0.4.3");
    h.publish("ametrite", "0.4.3");
    let summary = h.install("core");
    let failed = item(&summary, "ametrite", "install");
    assert!(
        failed.detail.contains("already owned by sirius-forester"),
        "{}",
        failed.detail
    );
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.7");
}

#[test]
fn a_receipt_for_a_different_bin_directory_is_refused() {
    let mut h = Harness::new();
    h.install("core");
    h.set_version("ametrite", "0.4.3");
    h.publish("ametrite", "0.4.3");
    h.layout.bin = h.layout.root.join("other-bin");
    let summary = h.install("core");
    let failed = item(&summary, "ametrite", "install");
    assert!(
        failed.detail.contains("was installed into"),
        "{}",
        failed.detail
    );
}

#[test]
fn a_concurrent_mutation_fails_fast_on_the_lock() {
    let mut h = Harness::new();
    let held = MutationLock::acquire(&h.layout.lock_path()).unwrap();
    let error = h.run(&opts(Operation::Install, "core")).unwrap_err();
    assert!(error.contains("already running"), "{error}");
    assert!(h.bin_names().is_empty());
    drop(held);
    assert!(h.run(&opts(Operation::Install, "core")).unwrap().ok);
}

#[test]
fn non_native_tools_are_reported_not_failed() {
    let mut h = Harness::new();
    let summary = h.install("full");
    assert!(summary.ok, "{}", summary.render_human());
    // PingMyBell builds on Windows (source-only): reported as a manual build,
    // never installed or hidden, and never a failure.
    let pingmybell = item(&summary, "pingmybell", "build_from_source");
    assert_eq!(pingmybell.result, "manual");
    assert!(!pingmybell.failed);
    assert!(pingmybell
        .detail
        .contains("passes its test suite on Windows 11"));
    assert!(pingmybell.detail.contains("github.com/Davidb3l/pingmybell"));
    let guignet = item(&summary, "guignet", "use_wsl2");
    assert_eq!(guignet.result, "wsl2");
    assert!(guignet.detail.contains("WSL2"));
    assert_eq!(
        item(&summary, "catryna-wikinelli", "install_plugin_runtime").result,
        "missing-runtime"
    );
}

#[test]
fn the_bun_runtime_is_checked_but_never_installed() {
    let mut h = Harness::new();
    let summary = h.install("docs");
    let missing = item(&summary, "catryna-wikinelli", "install_plugin_runtime");
    assert_eq!(missing.result, "missing-runtime");
    assert!(missing.detail.contains("bun.sh"));
    assert!(!missing.failed);
    h.on_path
        .insert("bun".to_string(), PathBuf::from(r"C:\bun\bun.exe"));
    let summary = h.install("docs");
    assert_eq!(
        item(&summary, "catryna-wikinelli", "install_plugin_runtime").result,
        "ready"
    );
}

// -------------------------------------------------------------------- PATH

#[test]
fn no_modify_path_leaves_the_user_path_alone_and_says_so() {
    let mut h = Harness::new();
    let mut options = opts(Operation::Install, "core");
    options.modify_path = false;
    let summary = h.run(&options).unwrap();
    assert!(summary.ok);
    assert!(!summary.path_changed);
    assert_eq!(h.store.writes, 0);
    assert_eq!(h.path_value(), r"C:\Windows;C:\Tools");
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.contains("--no-modify-path")));
}

#[test]
fn a_bin_dir_that_is_not_on_path_is_reported_without_saying_ahead() {
    let mut h = Harness::new();
    let stale = h._dir.path().join("stale-bin");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("amt.exe"), "amt 0.2.0").unwrap();
    h.path_dirs = vec![stale.clone()];
    let mut options = opts(Operation::Install, "core");
    options.modify_path = false;
    let summary = h.run(&options).unwrap();
    let warning = summary
        .warnings
        .iter()
        .find(|w| w.starts_with("amt on PATH currently resolves to"))
        .unwrap_or_else(|| panic!("{:?}", summary.warnings));
    assert!(warning.contains(&stale.join("amt.exe").display().to_string()));
    assert!(warning.contains(&format!(
        "Sothis's copy in {} is not on PATH",
        h.layout.bin.display()
    )));
    assert!(!warning.contains("ahead"), "{warning}");
    assert!(
        !summary
            .warnings
            .iter()
            .any(|w| w.contains("also exists at")),
        "{:?}",
        summary.warnings
    );
}

#[test]
fn copies_ahead_of_a_bin_dir_that_is_on_path_are_reported_as_shadowing() {
    let mut h = Harness::new();
    let stale = h._dir.path().join("stale-bin");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("amt.exe"), "amt 0.2.0").unwrap();
    h.path_dirs = vec![stale.clone(), h.layout.bin.clone()];
    let mut options = opts(Operation::Install, "core");
    options.modify_path = false;
    let summary = h.run(&options).unwrap();
    assert!(
        summary
            .warnings
            .iter()
            .any(|w| w.contains("amt also exists at")
                && w.contains("stale-bin")
                && w.contains("ahead of")),
        "{:?}",
        summary.warnings
    );
    assert!(!summary
        .warnings
        .iter()
        .any(|w| w.contains("currently resolves to")));
}

#[test]
fn a_path_prepend_makes_the_new_bin_lead_so_nothing_shadows_it() {
    let mut h = Harness::new();
    let stale = h._dir.path().join("stale-bin");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("amt.exe"), "amt 0.2.0").unwrap();
    h.path_dirs = vec![stale];
    let summary = h.install("core");
    assert!(summary.path_changed);
    assert!(!summary
        .warnings
        .iter()
        .any(|w| w.contains("also exists at") || w.contains("currently resolves to")));
}

#[test]
fn an_unwritable_user_path_is_a_failed_step() {
    struct FailingStore;
    impl crate::install::pathenv::PathStore for FailingStore {
        fn read(&self) -> Result<Option<PathValue>, String> {
            Err("registry denied".to_string())
        }
        fn write(&mut self, _: &PathValue) -> Result<(), String> {
            Err("registry denied".to_string())
        }
        fn broadcast_change(&mut self) -> Result<(), String> {
            Ok(())
        }
    }
    let h = Harness::new();
    let fetcher = MirrorFetcher {
        dir: h.mirror.clone(),
    };
    let expand = |_: &str| None;
    let find = |_: &str| None;
    let ctx = Context {
        manifest: &h.manifest,
        layout: &h.layout,
        platform: PLATFORM,
        exe_suffix: ".exe",
    };
    let mut store = FailingStore;
    let options = opts(Operation::Install, "core");
    let summary = engine::run(
        &ctx,
        &options,
        Services {
            fetcher: &fetcher,
            verifier: &h.verifier,
            probe: &h.probe,
            claude: &h.claude,
            path: PathMode::Store(&mut store),
            plugin_state_dir: None,
            path_dirs: Vec::new(),
            expand_env: &expand,
            find_command: &find,
        },
    )
    .unwrap();
    assert!(!summary.ok);
    assert_eq!(item(&summary, "sothis", "modify_path").result, "failed");
    // The tools themselves stay installed: PATH failure does not roll them back.
    assert!(h.receipt("ametrite").is_some());
}

#[test]
fn advisory_path_mode_prints_the_exact_line_and_never_writes() {
    let h = Harness::new();
    let fetcher = MirrorFetcher {
        dir: h.mirror.clone(),
    };
    let expand = |_: &str| None;
    let find = |_: &str| None;
    let ctx = Context {
        manifest: &h.manifest,
        layout: &h.layout,
        platform: PLATFORM,
        exe_suffix: ".exe",
    };
    let options = opts(Operation::Install, "core");
    let summary = engine::run(
        &ctx,
        &options,
        Services {
            fetcher: &fetcher,
            verifier: &h.verifier,
            probe: &h.probe,
            claude: &h.claude,
            path: PathMode::Advisory,
            plugin_state_dir: None,
            path_dirs: Vec::new(),
            expand_env: &expand,
            find_command: &find,
        },
    )
    .unwrap();
    assert!(summary.ok);
    assert!(!summary.path_changed);
    let line = format!("export PATH=\"{}:$PATH\"", h.layout.bin.display());
    assert!(
        summary.warnings.iter().any(|w| w.contains(&line)),
        "{:?}",
        summary.warnings
    );
}

// ------------------------------------------------------------------ plugins

#[test]
fn plugins_are_installed_through_claude_in_the_documented_order() {
    let mut h = Harness::new();
    let mut options = opts(Operation::Install, "core");
    options.plugins = true;
    let summary = h.run(&options).unwrap();
    assert!(summary.ok, "{}", summary.render_human());
    let calls = h.claude.calls.borrow().clone();
    assert_eq!(calls[0], "plugin marketplace add Davidb3l/Sothis");
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.starts_with("plugin marketplace add"))
            .count(),
        3,
        "the fake never records the marketplace, so each step adds it: {calls:?}"
    );
    for plugin in ["hayvenhurst@sothis", "sirius@sothis", "sothis@sothis"] {
        assert!(
            calls.contains(&format!("plugin install {plugin}")),
            "{calls:?}"
        );
    }
    assert_eq!(
        item(&summary, "sothis", "install_plugin").result,
        "installed"
    );
}

#[test]
fn the_marketplace_is_added_only_when_unknown_and_installed_plugins_are_skipped() {
    let mut h = Harness::new();
    fs::write(
        h.plugin_dir.join("known_marketplaces.json"),
        r#"{"sothis":{"source":{"source":"github","repo":"Davidb3l/Sothis"}}}"#,
    )
    .unwrap();
    fs::write(
        h.plugin_dir.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"sirius@sothis":[{"scope":"user"}]}}"#,
    )
    .unwrap();
    let mut options = opts(Operation::Install, "core");
    options.plugins = true;
    let summary = h.run(&options).unwrap();
    let calls = h.claude.calls.borrow().clone();
    assert!(
        !calls.iter().any(|c| c.contains("marketplace add")),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|c| c.contains("sirius@sothis")));
    assert_eq!(
        item(&summary, "sirius-forester", "install_plugin").result,
        "already-installed"
    );
    assert_eq!(
        item(&summary, "hayvenhurst", "install_plugin").result,
        "installed"
    );
}

#[test]
fn a_legacy_plugin_is_reported_with_the_migration_and_left_untouched() {
    let mut h = Harness::new();
    fs::write(
        h.plugin_dir.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"sirius@sirius-forester":[{"scope":"user"}]}}"#,
    )
    .unwrap();
    let mut options = opts(Operation::Install, "core");
    options.plugins = true;
    let summary = h.run(&options).unwrap();
    assert!(summary.ok);
    let legacy = item(&summary, "sirius-forester", "install_plugin");
    assert_eq!(legacy.result, "legacy-plugin");
    assert!(legacy
        .detail
        .contains("claude plugin uninstall sirius@sirius-forester"));
    assert!(!h
        .claude
        .calls
        .borrow()
        .iter()
        .any(|c| c.contains("sirius@sirius-forester") || c == "plugin install sirius@sothis"));
}

#[test]
fn no_plugins_never_invokes_claude_and_a_missing_cli_reports_commands() {
    let mut h = Harness::new();
    h.install("core");
    assert!(h.claude.calls.borrow().is_empty());

    let mut h = Harness::new();
    h.claude = FakeClaude::new(false);
    let mut options = opts(Operation::Install, "core");
    options.plugins = true;
    let summary = h.run(&options).unwrap();
    assert!(summary.ok);
    let manual = item(&summary, "hayvenhurst", "install_plugin");
    assert_eq!(manual.result, "manual");
    assert!(manual
        .detail
        .contains("claude plugin install hayvenhurst@sothis"));
}

#[test]
fn a_failing_plugin_install_is_reported_without_undoing_the_binary() {
    let mut h = Harness::new();
    h.claude.fail_on = Some("plugin install hayvenhurst");
    let mut options = opts(Operation::Install, "core");
    options.plugins = true;
    let summary = h.run(&options).unwrap();
    assert!(!summary.ok);
    assert!(item(&summary, "hayvenhurst", "install_plugin").failed);
    assert_eq!(h.bin_text("hayven.exe"), "hayven 0.0.8");
}

// ------------------------------------------------------------------- update

#[test]
fn update_distinguishes_missing_unmanaged_and_current_tools() {
    let mut h = Harness::new();
    let summary = h.run(&opts(Operation::Update, "core")).unwrap();
    assert!(summary.ok);
    assert_eq!(result(&summary, "ametrite"), "not-installed");
    assert!(h.bin_names().is_empty());

    h.on_path.insert(
        "amt".to_string(),
        PathBuf::from(r"C:\Users\t\.cargo\bin\amt.exe"),
    );
    let summary = h.run(&opts(Operation::Update, "core")).unwrap();
    let unmanaged = item(&summary, "ametrite", "check_for_update");
    assert_eq!(unmanaged.result, "unmanaged");
    assert!(unmanaged.detail.contains("sothis install"));
    assert!(
        h.bin_names().is_empty(),
        "update never installs an unmanaged tool"
    );

    h.install("core");
    let summary = h.run(&opts(Operation::Update, "core")).unwrap();
    assert_eq!(result(&summary, "ametrite"), "up-to-date");
}

#[test]
fn update_moves_to_the_recommended_version_and_retains_the_previous_one() {
    let mut h = Harness::new();
    h.set_version("hayvenhurst", "0.0.7");
    h.publish("hayvenhurst", "0.0.7");
    h.install("core");
    h.set_version("hayvenhurst", "0.0.8");
    h.publish("hayvenhurst", "0.0.8");
    let summary = h.run(&opts(Operation::Update, "core")).unwrap();
    assert!(summary.ok, "{}", summary.render_human());
    assert_eq!(
        item(&summary, "hayvenhurst", "check_for_update").result,
        "updated"
    );
    assert_eq!(h.bin_text("hayven.exe"), "hayven 0.0.8");
    let receipt = h.receipt("hayvenhurst").unwrap();
    assert_eq!(receipt.version, "0.0.8");
    assert_eq!(receipt.previous_version.as_deref(), Some("0.0.7"));
    assert!(h.layout.version_dir("hayvenhurst", "0.0.7").is_dir());
    assert!(h.layout.version_dir("hayvenhurst", "0.0.8").is_dir());
    h.assert_no_scratch();
}

#[test]
fn update_never_downgrades_without_the_flag() {
    let mut h = Harness::new();
    h.set_version("sirius-forester", "0.1.9");
    h.publish("sirius-forester", "0.1.9");
    h.install("core");
    h.set_version("sirius-forester", "0.1.7");
    let summary = h.run(&opts(Operation::Update, "core")).unwrap();
    assert_eq!(result(&summary, "sirius-forester"), "up-to-date");
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.9");
    let summary = h.install("core");
    assert_eq!(result(&summary, "sirius-forester"), "newer-installed");
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.9");

    let mut allow = opts(Operation::Update, "core");
    allow.allow_downgrade = true;
    let summary = h.run(&allow).unwrap();
    assert_eq!(result(&summary, "sirius-forester"), "downgraded");
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.7");
    assert_eq!(
        h.receipt("sirius-forester")
            .unwrap()
            .previous_version
            .as_deref(),
        Some("0.1.9")
    );
}

#[test]
fn old_versions_beyond_the_previous_one_are_pruned() {
    let mut h = Harness::new();
    for version in ["0.1.5", "0.1.6", "0.1.7"] {
        h.set_version("sirius-forester", version);
        h.publish("sirius-forester", version);
        h.install("core");
    }
    let dirs = fs::read_dir(h.layout.tool_dir("sirius-forester"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(dirs.into_iter().collect::<Vec<_>>(), ["0.1.6", "0.1.7"]);
}

// ----------------------------------------------------------------- rollback

fn rollback_opts(tool: &str) -> Options {
    let mut options = opts(Operation::Rollback, "n/a");
    options.rollback_tool = Some(tool.to_string());
    options
}

#[test]
fn rollback_restores_the_previous_version_and_can_roll_forward_again() {
    let mut h = Harness::new();
    h.set_version("hayvenhurst", "0.0.7");
    h.publish("hayvenhurst", "0.0.7");
    h.install("core");
    h.set_version("hayvenhurst", "0.0.8");
    h.publish("hayvenhurst", "0.0.8");
    h.install("core");
    assert_eq!(h.bin_text("hayven.exe"), "hayven 0.0.8");

    let summary = h.run(&rollback_opts("hayvenhurst")).unwrap();
    assert!(summary.ok, "{}", summary.render_human());
    assert_eq!(
        item(&summary, "hayvenhurst", "rollback").result,
        "rolled-back"
    );
    assert_eq!(h.bin_text("hayven.exe"), "hayven 0.0.7");
    assert_eq!(h.bin_text("hayven-native.exe"), "native 0.0.7");
    let receipt = h.receipt("hayvenhurst").unwrap();
    assert_eq!(receipt.version, "0.0.7");
    assert_eq!(receipt.previous_version.as_deref(), Some("0.0.8"));
    assert_eq!(receipt.signature, SignatureStatus::Verified);
    assert_eq!(receipt.asset, "hayvenhurst-0.0.7-windows-x64.tar.gz");
    h.assert_no_scratch();

    let summary = h.run(&rollback_opts("hayvenhurst")).unwrap();
    assert!(summary.ok);
    assert_eq!(h.bin_text("hayven.exe"), "hayven 0.0.8");
    assert_eq!(
        h.receipt("hayvenhurst")
            .unwrap()
            .previous_version
            .as_deref(),
        Some("0.0.7")
    );
}

#[test]
fn rollback_fails_cleanly_without_a_previous_version_or_with_a_damaged_payload() {
    let mut h = Harness::new();
    h.install("core");
    let summary = h.run(&rollback_opts("sirius-forester")).unwrap();
    assert!(!summary.ok);
    assert!(item(&summary, "sirius-forester", "rollback")
        .detail
        .contains("no previous version"));
    let summary = h.run(&rollback_opts("pingmybell"));
    assert!(summary.is_err());

    let summary = h.run(&rollback_opts("ametrite")).unwrap();
    assert!(!summary.ok);

    // A retained payload that no longer matches its recorded hash is refused.
    h.set_version("sirius-forester", "0.1.8");
    h.publish("sirius-forester", "0.1.8");
    h.install("core");
    fs::write(
        h.layout
            .version_dir("sirius-forester", "0.1.7")
            .join("sirius.exe"),
        "tampered",
    )
    .unwrap();
    let summary = h.run(&rollback_opts("sirius-forester")).unwrap();
    assert!(item(&summary, "sirius-forester", "rollback")
        .detail
        .contains("no longer matches"));
    assert_eq!(
        h.bin_text("sirius.exe"),
        "sirius 0.1.8",
        "the current version stays"
    );
    assert_eq!(h.receipt("sirius-forester").unwrap().version, "0.1.8");
}

#[test]
fn a_failed_rollback_version_check_restores_the_current_version() {
    let mut h = Harness::new();
    h.set_version("sirius-forester", "0.1.6");
    h.publish("sirius-forester", "0.1.6");
    h.install("core");
    h.set_version("sirius-forester", "0.1.7");
    h.publish("sirius-forester", "0.1.7");
    h.install("core");
    // Make the retained 0.1.6 binary "report" the wrong version.
    *h.probe.lie.borrow_mut() = Some(("sirius".to_string(), "sirius 3.0.0".to_string()));
    let summary = h.run(&rollback_opts("sirius-forester")).unwrap();
    assert!(!summary.ok);
    assert_eq!(h.bin_text("sirius.exe"), "sirius 0.1.7");
    assert_eq!(h.receipt("sirius-forester").unwrap().version, "0.1.7");
    h.assert_no_scratch();
}

// ---------------------------------------------------------------- uninstall

#[test]
fn uninstall_removes_only_owned_files_in_reverse_order_and_cleans_up() {
    let mut h = Harness::new();
    h.install("core");
    fs::write(h.layout.bin.join("my-own-script.cmd"), "echo hi").unwrap();
    let data = h._dir.path().join("project").join(".hayven");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("index.db"), "data").unwrap();

    let summary = h.run(&opts(Operation::Uninstall, "core")).unwrap();
    assert!(summary.ok, "{}", summary.render_human());
    let order = summary
        .items
        .iter()
        .map(|i| i.tool.as_str())
        .collect::<Vec<_>>();
    assert_eq!(order, ["sirius-forester", "hayvenhurst", "ametrite"]);
    assert!(summary
        .items
        .iter()
        .all(|i| i.result == "removed" && i.action == "remove"));
    assert_eq!(h.bin_names(), ["my-own-script.cmd"]);
    assert!(h.receipt("ametrite").is_none());
    assert!(!h.layout.tools_dir().join("hayvenhurst").exists());
    assert!(
        data.join("index.db").is_file(),
        "tool data is never touched"
    );
    // No receipts remain, so the PATH entry is removed too.
    assert!(summary.path_changed);
    assert!(!h.path_value().contains(&h.layout.bin.display().to_string()));
    assert_eq!(h.path_value(), r"C:\Windows;C:\Tools");
    h.assert_no_scratch();
}

#[test]
fn uninstall_keeps_the_path_entry_while_other_tools_remain() {
    let mut h = Harness::new();
    h.install("core");
    let mut docs = opts(Operation::Uninstall, "core");
    // Removing only the docs-profile subset is not possible with core, so
    // uninstall one tool by planning a profile that contains just it.
    h.manifest.profiles.insert(
        "only-sirius".to_string(),
        manifest::Profile {
            description: "x".to_string(),
            tools: vec!["sirius-forester".to_string()],
        },
    );
    docs.profile = "only-sirius".to_string();
    let summary = h.run(&docs).unwrap();
    assert!(summary.ok);
    assert!(!summary.path_changed);
    assert!(h.path_value().contains(&h.layout.bin.display().to_string()));
    assert!(h.receipt("ametrite").is_some());
    assert!(h.receipt("sirius-forester").is_none());
}

#[test]
fn uninstall_without_receipts_is_a_harmless_no_op_and_leaves_foreign_files() {
    let mut h = Harness::new();
    h.on_path.insert(
        "amt".to_string(),
        PathBuf::from(r"C:\Users\t\.cargo\bin\amt.exe"),
    );
    fs::create_dir_all(&h.layout.bin).unwrap();
    fs::write(h.layout.bin.join("amt.exe"), "someone else's").unwrap();
    let summary = h.run(&opts(Operation::Uninstall, "core")).unwrap();
    assert!(summary.ok);
    assert_eq!(result(&summary, "ametrite"), "not-installed");
    assert!(item(&summary, "ametrite", "remove")
        .detail
        .contains("not Sothis-managed"));
    assert_eq!(h.bin_text("amt.exe"), "someone else's");
    assert_eq!(h.path_value(), r"C:\Windows;C:\Tools");
}

#[test]
fn uninstall_modify_path_can_be_disabled_and_plugins_are_opt_in() {
    let mut h = Harness::new();
    h.install("core");
    fs::write(
        h.plugin_dir.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"sirius@sothis":[{}],"hayvenhurst@sothis":[{}],"sothis@sothis":[{}]}}"#,
    )
    .unwrap();
    let mut options = opts(Operation::Uninstall, "core");
    options.modify_path = false;
    let summary = h.run(&options).unwrap();
    assert!(summary.ok);
    assert!(!summary.path_changed);
    assert!(h.path_value().contains(&h.layout.bin.display().to_string()));
    assert!(
        h.claude.calls.borrow().is_empty(),
        "plugins stay without --plugins"
    );

    let mut h2 = Harness::new();
    h2.install("core");
    fs::write(
        h2.plugin_dir.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"sirius@sothis":[{}],"hayvenhurst@sothis":[{}],"sothis@sothis":[{}]}}"#,
    )
    .unwrap();
    let mut with_plugins = opts(Operation::Uninstall, "core");
    with_plugins.plugins = true;
    let summary = h2.run(&with_plugins).unwrap();
    assert!(summary.ok, "{}", summary.render_human());
    let calls = h2.claude.calls.borrow().clone();
    assert_eq!(
        calls,
        [
            "plugin uninstall sirius@sothis",
            "plugin uninstall hayvenhurst@sothis",
            "plugin uninstall sothis@sothis"
        ]
    );
}

// --------------------------------------------------------------- housekeeping

#[test]
fn stale_staging_is_cleaned_and_interrupted_rollback_data_is_preserved_with_a_warning() {
    let mut h = Harness::new();
    let stale = h.layout.staging_dir("crashed");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("junk"), "x").unwrap();
    let rollback = h.layout.rollback_dir("crashed");
    fs::create_dir_all(&rollback).unwrap();
    fs::write(rollback.join("amt.exe"), "someone's original").unwrap();
    let summary = h.install("core");
    assert!(summary.ok);
    assert!(!stale.exists());
    assert_eq!(
        fs::read_to_string(rollback.join("amt.exe")).unwrap(),
        "someone's original"
    );
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.contains("interrupted run")));
}

#[test]
fn a_corrupt_receipt_aborts_instead_of_guessing_ownership() {
    let mut h = Harness::new();
    h.install("core");
    fs::write(h.layout.receipt_path("ametrite"), "{\"not\":\"a receipt\"}").unwrap();
    let error = h.run(&opts(Operation::Update, "core")).unwrap_err();
    assert!(error.contains("receipt for ametrite"), "{error}");
}

#[test]
fn json_summary_has_the_documented_shape() {
    let mut h = Harness::new();
    let summary = h.install("core");
    let value = serde_json::to_value(&summary).unwrap();
    let mut keys = value
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        [
            "items",
            "ok",
            "operation",
            "pathChanged",
            "platform",
            "profile",
            "warnings"
        ]
    );
    let first = value["items"][0].as_object().unwrap();
    let mut item_keys = first.keys().cloned().collect::<Vec<_>>();
    item_keys.sort();
    assert_eq!(item_keys, ["action", "detail", "result", "tool", "version"]);
}

#[test]
fn a_failed_first_install_leaves_no_tool_directory_behind() {
    let mut h = Harness::new();
    *h.probe.lie.borrow_mut() = Some(("hayven.exe".to_string(), "hayven 9.9.9".to_string()));
    let summary = h.install("core");
    assert!(!summary.ok);
    assert!(item(&summary, "hayvenhurst", "install")
        .detail
        .contains("reports version 9.9.9"));
    assert!(!h.layout.tool_dir("hayvenhurst").exists());
    assert!(h.receipt("hayvenhurst").is_none());
    assert_eq!(h.bin_names(), ["amt.exe", "sirius.exe"]);
    h.assert_no_scratch();
}
