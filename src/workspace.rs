use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const CONFIG: &str = "{\n  \"schemaVersion\": 1,\n  \"profile\": \"core\"\n}\n";
const IGNORE: &str = "events/\nstate/\nstaging/\nrollback/\n";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Config {
    schema_version: u32,
    profile: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitResult {
    pub root: PathBuf,
    pub config: PathBuf,
    pub created_config: bool,
    pub updated_ignore: bool,
}

pub fn init(root: &Path) -> Result<InitResult, String> {
    let suite = root.join(".suite");
    fs::create_dir_all(&suite).map_err(|e| format!("could not create {}: {e}", suite.display()))?;
    let config = suite.join("config.json");
    let ignore = suite.join(".gitignore");
    let created_config = create_once(&config, CONFIG)?;
    validate_config(&config)?;
    let updated_ignore = ensure_ignore(&ignore)?;
    Ok(InitResult {
        root: root.to_path_buf(),
        config,
        created_config,
        updated_ignore,
    })
}

pub fn configured_profile(start: &Path) -> Result<Option<String>, String> {
    let mut current = Some(start);
    while let Some(directory) = current {
        let config = directory.join(".suite").join("config.json");
        if config.exists() {
            return read_config(&config).map(|config| Some(config.profile));
        }
        current = directory.parent();
    }
    Ok(None)
}

fn create_once(path: &Path, body: &str) -> Result<bool, String> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            let result = file
                .write_all(body.as_bytes())
                .map_err(|e| format!("could not write {}: {e}", path.display()))
                .and_then(|_| {
                    file.sync_all()
                        .map_err(|e| format!("could not sync {}: {e}", path.display()))
                });
            if let Err(error) = result {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(error);
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!("could not create {}: {error}", path.display())),
    }
}

fn validate_config(path: &Path) -> Result<(), String> {
    read_config(path).map(|_| ())
}

fn read_config(path: &Path) -> Result<Config, String> {
    let body = fs::read_to_string(path).map_err(|e| {
        format!(
            "existing config {} is not a readable file: {e}",
            path.display()
        )
    })?;
    let config: Config = serde_json::from_str(&body)
        .map_err(|e| format!("existing config {} is invalid JSON: {e}", path.display()))?;
    if config.schema_version != 1 {
        return Err(format!(
            "existing config {} must declare schemaVersion 1",
            path.display()
        ));
    }
    if config.profile.trim().is_empty() {
        return Err(format!(
            "existing config {} must declare a non-empty profile",
            path.display()
        ));
    }
    Ok(config)
}

fn ensure_ignore(path: &Path) -> Result<bool, String> {
    if create_once(path, IGNORE)? {
        return Ok(true);
    }
    let existing = fs::read_to_string(path).map_err(|e| {
        format!(
            "existing ignore {} is not a readable file: {e}",
            path.display()
        )
    })?;
    let missing = IGNORE
        .lines()
        .filter(|required| !existing.lines().any(|line| line.trim() == *required))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(false);
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("could not update {}: {e}", path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|e| format!("could not update {}: {e}", path.display()))?;
    }
    for line in missing {
        writeln!(file, "{line}")
            .map_err(|e| format!("could not update {}: {e}", path.display()))?;
    }
    file.sync_all()
        .map_err(|e| format!("could not sync {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn init_is_idempotent_and_never_overwrites_config() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sothis init spaces ü-{nonce}"));
        fs::create_dir_all(&root).unwrap();

        let first = init(&root).unwrap();
        assert!(first.created_config);
        assert!(first.updated_ignore);

        let second = init(&root).unwrap();
        assert!(!second.created_config);
        assert!(!second.updated_ignore);
        assert_eq!(fs::read_to_string(&first.config).unwrap(), CONFIG);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_rejects_invalid_existing_config() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sothis-invalid-config-{nonce}"));
        fs::create_dir_all(root.join(".suite")).unwrap();
        fs::write(root.join(".suite/config.json"), "not json").unwrap();
        let error = init(&root).unwrap_err();
        assert!(error.contains("invalid JSON"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_rejects_unknown_config_fields() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sothis-unknown-config-{nonce}"));
        fs::create_dir_all(root.join(".suite")).unwrap();
        fs::write(
            root.join(".suite/config.json"),
            r#"{"schemaVersion":1,"profile":"core","typo":true}"#,
        )
        .unwrap();
        let error = init(&root).unwrap_err();
        assert!(error.contains("unknown field"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_repairs_missing_runtime_ignores_without_removing_user_rules() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sothis-ignore-merge-{nonce}"));
        fs::create_dir_all(root.join(".suite")).unwrap();
        fs::write(root.join(".suite/config.json"), CONFIG).unwrap();
        fs::write(root.join(".suite/.gitignore"), "custom/\nevents/\n").unwrap();
        let result = init(&root).unwrap();
        assert!(result.updated_ignore);
        let ignore = fs::read_to_string(root.join(".suite/.gitignore")).unwrap();
        assert!(ignore.contains("custom/"));
        for required in IGNORE.lines() {
            assert!(ignore.lines().any(|line| line == required));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_profile_walks_to_the_workspace_root() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sothis-profile-walk-{nonce}"));
        let child = root.join("nested").join("deeper");
        fs::create_dir_all(&child).unwrap();
        init(&root).unwrap();
        assert_eq!(configured_profile(&child).unwrap().as_deref(), Some("core"));
        fs::remove_dir_all(root).unwrap();
    }
}
