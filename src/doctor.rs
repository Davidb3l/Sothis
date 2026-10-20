use crate::install::layout::{EnvVars, Layout};
use crate::install::legacy;
use crate::install::packaged::{self, PackageContext, RealPackageContext};
use crate::install::pathenv;
use crate::install::proc::{self, describe_spawn_error};
use crate::install::receipt;
use crate::manifest::{Manifest, Tool};
use crate::plan::{self, PlanAction};
use semver::Version;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    /// Something is off but the suite still works; never fails the doctor.
    Warn,
    Fail,
    Skip,
}

impl CheckStatus {
    /// The bracketed label of the human output.
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Pass => "OK",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skip => "SKIP",
        }
    }
}

#[derive(Debug)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, status: CheckStatus, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }

    fn pass(name: String, detail: String) -> Self {
        Check::new(name, CheckStatus::Pass, detail)
    }

    fn warn(name: String, detail: String) -> Self {
        Check::new(name, CheckStatus::Warn, detail)
    }

    fn fail(name: String, detail: String) -> Self {
        Check::new(name, CheckStatus::Fail, detail)
    }

    fn skip(name: String, detail: String) -> Self {
        Check::new(name, CheckStatus::Skip, detail)
    }

    /// A check counts as ok unless it explicitly failed; warnings and skips
    /// are not failures.
    pub fn ok(&self) -> bool {
        self.status != CheckStatus::Fail
    }
}

impl Serialize for Check {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Check", 4)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("ok", &self.ok())?;
        state.serialize_field("detail", &self.detail)?;
        state.end()
    }
}

/// Everything the doctor reads from the machine, injectable for tests.
#[derive(Debug, Clone)]
pub struct Env {
    /// The directories of PATH, in order.
    pub path_dirs: Vec<PathBuf>,
    pub cwd: PathBuf,
    /// The user's home directory (`HOME`, then `USERPROFILE`).
    pub home: Option<PathBuf>,
    /// Where Sothis keeps receipts; `None` when it cannot be resolved.
    pub layout: Option<Layout>,
    /// Claude Code's plugin state directory.
    pub plugins_dir: Option<PathBuf>,
    /// `%LOCALAPPDATA%` and `%APPDATA%`: what an MSIX container virtualizes.
    pub app_dirs: Vec<PathBuf>,
    /// The MSIX package full name when this process runs inside a package.
    pub package_name: Option<String>,
    /// The v0.1.0 root (`%LOCALAPPDATA%\Sothis`), Windows only.
    pub legacy_root: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Env {
        let home = ["HOME", "USERPROFILE"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .find(|path| path.is_absolute());
        let vars = EnvVars::from_process();
        let host = crate::platform::HostPlatform::detect().ok();
        Env {
            path_dirs: process_path_dirs(),
            cwd: std::env::current_dir().unwrap_or_default(),
            home,
            layout: host.and_then(|host| Layout::resolve_from(host, &vars).ok()),
            plugins_dir: claude_plugins_dir(),
            app_dirs: [vars.local_app_data.clone(), vars.app_data.clone()]
                .into_iter()
                .flatten()
                .collect(),
            package_name: RealPackageContext.package_full_name(),
            legacy_root: host.and_then(|host| legacy::legacy_root(host, &vars)),
        }
    }
}

fn process_path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default()
}

pub fn run(manifest: &Manifest, profile: &str, platform: &str) -> Result<Vec<Check>, String> {
    run_with(manifest, profile, platform, &Env::from_process())
}

pub fn run_with(
    manifest: &Manifest,
    profile: &str,
    platform: &str,
    env: &Env,
) -> Result<Vec<Check>, String> {
    let mut checks = Vec::new();
    let errors = crate::manifest::validate(manifest);
    checks.push(if errors.is_empty() {
        Check::pass(
            "manifest".to_string(),
            format!("schema v{} is valid", manifest.schema_version),
        )
    } else {
        Check::fail("manifest".to_string(), errors.join("; "))
    });
    if !errors.is_empty() {
        return Ok(checks);
    }

    let items = plan::build(manifest, profile, platform)?;
    let includes_sirius = items.iter().any(|item| {
        item.tool == "sirius-forester"
            && !matches!(
                item.action,
                PlanAction::SkipUnsupported | PlanAction::UseWsl2 | PlanAction::RemoveFromWsl2
            )
    });
    let sirius_dir = find_command_in("sirius", &env.path_dirs)
        .and_then(|path| path.parent().map(Path::to_path_buf));
    for item in items {
        let tool = manifest
            .tools
            .iter()
            .find(|tool| tool.id == item.tool)
            .expect("validated plan tool");
        match item.action {
            PlanAction::SkipUnsupported => checks.push(Check::skip(
                tool.id.clone(),
                item.note
                    .unwrap_or_else(|| format!("unsupported on {platform}")),
            )),
            PlanAction::UseWsl2 | PlanAction::RemoveFromWsl2 => checks.push(Check::skip(
                tool.id.clone(),
                item.note
                    .unwrap_or_else(|| format!("must be checked inside WSL2 on {platform}")),
            )),
            PlanAction::BuildFromSource if tool.command.is_none() => checks.push(Check::skip(
                tool.id.clone(),
                format!(
                    "source distribution with no CLI command; install and verify manually from https://github.com/{}",
                    tool.repository
                ),
            )),
            _ => {
                checks.push(check_tool(tool, env, platform, sirius_dir.as_deref()));
                if matches!(tool.id.as_str(), "ametrite" | "hayvenhurst") {
                    checks.push(check_tool_doctor(tool, env));
                }
                if tool.distribution.kind == "plugin-runtime" {
                    checks.push(check_plugin(tool, env));
                }
            }
        }
    }
    checks.extend(check_receipts(env));
    checks.extend(check_legacy_root(env));
    checks.extend(check_package_context(env));
    if includes_sirius {
        checks.push(check_sirius_contracts(env));
    }
    Ok(checks)
}

// ------------------------------------------------------------ delegated doctors

/// Run `<path> doctor --json` (optionally from `cwd`) and capture it.
fn run_doctor_json(path: &Path, cwd: Option<&Path>) -> std::io::Result<proc::Captured> {
    let mut command = Command::new(path);
    command.args(["doctor", "--json"]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    proc::run_capture(command, DOCTOR_TIMEOUT)
}

fn check_tool_doctor(tool: &Tool, env: &Env) -> Check {
    let check_name = format!("{}_health", tool.id);
    let Some(command) = tool.command.as_deref() else {
        return Check::fail(
            check_name,
            "manifest does not declare a command".to_string(),
        );
    };
    let Some(path) = find_command_in(command, &env.path_dirs) else {
        return Check::fail(check_name, format!("{command} is not on PATH"));
    };
    match run_doctor_json(&path, None) {
        Ok(output) => {
            let mut check = parse_delegated_doctor(
                &check_name,
                &tool.id,
                output.stdout.as_bytes(),
                output.stderr.as_bytes(),
            )
            .unwrap_or_else(|detail| Check::fail(check_name.clone(), detail));
            if !output.success {
                check.status = CheckStatus::Fail;
                check.detail = format!(
                    "{} doctor exited {:?}: {}",
                    tool.id, output.code, check.detail
                );
            }
            check
        }
        Err(error) => Check::fail(
            check_name,
            format!(
                "{} (running doctor --json)",
                describe_spawn_error(&path, &error)
            ),
        ),
    }
}

/// Parse the `doctor --json` output of a delegated suite tool.
///
/// The delegated tool's own top-level `ok` verdict is authoritative; a failing
/// sub-check only overrides an `ok: true` verdict when it is explicitly marked
/// `gating: true`. When the tool itself reports `ok: false`, the failing checks
/// named in the detail are filtered by `gating` defaulting to true, so checks
/// explicitly marked `gating: false` are never blamed.
fn parse_delegated_doctor(
    check_name: &str,
    tool_label: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Check, String> {
    let value: serde_json::Value = serde_json::from_slice(stdout).map_err(|error| {
        format!(
            "{tool_label} doctor did not return JSON: {error}; stderr={}",
            String::from_utf8_lossy(stderr).trim()
        )
    })?;
    let ok = value
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| format!("{tool_label} doctor JSON has no boolean ok field"))?;
    let failing = value
        .get("checks")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|check| {
            !check
                .get("ok")
                .or_else(|| check.get("pass"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|check| {
            let name = check.get("name").and_then(serde_json::Value::as_str)?;
            let gating = check.get("gating").and_then(serde_json::Value::as_bool);
            Some((name, gating))
        })
        .collect::<Vec<_>>();
    let explicitly_gating = failing
        .iter()
        .filter(|(_, gating)| *gating == Some(true))
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    let gating_by_default = failing
        .iter()
        .filter(|(_, gating)| gating.unwrap_or(true))
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    let advisory = failing
        .iter()
        .filter(|(_, gating)| *gating != Some(true))
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();

    let (status, detail) = if !ok {
        if gating_by_default.is_empty() {
            (
                CheckStatus::Fail,
                format!(
                    "{tool_label} reported a failed doctor result without naming a failed gating check"
                ),
            )
        } else {
            (
                CheckStatus::Fail,
                format!(
                    "failed {tool_label} doctor checks: {}",
                    gating_by_default.join(", ")
                ),
            )
        }
    } else if !explicitly_gating.is_empty() {
        (
            CheckStatus::Fail,
            format!(
                "failed {tool_label} doctor checks: {}",
                explicitly_gating.join(", ")
            ),
        )
    } else if !advisory.is_empty() {
        (
            CheckStatus::Pass,
            format!(
                "{tool_label} doctor passes (advisory: {})",
                advisory.join(", ")
            ),
        )
    } else {
        (CheckStatus::Pass, format!("{tool_label} doctor passes"))
    };
    Ok(Check {
        name: check_name.to_string(),
        status,
        detail,
    })
}

// ----------------------------------------------------------------- plugins

/// `$CLAUDE_PLUGINS_DIR`, else `~/.claude/plugins`.
pub(crate) fn claude_plugins_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_PLUGINS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .map(|home| home.join(".claude").join("plugins"))
        })
}

