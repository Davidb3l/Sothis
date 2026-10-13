use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub schema_version: u32,
    pub suite_version: String,
    pub marketplace: Marketplace,
    pub profiles: BTreeMap<String, Profile>,
    pub tools: Vec<Tool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Marketplace {
    pub canonical: MarketplaceRef,
    pub legacy: Vec<LegacyMarketplace>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceRef {
    pub name: String,
    pub repository: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct LegacyMarketplace {
    pub name: String,
    pub repository: String,
    pub remove_after_suite_releases: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub description: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_version: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub contracts: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRef>,
    pub distribution: Distribution,
    pub platforms: BTreeMap<String, Platform>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct PluginRef {
    pub name: String,
    pub marketplace: String,
    /// Oldest installed plugin version the suite supports (semver). Doctor
    /// compares it with the version recorded in `installed_plugins.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct Distribution {
    pub kind: String,
    pub verification: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Oldest acceptable `<runtime> --version` (semver); plugin-runtime only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minimum_version: Option<String>,
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_suffix: Option<String>,
    /// Top-level payload entries placed into the bin directory. `{exe}` expands
    /// to `.exe` on Windows and to nothing elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sigstore_identity: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub support: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub fn embedded() -> Result<Manifest, String> {
    serde_json::from_str(include_str!("../suite.manifest.json"))
        .map_err(|e| format!("embedded suite.manifest.json is invalid JSON: {e}"))
}

/// Expand the `{exe}` placeholder in a `files` entry.
pub fn expand_file_entry(entry: &str, exe_suffix: &str) -> String {
    entry.replace("{exe}", exe_suffix)
}

/// Check one `files` entry. Mirrored by the `files` item pattern in
/// `schemas/suite-manifest.schema.json`: a single path component built from
/// `[A-Za-z0-9_.+-]` and `{exe}`, starting with `[A-Za-z0-9_+-]` and never
/// containing `..`.
pub fn validate_file_entry(entry: &str) -> Result<(), String> {
    if entry.is_empty() {
        return Err("entry is empty".to_string());
    }
    if entry.contains('/') || entry.contains('\\') {
        return Err("entry must be a single top-level name (no path separators)".to_string());
    }
    if entry.contains("..") {
        return Err("entry must not contain '..'".to_string());
    }
    if !entry
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '+' | '-'))
    {
        return Err("entry must start with a letter, digit, '_', '+' or '-'".to_string());
    }
    let mut rest = entry;
    while let Some(index) = rest.find(['{', '}']) {
        if !rest[index..].starts_with("{exe}") {
            return Err("only the {exe} placeholder is supported".to_string());
        }
        rest = &rest[index + "{exe}".len()..];
    }
    let literal = entry.replace("{exe}", "");
    if let Some(bad) = literal
        .chars()
        .find(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '+' | '-')))
    {
        return Err(format!("character {bad:?} is not allowed"));
    }
    Ok(())
}

