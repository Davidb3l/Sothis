//! Orchestration of `install`, `update`, `uninstall` and `rollback`.
//!
//! Per tool the pipeline is: fetch -> sha256 -> Sigstore -> safe extract ->
//! retain under `tools/<id>/<version>` -> atomic placement -> `--version`
//! check -> receipt. Any failure undoes that tool's journal and leaves the
//! previous state exactly as it was; tools that already succeeded are kept.

use crate::install::extract;
use crate::install::fsx;
use crate::install::layout::Layout;
use crate::install::lock::MutationLock;
use crate::install::pathenv::{self, PathChange, PathStore};
use crate::install::place::{self, Journal, PlaceInput};
use crate::install::plugins::{self, ClaudeCli, PluginState, PluginTarget};
use crate::install::proc;
use crate::install::receipt::{self, InstallMeta, Receipt, RECEIPT_SCHEMA_VERSION};
use crate::install::source::{AssetRef, Fetcher, MAX_ASSET_BYTES, MAX_SIDECAR_BYTES};
use crate::install::verify::{self, ReleaseCheck, SignatureStatus, SignatureVerifier};
use crate::manifest::{self, Manifest, Tool};
use crate::plan::{self, PlanAction, PlanItem};
use semver::Version;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
const SUITE_PLUGIN: &str = "sothis";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Install,
    Update,
    Uninstall,
    Rollback,
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Operation::Install => "install",
            Operation::Update => "update",
            Operation::Uninstall => "uninstall",
            Operation::Rollback => "rollback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub operation: Operation,
    pub profile: String,
    pub allow_missing_cosign: bool,
    pub allow_downgrade: bool,
    pub modify_path: bool,
    /// install/update: manage Claude plugins (the inverse of `--no-plugins`);
    /// uninstall: also remove plugins (`--plugins`).
    pub plugins: bool,
    /// The tool id for `rollback`.
    pub rollback_tool: Option<String>,
}

pub struct Context<'a> {
    pub manifest: &'a Manifest,
    pub layout: &'a Layout,
    /// Platform key, for example `windows-x64`.
    pub platform: &'a str,
    /// `.exe` on Windows, empty elsewhere.
    pub exe_suffix: &'a str,
}

/// Runs an installed executable's `--version`.
pub trait VersionProbe {
    fn version_output(&self, exe: &Path) -> Result<String, String>;
}

pub struct RealProbe;

impl VersionProbe for RealProbe {
    fn version_output(&self, exe: &Path) -> Result<String, String> {
        let mut command = std::process::Command::new(exe);
        command.arg("--version");
        let output = proc::run_capture(command, VERSION_TIMEOUT)
            .map_err(|e| proc::describe_spawn_error(exe, &e))?;
        if !output.success {
            return Err(format!(
                "{} --version exited {:?}: {}",
                exe.display(),
                output.code,
                proc::tail(&output.stderr, 3)
            ));
        }
        Ok(if output.stdout.trim().is_empty() {
            output.stderr
        } else {
            output.stdout
        })
    }
}

pub enum PathMode<'a> {
    /// Edit the user PATH through a store (Windows registry).
    /// Constructed on Windows (and by tests).
    #[cfg_attr(not(windows), allow(dead_code))]
    Store(&'a mut dyn PathStore),
    /// Never edit anything; print the line to add (macOS/Linux).
    /// Constructed on macOS/Linux (and by tests).
    #[cfg_attr(windows, allow(dead_code))]
    Advisory,
}

pub struct Services<'a> {
    pub fetcher: &'a dyn Fetcher,
    pub verifier: &'a dyn SignatureVerifier,
    pub probe: &'a dyn VersionProbe,
    pub claude: &'a dyn ClaudeCli,
    pub path: PathMode<'a>,
    /// Directory holding Claude's `installed_plugins.json` etc.
    pub plugin_state_dir: Option<PathBuf>,
    /// The current process PATH, in order.
    pub path_dirs: Vec<PathBuf>,
    pub expand_env: &'a dyn Fn(&str) -> Option<String>,
    pub find_command: &'a dyn Fn(&str) -> Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemResult {
    pub tool: String,
    pub action: String,
    pub result: String,
    pub version: Option<String>,
    pub detail: String,
    #[serde(skip)]
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub ok: bool,
    pub operation: String,
    pub profile: String,
    pub platform: String,
    pub items: Vec<ItemResult>,
    pub path_changed: bool,
    pub warnings: Vec<String>,
}

impl Summary {
    pub fn render_human(&self) -> String {
        let mut out = format!(
            "{} {}: profile={}, platform={}\n",
            self.operation,
            if self.ok { "complete" } else { "FAILED" },
            self.profile,
            self.platform
        );
        for item in &self.items {
            let version = item
                .version
                .as_deref()
                .map(|v| format!(" {v}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "[{}] {}{} ({}) - {}\n",
                item.result, item.tool, version, item.action, item.detail
            ));
        }
        for warning in &self.warnings {
            out.push_str(&format!("warning: {warning}\n"));
        }
        out
    }
}