fn check_plugin(tool: &Tool, env: &Env) -> Check {
    let check_name = format!("{}_plugin", tool.id);
    let Some(plugin) = &tool.plugin else {
        return Check::fail(
            check_name,
            "plugin-runtime distribution has no plugin declaration".to_string(),
        );
    };
    let Some(state) = env
        .plugins_dir
        .as_ref()
        .map(|dir| dir.join("installed_plugins.json"))
    else {
        return Check::fail(
            check_name,
            "cannot locate the Claude Code plugin state directory".to_string(),
        );
    };
    let body = match std::fs::read_to_string(&state) {
        Ok(body) => body,
        Err(error) => {
            return Check::fail(
                check_name,
                format!("cannot read {}: {error}", state.display()),
            )
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => {
            return Check::fail(
                check_name,
                format!("{} is invalid JSON: {error}", state.display()),
            )
        }
    };
    let identifiers = plugin_identifiers(&value);
    let canonical = format!("{}@{}", plugin.name, plugin.marketplace);
    let prefix = format!("{}@", plugin.name);
    let presence = if identifiers.iter().any(|value| value == &canonical) {
        format!("{}@{} is installed", plugin.name, plugin.marketplace)
    } else if identifiers.iter().any(|value| value.starts_with(&prefix)) {
        format!(
            "{} is installed through a legacy/standalone marketplace; migrate to {}@{}",
            plugin.name, plugin.name, plugin.marketplace
        )
    } else {
        return Check::fail(
            check_name,
            format!("{}@{} is not installed", plugin.name, plugin.marketplace),
        );
    };

    let Some(minimum) = plugin.minimum_version.as_deref() else {
        return Check::pass(check_name, presence);
    };
    let Ok(minimum_version) = Version::parse(minimum) else {
        return Check::fail(
            check_name,
            format!("the manifest's plugin minimumVersion {minimum:?} is not semver"),
        );
    };
    // The highest installed version across every `<name>@<marketplace>` id and
    // scope (user, project, ...).
    let best = installed_plugin_versions(&value, &plugin.name)
        .into_iter()
        .filter_map(|(id, version)| version.map(|version| (id, version)))
        .max_by(|a, b| a.1.cmp(&b.1));
    match best {
        Some((id, version)) if version < minimum_version => Check::fail(
            check_name,
            format!(
                "{id} {version} is below the minimum {minimum} required by the suite manifest; update the plugin: claude plugin update {id}"
            ),
        ),
        Some((_, version)) => Check::pass(
            check_name,
            format!("{presence} (version {version}, requires >= {minimum})"),
        ),
        None => Check::warn(
            check_name,
            format!(
                "{presence}, but its version could not be read from {} (requires >= {minimum}); run: claude plugin update {canonical}",
                state.display()
            ),
        ),
    }
}

/// Every `<name>@<marketplace>` id in Claude's `installed_plugins.json`
/// (`{"plugins": {"<id>": [{"scope", "version", ...}, ...]}}`) with the
/// highest parseable version among its installs (user, project, ...).
pub(crate) fn installed_plugin_versions(
    state: &serde_json::Value,
    name: &str,
) -> Vec<(String, Option<Version>)> {
    let table = state
        .get("plugins")
        .and_then(serde_json::Value::as_object)
        .or_else(|| state.as_object());
    let prefix = format!("{name}@");
    let mut found = Vec::new();
    for (id, installs) in table.into_iter().flatten() {
        if !id.starts_with(&prefix) {
            continue;
        }
        let version = installs
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|install| install.get("version")?.as_str())
            .filter_map(|text| Version::parse(text.trim().trim_start_matches('v')).ok())
            .max();
        found.push((id.clone(), version));
    }
    found
}

pub(crate) fn plugin_identifiers(value: &serde_json::Value) -> Vec<String> {
    fn visit(value: &serde_json::Value, identifiers: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(object) => {
                for (key, value) in object {
                    if key.contains('@') {
                        identifiers.push(key.clone());
                    }
                    visit(value, identifiers);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, identifiers);
                }
            }
            serde_json::Value::String(value) if value.contains('@') => {
                identifiers.push(value.clone())
            }
            _ => {}
        }
    }

    let mut identifiers = Vec::new();
    visit(value, &mut identifiers);
    identifiers
}

// ----------------------------------------------------------- sirius contracts

