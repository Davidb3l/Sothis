//! Claude Code plugin steps, driven through the `claude plugin` CLI.
//!
//! Syntax (confirmed with `claude plugin --help`): `claude plugin marketplace
//! add <owner/repo>`, `claude plugin install <name>@<marketplace>`,
//! `claude plugin uninstall <name>@<marketplace>`, `claude plugin update
//! <name>@<marketplace>` and `claude plugin marketplace update <marketplace>`.
//! A legacy `<name>@sirius-forester` install is reported, never touched.

use crate::install::proc;
use crate::manifest::Manifest;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CLAUDE_TIMEOUT: Duration = Duration::from_secs(300);

pub trait ClaudeCli {
    fn is_available(&self) -> bool;
    /// Run `claude <args>`. `Ok` carries what it printed (stdout, then
    /// stderr); `Err` carries the failure with an output tail.
    fn run(&self, args: &[&str]) -> Result<String, String>;
}

pub struct RealClaude {
    path: Option<PathBuf>,
}

impl RealClaude {
    pub fn detect() -> Self {
        RealClaude {
            path: crate::doctor::find_command("claude"),
        }
    }
}

impl ClaudeCli for RealClaude {
    fn is_available(&self) -> bool {
        self.path.is_some()
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| "the claude CLI is not on PATH".to_string())?;
        let mut command = Command::new(path);
        command.args(args);
        match proc::run_capture(command, CLAUDE_TIMEOUT) {
            Ok(output) if output.success => Ok(format!("{}\n{}", output.stdout, output.stderr)
                .trim()
                .to_string()),
            Ok(output) => Err(format!(
                "claude {} exited {}: {}",
                args.join(" "),
                output
                    .code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "with a signal".to_string()),
                proc::tail(&format!("{}\n{}", output.stderr, output.stdout), 4)
            )),
            Err(error) => Err(proc::describe_spawn_error(path, &error)),
        }
    }
}

/// What Claude Code currently has installed, read from its plugin state files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginState {
    /// `name@marketplace` identifiers from `installed_plugins.json`.
    pub installed: Vec<String>,
    /// Marketplace names from `known_marketplaces.json`.
    pub marketplaces: Vec<String>,
}