fn action_name(action: &PlanAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{action:?}"))
}

/// Run one operation end to end under the global mutation lock.
pub fn run<'a>(
    ctx: &'a Context<'a>,
    opts: &'a Options,
    svc: Services<'a>,
) -> Result<Summary, String> {
    let _lock = MutationLock::acquire(&ctx.layout.lock_path())?;
    let mut engine = Engine {
        ctx,
        opts,
        svc,
        items: Vec::new(),
        warnings: Vec::new(),
        path_changed: false,
    };
    engine.cleanup_stale_scratch()?;
    match opts.operation {
        Operation::Install | Operation::Update => engine.install_or_update()?,
        Operation::Uninstall => engine.uninstall()?,
        Operation::Rollback => engine.rollback()?,
    }
    Ok(engine.finish())
}

struct Engine<'a> {
    ctx: &'a Context<'a>,
    opts: &'a Options,
    svc: Services<'a>,
    items: Vec<ItemResult>,
    warnings: Vec<String>,
    path_changed: bool,
}

fn find_tool<'m>(manifest: &'m Manifest, id: &str) -> Result<&'m Tool, String> {
    manifest
        .tools
        .iter()
        .find(|tool| tool.id == id)
        .ok_or_else(|| format!("unknown tool {id:?}"))
}

fn is_success(result: &str) -> bool {
    matches!(
        result,
        "installed"
            | "upgraded"
            | "repaired"
            | "downgraded"
            | "already-installed"
            | "newer-installed"
            | "up-to-date"
            | "updated"
    )
}

