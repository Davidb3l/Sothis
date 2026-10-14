//! Claude Code plugin steps, driven through the `claude plugin` CLI.
//!
//! Syntax (confirmed with `claude plugin --help`): `claude plugin marketplace
//! add <owner/repo>`, `claude plugin install <name>@<marketplace>`,
//! `claude plugin uninstall <name>@<marketplace>`. A legacy
//! `<name>@sirius-forester` install is reported, never touched.

use crate::install::proc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CLAUDE_TIMEOUT: Duration = Duration::from_secs(300);

pub trait ClaudeCli {
    fn is_available(&self) -> bool;
    /// Run `claude <args>`; `Err` carries the failure with a stderr tail.
    fn run(&self, args: &[&str]) -> Result<(), String>;
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

    fn run(&self, args: &[&str]) -> Result<(), String> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| "the claude CLI is not on PATH".to_string())?;
        let mut command = Command::new(path);
        command.args(args);
        match proc::run_capture(command, CLAUDE_TIMEOUT) {
            Ok(output) if output.success => Ok(()),
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
    fn id(&self) -> String {
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
        return StepResult::ok("already-installed", format!("{id} is installed"));
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
        Ok(()) => StepResult::ok("installed", format!("installed {id}")),
        Err(error) => StepResult::failed(format!("could not install {id}: {error}")),
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
        Ok(()) => StepResult::ok("removed", format!("uninstalled {id}")),
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
        pub calls: RefCell<Vec<String>>,
    }

    impl FakeClaude {
        pub fn new(available: bool) -> Self {
            FakeClaude {
                available,
                fail_on: None,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl ClaudeCli for FakeClaude {
        fn is_available(&self) -> bool {
            self.available
        }

        fn run(&self, args: &[&str]) -> Result<(), String> {
            let line = args.join(" ");
            self.calls.borrow_mut().push(line.clone());
            match self.fail_on {
                Some(needle) if line.contains(needle) => {
                    Err(format!("simulated failure of {line}"))
                }
                _ => Ok(()),
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