/// The Sothis workspace containing `cwd`: the nearest ancestor with a
/// `.sirius/` directory. The walk never looks at or above the user's home
/// directory when `cwd` is inside it (a stray `~/.sirius` or `/.sirius` must
/// not turn the whole machine into a workspace).
pub(crate) fn find_workspace_root(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let inside_home =
        home.is_some_and(|home| cwd.ancestors().any(|dir| pathenv::same_dir(dir, home)));
    for dir in cwd.ancestors() {
        if inside_home && home.is_some_and(|home| pathenv::same_dir(dir, home)) {
            return None;
        }
        if dir.join(".sirius").is_dir() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// `sirius doctor --json` is workspace-bound (it checks `.ametrite/`,
/// `.hayven/` and the gate config of the repo it runs in), so outside a
/// workspace it would report `ok: false` for reasons unrelated to the install.
/// Run it from the workspace root, or skip it.
fn check_sirius_contracts(env: &Env) -> Check {
    const NAME: &str = "suite_contracts";
    let Some(root) = find_workspace_root(&env.cwd, env.home.as_deref()) else {
        return Check::skip(
            NAME.to_string(),
            format!(
                "not inside a Sothis workspace (no .sirius/ found from {}); run sothis doctor from a repo initialized with sirius init",
                env.cwd.display()
            ),
        );
    };
    let Some(path) = find_command_in("sirius", &env.path_dirs) else {
        return Check::fail(
            NAME.to_string(),
            "sirius is not on PATH, so integration contracts cannot be checked".to_string(),
        );
    };
    match run_doctor_json(&path, Some(&root)) {
        Ok(output) => {
            match parse_delegated_doctor(
                NAME,
                "sirius",
                output.stdout.as_bytes(),
                output.stderr.as_bytes(),
            ) {
                Ok(mut check) => {
                    if !output.success {
                        check.status = CheckStatus::Fail;
                        check.detail =
                            format!("sirius doctor exited {:?}: {}", output.code, check.detail);
                    }
                    check.detail = format!("{} (workspace {})", check.detail, root.display());
                    check
                }
                Err(detail) => Check::fail(NAME.to_string(), detail),
            }
        }
        Err(error) => Check::fail(
            NAME.to_string(),
            format!(
                "{} (running doctor --json)",
                describe_spawn_error(&path, &error)
            ),
        ),
    }
}

// -------------------------------------------------------------------- receipts

/// Verify Sothis's own bookkeeping: every file a receipt owns must still hash
/// to what was installed, and the bin directory should be on PATH. Advisory
/// (warn): drift is the user's call, `sothis install` repairs it. No receipts
/// means Sothis manages nothing here, so there is nothing to report.
fn check_receipts(env: &Env) -> Option<Check> {
    const NAME: &str = "sothis_receipts";
    let layout = env.layout.as_ref()?;
    let receipts = match receipt::list(layout) {
        Ok(receipts) => receipts,
        Err(error) => {
            return Some(Check::warn(
                NAME.to_string(),
                format!("Sothis receipts could not be read: {error}"),
            ))
        }
    };
    if receipts.is_empty() {
        return None;
    }
    let mut problems = Vec::new();
    for receipt in &receipts {
        let drift = receipt::drift(receipt);
        if !drift.is_empty() {
            problems.push(format!(
                "{} {} drifted from its receipt ({}); run `sothis install` to repair it",
                receipt.tool,
                receipt.version,
                drift.join("; ")
            ));
        }
    }
    let mut bins: Vec<&Path> = Vec::new();
    for receipt in &receipts {
        if !bins
            .iter()
            .any(|bin| pathenv::same_dir(bin, &receipt.bin_dir))
        {
            bins.push(&receipt.bin_dir);
        }
    }
    let missing_from_path = bins
        .iter()
        .filter(|bin| !pathenv::dir_on_path(bin, &env.path_dirs))
        .map(|bin| bin.display().to_string())
        .collect::<Vec<_>>();
    if !missing_from_path.is_empty() {
        problems.push(format!(
            "the Sothis bin directory {} is not on PATH of this process (new terminals pick up `sothis install` PATH changes; with --no-modify-path add it yourself)",
            missing_from_path.join(", ")
        ));
    }
    let managed = receipts
        .iter()
        .map(|receipt| format!("{} {}", receipt.tool, receipt.version))
        .collect::<Vec<_>>()
        .join(", ");
    Some(if problems.is_empty() {
        Check::pass(
            NAME.to_string(),
            format!(
                "{} Sothis-managed tool(s) match their receipts ({managed}); {} on PATH",
                receipts.len(),
                bins.iter()
                    .map(|bin| bin.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    } else {
        Check::warn(NAME.to_string(), problems.join(" | "))
    })
}

/// A v0.1.0 install at the old `%LOCALAPPDATA%\Sothis` root. Advisory: it is
/// never adopted or deleted, but it keeps copies of the tools that nothing on
/// PATH reaches any more.
fn check_legacy_root(env: &Env) -> Option<Check> {
    let (legacy_root, layout) = (env.legacy_root.as_ref()?, env.layout.as_ref()?);
    if pathenv::same_dir(legacy_root, &layout.root) || !legacy::receipts_present(legacy_root) {
        return None;
    }
    Some(Check::warn(
        "legacy_root".to_string(),
        legacy::note(legacy_root, &layout.root),
    ))
}

/// A process known to be packaged, for [`packaged::virtualized_root_problem`].
struct KnownPackage<'a>(&'a str);

impl PackageContext for KnownPackage<'_> {
    fn package_full_name(&self) -> Option<String> {
        Some(self.0.to_string())
    }
}

/// MSIX awareness: say when this process is packaged, and fail when the
/// effective root would be silently virtualized (new writes under AppData go
/// to the package's private folder while the registry PATH edit does not).
fn check_package_context(env: &Env) -> Vec<Check> {
    let Some(name) = &env.package_name else {
        return Vec::new();
    };
    let mut checks = vec![Check::pass(
        "msix_package".to_string(),
        format!(
            "this process runs inside the Windows app package {name}; writes under %LOCALAPPDATA% and %APPDATA% are redirected into the package's private folder"
        ),
    )];
    if let Some(layout) = &env.layout {
        let package = KnownPackage(name);
        let problem = [&layout.root, &layout.bin]
            .into_iter()
            .find_map(|dir| packaged::virtualized_root_problem(dir, &env.app_dirs, &package));
        checks.push(match problem {
            Some(reason) => Check::fail("root_not_virtualized".to_string(), reason),
            None => Check::pass(
                "root_not_virtualized".to_string(),
                format!(
                    "the Sothis root {} is outside %LOCALAPPDATA% and %APPDATA%, so it is not virtualized",
                    layout.root.display()
                ),
            ),
        });
    }
    checks
}

// ----------------------------------------------------------------------- tools

/// What a tool's `--version` must satisfy: its own `minimumVersion`, or for a
/// plugin-runtime tool whose command is its runtime (Catryna / bun), the
/// distribution's `runtimeMinimumVersion`.
fn required_minimum(tool: &Tool) -> Option<&str> {
    tool.minimum_version.as_deref().or_else(|| {
        let runtime = tool.distribution.runtime.as_deref()?;
        (tool.distribution.kind == "plugin-runtime" && tool.command.as_deref() == Some(runtime))
            .then_some(tool.distribution.runtime_minimum_version.as_deref())
            .flatten()
    })
}

/// `<path> --version` (stdout, else stderr), shell-free with a timeout.
fn probe_version(path: &Path) -> Result<String, String> {
    let mut command = Command::new(path);
    command.arg("--version");
    let output = proc::run_capture(command, VERSION_TIMEOUT)
        .map_err(|error| describe_spawn_error(path, &error))?;
    if !output.success {
        return Err(format!(
            "{} --version exited {:?}",
            path.display(),
            output.code
        ));
    }
    Ok(if output.stdout.trim().is_empty() {
        output.stderr.trim().to_string()
    } else {
        output.stdout.trim().to_string()
    })
}

/// One copy of a command found on PATH.
struct PathCopy {
    path: PathBuf,
    /// The raw `--version` output, or why it could not be read.
    output: Result<String, String>,
}

impl PathCopy {
    fn version(&self) -> Option<Version> {
        self.output
            .as_ref()
            .ok()
            .and_then(|text| crate::version::extract(text))
    }

    fn label(&self) -> String {
        match &self.output {
            Ok(text) => match self.version() {
                Some(version) => version.to_string(),
                None => format!(
                    "unreadable version {:?}",
                    text.lines().next().unwrap_or_default()
                ),
            },
            Err(error) => format!("not runnable: {error}"),
        }
    }

    fn meets(&self, minimum: &str) -> bool {
        self.output
            .as_ref()
            .ok()
            .and_then(|text| crate::version::meets_minimum(text, minimum))
            .unwrap_or(false)
    }
}

/// Distinct files (by canonical path) a shell would resolve for `command`, in
/// PATH order. The same file reached through two PATH spellings is one copy.
fn distinct_copies(command: &str, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = Vec::<PathBuf>::new();
    let mut copies = Vec::new();
    for path in find_all_commands_in(command, dirs) {
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.iter().any(|earlier| earlier == &canonical) {
            continue;
        }
        seen.push(canonical);
        copies.push(path);
    }
    copies
}

fn check_tool(tool: &Tool, env: &Env, platform: &str, sirius_dir: Option<&Path>) -> Check {
    let Some(command) = tool.command.as_deref() else {
        return Check::fail(
            tool.id.clone(),
            "manifest does not declare a command".to_string(),
        );
    };
    let paths = distinct_copies(command, &env.path_dirs);
    let Some(path) = paths.first().cloned() else {
        return Check::fail(tool.id.clone(), format!("{command} is not on PATH"));
    };
    let copies = paths
        .into_iter()
        .map(|path| PathCopy {
            output: probe_version(&path),
            path,
        })
        .collect::<Vec<_>>();
    let effective = &copies[0];
    let required = required_minimum(tool);
    let requirement = required
        .map(|minimum| format!(", requires >= {minimum}"))
        .unwrap_or_default();

    let output = match &effective.output {
        Ok(output) => output,
        Err(error) => return Check::fail(tool.id.clone(), error.clone()),
    };
    let shown = output.lines().next().unwrap_or_default().trim();
    let version_result = required.map(|minimum| evaluate_version(output, minimum));
    let (version_ok, parse_error) = match version_result {
        Some(Ok(ok)) => (ok, None),
        Some(Err(error)) => (false, Some(error)),
        None => (true, None),
    };
    let primary_detail = parse_error
        .clone()
        .unwrap_or_else(|| format!("{} ({}{requirement})", path.display(), shown));
    if copies.len() == 1 {
        return Check::new(
            tool.id.clone(),
            if version_ok {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            primary_detail,
        );
    }

    // More than one distinct copy is on PATH.
    if !version_ok && parse_error.is_none() {
        let later_good = required.and_then(|minimum| {
            let good = copies[1..].iter().find(|copy| copy.meets(minimum))?;
            Some((minimum, good))
        });
        if let Some((minimum, good)) = later_good {
            return Check::fail(
                tool.id.clone(),
                format!(
                    "earlier PATH entry {} ({}) shadows {} ({}); remove the stale copy (requires >= {minimum}){}",
                    effective.path.display(),
                    effective.label(),
                    good.path.display(),
                    good.label(),
                    sirius_peer_note(tool, &copies, sirius_dir, platform)
                ),
            );
        }
    }
    let listing = copies
        .iter()
        .map(|copy| format!("{} -> {}", copy.path.display(), copy.label()))
        .collect::<Vec<_>>()
        .join("; ");
    if !version_ok {
        return Check::fail(
            tool.id.clone(),
            format!(
                "{primary_detail}; {} copies of {command} are on PATH: {listing}",
                copies.len()
            ),
        );
    }
    Check::warn(
        tool.id.clone(),
        format!(
            "{primary_detail}; {} copies of {command} are on PATH and the first one wins: {listing}; remove the stale copies so every shell and tool resolves the same one{}",
            copies.len(),
            sirius_peer_note(tool, &copies, sirius_dir, platform)
        ),
    )
}

/// Sirius (before its next release) resolves its peers from its own directory
/// first on Windows, so a stale peer sitting beside `sirius.exe` is the one it
/// runs no matter what PATH says.
fn sirius_peer_note(
    tool: &Tool,
    copies: &[PathCopy],
    sirius_dir: Option<&Path>,
    platform: &str,
) -> String {
    let Some(sirius_dir) = sirius_dir.filter(|_| platform.starts_with("windows")) else {
        return String::new();
    };
    if !matches!(tool.id.as_str(), "ametrite" | "hayvenhurst") {
        return String::new();
    }
    let newest = copies.iter().filter_map(PathCopy::version).max();
    let stale_beside_sirius = copies.iter().any(|copy| {
        copy.path
            .parent()
            .is_some_and(|dir| pathenv::same_dir(dir, sirius_dir))
            && match (copy.version(), &newest) {
                (Some(version), Some(newest)) => &version < newest,
                (None, _) => true,
                _ => false,
            }
    });
    if stale_beside_sirius {
        ". Note: on Windows sirius resolves its peer tools from its own directory first (fixed in the next sirius release), so the stale copy next to sirius.exe is the one sirius runs regardless of PATH order".to_string()
    } else {
        String::new()
    }
}

fn evaluate_version(actual: &str, minimum: &str) -> Result<bool, String> {
    crate::version::meets_minimum(actual, minimum)
        .ok_or_else(|| format!("could not parse semantic version from {actual:?}"))
}

// ------------------------------------------------------------ command lookup

/// First match of `command` on the process PATH.
pub fn find_command(command: &str) -> Option<PathBuf> {
    find_command_in(command, &process_path_dirs())
}

/// First match of `command` in `dirs`. A `command` containing a path
/// separator is checked directly instead.
pub fn find_command_in(command: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let candidate = Path::new(command);
    if candidate.components().count() > 1 && is_executable_file(candidate) {
        return Some(candidate.to_path_buf());
    }
    find_all_commands_in(command, dirs).into_iter().next()
}

/// What each PATH directory would resolve for `command` (one per directory,
/// in PATH order).
pub fn find_all_commands_in(command: &str, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let names = executable_extensions()
        .iter()
        .map(|extension| format!("{command}{extension}"))
        .collect::<Vec<_>>();
    pathenv::find_in_dirs(&names, dirs, &is_executable_file)
}

fn executable_extensions() -> Vec<String> {
    executable_extensions_for(cfg!(windows), std::env::var_os("PATHEXT"))
}

/// The suffixes a shell tries after `command`, in PATHEXT order.
///
/// Windows: `.exe`, `.com`, `.cmd` and `.bat` (std spawns `.cmd`/`.bat` safely
/// with cmd.exe argument escaping since Rust 1.77), never the empty suffix: an
/// extensionless file is not runnable on Windows, so probing for one only ever
/// finds POSIX shell shims. Elsewhere the name is used as given.
pub(crate) fn executable_extensions_for(windows: bool, pathext: Option<OsString>) -> Vec<String> {
    if !windows {
        return vec![String::new()];
    }
    const SUPPORTED: [&str; 4] = [".exe", ".com", ".cmd", ".bat"];
    let raw = pathext
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut extensions: Vec<String> = Vec::new();
    for part in raw.split(';') {
        let lower = part.trim().to_ascii_lowercase();
        if SUPPORTED.contains(&lower.as_str()) && !extensions.contains(&lower) {
            extensions.push(lower);
        }
    }
    if extensions.is_empty() {
        extensions = SUPPORTED.iter().map(|ext| ext.to_string()).collect();
    }
    extensions
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| {
                ["exe", "com", "cmd", "bat"]
                    .iter()
                    .any(|supported| extension.eq_ignore_ascii_case(supported))
            })
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fsx::testutil::TempDir;
    use std::fs;

    #[test]
    fn sirius_contract_json_fails_when_a_gating_check_fails() {
        let body = br#"{
            "ok": false,
            "checks": [
                {"name":"amt_present_and_schema","gating":true,"pass":false},
                {"name":"plugin_handoff","gating":false,"pass":false}
            ]
        }"#;
        let check = parse_delegated_doctor("suite_contracts", "sirius", body, b"").unwrap();
        assert!(!check.ok());
        assert!(check.detail.contains("amt_present_and_schema"));
        assert!(!check.detail.contains("plugin_handoff"));
    }

    #[test]
    fn sirius_contract_json_requires_a_boolean_ok() {
        let error = parse_delegated_doctor("suite_contracts", "sirius", br#"{"checks":[]}"#, b"")
            .unwrap_err();
        assert!(error.contains("boolean ok"));
    }

    #[test]
    fn sirius_contract_json_never_describes_a_false_result_as_passing() {
        let check = parse_delegated_doctor(
            "suite_contracts",
            "sirius",
            br#"{"ok":false,"checks":[]}"#,
            b"",
        )
        .unwrap();
        assert!(!check.ok());
        assert!(check.detail.contains("failed doctor result"));
    }

    #[test]
    fn malformed_installed_versions_fail_closed() {
        assert!(evaluate_version("unknown development build", "1.2.3").is_err());
        assert!(!evaluate_version("tool 1.2.2", "1.2.3").unwrap());
        assert!(evaluate_version("tool 1.2.3", "1.2.3").unwrap());
    }

    #[test]
    fn plugin_detection_uses_json_keys_and_values_not_substrings() {
        let state = serde_json::json!({
            "plugins": {
                "catryna@sothis": [{"scope": "user"}],
                "note": "not-catryna@sothis-extra"
            }
        });
        let identifiers = plugin_identifiers(&state);
        assert!(identifiers.iter().any(|value| value == "catryna@sothis"));
        assert!(!identifiers.iter().any(|value| value == "not-catryna"));
    }

    #[test]
    fn delegated_tool_doctor_surfaces_named_failures() {
        let check = parse_delegated_doctor(
            "hayvenhurst_health",
            "hayvenhurst",
            br#"{"ok":false,"checks":[{"name":"index_integrity","ok":false}]}"#,
            b"",
        )
        .unwrap();
        assert!(!check.ok());
        assert!(check.detail.contains("index_integrity"));
    }

    #[test]
    fn a_tools_own_ok_verdict_survives_a_non_gating_failing_check() {
        let check = parse_delegated_doctor(
            "hayvenhurst_health",
            "hayvenhurst",
            br#"{"ok":true,"checks":[
                {"name":"bun_version","ok":true,"gating":true},
                {"name":"tier3_model","ok":false},
                {"name":"index_integrity","ok":false,"gating":false}
            ]}"#,
            b"",
        )
        .unwrap();
        assert!(check.ok());
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("advisory"));
        assert!(check.detail.contains("tier3_model"));
        assert!(check.detail.contains("index_integrity"));
        assert!(!check.detail.contains("bun_version"));
    }

    #[test]
    fn an_explicitly_gating_failing_check_overrides_a_tools_ok_verdict() {
        let check = parse_delegated_doctor(
            "hayvenhurst_health",
            "hayvenhurst",
            br#"{"ok":true,"checks":[
                {"name":"native_check_ignored","ok":false,"gating":true},
                {"name":"tier3_model","ok":false}
            ]}"#,
            b"",
        )
        .unwrap();
        assert!(!check.ok());
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("native_check_ignored"));
        assert!(!check.detail.contains("tier3_model"));
    }

    #[test]
    fn a_clean_delegated_doctor_passes_without_advisories() {
        let check = parse_delegated_doctor(
            "ametrite_health",
            "ametrite",
            br#"{"ok":true,"checks":[{"name":"board_schema","ok":true}]}"#,
            b"",
        )
        .unwrap();
        assert!(check.ok());
        assert_eq!(check.detail, "ametrite doctor passes");
    }

    #[test]
    fn checks_serialize_with_status_and_derived_ok() {
        let skip = Check::skip("pingmybell".to_string(), "unsupported here".to_string());
        let value = serde_json::to_value(&skip).unwrap();
        assert_eq!(value["status"], "skip");
        assert_eq!(value["ok"], true);
        let fail = Check::fail("guignet".to_string(), "broken".to_string());
        let value = serde_json::to_value(&fail).unwrap();
        assert_eq!(value["status"], "fail");
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn bell_profile_on_windows_is_all_skips_and_passes() {
        let manifest = crate::manifest::embedded().unwrap();
        let dir = TempDir::new("doctor-skips");
        let checks = run_with(&manifest, "bell", "windows-x64", &env_in(&dir)).unwrap();
        assert!(checks.iter().all(Check::ok));
        let bell = checks
            .iter()
            .find(|check| check.name == "pingmybell")
            .unwrap();
        assert_eq!(bell.status, CheckStatus::Skip);
        assert!(!checks.iter().any(|check| check.name == "suite_contracts"));
    }

    #[test]
    fn source_only_tools_without_a_command_skip_with_an_honest_detail() {
        let manifest = crate::manifest::embedded().unwrap();
        let dir = TempDir::new("doctor-skips");
        let checks = run_with(&manifest, "bell", "macos-x64", &env_in(&dir)).unwrap();
        assert!(checks.iter().all(Check::ok));
        let bell = checks
            .iter()
            .find(|check| check.name == "pingmybell")
            .unwrap();
        assert_eq!(bell.status, CheckStatus::Skip);
        assert!(bell.detail.contains("install and verify manually"));
        assert!(bell.detail.contains("Davidb3l/pingmybell"));
    }

    #[test]
    fn wsl2_bridged_tools_are_skips_not_failures() {
        let manifest = crate::manifest::embedded().unwrap();
        let dir = TempDir::new("doctor-skips");
        let checks = run_with(&manifest, "eval", "windows-x64", &env_in(&dir)).unwrap();
        assert!(checks.iter().all(Check::ok));
        let guignet = checks.iter().find(|check| check.name == "guignet").unwrap();
        assert_eq!(guignet.status, CheckStatus::Skip);
        assert!(guignet.detail.contains("WSL2"));
    }

    #[test]
    fn full_profile_is_passable_where_every_tool_is_bridged_or_unsupported() {
        let manifest = crate::manifest::embedded().unwrap();
        let dir = TempDir::new("doctor-skips");
        let checks = run_with(&manifest, "full", "windows-arm64", &env_in(&dir)).unwrap();
        assert!(checks.iter().all(Check::ok));
        assert!(checks
            .iter()
            .filter(|check| check.name != "manifest")
            .all(|check| check.status == CheckStatus::Skip));
        assert!(!checks.iter().any(|check| check.name == "suite_contracts"));
    }

    // ---------------------------------------------------------------- helpers

    fn env_in(dir: &TempDir) -> Env {
        Env {
            path_dirs: Vec::new(),
            cwd: dir.path().to_path_buf(),
            home: None,
            layout: None,
            plugins_dir: None,
            app_dirs: Vec::new(),
            package_name: None,
            legacy_root: None,
        }
    }

    fn abs_path(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", path.replace('/', r"\")))
        } else {
            PathBuf::from(format!("/{path}"))
        }
    }

    fn packaged_env(dir: &TempDir, root: PathBuf) -> Env {
        let mut env = env_in(dir);
        env.package_name = Some("Claude_1.0.0.0_x64__abc".to_string());
        env.app_dirs = vec![
            abs_path("Users/t/AppData/Local"),
            abs_path("Users/t/AppData/Roaming"),
        ];
        env.layout = Some(Layout {
            bin: root.join("bin"),
            root,
        });
        env
    }

    #[test]
    fn nothing_is_reported_about_packages_when_not_packaged() {
        let dir = TempDir::new("doctor-unpackaged");
        let mut env = env_in(&dir);
        env.app_dirs = vec![abs_path("Users/t/AppData/Local")];
        env.layout = Some(Layout {
            root: abs_path("Users/t/AppData/Local/Sothis"),
            bin: abs_path("Users/t/AppData/Local/Sothis/bin"),
        });
        assert!(check_package_context(&env).is_empty());
    }

    #[test]
    fn a_packaged_process_with_an_appdata_root_fails_root_not_virtualized() {
        let dir = TempDir::new("doctor-virtualized");
        let env = packaged_env(&dir, abs_path("Users/t/AppData/Local/Sothis"));
        let checks = check_package_context(&env);
        let info = checks.iter().find(|c| c.name == "msix_package").unwrap();
        assert_eq!(info.status, CheckStatus::Pass);
        assert!(info.detail.contains("Claude_1.0.0.0_x64__abc"));
        let check = checks
            .iter()
            .find(|c| c.name == "root_not_virtualized")
            .unwrap();
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(
            check.detail.contains("unset SOTHIS_HOME"),
            "{}",
            check.detail
        );
        assert!(!check.ok());
    }

    #[test]
    fn a_sothis_home_override_under_appdata_also_fails_while_packaged() {
        let dir = TempDir::new("doctor-virtualized-home");
        let env = packaged_env(&dir, abs_path("Users/t/AppData/Roaming/trial"));
        let checks = check_package_context(&env);
        assert!(checks
            .iter()
            .any(|c| c.name == "root_not_virtualized" && c.status == CheckStatus::Fail));
    }

    #[test]
    fn a_packaged_process_with_the_profile_root_passes() {
        let dir = TempDir::new("doctor-profile-root");
        let env = packaged_env(&dir, abs_path("Users/t/.sothis"));
        let checks = check_package_context(&env);
        let check = checks
            .iter()
            .find(|c| c.name == "root_not_virtualized")
            .unwrap();
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
    }

    #[test]
    fn legacy_receipts_warn_without_touching_anything() {
        let dir = TempDir::new("doctor-legacy");
        let legacy_root = dir.path().join("local").join("Sothis");
        let mut env = env_in(&dir);
        env.legacy_root = Some(legacy_root.clone());
        env.layout = Some(Layout {
            root: dir.path().join(".sothis"),
            bin: dir.path().join(".sothis").join("bin"),
        });
        assert!(check_legacy_root(&env).is_none(), "no legacy install yet");
        let receipts = legacy_root.join("state").join("receipts");
        fs::create_dir_all(&receipts).unwrap();
        assert!(
            check_legacy_root(&env).is_none(),
            "an empty directory is nothing"
        );
        fs::write(receipts.join("amt.json"), "{}").unwrap();
        let check = check_legacy_root(&env).unwrap();
        assert_eq!(check.name, "legacy_root");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.ok(), "a warning never fails the doctor");
        assert!(check.detail.contains("sothis uninstall --profile"));
        assert!(receipts.join("amt.json").is_file(), "nothing is deleted");
        // The legacy root being the effective root (SOTHIS_HOME) is not legacy.
        env.layout = Some(Layout {
            root: legacy_root.clone(),
            bin: legacy_root.join("bin"),
        });
        assert!(check_legacy_root(&env).is_none());
    }

    /// A runnable fake command: a `.cmd` on Windows (which exercises the
    /// SOT-24 extension handling), an executable `sh` script elsewhere.
    fn write_tool(dir: &Path, name: &str, unix_body: &str, cmd_body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        if cfg!(windows) {
            let path = dir.join(format!("{name}.cmd"));
            fs::write(&path, format!("@echo off\r\n{cmd_body}\r\n")).unwrap();
            path
        } else {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\n{unix_body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    fn version_tool(dir: &Path, name: &str, version_text: &str) -> PathBuf {
        write_tool(
            dir,
            name,
            &format!("echo '{version_text}'"),
            &format!("echo {version_text}"),
        )
    }

    fn manifest_tool(id: &str) -> Tool {
        crate::manifest::embedded()
            .unwrap()
            .tools
            .into_iter()
            .find(|tool| tool.id == id)
            .unwrap()
    }

    fn write_plugin_state(dir: &Path, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("installed_plugins.json"), body).unwrap();
    }

    fn plugin_state(entries: &[(&str, &[(&str, &str)])]) -> String {
        let plugins = entries
            .iter()
            .map(|(id, installs)| {
                let installs = installs
                    .iter()
                    .map(|(scope, version)| {
                        format!(r#"{{"scope":"{scope}","version":"{version}"}}"#)
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!(r#""{id}":[{installs}]"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"version":2,"plugins":{{{plugins}}}}}"#)
    }

    // ---------------------------------------------------------- status/output

    #[test]
    fn warn_is_additive_and_never_fails_the_doctor() {
        let warn = Check::warn("amt".to_string(), "two copies".to_string());
        assert!(warn.ok());
        let value = serde_json::to_value(&warn).unwrap();
        assert_eq!(value["status"], "warn");
        assert_eq!(value["ok"], true);
        let fail = Check::fail("amt".to_string(), "no".to_string());
        assert!(!fail.ok());
        let all = [warn, Check::pass("x".into(), "y".into())];
        assert!(all.iter().all(Check::ok));
    }

    #[test]
    fn human_labels_include_warn() {
        assert_eq!(CheckStatus::Pass.label(), "OK");
        assert_eq!(CheckStatus::Warn.label(), "WARN");
        assert_eq!(CheckStatus::Fail.label(), "FAIL");
        assert_eq!(CheckStatus::Skip.label(), "SKIP");
    }

    // ------------------------------------------------- SOT-24 command lookup

    #[test]
    fn windows_extensions_include_cmd_and_bat_and_never_the_empty_probe() {
        let default = executable_extensions_for(true, None);
        assert_eq!(default, [".com", ".exe", ".bat", ".cmd"]);
        assert!(!default.iter().any(String::is_empty));
        // PATHEXT order wins; unsupported suffixes are dropped; case folds.
        let custom = executable_extensions_for(true, Some(".VBS;.CMD;.EXE;.cmd;.JS".into()));
        assert_eq!(custom, [".cmd", ".exe"]);
        // Nothing usable in PATHEXT still resolves the supported set.
        assert_eq!(
            executable_extensions_for(true, Some(".VBS".into())),
            [".exe", ".com", ".cmd", ".bat"]
        );
        assert_eq!(executable_extensions_for(true, Some("".into())), default);
        // Unix is unchanged: the bare name only.
        assert_eq!(executable_extensions_for(false, Some(".CMD".into())), [""]);
    }

    #[test]
    fn find_command_in_resolves_scripts_the_platform_can_run() {
        let dir = TempDir::new("doctor-find");
        let tools = dir.path().join("bin");
        let script = version_tool(&tools, "fake-sothis-tool", "1.0.0");
        let found = find_command_in("fake-sothis-tool", std::slice::from_ref(&tools)).unwrap();
        assert_eq!(found, script);
        assert!(find_command_in("not-a-sothis-tool", std::slice::from_ref(&tools)).is_none());
        // An explicit path is honored.
        assert_eq!(
            find_command_in(script.to_str().unwrap(), &[]),
            Some(script.clone())
        );
        // An extensionless shim is not runnable on Windows and must not be
        // mistaken for a command there.
        fs::write(tools.join("only-a-shim"), "#!/bin/sh\n").unwrap();
        #[cfg(windows)]
        assert!(find_command_in("only-a-shim", &[tools]).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_runs_cmd_scripts_found_on_path() {
        let dir = TempDir::new("doctor-cmd");
        let tools = dir.path().join("bin");
        version_tool(&tools, "fake-cmd-tool", "7.8.9");
        let found = find_command_in("fake-cmd-tool", &[tools]).unwrap();
        assert!(found.to_string_lossy().ends_with(".cmd"));
        assert_eq!(probe_version(&found).unwrap(), "7.8.9");
    }

    #[test]
    fn every_distinct_directory_contributes_one_copy() {
        let dir = TempDir::new("doctor-all");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        version_tool(&a, "fake-multi", "1.0.0");
        version_tool(&b, "fake-multi", "2.0.0");
        let dirs = vec![a.clone(), b.clone(), a.clone()];
        let all = find_all_commands_in("fake-multi", &dirs);
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all[0].starts_with(&a) && all[1].starts_with(&b));
        assert_eq!(distinct_copies("fake-multi", &dirs).len(), 2);
    }

    // ------------------------------------------------------ workspace-bound sirius

    #[test]
    fn the_workspace_root_is_the_nearest_ancestor_with_a_sirius_directory() {
        // Bounded by `dir` as home so a stray .sirius above the temp dir cannot
        // change the outcome on any machine.
        let dir = TempDir::new("doctor-workspace");
        let repo = dir.path().join("repo");
        let deep = repo.join("src").join("deep");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_workspace_root(&deep, Some(dir.path())), None);
        fs::create_dir_all(repo.join(".sirius")).unwrap();
        assert_eq!(
            find_workspace_root(&deep, Some(dir.path())),
            Some(repo.clone())
        );
        assert_eq!(
            find_workspace_root(&repo, Some(dir.path())),
            Some(repo.clone())
        );
        // A nested workspace wins over the outer one.
        fs::create_dir_all(repo.join("src").join(".sirius")).unwrap();
        assert_eq!(
            find_workspace_root(&deep, Some(dir.path())),
            Some(repo.join("src"))
        );
        // A `.sirius` that is a file is not a workspace.
        let loose = dir.path().join("loose");
        fs::create_dir_all(&loose).unwrap();
        fs::write(loose.join(".sirius"), "x").unwrap();
        assert_eq!(find_workspace_root(&loose, Some(dir.path())), None);
    }

    #[test]
    fn the_workspace_walk_never_looks_at_or_above_home() {
        let dir = TempDir::new("doctor-home");
        let home = dir.path().join("home");
        let project = home.join("projects").join("app");
        fs::create_dir_all(&project).unwrap();
        // Above home: ignored when cwd is inside home.
        fs::create_dir_all(dir.path().join(".sirius")).unwrap();
        assert_eq!(find_workspace_root(&project, Some(&home)), None);
        // Home itself: ignored too (a global ~/.sirius is not a workspace).
        fs::create_dir_all(home.join(".sirius")).unwrap();
        assert_eq!(find_workspace_root(&project, Some(&home)), None);
        assert_eq!(find_workspace_root(&home, Some(&home)), None);
        // Below home: found.
        fs::create_dir_all(home.join("projects").join(".sirius")).unwrap();
        assert_eq!(
            find_workspace_root(&project, Some(&home)),
            Some(home.join("projects"))
        );
        // A cwd outside home is not bounded by it.
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(
            find_workspace_root(&elsewhere, Some(&home)),
            Some(dir.path().to_path_buf())
        );
    }

    /// A fake `sirius` whose verdict depends on running inside a workspace
    /// (it sees `.sirius` only when its cwd is the workspace root).
    fn workspace_bound_sirius(dir: &Path) -> PathBuf {
        write_tool(
            dir,
            "sirius",
            r#"if [ -d .sirius ]; then echo '{"ok":true,"checks":[{"name":"amt_present_and_schema","gating":true,"ok":true}]}'; else echo '{"ok":false,"checks":[{"name":"workspace","gating":true,"ok":false}]}'; fi"#,
            "if exist .sirius goto inside\r\necho {\"ok\":false,\"checks\":[{\"name\":\"workspace\",\"gating\":true,\"ok\":false}]}\r\nexit /b 0\r\n:inside\r\necho {\"ok\":true,\"checks\":[{\"name\":\"amt_present_and_schema\",\"gating\":true,\"ok\":true}]}",
        )
    }

    #[test]
    fn suite_contracts_skip_outside_a_workspace_instead_of_failing() {
        let dir = TempDir::new("doctor-contracts-out");
        let bin = dir.path().join("bin");
        workspace_bound_sirius(&bin);
        let cwd = dir.path().join("not-a-repo");
        fs::create_dir_all(&cwd).unwrap();
        let mut env = env_in(&dir);
        env.cwd = cwd.clone();
        env.home = Some(dir.path().to_path_buf());
        env.path_dirs = vec![bin];
        let check = check_sirius_contracts(&env);
        assert_eq!(check.status, CheckStatus::Skip, "{}", check.detail);
        assert!(check.ok());
        assert_eq!(
            check.detail,
            format!(
                "not inside a Sothis workspace (no .sirius/ found from {}); run sothis doctor from a repo initialized with sirius init",
                cwd.display()
            )
        );
    }

    #[test]
    fn suite_contracts_run_sirius_from_the_workspace_root() {
        let dir = TempDir::new("doctor-contracts-in");
        let bin = dir.path().join("bin");
        workspace_bound_sirius(&bin);
        let repo = dir.path().join("repo");
        let nested = repo.join("crates").join("x");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(repo.join(".sirius")).unwrap();
        let mut env = env_in(&dir);
        // The doctor was started from a subdirectory; sirius must still run
        // from the root, where `.sirius` is visible.
        env.cwd = nested;
        env.home = Some(dir.path().to_path_buf());
        env.path_dirs = vec![bin];
        let check = check_sirius_contracts(&env);
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
        assert!(check.detail.contains("sirius doctor passes"));
        assert!(check.detail.contains(&repo.display().to_string()));
    }

    #[test]
    fn an_unhealthy_workspace_still_fails_the_contract_check() {
        let dir = TempDir::new("doctor-contracts-bad");
        let bin = dir.path().join("bin");
        write_tool(
            &bin,
            "sirius",
            r#"echo '{"ok":false,"checks":[{"name":"gate_config","gating":true,"ok":false}]}'"#,
            r#"echo {"ok":false,"checks":[{"name":"gate_config","gating":true,"ok":false}]}"#,
        );
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join(".sirius")).unwrap();
        let mut env = env_in(&dir);
        env.cwd = repo;
        env.path_dirs = vec![bin];
        let check = check_sirius_contracts(&env);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("gate_config"));
        // sirius missing from PATH inside a workspace is a failure.
        env.path_dirs = Vec::new();
        let check = check_sirius_contracts(&env);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("not on PATH"));
    }

    // ---------------------------------------------------------- PATH shadowing

    fn tool_env(dir: &TempDir, bins: &[&PathBuf]) -> Env {
        let mut env = env_in(dir);
        env.path_dirs = bins.iter().map(|bin| (*bin).clone()).collect();
        env
    }

    #[test]
    fn a_single_healthy_copy_passes_with_the_path_and_requirement() {
        let dir = TempDir::new("doctor-single");
        let good = dir.path().join("good");
        version_tool(&good, "amt", "amt 0.4.2");
        let amt = manifest_tool("ametrite");
        let check = check_tool(&amt, &tool_env(&dir, &[&good]), "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
        assert!(check.detail.contains("amt 0.4.2"));
        assert!(check.detail.contains("requires >= 0.3.0"));
        assert_eq!(check.name, "ametrite");
    }

    #[test]
    fn a_missing_command_fails() {
        let dir = TempDir::new("doctor-missing");
        let amt = manifest_tool("ametrite");
        let check = check_tool(&amt, &env_in(&dir), "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("amt is not on PATH"));
    }

    #[test]
    fn a_stale_first_copy_that_shadows_a_good_one_fails_with_both_paths() {
        let dir = TempDir::new("doctor-shadow-fail");
        let local = dir.path().join("local-bin");
        let cargo = dir.path().join("cargo-bin");
        let stale = version_tool(&local, "amt", "amt 0.2.0");
        let good = version_tool(&cargo, "amt", "amt 0.4.2");
        let amt = manifest_tool("ametrite");
        let env = tool_env(&dir, &[&local, &cargo]);
        let check = check_tool(&amt, &env, "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(!check.ok());
        let expected = format!(
            "earlier PATH entry {} (0.2.0) shadows {} (0.4.2); remove the stale copy",
            stale.display(),
            good.display()
        );
        assert!(check.detail.contains(&expected), "{}", check.detail);
    }

    #[test]
    fn a_good_first_copy_with_a_stale_later_one_warns_listing_every_version() {
        let dir = TempDir::new("doctor-shadow-warn");
        let cargo = dir.path().join("cargo-bin");
        let local = dir.path().join("local-bin");
        let good = version_tool(&cargo, "amt", "amt 0.4.2");
        let stale = version_tool(&local, "amt", "amt 0.2.0");
        let amt = manifest_tool("ametrite");
        let env = tool_env(&dir, &[&cargo, &local]);
        let check = check_tool(&amt, &env, "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Warn, "{}", check.detail);
        assert!(check.ok());
        assert!(check
            .detail
            .contains(&format!("{} -> 0.4.2", good.display())));
        assert!(check
            .detail
            .contains(&format!("{} -> 0.2.0", stale.display())));
        assert!(check.detail.contains("first one wins"));
        // No sirius beside the stale copy: no sirius note.
        assert!(!check.detail.contains("resolves its peer tools"));
    }

    #[test]
    fn the_sirius_peer_note_appears_only_for_a_stale_peer_beside_sirius_on_windows() {
        let dir = TempDir::new("doctor-sirius-note");
        let cargo = dir.path().join("cargo-bin");
        let local = dir.path().join("local-bin");
        version_tool(&cargo, "amt", "amt 0.4.2");
        version_tool(&local, "amt", "amt 0.2.0");
        let amt = manifest_tool("ametrite");
        let env = tool_env(&dir, &[&cargo, &local]);

        let check = check_tool(&amt, &env, "windows-x64", Some(&local));
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .detail
                .contains("sirius resolves its peer tools from its own directory first"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("fixed in the next sirius release"));

        // Sirius lives next to the GOOD copy: nothing stale beside it.
        let check = check_tool(&amt, &env, "windows-x64", Some(&cargo));
        assert!(!check.detail.contains("resolves its peer tools"));
        // Not Windows: the resolution quirk does not apply.
        let check = check_tool(&amt, &env, "linux-x64", Some(&local));
        assert!(!check.detail.contains("resolves its peer tools"));
        // The note also rides along on the hard failure.
        let env = tool_env(&dir, &[&local, &cargo]);
        let check = check_tool(&amt, &env, "windows-x64", Some(&local));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("remove the stale copy"));
        assert!(check.detail.contains("resolves its peer tools"));
    }

    #[test]
    fn the_same_directory_twice_on_path_is_not_a_shadow() {
        let dir = TempDir::new("doctor-dup-dir");
        let bin = dir.path().join("bin");
        version_tool(&bin, "amt", "amt 0.4.2");
        let amt = manifest_tool("ametrite");
        let env = tool_env(&dir, &[&bin, &bin.clone()]);
        let check = check_tool(&amt, &env, "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
    }

    #[test]
    fn every_copy_stale_is_a_plain_failure_that_still_lists_them() {
        let dir = TempDir::new("doctor-all-stale");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        version_tool(&a, "amt", "amt 0.2.0");
        version_tool(&b, "amt", "amt 0.1.0");
        let amt = manifest_tool("ametrite");
        let check = check_tool(&amt, &tool_env(&dir, &[&a, &b]), "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(!check.detail.contains("shadows"));
        assert!(check.detail.contains("2 copies of amt"));
        assert!(check.detail.contains("-> 0.1.0"));
    }

    #[test]
    fn an_unparseable_version_fails_closed() {
        let dir = TempDir::new("doctor-unparseable");
        let bin = dir.path().join("bin");
        version_tool(&bin, "amt", "development build");
        let amt = manifest_tool("ametrite");
        let check = check_tool(&amt, &tool_env(&dir, &[&bin]), "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("could not parse semantic version"));
    }

    // ------------------------------------------------------------ runtime floor

    #[test]
    fn the_bun_runtime_is_held_to_the_manifests_runtime_minimum() {
        let catryna = manifest_tool("catryna-wikinelli");
        assert_eq!(required_minimum(&catryna), Some("1.2.0"));
        let dir = TempDir::new("doctor-bun");
        let old = dir.path().join("old");
        version_tool(&old, "bun", "1.1.43");
        let check = check_tool(&catryna, &tool_env(&dir, &[&old]), "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Fail, "{}", check.detail);
        assert!(check.detail.contains("requires >= 1.2.0"));

        let new = dir.path().join("new");
        version_tool(&new, "bun", "1.2.5");
        let check = check_tool(&catryna, &tool_env(&dir, &[&new]), "windows-x64", None);
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
        assert!(check.detail.contains("requires >= 1.2.0"));

        // The floor never applies to tools that are not their own runtime.
        assert_eq!(required_minimum(&manifest_tool("guignet")), None);
        assert_eq!(required_minimum(&manifest_tool("ametrite")), Some("0.3.0"));
    }

    // -------------------------------------------------------- plugin versions

    fn plugin_check(dir: &TempDir, state: &str) -> Check {
        let plugins = dir.path().join("plugins");
        write_plugin_state(&plugins, state);
        let mut env = env_in(dir);
        env.plugins_dir = Some(plugins);
        check_plugin(&manifest_tool("catryna-wikinelli"), &env)
    }

    #[test]
    fn a_plugin_below_its_minimum_fails_with_the_update_command() {
        let dir = TempDir::new("doctor-plugin-old");
        let check = plugin_check(
            &dir,
            &plugin_state(&[("catryna@sothis", &[("user", "1.3.0")])]),
        );
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check
            .detail
            .contains("catryna@sothis 1.3.0 is below the minimum 1.3.1"));
        assert!(check
            .detail
            .contains("update the plugin: claude plugin update catryna@sothis"));
    }

    #[test]
    fn the_highest_version_across_scopes_and_marketplaces_counts() {
        let dir = TempDir::new("doctor-plugin-scopes");
        let check = plugin_check(
            &dir,
            &plugin_state(&[
                (
                    "catryna@sirius-forester",
                    &[("user", "1.3.0"), ("project", "1.3.0")],
                ),
                ("catryna@sothis", &[("user", "1.3.0"), ("project", "1.4.0")]),
            ]),
        );
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
        assert!(check.detail.contains("catryna@sothis is installed"));
        assert!(check.detail.contains("version 1.4.0"));
    }

    #[test]
    fn a_legacy_only_install_is_checked_and_updated_under_its_own_id() {
        let dir = TempDir::new("doctor-plugin-legacy");
        let check = plugin_check(
            &dir,
            &plugin_state(&[("catryna@sirius-forester", &[("user", "1.3.0")])]),
        );
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check
            .detail
            .contains("claude plugin update catryna@sirius-forester"));
        let ok = plugin_check(
            &dir,
            &plugin_state(&[("catryna@sirius-forester", &[("user", "1.3.1")])]),
        );
        assert_eq!(ok.status, CheckStatus::Pass);
        assert!(ok.detail.contains("legacy/standalone marketplace"));
    }

    #[test]
    fn an_unknown_plugin_version_warns_and_a_missing_plugin_fails() {
        let dir = TempDir::new("doctor-plugin-unknown");
        let unknown = plugin_check(
            &dir,
            &plugin_state(&[("catryna@sothis", &[("user", "unknown")])]),
        );
        assert_eq!(unknown.status, CheckStatus::Warn, "{}", unknown.detail);
        assert!(unknown.ok());
        assert!(unknown.detail.contains("could not be read"));
        let missing = plugin_check(
            &dir,
            &plugin_state(&[("other@sothis", &[("user", "9.9.9")])]),
        );
        assert_eq!(missing.status, CheckStatus::Fail);
        assert!(missing.detail.contains("catryna@sothis is not installed"));
        // A shape without the `plugins` wrapper is still read.
        let flat = plugin_check(&dir, r#"{"catryna@sothis":[{"version":"1.3.1"}]}"#);
        assert_eq!(flat.status, CheckStatus::Pass, "{}", flat.detail);
    }

    #[test]
    fn a_plugin_without_a_declared_minimum_keeps_the_presence_only_check() {
        let dir = TempDir::new("doctor-plugin-nomin");
        let plugins = dir.path().join("plugins");
        write_plugin_state(
            &plugins,
            &plugin_state(&[("catryna@sothis", &[("user", "0.0.1")])]),
        );
        let mut env = env_in(&dir);
        env.plugins_dir = Some(plugins);
        let mut catryna = manifest_tool("catryna-wikinelli");
        catryna.plugin.as_mut().unwrap().minimum_version = None;
        let check = check_plugin(&catryna, &env);
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, "catryna@sothis is installed");
    }

    #[test]
    fn installed_plugin_versions_reads_the_real_file_shape() {
        let state: serde_json::Value = serde_json::from_str(
            r#"{"version":2,"plugins":{
                "catryna@sirius-forester":[
                    {"scope":"user","version":"1.3.0","installPath":"C:\\x"},
                    {"scope":"project","version":"1.3.0","projectPath":"C:\\y"}],
                "catryna-extras@sothis":[{"version":"9.0.0"}],
                "sirius@sothis":[{"version":"0.2.0"}]}}"#,
        )
        .unwrap();
        let found = installed_plugin_versions(&state, "catryna");
        assert_eq!(
            found,
            [(
                "catryna@sirius-forester".to_string(),
                Some(Version::new(1, 3, 0))
            )]
        );
    }

    // --------------------------------------------------------------- receipts

    use crate::install::receipt::{FileKind, Receipt, ReceiptFile};
    use crate::install::verify::SignatureStatus;

    fn receipt_env(dir: &TempDir) -> (Env, Layout) {
        let layout = Layout {
            root: dir.path().join("sothis"),
            bin: dir.path().join("sothis").join("bin"),
        };
        fs::create_dir_all(&layout.bin).unwrap();
        let mut env = env_in(dir);
        env.layout = Some(layout.clone());
        env.path_dirs = vec![layout.bin.clone()];
        (env, layout)
    }

    fn install_fake(layout: &Layout, tool: &str, exe: &str, body: &[u8]) -> Receipt {
        fs::write(layout.bin.join(exe), body).unwrap();
        let receipt = Receipt {
            schema_version: 1,
            tool: tool.to_string(),
            version: "0.4.2".to_string(),
            installed_at: "2026-10-12T00:00:00Z".to_string(),
            platform: "windows-x64".to_string(),
            asset: format!("{tool}.zip"),
            asset_sha256: "a".repeat(64),
            signature: SignatureStatus::NotPublished,
            bin_dir: layout.bin.clone(),
            files: vec![ReceiptFile {
                path: exe.to_string(),
                kind: FileKind::File,
                sha256: Some(crate::install::fsx::sha256_file(&layout.bin.join(exe)).unwrap()),
            }],
            previous_version: None,
            adopted: Vec::new(),
        };
        receipt::write(layout, &receipt).unwrap();
        receipt
    }

    #[test]
    fn no_receipts_means_no_receipt_check() {
        let dir = TempDir::new("doctor-no-receipts");
        let (env, _) = receipt_env(&dir);
        assert!(check_receipts(&env).is_none());
        let mut no_layout = env_in(&dir);
        no_layout.layout = None;
        assert!(check_receipts(&no_layout).is_none());
    }

    #[test]
    fn intact_receipts_on_a_bin_that_is_on_path_pass() {
        let dir = TempDir::new("doctor-receipts-ok");
        let (env, layout) = receipt_env(&dir);
        install_fake(&layout, "ametrite", "amt.exe", b"amt");
        let check = check_receipts(&env).unwrap();
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
        assert_eq!(check.name, "sothis_receipts");
        assert!(check.detail.contains("ametrite 0.4.2"));
        assert!(check.detail.contains("on PATH"));
    }

    #[test]
    fn drifted_and_missing_files_warn_naming_the_tool() {
        let dir = TempDir::new("doctor-receipts-drift");
        let (env, layout) = receipt_env(&dir);
        install_fake(&layout, "ametrite", "amt.exe", b"amt");
        install_fake(&layout, "sirius-forester", "sirius.exe", b"sirius");
        fs::write(layout.bin.join("amt.exe"), b"tampered").unwrap();
        fs::remove_file(layout.bin.join("sirius.exe")).unwrap();
        let check = check_receipts(&env).unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.ok(), "drift must not fail the doctor");
        assert!(
            check
                .detail
                .contains("ametrite 0.4.2 drifted from its receipt (amt.exe was modified"),
            "{}",
            check.detail
        );
        assert!(
            check
                .detail
                .contains("sirius-forester 0.4.2 drifted from its receipt (sirius.exe is missing"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("sothis install"));
    }

    #[test]
    fn a_bin_directory_missing_from_path_warns() {
        let dir = TempDir::new("doctor-receipts-path");
        let (mut env, layout) = receipt_env(&dir);
        install_fake(&layout, "ametrite", "amt.exe", b"amt");
        env.path_dirs = vec![dir.path().join("elsewhere")];
        let check = check_receipts(&env).unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("is not on PATH"));
        assert!(check.detail.contains(&layout.bin.display().to_string()));
        assert!(!check.detail.contains("drifted"));
    }

    #[test]
    fn an_unreadable_receipt_is_a_warning_not_a_crash() {
        let dir = TempDir::new("doctor-receipts-corrupt");
        let (env, layout) = receipt_env(&dir);
        fs::create_dir_all(layout.receipts_dir()).unwrap();
        fs::write(layout.receipt_path("ametrite"), "{not json").unwrap();
        let check = check_receipts(&env).unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.detail.contains("could not be read"));
    }

    #[test]
    fn run_with_adds_the_receipt_check_after_the_tool_checks() {
        let dir = TempDir::new("doctor-run-order");
        let (env, layout) = receipt_env(&dir);
        install_fake(&layout, "ametrite", "amt.exe", b"amt");
        let manifest = crate::manifest::embedded().unwrap();
        // Everything in `bell` is a skip, so only the receipt check is extra.
        let checks = run_with(&manifest, "bell", "windows-x64", &env).unwrap();
        let names = checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["manifest", "pingmybell", "sothis_receipts"]);
        assert!(checks.iter().all(Check::ok));
    }
}