/// Read the state files from `dir` (normally `~/.claude/plugins`). Missing
/// files mean "nothing installed yet"; malformed JSON is an error.
pub fn load_state(dir: Option<&Path>) -> Result<PluginState, String> {
    let Some(dir) = dir else {
        return Ok(PluginState::default());
    };
    let read_json = |name: &str| -> Result<Option<serde_json::Value>, String> {
        let path = dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str(&body)
                .map(Some)
                .map_err(|e| format!("{} is invalid JSON: {e}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    };
    let installed = read_json("installed_plugins.json")?
        .map(|value| crate::doctor::plugin_identifiers(&value))
        .unwrap_or_default();
    let marketplaces = read_json("known_marketplaces.json")?
        .and_then(|value| value.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();
    Ok(PluginState {
        installed,
        marketplaces,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub result: &'static str,
    pub detail: String,
    pub failed: bool,
}

impl StepResult {
    fn ok(result: &'static str, detail: String) -> Self {
        StepResult {
            result,
            detail,
            failed: false,
        }
    }

    fn failed(detail: String) -> Self {
        StepResult {
            result: "failed",
            detail,
            failed: true,
        }
    }
}

pub struct PluginTarget<'a> {
    pub name: &'a str,
    pub marketplace: &'a str,
    /// GitHub `owner/repo` of the marketplace.
    pub marketplace_repo: &'a str,
}

impl PluginTarget<'_> {
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.marketplace)
    }

    fn commands(&self, state: &PluginState) -> Vec<String> {
        let mut commands = Vec::new();
        if !state.marketplaces.iter().any(|m| m == self.marketplace) {
            commands.push(format!(
                "claude plugin marketplace add {}",
                self.marketplace_repo
            ));
        }
        commands.push(format!("claude plugin install {}", self.id()));
        commands
    }
}

fn legacy_ids(state: &PluginState, target: &PluginTarget<'_>) -> Vec<String> {
    let prefix = format!("{}@", target.name);
    let canonical = target.id();
    let mut legacy = state
        .installed
        .iter()
        .filter(|id| id.starts_with(&prefix) && **id != canonical)
        .cloned()
        .collect::<Vec<_>>();
    legacy.sort();
    legacy.dedup();
    legacy
}

/// Make sure `<name>@<marketplace>` is installed.
pub fn ensure_plugin(
    target: &PluginTarget<'_>,
    state: &PluginState,
    claude: &dyn ClaudeCli,
) -> StepResult {
    let id = target.id();
    if state.installed.contains(&id) {
        return StepResult::ok(
            "already-installed",
            format!("{id} is installed; run `sothis update` to refresh it"),
        );
    }
    let legacy = legacy_ids(state, target);
    if !legacy.is_empty() {
        return StepResult::ok(
            "legacy-plugin",
            format!(
                "{} is installed through a legacy marketplace; migrate manually: claude plugin uninstall {} && claude plugin install {id}",
                target.name,
                legacy.join(" ")
            ),
        );
    }
    if !claude.is_available() {
        return StepResult::ok(
            "manual",
            format!(
                "the claude CLI is not on PATH; run: {}",
                target.commands(state).join(" && ")
            ),
        );
    }
    if !state.marketplaces.iter().any(|m| m == target.marketplace) {
        if let Err(error) = claude.run(&["plugin", "marketplace", "add", target.marketplace_repo]) {
            return StepResult::failed(format!(
                "could not add the {} marketplace: {error}",
                target.marketplace
            ));
        }
    }
    match claude.run(&["plugin", "install", &id]) {
        Ok(_) => StepResult::ok("installed", format!("installed {id}")),
        Err(error) => StepResult::failed(format!("could not install {id}: {error}")),
    }
}

/// One canonical suite plugin that `sothis update` refreshes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateTarget {
    /// The manifest tool id the plugin belongs to, or `sothis` for the suite plugin.
    pub tool: String,
    pub plugin: String,
    pub marketplace: String,
}

impl UpdateTarget {
    pub fn id(&self) -> String {
        format!("{}@{}", self.plugin, self.marketplace)
    }
}

/// The canonical suite plugins (every tool's plugin, then `sothis@sothis`)
/// that are installed. Legacy `@sirius-forester` installs are not included.
pub fn update_targets(manifest: &Manifest, state: &PluginState) -> Vec<UpdateTarget> {
    let mut targets = Vec::new();
    let canonical = &manifest.marketplace.canonical.name;
    let all = manifest
        .tools
        .iter()
        .filter_map(|tool| {
            tool.plugin.as_ref().map(|plugin| UpdateTarget {
                tool: tool.id.clone(),
                plugin: plugin.name.clone(),
                marketplace: plugin.marketplace.clone(),
            })
        })
        .chain(std::iter::once(UpdateTarget {
            tool: "sothis".to_string(),
            plugin: "sothis".to_string(),
            marketplace: canonical.clone(),
        }));
    for target in all {
        if state.installed.contains(&target.id()) && !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// The commands `sothis update` runs for `targets`, for plans and for the
/// no-`claude`-on-PATH report: one marketplace refresh, then each plugin.
pub fn update_commands(marketplace: &str, targets: &[UpdateTarget]) -> Vec<String> {
    if targets.is_empty() {
        return Vec::new();
    }
    std::iter::once(format!("claude plugin marketplace update {marketplace}"))
        .chain(
            targets
                .iter()
                .map(|target| format!("claude plugin update {}", target.id())),
        )
        .collect()
}

/// Read what `claude plugin update` printed. The CLI says either that the
/// plugin was updated (from one version to another, restart required) or that
/// it is already at the latest version; anything else that exits 0 is
/// reported as `checked` with the output verbatim rather than guessed at.
pub fn classify_update_output(output: &str) -> &'static str {
    let text = output.to_lowercase();
    if text.contains("already")
        || text.contains("up to date")
        || text.contains("up-to-date")
        || text.contains("latest version")
        || text.contains("no update")
    {
        "up-to-date"
    } else if text.contains("updated") || text.contains("updating") {
        "updated"
    } else {
        "checked"
    }
}

/// Refresh the marketplace catalog (one call for all plugins).
pub fn update_marketplace(marketplace: &str, claude: &dyn ClaudeCli) -> StepResult {
    match claude.run(&["plugin", "marketplace", "update", marketplace]) {
        Ok(_) => StepResult::ok(
            "updated",
            format!("refreshed the {marketplace} marketplace"),
        ),
        Err(error) => StepResult::failed(format!(
            "could not refresh the {marketplace} marketplace: {error}; plugin updates were skipped"
        )),
    }
}

/// `claude plugin update <id>`, classified from what it prints.
pub fn update_plugin(target: &UpdateTarget, claude: &dyn ClaudeCli) -> StepResult {
    let id = target.id();
    match claude.run(&["plugin", "update", &id]) {
        Ok(output) => {
            let line = output
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
                .to_string();
            match classify_update_output(&output) {
                "updated" => StepResult::ok(
                    "updated",
                    if line.to_lowercase().contains("restart") {
                        format!("{id}: {line}")
                    } else {
                        format!("{id}: {line} (restart Claude Code to apply)")
                    },
                ),
                "up-to-date" => StepResult::ok("up-to-date", format!("{id} is already current")),
                _ => StepResult::ok(
                    "checked",
                    if line.is_empty() {
                        format!("{id}: claude plugin update finished without output")
                    } else {
                        format!("{id}: {line}")
                    },
                ),
            }
        }
        Err(error) => StepResult::failed(format!("could not update {id}: {error}")),
    }
}

/// Remove `<name>@<marketplace>` if it is installed. Legacy installs are
/// never touched.
pub fn remove_plugin(
    target: &PluginTarget<'_>,
    state: &PluginState,
    claude: &dyn ClaudeCli,
) -> StepResult {
    let id = target.id();
    if !state.installed.contains(&id) {
        return StepResult::ok("not-installed", format!("{id} is not installed"));
    }
    if !claude.is_available() {
        return StepResult::ok(
            "manual",
            format!("the claude CLI is not on PATH; run: claude plugin uninstall {id}"),
        );
    }
    match claude.run(&["plugin", "uninstall", &id]) {
        Ok(_) => StepResult::ok("removed", format!("uninstalled {id}")),
        Err(error) => StepResult::failed(format!("could not uninstall {id}: {error}")),
    }
}

#[cfg(test)]
pub mod testutil {
    use super::ClaudeCli;
    use std::cell::RefCell;

    pub struct FakeClaude {
        pub available: bool,
        pub fail_on: Option<&'static str>,
        /// (needle, stdout): the first needle contained in a call's arguments
        /// decides what that call prints.
        pub outputs: Vec<(&'static str, &'static str)>,
        pub calls: RefCell<Vec<String>>,
    }

    impl FakeClaude {
        pub fn new(available: bool) -> Self {
            FakeClaude {
                available,
                fail_on: None,
                outputs: Vec::new(),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl ClaudeCli for FakeClaude {
        fn is_available(&self) -> bool {
            self.available
        }

        fn run(&self, args: &[&str]) -> Result<String, String> {
            let line = args.join(" ");
            self.calls.borrow_mut().push(line.clone());
            match self.fail_on {
                Some(needle) if line.contains(needle) => {
                    Err(format!("simulated failure of {line}"))
                }
                _ => Ok(self
                    .outputs
                    .iter()
                    .find(|(needle, _)| line.contains(needle))
                    .map(|(_, output)| output.to_string())
                    .unwrap_or_default()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::FakeClaude;
    use super::*;
    use crate::install::fsx::testutil::TempDir;
    use std::fs;

    fn target() -> PluginTarget<'static> {
        PluginTarget {
            name: "sirius",
            marketplace: "sothis",
            marketplace_repo: "Davidb3l/Sothis",
        }
    }

    fn state(installed: &[&str], marketplaces: &[&str]) -> PluginState {
        PluginState {
            installed: installed.iter().map(|s| s.to_string()).collect(),
            marketplaces: marketplaces.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn state_is_read_from_the_claude_plugin_files() {
        let dir = TempDir::new("plugin-state");
        assert_eq!(
            load_state(Some(dir.path())).unwrap(),
            PluginState::default()
        );
        assert_eq!(load_state(None).unwrap(), PluginState::default());
        fs::write(
            dir.path().join("installed_plugins.json"),
            r#"{"version":2,"plugins":{"sirius@sirius-forester":[{"scope":"user"}]}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("known_marketplaces.json"),
            r#"{"sirius-forester":{"source":{}},"sothis":{"source":{}}}"#,
        )
        .unwrap();
        let loaded = load_state(Some(dir.path())).unwrap();
        assert_eq!(loaded.installed, ["sirius@sirius-forester"]);
        assert_eq!(loaded.marketplaces, ["sirius-forester", "sothis"]);
        fs::write(dir.path().join("installed_plugins.json"), "{oops").unwrap();
        assert!(load_state(Some(dir.path())).is_err());
    }

    #[test]
    fn an_installed_canonical_plugin_is_a_no_op() {
        let claude = FakeClaude::new(true);
        let result = ensure_plugin(&target(), &state(&["sirius@sothis"], &["sothis"]), &claude);
        assert_eq!(result.result, "already-installed");
        assert!(result.detail.contains("run `sothis update` to refresh it"));
        assert!(claude.calls.borrow().is_empty());
    }

    #[test]
    fn a_legacy_plugin_is_reported_and_left_alone() {
        let claude = FakeClaude::new(true);
        let result = ensure_plugin(
            &target(),
            &state(&["sirius@sirius-forester"], &["sirius-forester"]),
            &claude,
        );
        assert_eq!(result.result, "legacy-plugin");
        assert!(!result.failed);
        assert!(result
            .detail
            .contains("claude plugin uninstall sirius@sirius-forester"));
        assert!(result
            .detail
            .contains("claude plugin install sirius@sothis"));
        assert!(
            claude.calls.borrow().is_empty(),
            "legacy installs are never touched"
        );
    }

    #[test]
    fn a_missing_plugin_adds_the_marketplace_once_then_installs() {
        let claude = FakeClaude::new(true);
        let result = ensure_plugin(&target(), &state(&[], &[]), &claude);
        assert_eq!(result.result, "installed");
        assert_eq!(
            *claude.calls.borrow(),
            [
                "plugin marketplace add Davidb3l/Sothis",
                "plugin install sirius@sothis"
            ]
        );
        let claude = FakeClaude::new(true);
        let result = ensure_plugin(&target(), &state(&[], &["sothis"]), &claude);
        assert_eq!(result.result, "installed");
        assert_eq!(*claude.calls.borrow(), ["plugin install sirius@sothis"]);
    }

    #[test]
    fn without_the_claude_cli_the_exact_commands_are_reported() {
        let claude = FakeClaude::new(false);
        let result = ensure_plugin(&target(), &state(&[], &[]), &claude);
        assert_eq!(result.result, "manual");
        assert!(!result.failed);
        assert!(result.detail.contains(
            "claude plugin marketplace add Davidb3l/Sothis && claude plugin install sirius@sothis"
        ));
        assert!(claude.calls.borrow().is_empty());
    }

    #[test]
    fn claude_failures_are_reported_as_failed_steps() {
        let mut claude = FakeClaude::new(true);
        claude.fail_on = Some("marketplace add");
        let result = ensure_plugin(&target(), &state(&[], &[]), &claude);
        assert!(result.failed);
        assert!(result.detail.contains("marketplace"));

        let mut claude = FakeClaude::new(true);
        claude.fail_on = Some("plugin install");
        let result = ensure_plugin(&target(), &state(&[], &["sothis"]), &claude);
        assert!(result.failed);
        assert!(result.detail.contains("sirius@sothis"));
    }

    fn manifest() -> Manifest {
        crate::manifest::embedded().unwrap()
    }

    fn sirius_target() -> UpdateTarget {
        UpdateTarget {
            tool: "sirius-forester".to_string(),
            plugin: "sirius".to_string(),
            marketplace: "sothis".to_string(),
        }
    }

    #[test]
    fn update_output_is_classified_from_what_the_cli_prints() {
        for (output, expected) in [
            (
                "Plugin \"hayvenhurst\" updated from 0.0.9 to 0.0.10. Restart to apply changes.",
                "updated",
            ),
            (
                "hayvenhurst is already at the latest version (0.0.10).",
                "up-to-date",
            ),
            ("Plugin is already up to date", "up-to-date"),
            ("something unexpected", "checked"),
            ("", "checked"),
        ] {
            assert_eq!(classify_update_output(output), expected, "{output}");
        }
    }

    #[test]
    fn update_targets_are_the_installed_canonical_suite_plugins_only() {
        let m = manifest();
        let none = update_targets(&m, &state(&[], &["sothis"]));
        assert!(none.is_empty());
        let targets = update_targets(
            &m,
            &state(
                &[
                    "sothis@sothis",
                    "sirius@sothis",
                    "sirius@sirius-forester",
                    "unrelated@elsewhere",
                ],
                &["sothis"],
            ),
        );
        let ids = targets.iter().map(UpdateTarget::id).collect::<Vec<_>>();
        assert_eq!(ids, ["sirius@sothis", "sothis@sothis"]);
        assert_eq!(
            update_commands("sothis", &targets),
            [
                "claude plugin marketplace update sothis",
                "claude plugin update sirius@sothis",
                "claude plugin update sothis@sothis"
            ]
        );
        assert!(update_commands("sothis", &[]).is_empty());
    }

    #[test]
    fn update_plugin_reports_updated_current_checked_and_failed() {
        let mut claude = FakeClaude::new(true);
        claude.outputs = vec![("update sirius@sothis", "Plugin updated from 0.2.6 to 0.2.7")];
        let step = update_plugin(&sirius_target(), &claude);
        assert_eq!(step.result, "updated");
        assert!(step.detail.contains("0.2.6 to 0.2.7"));
        assert!(step.detail.contains("restart"));

        claude.outputs = vec![("update sirius", "already at the latest version")];
        assert_eq!(
            update_plugin(&sirius_target(), &claude).result,
            "up-to-date"
        );

        claude.outputs = vec![("update sirius", "all done")];
        let step = update_plugin(&sirius_target(), &claude);
        assert_eq!(step.result, "checked");
        assert!(step.detail.contains("all done"));

        claude.fail_on = Some("plugin update");
        let step = update_plugin(&sirius_target(), &claude);
        assert!(step.failed);
        assert!(step.detail.contains("sirius@sothis"));
    }

    #[test]
    fn the_marketplace_refresh_names_the_marketplace() {
        let claude = FakeClaude::new(true);
        assert_eq!(update_marketplace("sothis", &claude).result, "updated");
        assert_eq!(*claude.calls.borrow(), ["plugin marketplace update sothis"]);
        let mut failing = FakeClaude::new(true);
        failing.fail_on = Some("marketplace update");
        let step = update_marketplace("sothis", &failing);
        assert!(step.failed);
        assert!(step.detail.contains("plugin updates were skipped"));
    }

    #[test]
    fn removal_only_touches_the_canonical_plugin() {
        let claude = FakeClaude::new(true);
        let result = remove_plugin(&target(), &state(&["sirius@sirius-forester"], &[]), &claude);
        assert_eq!(result.result, "not-installed");
        assert!(claude.calls.borrow().is_empty());

        let result = remove_plugin(&target(), &state(&["sirius@sothis"], &[]), &claude);
        assert_eq!(result.result, "removed");
        assert_eq!(*claude.calls.borrow(), ["plugin uninstall sirius@sothis"]);

        let offline = FakeClaude::new(false);
        let result = remove_plugin(&target(), &state(&["sirius@sothis"], &[]), &offline);
        assert_eq!(result.result, "manual");
    }
}
