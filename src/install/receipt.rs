//! Ownership receipts: `<root>/state/receipts/<tool>.json`.
//!
//! A receipt is the only thing that makes a file in the bin directory "owned"
//! by Sothis. It is written atomically (tmp file + rename) and read
//! fail-closed: unknown fields, wrong schema version or malformed values are
//! errors, never silently ignored.

use crate::install::fsx;
use crate::install::layout::Layout;
use crate::install::verify::SignatureStatus;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptFile {
    pub path: String,
    pub kind: FileKind,
    /// sha256 hex for files; `null` for directories.
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adopted {
    /// The bin entry that existed before Sothis placed its own.
    pub path: String,
    /// Absolute path under `<root>/backups/<tool>/` holding the original.
    pub backup: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Receipt {
    pub schema_version: u32,
    pub tool: String,
    pub version: String,
    pub installed_at: String,
    pub platform: String,
    pub asset: String,
    pub asset_sha256: String,
    pub signature: SignatureStatus,
    pub bin_dir: PathBuf,
    pub files: Vec<ReceiptFile>,
    pub previous_version: Option<String>,
    pub adopted: Vec<Adopted>,
}

fn safe_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && !name.contains("..")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.contains('\0')
}

impl Receipt {
    /// Structural validation shared by read and write.
    pub fn validate(&self, expected_tool: &str) -> Result<(), String> {
        let bad = |why: String| Err(format!("receipt for {expected_tool} is invalid: {why}"));
        if self.schema_version != RECEIPT_SCHEMA_VERSION {
            return bad(format!(
                "unsupported schemaVersion {} (this binary supports {RECEIPT_SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        if self.tool != expected_tool {
            return bad(format!("it names tool {:?}", self.tool));
        }
        if Version::parse(&self.version).is_err() {
            return bad(format!("version {:?} is not semver", self.version));
        }
        if let Some(previous) = &self.previous_version {
            if Version::parse(previous).is_err() {
                return bad(format!("previousVersion {previous:?} is not semver"));
            }
        }
        if !fsx::is_sha256_hex(&self.asset_sha256) {
            return bad("assetSha256 is not a 64-digit hex digest".to_string());
        }
        if !self.bin_dir.is_absolute() {
            return bad("binDir is not absolute".to_string());
        }
        if self.files.is_empty() {
            return bad("it lists no files".to_string());
        }
        for file in &self.files {
            if !safe_entry_name(&file.path) {
                return bad(format!("unsafe file path {:?}", file.path));
            }
            match (file.kind, &file.sha256) {
                (FileKind::File, Some(hash)) if fsx::is_sha256_hex(hash) => {}
                (FileKind::Dir, None) => {}
                _ => {
                    return bad(format!(
                        "file {:?} has an inconsistent kind/sha256 pair",
                        file.path
                    ))
                }
            }
        }
        for adopted in &self.adopted {
            if !safe_entry_name(&adopted.path) {
                return bad(format!("unsafe adopted path {:?}", adopted.path));
            }
            if !Path::new(&adopted.backup).is_absolute() {
                return bad(format!(
                    "adopted backup {:?} is not absolute",
                    adopted.backup
                ));
            }
        }
        Ok(())
    }

    pub fn owns(&self, entry: &str) -> bool {
        self.files.iter().any(|file| file.path == entry)
    }
}

/// Per-version record kept inside `tools/<id>/<version>/` so `rollback` can
/// re-place that version with its original provenance.
pub const INSTALL_META_FILE: &str = ".sothis-install.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InstallMeta {
    pub schema_version: u32,
    pub asset: String,
    pub asset_sha256: String,
    pub signature: SignatureStatus,
    pub files: Vec<ReceiptFile>,
}

impl InstallMeta {
    pub fn validate(&self) -> Result<(), String> {
        let bad = |why: String| Err(format!("install metadata is invalid: {why}"));
        if self.schema_version != RECEIPT_SCHEMA_VERSION {
            return bad(format!("unsupported schemaVersion {}", self.schema_version));
        }
        if !fsx::is_sha256_hex(&self.asset_sha256) {
            return bad("assetSha256 is not a 64-digit hex digest".to_string());
        }
        if self.files.is_empty() {
            return bad("it lists no files".to_string());
        }
        for file in &self.files {
            if !safe_entry_name(&file.path) {
                return bad(format!("unsafe file path {:?}", file.path));
            }
            match (file.kind, &file.sha256) {
                (FileKind::File, Some(hash)) if fsx::is_sha256_hex(hash) => {}
                (FileKind::Dir, None) => {}
                _ => {
                    return bad(format!(
                        "file {:?} has an inconsistent kind/sha256",
                        file.path
                    ))
                }
            }
        }
        Ok(())
    }
}

pub fn write_meta(version_dir: &Path, meta: &InstallMeta) -> Result<(), String> {
    meta.validate()?;
    let body = serde_json::to_string_pretty(meta)
        .map_err(|e| format!("could not serialize install metadata: {e}"))?;
    write_atomic(
        &version_dir.join(INSTALL_META_FILE),
        format!("{body}\n").as_bytes(),
    )
}

pub fn read_meta(version_dir: &Path) -> Result<InstallMeta, String> {
    let path = version_dir.join(INSTALL_META_FILE);
    let body = fs::read_to_string(&path)
        .map_err(|e| format!("could not read install metadata {}: {e}", path.display()))?;
    let meta: InstallMeta = serde_json::from_str(&body)
        .map_err(|e| format!("install metadata {} is unreadable: {e}", path.display()))?;
    meta.validate()
        .map_err(|e| format!("{e} ({})", path.display()))?;
    Ok(meta)
}

pub fn parse(body: &str, expected_tool: &str) -> Result<Receipt, String> {
    let receipt: Receipt = serde_json::from_str(body)
        .map_err(|e| format!("receipt for {expected_tool} is unreadable: {e}"))?;
    receipt.validate(expected_tool)?;
    Ok(receipt)
}

/// Read one tool's receipt. `Ok(None)` when the tool is not Sothis-managed.
pub fn read(layout: &Layout, tool: &str) -> Result<Option<Receipt>, String> {
    let path = layout.receipt_path(tool);
    match fs::read_to_string(&path) {
        Ok(body) => parse(&body, tool)
            .map(Some)
            .map_err(|e| format!("{e} ({})", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

/// Atomically write a receipt (tmp file in the same directory + rename).
pub fn write(layout: &Layout, receipt: &Receipt) -> Result<(), String> {
    receipt.validate(&receipt.tool)?;
    let path = layout.receipt_path(&receipt.tool);
    let body = serde_json::to_string_pretty(receipt)
        .map_err(|e| format!("could not serialize receipt: {e}"))?;
    write_atomic(&path, format!("{body}\n").as_bytes())
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("could not write {}: {error}", path.display()));
    }
    Ok(())
}

/// Every receipt on disk, sorted by tool id. A corrupt receipt is an error:
/// guessing ownership would be unsafe.
pub fn list(layout: &Layout) -> Result<Vec<Receipt>, String> {
    let dir = layout.receipts_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read {}: {error}", dir.display())),
    };
    let mut receipts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(tool) = name.strip_suffix(".json") else {
            continue; // tmp files and other debris
        };
        if name.starts_with('.') {
            continue;
        }
        if let Some(receipt) = read(layout, tool)? {
            receipts.push(receipt);
        }
    }
    receipts.sort_by(|a, b| a.tool.cmp(&b.tool));
    Ok(receipts)
}

/// Does everything the receipt lists still exist in `bin_dir` unchanged?
pub fn matches_disk(receipt: &Receipt) -> bool {
    receipt.files.iter().all(|file| {
        let path = receipt.bin_dir.join(&file.path);
        match (file.kind, fsx::entry_hash(&path)) {
            (FileKind::File, Ok(Some(hash))) => Some(&hash) == file.sha256.as_ref(),
            (FileKind::Dir, Ok(None)) => true,
            _ => false,
        }
    })
}

/// Human-readable differences between a receipt and the files on disk
/// (empty when everything is intact). Never modifies anything.
pub fn drift(receipt: &Receipt) -> Vec<String> {
    let mut problems = Vec::new();
    for file in &receipt.files {
        let path = receipt.bin_dir.join(&file.path);
        if !fsx::exists_nofollow(&path) {
            problems.push(format!("{} is missing", file.path));
            continue;
        }
        match (file.kind, fsx::entry_hash(&path)) {
            (FileKind::File, Ok(Some(hash))) if Some(&hash) == file.sha256.as_ref() => {}
            (FileKind::File, Ok(Some(_))) => {
                problems.push(format!("{} was modified (sha256 differs)", file.path))
            }
            (FileKind::File, Ok(None)) => {
                problems.push(format!("{} is now a directory", file.path))
            }
            (FileKind::Dir, Ok(None)) => {}
            (FileKind::Dir, Ok(Some(_))) => problems.push(format!("{} is now a file", file.path)),
            (_, Err(error)) => problems.push(format!("{} is unreadable: {error}", file.path)),
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fsx::testutil::TempDir;

    pub fn sample(layout: &Layout) -> Receipt {
        Receipt {
            schema_version: 1,
            tool: "hayvenhurst".to_string(),
            version: "0.0.8".to_string(),
            installed_at: "2026-10-12T00:00:00Z".to_string(),
            platform: "windows-x64".to_string(),
            asset: "hayvenhurst-0.0.8-windows-x64.tar.gz".to_string(),
            asset_sha256: "a".repeat(64),
            signature: SignatureStatus::Verified,
            bin_dir: layout.bin.clone(),
            files: vec![
                ReceiptFile {
                    path: "hayven.exe".to_string(),
                    kind: FileKind::File,
                    sha256: Some("b".repeat(64)),
                },
                ReceiptFile {
                    path: "skill".to_string(),
                    kind: FileKind::Dir,
                    sha256: None,
                },
            ],
            previous_version: None,
            adopted: Vec::new(),
        }
    }

    fn layout(dir: &TempDir) -> Layout {
        Layout {
            root: dir.path().to_path_buf(),
            bin: dir.path().join("bin"),
        }
    }

    #[test]
    fn receipts_round_trip_with_the_documented_shape() {
        let dir = TempDir::new("receipt");
        let layout = layout(&dir);
        let receipt = sample(&layout);
        write(&layout, &receipt).unwrap();
        assert_eq!(read(&layout, "hayvenhurst").unwrap(), Some(receipt.clone()));
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(layout.receipt_path("hayvenhurst")).unwrap())
                .unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["previousVersion"], serde_json::Value::Null);
        assert_eq!(value["signature"], "verified");
        assert_eq!(value["files"][1]["kind"], "dir");
        assert_eq!(value["files"][1]["sha256"], serde_json::Value::Null);
        assert!(value["adopted"].as_array().unwrap().is_empty());
        assert_eq!(list(&layout).unwrap(), vec![receipt]);
    }

    #[test]
    fn writes_are_atomic_and_leave_no_temp_files() {
        let dir = TempDir::new("receipt-atomic");
        let layout = layout(&dir);
        let mut receipt = sample(&layout);
        write(&layout, &receipt).unwrap();
        receipt.version = "0.0.9".to_string();
        receipt.previous_version = Some("0.0.8".to_string());
        write(&layout, &receipt).unwrap();
        assert_eq!(
            read(&layout, "hayvenhurst").unwrap().unwrap().version,
            "0.0.9"
        );
        let names = fs::read_dir(layout.receipts_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["hayvenhurst.json"]);
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        let dir = TempDir::new("receipt-unknown");
        let layout = layout(&dir);
        let receipt = sample(&layout);
        let good = serde_json::to_value(&receipt).unwrap();

        let mut top = good.clone();
        top["extra"] = serde_json::json!(true);
        assert!(parse(&top.to_string(), "hayvenhurst")
            .unwrap_err()
            .contains("unknown field"));

        let mut nested = good.clone();
        nested["files"][0]["mode"] = serde_json::json!(493);
        assert!(parse(&nested.to_string(), "hayvenhurst")
            .unwrap_err()
            .contains("unknown field"));

        let mut adopted = good;
        adopted["adopted"] = serde_json::json!([{"path":"a","backup":"/b","extra":1}]);
        assert!(parse(&adopted.to_string(), "hayvenhurst")
            .unwrap_err()
            .contains("unknown field"));
    }

    #[test]
    fn invalid_receipts_fail_closed() {
        let dir = TempDir::new("receipt-invalid");
        let layout = layout(&dir);
        let check = |mutate: &dyn Fn(&mut Receipt), needle: &str| {
            let mut receipt = sample(&layout);
            mutate(&mut receipt);
            let error = receipt.validate("hayvenhurst").unwrap_err();
            assert!(error.contains(needle), "{error}");
        };
        check(&|r| r.schema_version = 2, "schemaVersion");
        check(&|r| r.tool = "other".to_string(), "names tool");
        check(&|r| r.version = "latest".to_string(), "not semver");
        check(
            &|r| r.previous_version = Some("x".to_string()),
            "previousVersion",
        );
        check(&|r| r.asset_sha256 = "zz".to_string(), "assetSha256");
        check(&|r| r.bin_dir = PathBuf::from("relative"), "binDir");
        check(&|r| r.files.clear(), "no files");
        check(
            &|r| r.files[0].path = "../x".to_string(),
            "unsafe file path",
        );
        check(&|r| r.files[0].sha256 = None, "inconsistent");
        check(
            &|r| r.files[1].sha256 = Some("c".repeat(64)),
            "inconsistent",
        );
        check(
            &|r| {
                r.adopted.push(Adopted {
                    path: "a/b".to_string(),
                    backup: "/x".to_string(),
                })
            },
            "unsafe adopted",
        );
        check(
            &|r| {
                r.adopted.push(Adopted {
                    path: "a".to_string(),
                    backup: "rel".to_string(),
                })
            },
            "not absolute",
        );
        // An unwritable receipt is never written.
        let mut broken = sample(&layout);
        broken.version = "nope".to_string();
        assert!(write(&layout, &broken).is_err());
        assert!(!layout.receipt_path("hayvenhurst").exists());
    }

    #[test]
    fn a_corrupt_receipt_on_disk_is_an_error_not_an_absent_tool() {
        let dir = TempDir::new("receipt-corrupt");
        let layout = layout(&dir);
        fs::create_dir_all(layout.receipts_dir()).unwrap();
        fs::write(layout.receipt_path("amt"), "{not json").unwrap();
        assert!(read(&layout, "amt").is_err());
        assert!(list(&layout).is_err());
        assert_eq!(read(&layout, "absent").unwrap(), None);
    }

    #[test]
    fn drift_names_each_missing_modified_or_retyped_entry() {
        let dir = TempDir::new("receipt-drift");
        let layout = layout(&dir);
        fs::create_dir_all(layout.bin.join("skill")).unwrap();
        let exe = layout.bin.join("hayven.exe");
        fs::write(&exe, b"abc").unwrap();
        let mut receipt = sample(&layout);
        receipt.files[0].sha256 = Some(fsx::sha256_file(&exe).unwrap());
        assert!(drift(&receipt).is_empty());
        assert_eq!(drift(&receipt).is_empty(), matches_disk(&receipt));

        fs::write(&exe, b"abd").unwrap();
        assert_eq!(
            drift(&receipt),
            ["hayven.exe was modified (sha256 differs)"]
        );
        fs::remove_file(&exe).unwrap();
        fs::remove_dir(layout.bin.join("skill")).unwrap();
        assert_eq!(
            drift(&receipt),
            ["hayven.exe is missing", "skill is missing"]
        );
        fs::create_dir_all(&exe).unwrap();
        fs::write(layout.bin.join("skill"), b"x").unwrap();
        assert_eq!(
            drift(&receipt),
            ["hayven.exe is now a directory", "skill is now a file"]
        );
        assert!(!matches_disk(&receipt));
    }

    #[test]
    fn matches_disk_detects_missing_and_modified_files() {
        let dir = TempDir::new("receipt-disk");
        let layout = layout(&dir);
        fs::create_dir_all(layout.bin.join("skill")).unwrap();
        let exe = layout.bin.join("hayven.exe");
        fs::write(&exe, b"abc").unwrap();
        let mut receipt = sample(&layout);
        receipt.files[0].sha256 = Some(fsx::sha256_file(&exe).unwrap());
        assert!(matches_disk(&receipt));
        fs::write(&exe, b"abd").unwrap();
        assert!(!matches_disk(&receipt));
        fs::write(&exe, b"abc").unwrap();
        fs::remove_dir(layout.bin.join("skill")).unwrap();
        assert!(!matches_disk(&receipt));
    }
}