impl Engine<'_> {
    fn finish(self) -> Summary {
        Summary {
            ok: !self.items.iter().any(|item| item.failed),
            operation: self.opts.operation.name().to_string(),
            profile: self.opts.profile.clone(),
            platform: self.ctx.platform.to_string(),
            items: self.items,
            path_changed: self.path_changed,
            warnings: self.warnings,
        }
    }

    fn push(
        &mut self,
        tool: &str,
        action: &str,
        result: &str,
        version: Option<String>,
        detail: String,
        failed: bool,
    ) {
        self.items.push(ItemResult {
            tool: tool.to_string(),
            action: action.to_string(),
            result: result.to_string(),
            version,
            detail,
            failed,
        });
    }

    /// Under the lock no other transaction can be live, so staging leftovers
    /// are garbage. Rollback leftovers may hold a user's only backup, so they
    /// are reported, never deleted.
    fn cleanup_stale_scratch(&mut self) -> Result<(), String> {
        let layout = self.ctx.layout;
        if let Ok(entries) = fs::read_dir(layout.staging_root()) {
            for entry in entries.flatten() {
                let _ = fsx::remove_any(&entry.path());
            }
        }
        if let Ok(mut entries) = fs::read_dir(layout.rollback_root()) {
            if entries.next().is_some() {
                self.warnings.push(format!(
                    "leftover data from an interrupted run exists in {}; inspect it before deleting (it may hold your previous files)",
                    layout.rollback_root().display()
                ));
            }
        }
        Ok(())
    }

    fn receipts(&self) -> Result<BTreeMap<String, Receipt>, String> {
        Ok(receipt::list(self.ctx.layout)?
            .into_iter()
            .map(|r| (r.tool.clone(), r))
            .collect())
    }

    fn plugin_state(&self) -> PluginState {
        plugins::load_state(self.svc.plugin_state_dir.as_deref()).unwrap_or_else(|error| {
            // An unreadable state file must not block binary installs; the
            // plugin step then falls back to asking claude.
            eprintln!("sothis: warning: {error}");
            PluginState::default()
        })
    }

    // ---------------------------------------------------------------- install

    fn install_or_update(&mut self) -> Result<(), String> {
        let operation = self.opts.operation;
        let base = plan::build(self.ctx.manifest, &self.opts.profile, self.ctx.platform)?;
        let mut reported = base.clone();
        plan::apply_operation(&mut reported, operation.name())?;
        let mut tool_ok = BTreeMap::new();
        for (item, shown) in base.iter().zip(&reported) {
            let tool = find_tool(self.ctx.manifest, &item.tool)?;
            let action = action_name(&shown.action);
            match item.action {
                PlanAction::Install => {
                    let ok = self.install_item(tool, &action)?;
                    tool_ok.insert(tool.id.clone(), ok);
                    if ok && self.opts.plugins {
                        self.ensure_tool_plugin(tool);
                    }
                }
                PlanAction::InstallPluginRuntime => self.runtime_item(tool, &action),
                PlanAction::BuildFromSource => self.manual_item(tool, item, &action, "manual"),
                PlanAction::UseWsl2 => self.manual_item(tool, item, &action, "wsl2"),
                PlanAction::SkipUnsupported => self.manual_item(tool, item, &action, "unsupported"),
                _ => return Err(format!("unexpected plan action {:?}", item.action)),
            }
        }
        if self.opts.plugins {
            self.ensure_suite_plugin();
        }
        let any_managed = self
            .items
            .iter()
            .any(|item| item.action != "install_plugin" && is_success(&item.result));
        if any_managed {
            self.path_step();
            self.shadow_warnings();
        }
        Ok(())
    }

    fn manual_item(&mut self, tool: &Tool, item: &PlanItem, action: &str, result: &str) {
        let mut detail = item.note.clone().unwrap_or_else(|| {
            format!("{} is not installed by sothis on this platform", tool.name)
        });
        if result == "manual" {
            detail.push_str(&format!(
                " Build and install it from https://github.com/{}.",
                tool.repository
            ));
        } else {
            detail.push_str(&format!(" See https://github.com/{}.", tool.repository));
        }
        self.push(&tool.id, action, result, None, detail, false);
    }

    fn runtime_item(&mut self, tool: &Tool, action: &str) {
        let runtime = tool.distribution.runtime.clone().unwrap_or_default();
        let found = (self.svc.find_command)(&runtime);
        match found {
            Some(path) => self.push(
                &tool.id,
                action,
                "ready",
                None,
                format!("{runtime} runtime found at {}", path.display()),
                false,
            ),
            None => self.push(
                &tool.id,
                action,
                "missing-runtime",
                None,
                format!(
                    "the {runtime} runtime is not on PATH and sothis does not install it; install it from https://bun.sh (for example: winget install Oven-sh.Bun, or brew install oven-sh/bun/bun) and rerun"
                ),
                false,
            ),
        }
        if self.opts.plugins {
            self.ensure_tool_plugin(tool);
        }
    }

    fn ensure_tool_plugin(&mut self, tool: &Tool) {
        let Some(plugin) = &tool.plugin else { return };
        let state = self.plugin_state();
        let target = PluginTarget {
            name: &plugin.name,
            marketplace: &plugin.marketplace,
            marketplace_repo: &self.ctx.manifest.marketplace.canonical.repository,
        };
        let step = plugins::ensure_plugin(&target, &state, self.svc.claude);
        self.push(
            &tool.id,
            "install_plugin",
            step.result,
            None,
            step.detail,
            step.failed,
        );
    }

    fn ensure_suite_plugin(&mut self) {
        let state = self.plugin_state();
        let target = PluginTarget {
            name: SUITE_PLUGIN,
            marketplace: &self.ctx.manifest.marketplace.canonical.name,
            marketplace_repo: &self.ctx.manifest.marketplace.canonical.repository,
        };
        let step = plugins::ensure_plugin(&target, &state, self.svc.claude);
        self.push(
            "sothis",
            "install_plugin",
            step.result,
            None,
            step.detail,
            step.failed,
        );
    }

    /// Decide what to do for one tool-installer tool. Returns whether the tool
    /// ended up installed (so plugins and PATH steps make sense).
    fn install_item(&mut self, tool: &Tool, action: &str) -> Result<bool, String> {
        let target_text = tool
            .recommended_version
            .clone()
            .ok_or_else(|| format!("{} has no recommendedVersion", tool.id))?;
        let target = Version::parse(&target_text)
            .map_err(|e| format!("{} recommendedVersion is invalid: {e}", tool.id))?;
        let current = receipt::read(self.ctx.layout, &tool.id)?;
        let updating = self.opts.operation == Operation::Update;
        let allow_downgrade = self.opts.allow_downgrade;

        let Some(current) = current else {
            if updating {
                let command = tool.command.clone().unwrap_or_default();
                let on_path = (self.svc.find_command)(&command);
                let (result, detail) = match on_path {
                    Some(path) => (
                        "unmanaged",
                        format!(
                            "{command} exists at {} but was not installed by sothis; run `sothis install --profile {}` to take it over (the existing file is backed up and restored on uninstall)",
                            path.display(),
                            self.opts.profile
                        ),
                    ),
                    None => (
                        "not-installed",
                        format!(
                            "{} is not installed; run `sothis install --profile {}`",
                            tool.name, self.opts.profile
                        ),
                    ),
                };
                self.push(&tool.id, action, result, None, detail, false);
                return Ok(false);
            }
            return Ok(self.run_install(tool, action, &target_text, None, "installed"));
        };

        let installed = Version::parse(&current.version)
            .map_err(|e| format!("receipt for {} has a bad version: {e}", tool.id))?;
        let intact = receipt::matches_disk(&current);
        match installed.cmp(&target) {
            std::cmp::Ordering::Equal if intact => {
                let (result, detail) = if updating {
                    (
                        "up-to-date",
                        format!("{} {} is the recommended version", tool.name, installed),
                    )
                } else {
                    (
                        "already-installed",
                        format!(
                            "{} {} is already installed and its files match the receipt",
                            tool.name, installed
                        ),
                    )
                };
                self.push(
                    &tool.id,
                    action,
                    result,
                    Some(current.version.clone()),
                    detail,
                    false,
                );
                Ok(true)
            }
            std::cmp::Ordering::Equal if updating => {
                self.push(
                    &tool.id,
                    action,
                    "up-to-date",
                    Some(current.version.clone()),
                    "installed files differ from the receipt; run `sothis install` to repair them"
                        .to_string(),
                    false,
                );
                Ok(true)
            }
            std::cmp::Ordering::Equal => {
                Ok(self.run_install(tool, action, &target_text, Some(&current), "repaired"))
            }
            std::cmp::Ordering::Less => {
                let label = if updating { "updated" } else { "upgraded" };
                Ok(self.run_install(tool, action, &target_text, Some(&current), label))
            }
            std::cmp::Ordering::Greater if allow_downgrade => {
                Ok(self.run_install(tool, action, &target_text, Some(&current), "downgraded"))
            }
            std::cmp::Ordering::Greater => {
                self.push(
                    &tool.id,
                    action,
                    if updating { "up-to-date" } else { "newer-installed" },
                    Some(current.version.clone()),
                    format!(
                        "installed {} is newer than the recommended {target_text}; pass --allow-downgrade to replace it",
                        current.version
                    ),
                    false,
                );
                Ok(true)
            }
        }
    }

    fn run_install(
        &mut self,
        tool: &Tool,
        action: &str,
        version: &str,
        previous: Option<&Receipt>,
        success_label: &str,
    ) -> bool {
        match self.perform_install(tool, version, previous) {
            Ok(done) => {
                let mut detail = format!(
                    "{} -> {} (signature: {})",
                    done.receipt.asset,
                    self.ctx.layout.bin.display(),
                    signature_label(done.receipt.signature)
                );
                if let Some(old) = previous {
                    if old.version != version {
                        detail.push_str(&format!(
                            "; previous version {} retained for rollback",
                            old.version
                        ));
                    }
                }
                if !done.receipt.adopted.is_empty() {
                    detail.push_str(&format!(
                        "; adopted and backed up {} existing file(s)",
                        done.receipt.adopted.len()
                    ));
                }
                self.warnings.extend(done.warnings);
                self.push(
                    &tool.id,
                    action,
                    success_label,
                    Some(version.to_string()),
                    detail,
                    false,
                );
                true
            }
            Err(error) => {
                self.push(
                    &tool.id,
                    action,
                    "failed",
                    Some(version.to_string()),
                    error,
                    true,
                );
                false
            }
        }
    }

    fn perform_install(
        &mut self,
        tool: &Tool,
        version: &str,
        previous: Option<&Receipt>,
    ) -> Result<Done, String> {
        let txn = fsx::new_txn_id();
        let mut journal = Journal::new();
        let mut warnings = Vec::new();
        let result = self.install_steps(tool, version, previous, &txn, &mut journal, &mut warnings);
        let layout = self.ctx.layout;
        match result {
            Ok(receipt) => {
                journal.commit();
                warnings.append(&mut journal.notes);
                let _ = fsx::remove_any(&layout.staging_dir(&txn));
                let _ = fsx::remove_any(&layout.rollback_dir(&txn));
                if let Some(problem) = self.prune_versions(tool, &receipt) {
                    warnings.push(problem);
                }
                Ok(Done { receipt, warnings })
            }
            Err(error) => {
                let problems = journal.undo();
                let _ = fsx::remove_any(&layout.staging_dir(&txn));
                let _ = fs::remove_dir(layout.tool_dir(&tool.id)); // only if empty
                if problems.is_empty() {
                    let _ = fsx::remove_any(&layout.rollback_dir(&txn));
                    Err(format!("{error}; rolled back, no changes were kept"))
                } else {
                    Err(format!(
                        "{error}; ROLLBACK INCOMPLETE: {}; originals may be under {}",
                        problems.join("; "),
                        layout.rollback_dir(&txn).display()
                    ))
                }
            }
        }
    }

    fn install_steps(
        &mut self,
        tool: &Tool,
        version: &str,
        previous: Option<&Receipt>,
        txn: &str,
        journal: &mut Journal,
        warnings: &mut Vec<String>,
    ) -> Result<Receipt, String> {
        let ctx = self.ctx;
        let layout = ctx.layout;
        let template = tool
            .distribution
            .assets
            .get(ctx.platform)
            .ok_or_else(|| format!("{} has no release asset for {}", tool.id, ctx.platform))?;
        let asset = template.replace("{version}", version);
        let files = expand_files(tool, ctx.exe_suffix)?;
        self.check_ownership(tool, previous, &files)?;

        let staging = layout.staging_dir(txn);
        let downloads = staging.join("download");
        let payload = staging.join("payload");
        fs::create_dir_all(&downloads)
            .map_err(|e| format!("could not create {}: {e}", downloads.display()))?;

        // 1. Download.
        let archive = downloads.join(&asset);
        let fetched = self.svc.fetcher.fetch(
            &AssetRef {
                repository: &tool.repository,
                version,
                name: &asset,
            },
            &archive,
            MAX_ASSET_BYTES,
        )?;
        let suffix = tool
            .distribution
            .checksum_suffix
            .as_deref()
            .unwrap_or(".sha256");
        let checksum_name = format!("{asset}{suffix}");
        let checksum_path = downloads.join(&checksum_name);
        self.svc.fetcher.fetch(
            &AssetRef {
                repository: &tool.repository,
                version,
                name: &checksum_name,
            },
            &checksum_path,
            MAX_SIDECAR_BYTES,
        )?;
        let checksum_body = fs::read_to_string(&checksum_path)
            .map_err(|e| format!("could not read {checksum_name}: {e}"))?;

        let sigstore = tool.distribution.verification == "sha256+sigstore";
        let bundle_name = format!("{asset}.sigstore.json");
        let bundle_path = downloads.join(&bundle_name);
        if sigstore {
            self.svc.fetcher.fetch(
                &AssetRef {
                    repository: &tool.repository,
                    version,
                    name: &bundle_name,
                },
                &bundle_path,
                MAX_SIDECAR_BYTES,
            )?;
        }

        // 2. Verify (checksum, then signature) before anything is unpacked.
        let identity = tool
            .distribution
            .sigstore_identity
            .as_deref()
            .map(|template| verify::sigstore_identity(template, version));
        let outcome = verify::verify_release(
            &ReleaseCheck {
                asset_name: &asset,
                asset_path: &archive,
                actual_sha256: &fetched.sha256,
                checksum_body: &checksum_body,
                verification: &tool.distribution.verification,
                bundle_path: sigstore.then_some(bundle_path.as_path()),
                identity: identity.as_deref(),
            },
            self.svc.verifier,
            self.opts.allow_missing_cosign,
        )?;
        warnings.extend(outcome.warnings);

        // 3. Safe extraction and a pre-flight of the declared entries.
        let format = extract::format_for(&asset)?;
        extract::extract_archive(&archive, format, &payload, &staging.join("work"))?;
        place::check_payload(&payload, &files)?;

        // 4. Retain the full payload for rollback; the previous directory of
        //    the same version (a repair) is set aside until the end.
        let retained = layout.version_dir(&tool.id, version);
        if fsx::exists_nofollow(&retained) {
            journal.move_aside(
                &retained,
                &layout.rollback_dir(txn).join("previous-payload"),
            )?;
        }
        fsx::move_path(&payload, &retained).map_err(|e| {
            format!(
                "could not retain the payload in {}: {e}",
                retained.display()
            )
        })?;
        journal.created(retained.clone());

        // 5. Place, then prove the installed binary reports the right version.
        let placed = place::place(
            &PlaceInput {
                layout,
                tool_id: &tool.id,
                txn,
                payload: &retained,
                new_files: &files,
                previous,
            },
            journal,
        )?;
        self.check_installed_version(tool, version)?;

        // 6. Provenance for rollback, then the receipt (the commit point).
        receipt::write_meta(
            &retained,
            &InstallMeta {
                schema_version: RECEIPT_SCHEMA_VERSION,
                asset: asset.clone(),
                asset_sha256: fetched.sha256.clone(),
                signature: outcome.signature,
                files: placed.files.clone(),
            },
        )?;
        let new_receipt = Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            tool: tool.id.clone(),
            version: version.to_string(),
            installed_at: fsx::now_rfc3339(),
            platform: ctx.platform.to_string(),
            asset,
            asset_sha256: fetched.sha256,
            signature: outcome.signature,
            bin_dir: layout.bin.clone(),
            files: placed.files,
            previous_version: match previous {
                Some(old) if old.version != version => Some(old.version.clone()),
                Some(old) => old.previous_version.clone(),
                None => None,
            },
            adopted: placed.adopted,
        };
        receipt::write(layout, &new_receipt)?;
        Ok(new_receipt)
    }

    /// Refuse to place over entries another tool's receipt owns, or into a
    /// different bin directory than the one the receipt recorded.
    fn check_ownership(
        &self,
        tool: &Tool,
        previous: Option<&Receipt>,
        files: &[String],
    ) -> Result<(), String> {
        if let Some(previous) = previous {
            if previous.bin_dir != self.ctx.layout.bin {
                return Err(format!(
                    "{} was installed into {}, but the current bin directory is {}; uninstall it first",
                    tool.id,
                    previous.bin_dir.display(),
                    self.ctx.layout.bin.display()
                ));
            }
        }
        for other in self.receipts()?.values() {
            if other.tool == tool.id {
                continue;
            }
            if let Some(entry) = files.iter().find(|entry| other.owns(entry)) {
                return Err(format!(
                    "{entry} is already owned by {}; refusing to replace it",
                    other.tool
                ));
            }
        }
        Ok(())
    }

    fn check_installed_version(&self, tool: &Tool, expected: &str) -> Result<(), String> {
        let command = tool
            .command
            .as_deref()
            .ok_or_else(|| format!("{} declares no command", tool.id))?;
        let exe = self
            .ctx
            .layout
            .bin
            .join(format!("{command}{}", self.ctx.exe_suffix));
        let output = self.svc.probe.version_output(&exe)?;
        let reported = crate::version::extract(&output).ok_or_else(|| {
            format!(
                "{} --version printed no parseable version: {:?}",
                exe.display(),
                proc::tail(&output, 2)
            )
        })?;
        let wanted = Version::parse(expected)
            .map_err(|e| format!("expected version {expected:?} is invalid: {e}"))?;
        if reported != wanted {
            return Err(format!(
                "{} reports version {reported}, expected {wanted}",
                exe.display()
            ));
        }
        Ok(())
    }

    /// Keep the current and previous version directories; prune the rest.
    fn prune_versions(&self, tool: &Tool, receipt: &Receipt) -> Option<String> {
        let dir = self.ctx.layout.tool_dir(&tool.id);
        let keep = [
            Some(receipt.version.as_str()),
            receipt.previous_version.as_deref(),
        ];
        let mut problems = Vec::new();
        for entry in fs::read_dir(&dir).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if keep.iter().flatten().any(|kept| *kept == name) {
                continue;
            }
            if let Err(error) = fsx::remove_any(&entry.path()) {
                problems.push(format!("{}: {error}", entry.path().display()));
            }
        }
        (!problems.is_empty())
            .then(|| format!("could not prune old versions: {}", problems.join("; ")))
    }

    // -------------------------------------------------------------- uninstall

    fn uninstall(&mut self) -> Result<(), String> {
        let base = plan::build(self.ctx.manifest, &self.opts.profile, self.ctx.platform)?;
        let mut reported = base.clone();
        plan::apply_operation(&mut reported, "uninstall")?;
        let mut order = base.into_iter().zip(reported).collect::<Vec<_>>();
        order.reverse();
        for (item, shown) in order {
            let tool = find_tool(self.ctx.manifest, &item.tool)?;
            let action = action_name(&shown.action);
            match item.action {
                PlanAction::Install => {
                    self.uninstall_item(tool, &action);
                    if self.opts.plugins {
                        self.remove_tool_plugin(tool);
                    }
                }
                PlanAction::InstallPluginRuntime => {
                    if self.opts.plugins {
                        self.remove_tool_plugin(tool);
                    } else {
                        self.push(
                            &tool.id,
                            &action,
                            "kept-plugin",
                            None,
                            "the plugin was left installed; pass --plugins to remove it"
                                .to_string(),
                            false,
                        );
                    }
                }
                PlanAction::BuildFromSource => self.manual_item(tool, &item, &action, "manual"),
                PlanAction::UseWsl2 => self.manual_item(tool, &item, &action, "wsl2"),
                PlanAction::SkipUnsupported => {
                    self.manual_item(tool, &item, &action, "unsupported")
                }
                _ => return Err(format!("unexpected plan action {:?}", item.action)),
            }
        }
        let remaining = self.receipts()?;
        if remaining.is_empty() {
            if self.opts.plugins {
                self.remove_suite_plugin();
            }
            self.path_removal();
            // Best effort: leave no empty skeleton behind.
            let _ = fs::remove_dir(&self.ctx.layout.bin);
        }
        Ok(())
    }

    fn uninstall_item(&mut self, tool: &Tool, action: &str) {
        let layout = self.ctx.layout;
        let current = match receipt::read(layout, &tool.id) {
            Ok(current) => current,
            Err(error) => {
                self.push(&tool.id, action, "failed", None, error, true);
                return;
            }
        };
        let Some(current) = current else {
            let command = tool.command.clone().unwrap_or_default();
            let detail = match (self.svc.find_command)(&command) {
                Some(path) => format!(
                    "{} is not Sothis-managed; the existing {} was left untouched",
                    tool.name,
                    path.display()
                ),
                None => format!("{} is not installed", tool.name),
            };
            self.push(&tool.id, action, "not-installed", None, detail, false);
            return;
        };
        let txn = fsx::new_txn_id();
        let mut journal = Journal::new();
        match place::remove_owned(layout, &current, &txn, &mut journal) {
            Ok(report) => {
                journal.commit();
                self.warnings.append(&mut journal.notes);
                self.warnings.extend(report.warnings);
                let _ = fsx::remove_any(&layout.rollback_dir(&txn));
                let _ = fs::remove_dir(layout.backups_dir(&tool.id));
                let restored = current.adopted.len();
                let detail = if restored > 0 {
                    format!("removed; restored {restored} previously existing file(s)")
                } else {
                    "removed; tool data (.ametrite, .hayven, .sirius, .suite) was not touched"
                        .to_string()
                };
                self.push(
                    &tool.id,
                    action,
                    "removed",
                    Some(current.version),
                    detail,
                    false,
                );
            }
            Err(error) => {
                let problems = journal.undo();
                let _ = fsx::remove_any(&layout.rollback_dir(&txn));
                let detail = if problems.is_empty() {
                    format!("{error}; rolled back, nothing was removed")
                } else {
                    format!("{error}; ROLLBACK INCOMPLETE: {}", problems.join("; "))
                };
                self.push(
                    &tool.id,
                    action,
                    "failed",
                    Some(current.version),
                    detail,
                    true,
                );
            }
        }
    }

    fn remove_tool_plugin(&mut self, tool: &Tool) {
        let Some(plugin) = &tool.plugin else { return };
        let state = self.plugin_state();
        let target = PluginTarget {
            name: &plugin.name,
            marketplace: &plugin.marketplace,
            marketplace_repo: &self.ctx.manifest.marketplace.canonical.repository,
        };
        let step = plugins::remove_plugin(&target, &state, self.svc.claude);
        self.push(
            &tool.id,
            "remove_plugin",
            step.result,
            None,
            step.detail,
            step.failed,
        );
    }

    fn remove_suite_plugin(&mut self) {
        let state = self.plugin_state();
        let target = PluginTarget {
            name: SUITE_PLUGIN,
            marketplace: &self.ctx.manifest.marketplace.canonical.name,
            marketplace_repo: &self.ctx.manifest.marketplace.canonical.repository,
        };
        let step = plugins::remove_plugin(&target, &state, self.svc.claude);
        self.push(
            "sothis",
            "remove_plugin",
            step.result,
            None,
            step.detail,
            step.failed,
        );
    }

    // --------------------------------------------------------------- rollback

    fn rollback(&mut self) -> Result<(), String> {
        let id = self
            .opts
            .rollback_tool
            .clone()
            .ok_or_else(|| "rollback requires a tool id".to_string())?;
        let tool = find_tool(self.ctx.manifest, &id)?;
        if tool.distribution.kind != "tool-installer" {
            return Err(format!("{id} is not a tool installed by sothis"));
        }
        match self.rollback_tool(tool) {
            Ok((from, to)) => self.push(
                &tool.id,
                "rollback",
                "rolled-back",
                Some(to.clone()),
                format!("{from} -> {to}; {from} is retained, run rollback again to return"),
                false,
            ),
            Err(error) => self.push(&tool.id, "rollback", "failed", None, error, true),
        }
        Ok(())
    }

    fn rollback_tool(&mut self, tool: &Tool) -> Result<(String, String), String> {
        let layout = self.ctx.layout;
        let current = receipt::read(layout, &tool.id)?
            .ok_or_else(|| format!("{} is not Sothis-managed; nothing to roll back", tool.id))?;
        let target = current.previous_version.clone().ok_or_else(|| {
            format!(
                "{} {} has no previous version recorded; nothing to roll back to",
                tool.id, current.version
            )
        })?;
        let retained = layout.version_dir(&tool.id, &target);
        if !retained.is_dir() {
            return Err(format!(
                "the retained payload for {} {target} is missing ({})",
                tool.id,
                retained.display()
            ));
        }
        let meta = receipt::read_meta(&retained)?;
        // The retained payload must still be exactly what was verified.
        for file in &meta.files {
            let actual = fsx::entry_hash(&retained.join(&file.path))
                .map_err(|e| format!("retained payload is damaged: {e}"))?;
            if actual != file.sha256 {
                return Err(format!(
                    "retained payload entry {} no longer matches its recorded hash",
                    file.path
                ));
            }
        }
        let files = meta
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect::<Vec<_>>();
        self.check_ownership(tool, Some(&current), &files)?;

        let txn = fsx::new_txn_id();
        let mut journal = Journal::new();
        let result = (|| {
            let placed = place::place(
                &PlaceInput {
                    layout,
                    tool_id: &tool.id,
                    txn: &txn,
                    payload: &retained,
                    new_files: &files,
                    previous: Some(&current),
                },
                &mut journal,
            )?;
            self.check_installed_version(tool, &target)?;
            let rolled = Receipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                tool: tool.id.clone(),
                version: target.clone(),
                installed_at: fsx::now_rfc3339(),
                platform: self.ctx.platform.to_string(),
                asset: meta.asset.clone(),
                asset_sha256: meta.asset_sha256.clone(),
                signature: meta.signature,
                bin_dir: layout.bin.clone(),
                files: placed.files,
                previous_version: Some(current.version.clone()),
                adopted: placed.adopted,
            };
            receipt::write(layout, &rolled)?;
            Ok::<(), String>(())
        })();
        match result {
            Ok(()) => {
                journal.commit();
                self.warnings.append(&mut journal.notes);
                let _ = fsx::remove_any(&layout.staging_dir(&txn));
                let _ = fsx::remove_any(&layout.rollback_dir(&txn));
                Ok((current.version, target))
            }
            Err(error) => {
                let problems = journal.undo();
                let _ = fsx::remove_any(&layout.staging_dir(&txn));
                if problems.is_empty() {
                    let _ = fsx::remove_any(&layout.rollback_dir(&txn));
                    Err(format!("{error}; the current version was left in place"))
                } else {
                    Err(format!(
                        "{error}; ROLLBACK INCOMPLETE: {}",
                        problems.join("; ")
                    ))
                }
            }
        }
    }

    // ------------------------------------------------------------------- PATH

    fn path_step(&mut self) {
        let bin = self.ctx.layout.bin.clone();
        let modify = self.opts.modify_path;
        let on_path = pathenv::dir_on_path(&bin, &self.svc.path_dirs);
        let expand = self.svc.expand_env;
        match &mut self.svc.path {
            PathMode::Store(store) if modify => match pathenv::ensure_present(*store, &bin, expand)
            {
                Ok(PathChange::Added) => {
                    self.path_changed = true;
                    self.warnings.push(format!(
                        "added {} to the user PATH. {}",
                        bin.display(),
                        pathenv::NEW_PROCESS_NOTE
                    ));
                }
                Ok(_) => {
                    if !on_path {
                        self.warnings.push(format!(
                            "{} is already on the user PATH. {}",
                            bin.display(),
                            pathenv::NEW_PROCESS_NOTE
                        ));
                    }
                }
                Err(error) => {
                    self.items.push(ItemResult {
                        tool: "sothis".to_string(),
                        action: "modify_path".to_string(),
                        result: "failed".to_string(),
                        version: None,
                        detail: format!("could not update the user PATH: {error}"),
                        failed: true,
                    });
                }
            },
            PathMode::Store(_) => {
                if !on_path {
                    self.warnings.push(format!(
                        "{} is not on PATH (--no-modify-path); add it yourself or rerun without the flag. {}",
                        bin.display(),
                        pathenv::NEW_PROCESS_NOTE
                    ));
                }
            }
            PathMode::Advisory => {
                if !on_path {
                    self.warnings.push(format!(
                        "{} is not on PATH; add this line to your shell profile: {}. {}",
                        bin.display(),
                        pathenv::unix_path_line(&bin),
                        pathenv::NEW_PROCESS_NOTE
                    ));
                }
            }
        }
    }

    fn path_removal(&mut self) {
        if !self.opts.modify_path {
            return;
        }
        let bin = self.ctx.layout.bin.clone();
        let expand = self.svc.expand_env;
        if let PathMode::Store(store) = &mut self.svc.path {
            match pathenv::ensure_absent(*store, &bin, expand) {
                Ok(PathChange::Removed) => {
                    self.path_changed = true;
                    self.warnings.push(format!(
                        "removed {} from the user PATH. {}",
                        bin.display(),
                        pathenv::NEW_PROCESS_NOTE
                    ));
                }
                Ok(_) => {}
                Err(error) => self.items.push(ItemResult {
                    tool: "sothis".to_string(),
                    action: "modify_path".to_string(),
                    result: "failed".to_string(),
                    version: None,
                    detail: format!("could not update the user PATH: {error}"),
                    failed: true,
                }),
            }
        }
    }

    /// Warn when another copy of an installed command would be resolved
    /// instead of Sothis's. If the user PATH was just changed, the install dir
    /// is assumed to lead the user entries of a fresh shell.
    ///
    /// Two wordings, because they are different problems: when the install
    /// dir is on PATH, copies *ahead* of it shadow it; when it is not on PATH
    /// at all there is no "ahead", the shell simply never finds Sothis's copy.
    fn shadow_warnings(&mut self) {
        let bin = self.ctx.layout.bin.clone();
        let mut dirs = self.svc.path_dirs.clone();
        if self.path_changed {
            dirs.insert(0, bin.clone());
        }
        let bin_on_path = pathenv::dir_on_path(&bin, &dirs);
        let installed = self
            .items
            .iter()
            .filter(|item| item.action != "install_plugin" && is_success(&item.result))
            .map(|item| item.tool.clone())
            .collect::<Vec<_>>();
        for id in installed {
            let Ok(tool) = find_tool(self.ctx.manifest, &id) else {
                continue;
            };
            let Some(command) = &tool.command else {
                continue;
            };
            let file_name = format!("{command}{}", self.ctx.exe_suffix);
            let shadows = pathenv::find_shadowing(&file_name, &bin, &dirs);
            if bin_on_path {
                for shadow in shadows {
                    self.warnings.push(format!(
                        "{command} also exists at {}, which is ahead of {} on PATH; `{command}` may resolve to the other copy",
                        shadow.display(),
                        bin.display()
                    ));
                }
            } else if let Some(first) = shadows.first() {
                self.warnings.push(format!(
                    "{command} on PATH currently resolves to {}; Sothis's copy in {} is not on PATH",
                    first.display(),
                    bin.display()
                ));
            }
        }
    }
}

struct Done {
    receipt: Receipt,
    warnings: Vec<String>,
}

fn signature_label(status: SignatureStatus) -> &'static str {
    match status {
        SignatureStatus::Verified => "verified",
        SignatureStatus::SkippedNoCosign => "SKIPPED, cosign missing",
        SignatureStatus::NotPublished => "not published; checksum only",
    }
}

fn expand_files(tool: &Tool, exe_suffix: &str) -> Result<Vec<String>, String> {
    let files = tool
        .distribution
        .files
        .as_ref()
        .ok_or_else(|| format!("{} declares no files to place", tool.id))?;
    files
        .iter()
        .map(|entry| {
            manifest::validate_file_entry(entry)
                .map_err(|e| format!("{} files entry {entry:?}: {e}", tool.id))?;
            Ok(manifest::expand_file_entry(entry, exe_suffix))
        })
        .collect()
}