pub fn validate(manifest: &Manifest) -> Vec<String> {
    let mut errors = Vec::new();
    if manifest.schema != "./schemas/suite-manifest.schema.json" {
        errors.push(format!(
            "$schema must reference ./schemas/suite-manifest.schema.json, found {}",
            manifest.schema
        ));
    }
    if manifest.schema_version != 1 {
        errors.push(format!(
            "unsupported schemaVersion {}; this binary supports 1",
            manifest.schema_version
        ));
    }
    if manifest.marketplace.canonical.name != "sothis" {
        errors.push("canonical marketplace must be named sothis".to_string());
    }
    if manifest.marketplace.canonical.repository != "Davidb3l/Sothis" {
        errors.push("canonical marketplace repository must be Davidb3l/Sothis".to_string());
    }
    if Version::parse(&manifest.suite_version).is_err() {
        errors.push(format!(
            "suiteVersion is not semantic versioning: {}",
            manifest.suite_version
        ));
    }
    for legacy in &manifest.marketplace.legacy {
        if legacy.name.trim().is_empty() || legacy.repository.trim().is_empty() {
            errors.push("legacy marketplace entries require a name and repository".to_string());
        }
        if legacy.remove_after_suite_releases == 0 {
            errors.push(format!(
                "legacy marketplace {} has a zero-release migration window",
                legacy.name
            ));
        }
    }

    let mut ids = BTreeSet::new();
    let mut plugin_names = BTreeSet::new();
    let required_platforms = [
        "linux-x64",
        "linux-arm64",
        "macos-x64",
        "macos-arm64",
        "windows-x64",
        "windows-arm64",
    ];
    for tool in &manifest.tools {
        if tool.id.is_empty()
            || !tool
                .id
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        {
            errors.push(format!("invalid tool id: {:?}", tool.id));
        }
        if tool.name.trim().is_empty() {
            errors.push(format!("{} has an empty name", tool.id));
        }
        if !tool.contracts.is_null() && !tool.contracts.is_object() {
            errors.push(format!("{} contracts must be a JSON object", tool.id));
        }
        if tool.repository.split('/').count() != 2
            || tool.repository.contains("..")
            || tool.repository.contains('\\')
        {
            errors.push(format!(
                "{} has an invalid GitHub owner/repository value: {}",
                tool.id, tool.repository
            ));
        }
        if !ids.insert(tool.id.as_str()) {
            errors.push(format!("duplicate tool id: {}", tool.id));
        }
        if tool.platforms.is_empty() {
            errors.push(format!("{} has no platform declarations", tool.id));
        }
        for platform in required_platforms {
            if !tool.platforms.contains_key(platform) {
                errors.push(format!("{} does not declare {platform}", tool.id));
            }
        }
        for (platform, support) in &tool.platforms {
            if !required_platforms.contains(&platform.as_str()) {
                errors.push(format!("{} declares unknown platform {platform}", tool.id));
            }
            if !matches!(
                support.support.as_str(),
                "native"
                    | "native-preview"
                    | "plugin-runtime"
                    | "source-only"
                    | "wsl2"
                    | "unsupported"
            ) {
                errors.push(format!(
                    "{} has unknown support value {} for {platform}",
                    tool.id, support.support
                ));
            }
            if matches!(support.support.as_str(), "native" | "native-preview")
                && tool.distribution.kind == "tool-installer"
                && !tool.distribution.assets.contains_key(platform)
            {
                errors.push(format!(
                    "{} is {} on {platform} but has no release asset",
                    tool.id, support.support
                ));
            }
        }
        if let Some(command) = &tool.command {
            if command.trim().is_empty()
                || command.contains('/')
                || command.contains('\\')
                || command.chars().any(char::is_whitespace)
            {
                errors.push(format!("{} has an invalid command: {command:?}", tool.id));
            }
        }
        let minimum = tool
            .minimum_version
            .as_deref()
            .map(|value| Version::parse(value).map_err(|_| value));
        let recommended = tool
            .recommended_version
            .as_deref()
            .map(|value| Version::parse(value).map_err(|_| value));
        if let Some(Err(value)) = &minimum {
            errors.push(format!("{} has invalid minimumVersion {value}", tool.id));
        }
        if let Some(Err(value)) = &recommended {
            errors.push(format!(
                "{} has invalid recommendedVersion {value}",
                tool.id
            ));
        }
        if let (Some(Ok(minimum)), Some(Ok(recommended))) = (&minimum, &recommended) {
            if recommended < minimum {
                errors.push(format!(
                    "{} recommends {recommended}, below minimum {minimum}",
                    tool.id
                ));
            }
        }
        if !matches!(
            tool.distribution.kind.as_str(),
            "tool-installer" | "plugin-runtime" | "source"
        ) {
            errors.push(format!(
                "{} has unknown distribution kind {}",
                tool.id, tool.distribution.kind
            ));
        }
        if !matches!(
            tool.distribution.verification.as_str(),
            "sha256" | "sha256+sigstore" | "plugin-manager" | "source-build"
        ) {
            errors.push(format!(
                "{} has unknown verification mode {}",
                tool.id, tool.distribution.verification
            ));
        }
        match tool.distribution.kind.as_str() {
            "tool-installer" => {
                if !matches!(
                    tool.distribution.verification.as_str(),
                    "sha256" | "sha256+sigstore"
                ) {
                    errors.push(format!(
                        "{} tool-installer must use checksum verification",
                        tool.id
                    ));
                }
                if tool.command.is_none()
                    || tool.minimum_version.is_none()
                    || tool.recommended_version.is_none()
                {
                    errors.push(format!(
                        "{} tool-installer requires command, minimumVersion, and recommendedVersion",
                        tool.id
                    ));
                }
                if tool.distribution.assets.is_empty() {
                    errors.push(format!("{} tool-installer has no release assets", tool.id));
                }
                match &tool.distribution.files {
                    None => errors.push(format!(
                        "{} tool-installer requires a files list of payload entries",
                        tool.id
                    )),
                    Some(files) if files.is_empty() => errors.push(format!(
                        "{} tool-installer has an empty files list",
                        tool.id
                    )),
                    Some(files) => {
                        let mut seen = BTreeSet::new();
                        for entry in files {
                            if let Err(reason) = validate_file_entry(entry) {
                                errors.push(format!(
                                    "{} has an invalid files entry {entry:?}: {reason}",
                                    tool.id
                                ));
                            }
                            if !seen.insert(entry.as_str()) {
                                errors.push(format!(
                                    "{} has a duplicate files entry {entry:?}",
                                    tool.id
                                ));
                            }
                        }
                    }
                }
            }
            "plugin-runtime" => {
                if tool.distribution.verification != "plugin-manager" {
                    errors.push(format!(
                        "{} plugin-runtime must use plugin-manager verification",
                        tool.id
                    ));
                }
                if tool.plugin.is_none() || tool.distribution.runtime.is_none() {
                    errors.push(format!(
                        "{} plugin-runtime requires plugin and runtime declarations",
                        tool.id
                    ));
                }
                if !tool.distribution.assets.is_empty() {
                    errors.push(format!(
                        "{} plugin-runtime must not declare release assets",
                        tool.id
                    ));
                }
            }
            "source" => {
                if tool.distribution.verification != "source-build" {
                    errors.push(format!(
                        "{} source distribution must use source-build verification",
                        tool.id
                    ));
                }
                if !tool.distribution.assets.is_empty() {
                    errors.push(format!(
                        "{} source distribution must not declare release assets",
                        tool.id
                    ));
                }
            }
            _ => {}
        }
        if tool.distribution.kind != "tool-installer"
            && (tool.distribution.checksum_suffix.is_some()
                || tool.distribution.sigstore_identity.is_some())
        {
            errors.push(format!(
                "{} non-installer distribution must not declare checksum or Sigstore metadata",
                tool.id
            ));
        }
        if tool.distribution.kind != "tool-installer" && tool.distribution.files.is_some() {
            errors.push(format!(
                "{} non-installer distribution must not declare a files list",
                tool.id
            ));
        }
        if let Some(minimum) = &tool.distribution.runtime_minimum_version {
            if Version::parse(minimum).is_err() {
                errors.push(format!(
                    "{} has invalid runtimeMinimumVersion {minimum}",
                    tool.id
                ));
            }
            if tool.distribution.kind != "plugin-runtime" {
                errors.push(format!(
                    "{} declares runtimeMinimumVersion but is not a plugin-runtime distribution",
                    tool.id
                ));
            } else if tool.distribution.runtime.is_none() {
                errors.push(format!(
                    "{} declares runtimeMinimumVersion without a runtime",
                    tool.id
                ));
            }
        }
        if tool.distribution.verification.contains("sha256")
            && tool.distribution.checksum_suffix.as_deref() != Some(".sha256")
        {
            errors.push(format!(
                "{} checksum verification requires .sha256",
                tool.id
            ));
        }
        if tool.distribution.verification == "sha256+sigstore"
            && tool.distribution.sigstore_identity.is_none()
        {
            errors.push(format!("{} is missing a Sigstore identity", tool.id));
        }
        if let Some(identity) = &tool.distribution.sigstore_identity {
            let expected = format!(
                "https://github.com/{}/.github/workflows/release.yml@refs/tags/v{{version}}",
                tool.repository
            );
            if tool.distribution.verification != "sha256+sigstore" {
                errors.push(format!(
                    "{} declares a Sigstore identity without sha256+sigstore verification",
                    tool.id
                ));
            }
            if identity != &expected {
                errors.push(format!(
                    "{} has unexpected Sigstore identity {}; expected {}",
                    tool.id, identity, expected
                ));
            }
        }
        for (platform, asset) in &tool.distribution.assets {
            let support = tool.platforms.get(platform);
            if support.is_none() {
                errors.push(format!(
                    "{} has an asset for unknown platform {platform}",
                    tool.id
                ));
            } else if !matches!(
                support.map(|value| value.support.as_str()),
                Some("native" | "native-preview")
            ) {
                errors.push(format!(
                    "{} has a release asset for non-native platform {platform}",
                    tool.id
                ));
            }
            if asset.trim().is_empty()
                || asset.contains('/')
                || asset.contains('\\')
                || asset.contains("..")
            {
                errors.push(format!(
                    "{} has an unsafe asset name for {platform}",
                    tool.id
                ));
            }
            if asset.contains("{version}") && tool.recommended_version.is_none() {
                errors.push(format!(
                    "{} asset for {platform} needs a recommendedVersion",
                    tool.id
                ));
            }
            if asset
                .replace("{version}", "")
                .chars()
                .any(|ch| ch == '{' || ch == '}')
            {
                errors.push(format!(
                    "{} asset for {platform} contains an unknown template placeholder",
                    tool.id
                ));
            }
        }
        if let Some(plugin) = &tool.plugin {
            if plugin.name.is_empty()
                || !plugin
                    .name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
            {
                errors.push(format!("{} has an invalid plugin name", tool.id));
            }
            if let Some(minimum) = &plugin.minimum_version {
                if Version::parse(minimum).is_err() {
                    errors.push(format!(
                        "{} has invalid plugin minimumVersion {minimum}",
                        tool.id
                    ));
                }
            }
            if !plugin_names.insert(plugin.name.as_str()) {
                errors.push(format!("duplicate plugin name: {}", plugin.name));
            }
            if plugin.marketplace != manifest.marketplace.canonical.name {
                errors.push(format!(
                    "{} plugin points at marketplace {}, expected {}",
                    tool.id, plugin.marketplace, manifest.marketplace.canonical.name
                ));
            }
        }
    }

    for (name, profile) in &manifest.profiles {
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        {
            errors.push(format!("invalid profile name: {name:?}"));
        }
        if profile.description.trim().is_empty() {
            errors.push(format!("profile {name} has an empty description"));
        }
        if profile.tools.is_empty() {
            errors.push(format!("profile {name} is empty"));
        }
        let mut seen = BTreeSet::new();
        for id in &profile.tools {
            if !ids.contains(id.as_str()) {
                errors.push(format!("profile {name} references unknown tool {id}"));
            }
            if !seen.insert(id) {
                errors.push(format!("profile {name} contains duplicate tool {id}"));
            }
        }
    }

    let expected_core = ["ametrite", "hayvenhurst", "sirius-forester"];
    match manifest.profiles.get("core") {
        Some(core) => {
            for id in expected_core {
                if !core.tools.iter().any(|tool| tool == id) {
                    errors.push(format!("core profile is missing {id}"));
                }
            }
            if core.tools.len() != expected_core.len() {
                errors
                    .push("core profile must contain exactly the three runtime tools".to_string());
            }
        }
        None => errors.push("missing core profile".to_string()),
    }

    match manifest.profiles.get("full") {
        Some(full) => {
            let full_ids = full
                .tools
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            if full_ids != ids {
                errors.push("full profile must contain every tool exactly once".to_string());
            }
        }
        None => errors.push("missing full profile".to_string()),
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_is_valid() {
        let manifest = embedded().expect("manifest parses");
        assert_eq!(validate(&manifest), Vec::<String>::new());
    }

    #[test]
    fn every_profile_reference_resolves_once() {
        let manifest = embedded().unwrap();
        let ids = manifest
            .tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<BTreeSet<_>>();
        for profile in manifest.profiles.values() {
            assert_eq!(
                profile.tools.len(),
                profile.tools.iter().collect::<BTreeSet<_>>().len()
            );
            assert!(profile.tools.iter().all(|id| ids.contains(id.as_str())));
        }
    }

    #[test]
    fn unknown_fields_fail_closed() {
        let body = include_str!("../suite.manifest.json").replace("suiteVersion", "suiteVersoin");
        assert!(serde_json::from_str::<Manifest>(&body).is_err());
    }

    #[test]
    fn missing_legacy_marketplaces_fail_closed_but_an_empty_list_is_valid() {
        let mut body: serde_json::Value =
            serde_json::from_str(include_str!("../suite.manifest.json")).unwrap();
        let marketplace = body
            .get_mut("marketplace")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        marketplace.remove("legacy");
        assert!(serde_json::from_value::<Manifest>(body.clone()).is_err());

        body["marketplace"]["legacy"] = serde_json::json!([]);
        let manifest: Manifest = serde_json::from_value(body).unwrap();
        assert_eq!(validate(&manifest), Vec::<String>::new());
    }

    #[test]
    fn native_tools_require_an_asset_for_the_platform() {
        let mut manifest = embedded().unwrap();
        let sirius = manifest
            .tools
            .iter_mut()
            .find(|tool| tool.id == "sirius-forester")
            .unwrap();
        sirius.distribution.assets.remove("windows-x64");
        assert!(validate(&manifest)
            .iter()
            .any(|error| error.contains("no release asset")));
    }

    #[test]
    fn distribution_kinds_reject_incompatible_metadata() {
        let mut manifest = embedded().unwrap();
        let catryna = manifest
            .tools
            .iter_mut()
            .find(|tool| tool.id == "catryna-wikinelli")
            .unwrap();
        catryna.distribution.verification = "sha256".to_string();
        catryna
            .distribution
            .assets
            .insert("windows-x64".to_string(), "catryna.zip".to_string());
        let errors = validate(&manifest).join("; ");
        assert!(errors.contains("plugin-runtime must use plugin-manager"));
        assert!(errors.contains("plugin-runtime must not declare release assets"));
        assert!(errors.contains("release asset for non-native platform"));
    }

    #[test]
    fn serialized_manifest_round_trips_without_schema_nulls() {
        let manifest = embedded().unwrap();
        let serialized = serde_json::to_string(&manifest).unwrap();
        assert!(!serialized.contains(":null"));
        let reparsed: Manifest = serde_json::from_str(&serialized).unwrap();
        assert!(validate(&reparsed).is_empty());
    }

    #[test]
    fn published_schema_is_valid_json_and_fail_closed_at_the_root() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/suite-manifest.schema.json")).unwrap();
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            schema.pointer("/$defs/tool/additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    fn tool_mut<'a>(manifest: &'a mut Manifest, id: &str) -> &'a mut Tool {
        manifest
            .tools
            .iter_mut()
            .find(|tool| tool.id == id)
            .unwrap()
    }

    #[test]
    fn installer_tools_declare_their_payload_files() {
        let manifest = embedded().unwrap();
        let files = |id: &str| {
            manifest
                .tools
                .iter()
                .find(|tool| tool.id == id)
                .unwrap()
                .distribution
                .files
                .clone()
                .unwrap()
        };
        assert_eq!(files("ametrite"), ["amt{exe}"]);
        assert_eq!(
            files("hayvenhurst"),
            ["hayven{exe}", "hayven-native{exe}", "skill", "viewer"]
        );
        assert_eq!(files("sirius-forester"), ["sirius{exe}"]);
        assert_eq!(expand_file_entry("amt{exe}", ".exe"), "amt.exe");
        assert_eq!(expand_file_entry("amt{exe}", ""), "amt");
        assert_eq!(expand_file_entry("skill", ".exe"), "skill");
    }

    #[test]
    fn recommended_versions_track_the_published_releases() {
        let manifest = embedded().unwrap();
        let recommended = |id: &str| {
            manifest
                .tools
                .iter()
                .find(|tool| tool.id == id)
                .unwrap()
                .recommended_version
                .clone()
                .unwrap()
        };
        assert_eq!(recommended("ametrite"), "0.4.2");
        assert_eq!(recommended("hayvenhurst"), "0.0.8");
        assert_eq!(recommended("sirius-forester"), "0.1.7");
    }

    #[test]
    fn tool_installers_require_a_non_empty_files_list() {
        let mut manifest = embedded().unwrap();
        tool_mut(&mut manifest, "ametrite").distribution.files = None;
        assert!(validate(&manifest)
            .iter()
            .any(|error| error.contains("requires a files list")));

        tool_mut(&mut manifest, "ametrite").distribution.files = Some(Vec::new());
        assert!(validate(&manifest)
            .iter()
            .any(|error| error.contains("empty files list")));
    }

    #[test]
    fn files_are_forbidden_for_other_distribution_kinds() {
        let mut manifest = embedded().unwrap();
        tool_mut(&mut manifest, "catryna-wikinelli")
            .distribution
            .files = Some(vec!["catryna".to_string()]);
        tool_mut(&mut manifest, "guignet").distribution.files = Some(vec!["guignet".to_string()]);
        let errors = validate(&manifest);
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.contains("must not declare a files list"))
                .count(),
            2,
            "{errors:?}"
        );
    }

    #[test]
    fn unsafe_and_duplicate_files_entries_are_rejected() {
        let mut manifest = embedded().unwrap();
        tool_mut(&mut manifest, "hayvenhurst").distribution.files = Some(
            [
                "hayven{exe}",
                "hayven{exe}",
                "../evil",
                "sub/dir",
                r"back\slash",
                "{version}",
                "",
            ]
            .map(String::from)
            .to_vec(),
        );
        let errors = validate(&manifest).join("; ");
        assert!(errors.contains("duplicate files entry"), "{errors}");
        assert!(errors.contains("\"../evil\""), "{errors}");
        assert!(errors.contains("\"sub/dir\""), "{errors}");
        assert!(errors.contains(r#""back\\slash""#), "{errors}");
        assert!(errors.contains("\"{version}\""), "{errors}");
        assert!(errors.contains("\"\""), "{errors}");
    }

    #[test]
    fn manifest_files_field_rejects_unknown_neighbours() {
        let body = include_str!("../suite.manifest.json").replace("\"files\"", "\"fiels\"");
        assert!(serde_json::from_str::<Manifest>(&body).is_err());
    }

    /// The Rust validator and the published schema describe the same `files`
    /// entry grammar. There is no JSON-schema engine in the crate, so this
    /// evaluates the schema's own `pattern` / `not.pattern` for a corpus of
    /// good and bad names and requires the answers to match the validator.
    #[test]
    fn files_entry_grammar_matches_the_published_schema() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/suite-manifest.schema.json")).unwrap();
        let entry = schema.pointer("/$defs/fileEntry").expect("fileEntry def");
        let pattern =
            regex::Regex::new(entry["pattern"].as_str().expect("fileEntry pattern")).unwrap();
        let forbidden = regex::Regex::new(
            entry
                .pointer("/not/pattern")
                .and_then(serde_json::Value::as_str)
                .expect("fileEntry not.pattern"),
        )
        .unwrap();
        let corpus = [
            "amt{exe}",
            "hayven-native{exe}",
            "skill",
            "viewer",
            "a.b",
            "x_y+z",
            "",
            ".hidden",
            "..",
            "a..b",
            "a/b",
            r"a\b",
            "{exe}",
            "{exe}amt",
            "amt{exe",
            "amt{EXE}",
            "amt{version}",
            "amt}",
            "a b",
            "a:b",
            "ü",
            "amt{exe}{exe}",
        ];
        for name in corpus {
            let schema_accepts = pattern.is_match(name) && !forbidden.is_match(name);
            assert_eq!(
                schema_accepts,
                validate_file_entry(name).is_ok(),
                "schema and validator disagree on {name:?}"
            );
        }
        assert_eq!(
            schema.pointer("/$defs/distribution/properties/files/uniqueItems"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            schema.pointer("/$defs/distribution/properties/files/minItems"),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn schema_requires_files_exactly_for_tool_installers() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/suite-manifest.schema.json")).unwrap();
        let all_of = schema
            .pointer("/$defs/distribution/allOf")
            .and_then(serde_json::Value::as_array)
            .unwrap();
        let branch = |kind: &str| {
            all_of
                .iter()
                .find(|branch| {
                    branch.pointer("/if/properties/kind/const") == Some(&serde_json::json!(kind))
                })
                .unwrap_or_else(|| panic!("no schema branch for {kind}"))
        };
        let required = branch("tool-installer")
            .pointer("/then/required")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .clone();
        assert!(required.contains(&serde_json::json!("files")));
        for kind in ["plugin-runtime", "source"] {
            let forbidden = branch(kind)
                .pointer("/then/not/anyOf")
                .and_then(serde_json::Value::as_array)
                .unwrap();
            assert!(
                forbidden.contains(&serde_json::json!({ "required": ["files"] })),
                "{kind} must forbid files"
            );
        }
    }

    fn tool<'a>(manifest: &'a Manifest, id: &str) -> &'a Tool {
        manifest.tools.iter().find(|tool| tool.id == id).unwrap()
    }

    #[test]
    fn manifest_facts_from_the_per_repo_reviews_are_encoded() {
        let manifest = embedded().unwrap();
        // 0.3.0 bound the board to 127.0.0.1 and added Host/cross-origin guards.
        let ametrite = tool(&manifest, "ametrite");
        assert_eq!(ametrite.minimum_version.as_deref(), Some("0.3.0"));
        assert_eq!(ametrite.recommended_version.as_deref(), Some("0.4.2"));
        // The SQLite meta.schema_version, not `amt doctor --json`'s envelope.
        assert_eq!(ametrite.contracts["minimumSchemaVersion"], 3);

        let catryna = tool(&manifest, "catryna-wikinelli");
        let plugin = catryna.plugin.as_ref().unwrap();
        assert_eq!(plugin.minimum_version.as_deref(), Some("1.3.1"));
        assert_eq!(
            catryna.distribution.runtime_minimum_version.as_deref(),
            Some("1.2.0")
        );
        let note = catryna.platforms["windows-x64"].note.as_deref().unwrap();
        assert!(note.contains("shell-free from 1.3.1"), "{note}");

        let bell = tool(&manifest, "pingmybell");
        assert!(bell.command.is_none());
        assert_eq!(bell.platforms["windows-x64"].support, "source-only");
        assert!(bell.platforms["windows-x64"]
            .note
            .as_deref()
            .unwrap()
            .contains("passes its test suite on Windows 11"));
        for platform in ["windows-arm64", "linux-x64", "linux-arm64"] {
            assert_eq!(
                bell.platforms[platform].support, "unsupported",
                "{platform}"
            );
        }
        assert_eq!(
            tool(&manifest, "guignet").command.as_deref(),
            Some("guignet")
        );
    }

    #[test]
    fn plugin_and_runtime_minimum_versions_must_be_semver() {
        let mut manifest = embedded().unwrap();
        tool_mut(&mut manifest, "catryna-wikinelli")
            .plugin
            .as_mut()
            .unwrap()
            .minimum_version = Some("1.3".to_string());
        tool_mut(&mut manifest, "catryna-wikinelli")
            .distribution
            .runtime_minimum_version = Some("latest".to_string());
        let errors = validate(&manifest).join("; ");
        assert!(
            errors.contains("invalid plugin minimumVersion 1.3"),
            "{errors}"
        );
        assert!(
            errors.contains("invalid runtimeMinimumVersion latest"),
            "{errors}"
        );
    }

    #[test]
    fn runtime_minimum_version_is_for_plugin_runtime_distributions_only() {
        let mut manifest = embedded().unwrap();
        tool_mut(&mut manifest, "guignet")
            .distribution
            .runtime_minimum_version = Some("1.2.0".to_string());
        tool_mut(&mut manifest, "ametrite")
            .distribution
            .runtime_minimum_version = Some("1.2.0".to_string());
        let errors = validate(&manifest);
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.contains("is not a plugin-runtime distribution"))
                .count(),
            2,
            "{errors:?}"
        );
    }

    #[test]
    fn new_optional_fields_stay_optional_and_round_trip() {
        let mut manifest = embedded().unwrap();
        tool_mut(&mut manifest, "catryna-wikinelli")
            .plugin
            .as_mut()
            .unwrap()
            .minimum_version = None;
        tool_mut(&mut manifest, "catryna-wikinelli")
            .distribution
            .runtime_minimum_version = None;
        assert_eq!(validate(&manifest), Vec::<String>::new());
        let serialized = serde_json::to_string(&manifest).unwrap();
        assert!(!serialized.contains("minimumVersion\":null"));
        assert!(!serialized.contains("runtimeMinimumVersion"));
        let again = embedded().unwrap();
        let text = serde_json::to_string(&again).unwrap();
        assert!(text.contains("\"runtimeMinimumVersion\":\"1.2.0\""));
        assert!(text.contains("\"minimumVersion\":\"1.3.1\""));
    }

    /// The schema's `version` pattern and `semver::Version::parse` (what the
    /// validator uses) must accept and reject the same strings.
    #[test]
    fn schema_version_pattern_matches_the_validator() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/suite-manifest.schema.json")).unwrap();
        let pattern = regex::Regex::new(
            schema
                .pointer("/$defs/version/pattern")
                .unwrap()
                .as_str()
                .unwrap(),
        )
        .unwrap();
        for text in [
            "0.3.0",
            "1.3.1",
            "10.20.30",
            "1.0.0-rc.1",
            "1.0.0+build.5",
            "1.3",
            "1",
            "v1.3.1",
            "01.2.3",
            "1.2.3.4",
            "latest",
            "",
            " 1.2.3",
            "1.2.-3",
        ] {
            assert_eq!(
                pattern.is_match(text),
                Version::parse(text).is_ok(),
                "schema and validator disagree on {text:?}"
            );
        }
    }

    #[test]
    fn schema_declares_the_new_optional_fields_in_the_right_places() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/suite-manifest.schema.json")).unwrap();
        let version_ref = serde_json::json!("#/$defs/version");
        assert_eq!(
            schema.pointer("/$defs/plugin/properties/minimumVersion/$ref"),
            Some(&version_ref)
        );
        assert_eq!(
            schema.pointer("/$defs/distribution/properties/runtimeMinimumVersion/$ref"),
            Some(&version_ref)
        );
        // Optional: never in a required list.
        assert!(!schema["$defs"]["plugin"]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("minimumVersion")));
        let all_of = schema
            .pointer("/$defs/distribution/allOf")
            .and_then(serde_json::Value::as_array)
            .unwrap();
        let branch = |kind: &str| {
            all_of
                .iter()
                .find(|branch| {
                    branch.pointer("/if/properties/kind/const") == Some(&serde_json::json!(kind))
                })
                .unwrap()
        };
        let forbidden = serde_json::json!({ "required": ["runtimeMinimumVersion"] });
        assert_eq!(
            branch("tool-installer").pointer("/then/not"),
            Some(&forbidden)
        );
        assert!(branch("source")
            .pointer("/then/not/anyOf")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .contains(&forbidden));
        assert!(!branch("plugin-runtime")
            .pointer("/then/not/anyOf")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .contains(&forbidden));
    }

    /// The README and CONTRACTS.md state manifest facts in prose. Pin the ones
    /// that would silently rot if the manifest changed.
    #[test]
    fn docs_agree_with_the_manifest() {
        let readme = include_str!("../README.md");
        let contracts = include_str!("../docs/CONTRACTS.md");
        let manifest = embedded().unwrap();
        for name in manifest.profiles.keys() {
            assert!(
                readme.contains(&format!("| `{name}`")),
                "README profile table lacks `{name}`"
            );
        }
        let catryna = tool(&manifest, "catryna-wikinelli");
        let plugin_minimum = catryna
            .plugin
            .as_ref()
            .unwrap()
            .minimum_version
            .clone()
            .unwrap();
        let runtime_minimum = catryna
            .distribution
            .runtime_minimum_version
            .clone()
            .unwrap();
        assert!(readme.contains(&format!("needs `{plugin_minimum}`")));
        assert!(readme.contains(&format!("Bun (>= {runtime_minimum})")));
        assert!(contracts.contains(&format!("Catryna requires `{plugin_minimum}`")));
        // The schema numbers CONTRACTS.md explains are the manifest's.
        let ametrite = tool(&manifest, "ametrite");
        let minimum_schema = ametrite.contracts["minimumSchemaVersion"].as_u64().unwrap();
        assert!(contracts.contains(&format!("requires at least `{minimum_schema}`")));
        let sirius = tool(&manifest, "sirius-forester");
        assert_eq!(
            sirius.contracts["minimumAmetriteSchemaVersion"]
                .as_u64()
                .unwrap(),
            minimum_schema
        );
        // Tools without a CLI are not advertised with one.
        let bell = tool(&manifest, "pingmybell");
        assert!(bell.command.is_none());
        let bell_row = readme
            .lines()
            .find(|line| line.starts_with("| PingMyBell"))
            .unwrap();
        assert!(!bell_row.contains("`pingmybell`"), "{bell_row}");
        // The plan the README promises for Windows matches the manifest.
        assert_eq!(bell.platforms["windows-x64"].support, "source-only");
        assert!(readme.contains("PingMyBell builds from source on Windows"));
    }
}
