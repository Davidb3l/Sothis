use crate::manifest::Manifest;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Install,
    InstallPluginRuntime,
    BuildFromSource,
    UseWsl2,
    SkipUnsupported,
    CheckForUpdate,
    Remove,
    RemovePluginRuntime,
    RemoveFromWsl2,
}

pub fn apply_operation(items: &mut [PlanItem], operation: &str) -> Result<(), String> {
    for item in items {
        item.action = match (operation, &item.action) {
            ("install", action) => action.clone(),
            ("update", PlanAction::SkipUnsupported) => PlanAction::SkipUnsupported,
            ("update", PlanAction::UseWsl2) => PlanAction::UseWsl2,
            ("update", _) => PlanAction::CheckForUpdate,
            ("uninstall", PlanAction::InstallPluginRuntime) => PlanAction::RemovePluginRuntime,
            ("uninstall", PlanAction::UseWsl2) => PlanAction::RemoveFromWsl2,
            ("uninstall", PlanAction::SkipUnsupported) => PlanAction::SkipUnsupported,
            ("uninstall", _) => PlanAction::Remove,
            (other, _) => return Err(format!("unknown plan operation: {other}")),
        };
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    pub tool: String,
    pub name: String,
    pub action: PlanAction,
    pub support: String,
    pub version: Option<String>,
    pub note: Option<String>,
}

pub fn build(manifest: &Manifest, profile: &str, platform: &str) -> Result<Vec<PlanItem>, String> {
    let profile = manifest
        .profiles
        .get(profile)
        .ok_or_else(|| format!("unknown profile: {profile}"))?;

    profile
        .tools
        .iter()
        .map(|id| {
            let tool = manifest
                .tools
                .iter()
                .find(|tool| &tool.id == id)
                .ok_or_else(|| format!("profile references unknown tool: {id}"))?;
            let support = tool
                .platforms
                .get(platform)
                .ok_or_else(|| format!("{} does not declare support for {platform}", tool.id))?;
            let action = match support.support.as_str() {
                "native" | "native-preview" => PlanAction::Install,
                "plugin-runtime" => PlanAction::InstallPluginRuntime,
                "source-only" => PlanAction::BuildFromSource,
                "wsl2" => PlanAction::UseWsl2,
                "unsupported" => PlanAction::SkipUnsupported,
                other => return Err(format!("{} has unknown support value {other}", tool.id)),
            };
            Ok(PlanItem {
                tool: tool.id.clone(),
                name: tool.name.clone(),
                action,
                support: support.support.clone(),
                version: tool.recommended_version.clone(),
                note: support.note.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    #[test]
    fn windows_full_plan_is_honest_about_bridges_and_unsupported_tools() {
        let manifest = manifest::embedded().unwrap();
        let plan = build(&manifest, "full", "windows-x64").unwrap();
        assert_eq!(plan.len(), 6);
        assert_eq!(
            plan.iter()
                .find(|item| item.tool == "pingmybell")
                .unwrap()
                .action,
            PlanAction::BuildFromSource
        );
        assert_eq!(
            plan.iter()
                .find(|item| item.tool == "guignet")
                .unwrap()
                .action,
            PlanAction::UseWsl2
        );
        assert_eq!(
            plan.iter()
                .find(|item| item.tool == "sirius-forester")
                .unwrap()
                .support,
            "native-preview"
        );
    }

    #[test]
    fn pingmybell_is_source_only_on_windows_x64_and_unsupported_elsewhere() {
        let manifest = manifest::embedded().unwrap();
        let action = |platform: &str| {
            build(&manifest, "bell", platform).unwrap()[0]
                .action
                .clone()
        };
        assert_eq!(action("windows-x64"), PlanAction::BuildFromSource);
        assert_eq!(action("macos-arm64"), PlanAction::BuildFromSource);
        for platform in ["windows-arm64", "linux-x64", "linux-arm64"] {
            assert_eq!(action(platform), PlanAction::SkipUnsupported, "{platform}");
        }
        let plan = build(&manifest, "bell", "windows-x64").unwrap();
        assert_eq!(plan[0].support, "source-only");
        assert!(plan[0]
            .note
            .as_deref()
            .unwrap()
            .contains("Builds and passes its test suite on Windows 11"));
    }

    /// The PowerShell contract in ci.yml asserts per-tool actions for a
    /// windows-x64 full plan. Keep those literal assertions honest: every one
    /// of them must equal what `build` actually produces.
    #[test]
    fn ci_windows_contract_assertions_match_the_real_plan() {
        let ci = include_str!("../.github/workflows/ci.yml");
        let assertion = regex::Regex::new(
            r"\(\$plan\.items \| Where-Object tool -eq '([a-z-]+)'\)\.action -ne '([a-z_0-9]+)'",
        )
        .unwrap();
        let manifest = manifest::embedded().unwrap();
        let plan = build(&manifest, "full", "windows-x64").unwrap();
        let mut seen = Vec::new();
        for capture in assertion.captures_iter(ci) {
            let (tool, expected) = (&capture[1], &capture[2]);
            let item = plan
                .iter()
                .find(|item| item.tool == tool)
                .unwrap_or_else(|| panic!("ci.yml asserts on unknown tool {tool}"));
            let actual = serde_json::to_value(&item.action).unwrap();
            assert_eq!(actual, expected, "ci.yml expects {tool} to be {expected}");
            seen.push(tool.to_string());
        }
        seen.sort();
        assert_eq!(seen, ["guignet", "pingmybell"]);
        assert!(ci.contains("PingMyBell must be source-only on Windows"));
    }

    #[test]
    fn core_plan_contains_exactly_the_runtime_loop_dependencies() {
        let manifest = manifest::embedded().unwrap();
        let plan = build(&manifest, "core", "windows-x64").unwrap();
        let ids = plan
            .iter()
            .map(|item| item.tool.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["ametrite", "hayvenhurst", "sirius-forester"]);
    }

    #[test]
    fn uninstall_plan_uses_removal_actions() {
        let manifest = manifest::embedded().unwrap();
        let mut plan = build(&manifest, "docs", "windows-x64").unwrap();
        apply_operation(&mut plan, "uninstall").unwrap();
        assert!(plan
            .iter()
            .take(3)
            .all(|item| item.action == PlanAction::Remove));
        assert_eq!(plan.last().unwrap().action, PlanAction::RemovePluginRuntime);
    }

    #[test]
    fn windows_arm64_degrades_to_explicit_support_results() {
        let manifest = manifest::embedded().unwrap();
        let plan = build(&manifest, "full", "windows-arm64").unwrap();
        assert_eq!(plan.len(), 6);
        assert_eq!(
            plan.iter()
                .find(|item| item.tool == "guignet")
                .unwrap()
                .action,
            PlanAction::UseWsl2
        );
        assert!(plan
            .iter()
            .filter(|item| item.tool != "guignet")
            .all(|item| item.action == PlanAction::SkipUnsupported));
    }
}
