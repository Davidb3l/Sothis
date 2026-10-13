//! Atomic, rollback-safe placement of tool entries into the bin directory.
//!
//! Every mutation is recorded in a [`Journal`]; on failure the journal is
//! undone in reverse order, restoring the exact prior state. Existing entries
//! are *renamed* aside (never overwritten), which also works for a running
//! executable on Windows.

use crate::install::fsx::{self, MoveKind};
use crate::install::layout::Layout;
use crate::install::receipt::{Adopted, FileKind, Receipt, ReceiptFile};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
enum Step {
    /// Something this transaction created; undo removes it.
    Created(PathBuf),
    /// Something this transaction moved; undo moves it back.
    Moved { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Default)]
pub struct Journal {
    steps: Vec<Step>,
    /// Non-fatal observations (for example a cross-device move fallback).
    pub notes: Vec<String>,
}

impl Journal {
    pub fn new() -> Self {
        Journal::default()
    }

    pub fn created(&mut self, path: PathBuf) {
        self.steps.push(Step::Created(path));
    }

    /// Move `from` to `to` and remember how to put it back.
    pub fn move_aside(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        match fsx::move_path(from, to) {
            Ok(kind) => {
                if kind == MoveKind::CopiedAcrossDevices {
                    self.notes.push(format!(
                        "{} and {} are on different devices; moved by copy + remove instead of rename",
                        from.display(),
                        to.display()
                    ));
                }
                self.steps.push(Step::Moved {
                    from: from.to_path_buf(),
                    to: to.to_path_buf(),
                });
                Ok(())
            }
            Err(error) => Err(format!(
                "could not move {} to {}: {error}",
                from.display(),
                to.display()
            )),
        }
    }

    /// Undo everything, newest first. Returns the problems that prevented a
    /// perfect restore (empty means the prior state is back exactly).
    pub fn undo(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        while let Some(step) = self.steps.pop() {
            match step {
                Step::Created(path) => {
                    if let Err(error) = fsx::remove_any(&path) {
                        problems.push(format!("could not remove {}: {error}", path.display()));
                    }
                }
                Step::Moved { from, to } => {
                    if fsx::exists_nofollow(&from) {
                        problems.push(format!(
                            "cannot restore {}: {} is occupied; the original is kept at {}",
                            from.display(),
                            from.display(),
                            to.display()
                        ));
                    } else if let Err(error) = fsx::move_path(&to, &from) {
                        problems.push(format!(
                            "could not restore {} from {}: {error}",
                            from.display(),
                            to.display()
                        ));
                    }
                }
            }
        }
        problems
    }

    /// Forget the steps after a successful transaction.
    pub fn commit(&mut self) {
        self.steps.clear();
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

pub struct PlaceInput<'a> {
    pub layout: &'a Layout,
    pub tool_id: &'a str,
    pub txn: &'a str,
    /// Extracted payload, normally `<root>/tools/<id>/<version>`.
    pub payload: &'a Path,
    /// `files` with `{exe}` already expanded.
    pub new_files: &'a [String],
    /// The tool's current receipt, if it is already Sothis-managed.
    pub previous: Option<&'a Receipt>,
}

#[derive(Debug, Clone)]
pub struct Placed {
    pub files: Vec<ReceiptFile>,
    /// Adopted priors carried from the previous receipt plus any new ones.
    pub adopted: Vec<Adopted>,
}

fn stage_name(txn: &str, entry: &str) -> String {
    format!(".sothis-stage-{txn}-{entry}")
}

/// Verify every `files` entry exists in the payload as a regular file or
/// directory. Done before the bin directory is touched.
pub fn check_payload(payload: &Path, files: &[String]) -> Result<(), String> {
    for entry in files {
        let path = payload.join(entry);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("payload entry {entry} is a symlink"))
            }
            Ok(_) => {}
            Err(_) => {
                return Err(format!(
                    "the release archive does not contain the expected entry {entry:?}"
                ))
            }
        }
    }
    Ok(())
}

/// Place `new_files` from the payload into the bin directory.
pub fn place(input: &PlaceInput<'_>, journal: &mut Journal) -> Result<Placed, String> {
    place_with(input, journal, &mut |_| Ok(()))
}

/// [`place`] with a fault-injection seam called before each swap (tests).
pub fn place_with(
    input: &PlaceInput<'_>,
    journal: &mut Journal,
    before_swap: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Result<Placed, String> {
    let bin = &input.layout.bin;
    check_payload(input.payload, input.new_files)?;
    fs::create_dir_all(bin).map_err(|e| format!("could not create {}: {e}", bin.display()))?;

    // 1. Stage private copies next to their destinations (same filesystem, so
    //    the final rename is atomic) and hash what will actually be installed.
    let mut files = Vec::new();
    for entry in input.new_files {
        let source = input.payload.join(entry);
        let staged = bin.join(stage_name(input.txn, entry));
        journal.created(staged.clone());
        fsx::copy_recursive(&source, &staged)
            .map_err(|e| format!("could not stage {entry}: {e}"))?;
        let hash = fsx::entry_hash(&staged)?;
        if hash != fsx::entry_hash(&source)? {
            return Err(format!("staged copy of {entry} differs from the payload"));
        }
        files.push(ReceiptFile {
            path: entry.clone(),
            kind: if hash.is_some() {
                FileKind::File
            } else {
                FileKind::Dir
            },
            sha256: hash,
        });
    }

    // 2. Swap each entry in: move any existing one aside, rename the staged
    //    copy into place.
    let rollback = input.layout.rollback_dir(input.txn);
    let mut adopted = input
        .previous
        .map(|receipt| receipt.adopted.clone())
        .unwrap_or_default();
    for entry in input.new_files {
        before_swap(entry)?;
        let target = bin.join(entry);
        if fsx::exists_nofollow(&target) {
            let owned = input.previous.is_some_and(|receipt| receipt.owns(entry));
            let backup = if owned {
                rollback.join(entry)
            } else {
                let dir = input.layout.backups_dir(input.tool_id);
                let plain = dir.join(entry);
                if fsx::exists_nofollow(&plain) {
                    dir.join(format!("{}-{entry}", input.txn))
                } else {
                    plain
                }
            };
            journal.move_aside(&target, &backup)?;
            if !owned {
                adopted.push(Adopted {
                    path: entry.clone(),
                    backup: backup.to_string_lossy().into_owned(),
                });
            }
        }
        let staged = bin.join(stage_name(input.txn, entry));
        fs::rename(&staged, &target)
            .map_err(|e| format!("could not move {entry} into {}: {e}", bin.display()))?;
        journal.created(target);
    }

    // 3. Entries the previous version owned that the new one no longer ships.
    if let Some(previous) = input.previous {
        for old in &previous.files {
            if input.new_files.iter().any(|entry| entry == &old.path) {
                continue;
            }
            let target = bin.join(&old.path);
            if fsx::exists_nofollow(&target) {
                journal.move_aside(&target, &rollback.join(&old.path))?;
            }
        }
    }
    Ok(Placed { files, adopted })
}

#[derive(Debug, Default)]
pub struct RemoveReport {
    pub warnings: Vec<String>,
}

/// Remove everything a receipt owns and restore adopted priors. The caller
/// commits the journal (and deletes the rollback directory) on success.
pub fn remove_owned(
    layout: &Layout,
    receipt: &Receipt,
    txn: &str,
    journal: &mut Journal,
) -> Result<RemoveReport, String> {
    let mut report = RemoveReport::default();
    let rollback = layout.rollback_dir(txn);
    for file in &receipt.files {
        let target = receipt.bin_dir.join(&file.path);
        if !fsx::exists_nofollow(&target) {
            continue;
        }
        if file.kind == FileKind::File {
            match fsx::entry_hash(&target) {
                Ok(hash) if hash == file.sha256 => {}
                _ => report.warnings.push(format!(
                    "{} was modified after install; removing it anyway",
                    target.display()
                )),
            }
        }
        journal.move_aside(&target, &rollback.join(&file.path))?;
    }
    for adopted in &receipt.adopted {
        let original = receipt.bin_dir.join(&adopted.path);
        let backup = PathBuf::from(&adopted.backup);
        if !fsx::exists_nofollow(&backup) {
            report.warnings.push(format!(
                "the backup of {} at {} is missing; the original was not restored",
                original.display(),
                backup.display()
            ));
            continue;
        }
        if fsx::exists_nofollow(&original) {
            report.warnings.push(format!(
                "{} exists and is not Sothis-owned; the backup was left at {}",
                original.display(),
                backup.display()
            ));
            continue;
        }
        journal.move_aside(&backup, &original)?;
    }
    let tool_dir = layout.tool_dir(&receipt.tool);
    if fsx::exists_nofollow(&tool_dir) {
        journal.move_aside(&tool_dir, &rollback.join("tool-versions"))?;
    }
    let receipt_path = layout.receipt_path(&receipt.tool);
    if fsx::exists_nofollow(&receipt_path) {
        journal.move_aside(&receipt_path, &rollback.join("receipt.json"))?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fsx::testutil::TempDir;
    use crate::install::receipt::{self, Receipt};
    use crate::install::verify::SignatureStatus;

    struct Fixture {
        _dir: TempDir,
        layout: Layout,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new("place");
        let layout = Layout {
            root: dir.path().join("home"),
            bin: dir.path().join("home").join("bin"),
        };
        fs::create_dir_all(&layout.bin).unwrap();
        Fixture { _dir: dir, layout }
    }

    /// A payload with `tool.exe` (content `exe`) and a `skill/` directory.
    fn payload(layout: &Layout, version: &str, exe: &str) -> PathBuf {
        let dir = layout.version_dir("demo", version);
        fs::create_dir_all(dir.join("skill")).unwrap();
        fs::write(dir.join("tool.exe"), exe).unwrap();
        fs::write(dir.join("skill").join("s.md"), format!("skill {version}")).unwrap();
        dir
    }

    fn files() -> Vec<String> {
        vec!["tool.exe".to_string(), "skill".to_string()]
    }

    fn receipt_for(
        layout: &Layout,
        version: &str,
        placed: &Placed,
        previous: Option<&str>,
    ) -> Receipt {
        Receipt {
            schema_version: 1,
            tool: "demo".to_string(),
            version: version.to_string(),
            installed_at: "2026-10-12T00:00:00Z".to_string(),
            platform: "windows-x64".to_string(),
            asset: "demo.zip".to_string(),
            asset_sha256: "a".repeat(64),
            signature: SignatureStatus::NotPublished,
            bin_dir: layout.bin.clone(),
            files: placed.files.clone(),
            previous_version: previous.map(str::to_string),
            adopted: placed.adopted.clone(),
        }
    }

    fn place_version(
        layout: &Layout,
        txn: &str,
        version: &str,
        previous: Option<&Receipt>,
        files: &[String],
    ) -> Result<(Placed, Journal), String> {
        let payload = payload(layout, version, &format!("exe {version}"));
        let mut journal = Journal::new();
        let placed = place(
            &PlaceInput {
                layout,
                tool_id: "demo",
                txn,
                payload: &payload,
                new_files: files,
                previous,
            },
            &mut journal,
        )?;
        Ok((placed, journal))
    }

    fn bin_names(layout: &Layout) -> Vec<String> {
        let mut names = fs::read_dir(&layout.bin)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn a_fresh_placement_copies_files_and_directories_and_leaves_no_staging() {
        let fx = fixture();
        let (placed, mut journal) =
            place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        journal.commit();
        assert_eq!(bin_names(&fx.layout), ["skill", "tool.exe"]);
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 1.0.0"
        );
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("skill").join("s.md")).unwrap(),
            "skill 1.0.0"
        );
        assert_eq!(placed.files[0].kind, FileKind::File);
        assert!(placed.files[0].sha256.is_some());
        assert_eq!(placed.files[1].kind, FileKind::Dir);
        assert_eq!(placed.files[1].sha256, None);
        assert!(placed.adopted.is_empty());
    }

    #[test]
    fn replacing_an_owned_install_renames_the_old_one_aside_and_undo_restores_it() {
        let fx = fixture();
        let (placed1, mut j1) = place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        j1.commit();
        let r1 = receipt_for(&fx.layout, "1.0.0", &placed1, None);

        let (_placed2, mut j2) =
            place_version(&fx.layout, "t2", "2.0.0", Some(&r1), &files()).unwrap();
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 2.0.0"
        );
        let rollback = fx.layout.rollback_dir("t2");
        assert_eq!(
            fs::read_to_string(rollback.join("tool.exe")).unwrap(),
            "exe 1.0.0"
        );
        assert_eq!(
            fs::read_to_string(rollback.join("skill").join("s.md")).unwrap(),
            "skill 1.0.0"
        );

        // Simulate a later failure (for example the --version check).
        let problems = j2.undo();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 1.0.0"
        );
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("skill").join("s.md")).unwrap(),
            "skill 1.0.0"
        );
        assert_eq!(
            bin_names(&fx.layout),
            ["skill", "tool.exe"],
            "no stage debris"
        );
    }

    #[test]
    fn a_failure_midway_restores_every_original_and_removes_what_was_placed() {
        let fx = fixture();
        let (placed1, mut j1) = place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        j1.commit();
        let r1 = receipt_for(&fx.layout, "1.0.0", &placed1, None);

        let payload2 = payload(&fx.layout, "2.0.0", "exe 2.0.0");
        let mut journal = Journal::new();
        let files = files();
        let mut swaps = 0;
        let error = place_with(
            &PlaceInput {
                layout: &fx.layout,
                tool_id: "demo",
                txn: "t2",
                payload: &payload2,
                new_files: &files,
                previous: Some(&r1),
            },
            &mut journal,
            &mut |entry| {
                swaps += 1;
                if entry == "skill" {
                    Err("injected failure".to_string())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error, "injected failure");
        // tool.exe was already swapped to 2.0.0 when the second entry failed.
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 2.0.0"
        );
        let problems = journal.undo();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(swaps, 2);
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 1.0.0"
        );
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("skill").join("s.md")).unwrap(),
            "skill 1.0.0"
        );
        assert_eq!(bin_names(&fx.layout), ["skill", "tool.exe"]);
    }

    #[test]
    fn missing_payload_entries_fail_before_the_bin_directory_is_touched() {
        let fx = fixture();
        fs::write(fx.layout.bin.join("tool.exe"), "mine").unwrap();
        let payload = fx.layout.version_dir("demo", "1.0.0");
        fs::create_dir_all(&payload).unwrap();
        fs::write(payload.join("tool.exe"), "new").unwrap();
        let mut journal = Journal::new();
        let files = vec!["tool.exe".to_string(), "missing{x}".to_string()];
        let error = place(
            &PlaceInput {
                layout: &fx.layout,
                tool_id: "demo",
                txn: "t1",
                payload: &payload,
                new_files: &files,
                previous: None,
            },
            &mut journal,
        )
        .unwrap_err();
        assert!(
            error.contains("does not contain the expected entry"),
            "{error}"
        );
        assert!(journal.is_empty());
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "mine"
        );
        assert_eq!(bin_names(&fx.layout), ["tool.exe"]);
    }

    #[test]
    fn an_unowned_prior_is_adopted_with_a_persistent_backup_and_restored_on_uninstall() {
        let fx = fixture();
        fs::write(fx.layout.bin.join("tool.exe"), "user's own 0.2.0").unwrap();
        fs::write(fx.layout.bin.join("unrelated.txt"), "keep me").unwrap();
        let (placed, mut journal) =
            place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        journal.commit();
        assert_eq!(placed.adopted.len(), 1);
        assert_eq!(placed.adopted[0].path, "tool.exe");
        let backup = PathBuf::from(&placed.adopted[0].backup);
        assert!(backup.starts_with(fx.layout.backups_dir("demo")));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "user's own 0.2.0");
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 1.0.0"
        );

        let receipt = receipt_for(&fx.layout, "1.0.0", &placed, None);
        receipt::write(&fx.layout, &receipt).unwrap();

        let mut uninstall = Journal::new();
        let report = remove_owned(&fx.layout, &receipt, "t9", &mut uninstall).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        uninstall.commit();
        fsx::remove_any(&fx.layout.rollback_dir("t9")).unwrap();

        // The user's original is back; the unrelated file was never touched;
        // everything Sothis owned is gone.
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "user's own 0.2.0"
        );
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("unrelated.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(bin_names(&fx.layout), ["tool.exe", "unrelated.txt"]);
        assert!(!fx.layout.tool_dir("demo").exists());
        assert!(!fx.layout.receipt_path("demo").exists());
    }

    #[test]
    fn adopted_priors_survive_an_upgrade_in_the_receipt() {
        let fx = fixture();
        fs::write(fx.layout.bin.join("tool.exe"), "original").unwrap();
        let (placed1, mut j1) = place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        j1.commit();
        let r1 = receipt_for(&fx.layout, "1.0.0", &placed1, None);
        let (placed2, mut j2) =
            place_version(&fx.layout, "t2", "2.0.0", Some(&r1), &files()).unwrap();
        j2.commit();
        assert_eq!(
            placed2.adopted, placed1.adopted,
            "adopted is carried forward"
        );
        assert!(Path::new(&placed2.adopted[0].backup).exists());
    }

    #[test]
    fn rolling_back_replaces_the_current_files_with_the_previous_version() {
        let fx = fixture();
        let (p1, mut j1) = place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        j1.commit();
        let r1 = receipt_for(&fx.layout, "1.0.0", &p1, None);
        let (p2, mut j2) = place_version(&fx.layout, "t2", "2.0.0", Some(&r1), &files()).unwrap();
        j2.commit();
        let r2 = receipt_for(&fx.layout, "2.0.0", &p2, Some("1.0.0"));
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 2.0.0"
        );

        // tools/demo/1.0.0 is still on disk, so re-placing it is a rollback.
        let payload1 = fx.layout.version_dir("demo", "1.0.0");
        assert!(payload1.is_dir());
        let mut j3 = Journal::new();
        let files = files();
        place(
            &PlaceInput {
                layout: &fx.layout,
                tool_id: "demo",
                txn: "t3",
                payload: &payload1,
                new_files: &files,
                previous: Some(&r2),
            },
            &mut j3,
        )
        .unwrap();
        j3.commit();
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 1.0.0"
        );
    }

    #[test]
    fn files_the_new_version_no_longer_ships_are_retired_and_restored_on_undo() {
        let fx = fixture();
        let (p1, mut j1) = place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        j1.commit();
        let r1 = receipt_for(&fx.layout, "1.0.0", &p1, None);
        let only_exe = vec!["tool.exe".to_string()];
        let (_p2, mut j2) = place_version(&fx.layout, "t2", "2.0.0", Some(&r1), &only_exe).unwrap();
        assert_eq!(bin_names(&fx.layout), ["tool.exe"], "skill/ was retired");
        assert!(j2.undo().is_empty());
        assert_eq!(bin_names(&fx.layout), ["skill", "tool.exe"]);
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "exe 1.0.0"
        );
    }

    #[test]
    fn uninstall_warns_about_modified_files_and_can_be_undone() {
        let fx = fixture();
        let (p1, mut j1) = place_version(&fx.layout, "t1", "1.0.0", None, &files()).unwrap();
        j1.commit();
        let r1 = receipt_for(&fx.layout, "1.0.0", &p1, None);
        receipt::write(&fx.layout, &r1).unwrap();
        fs::write(fx.layout.bin.join("tool.exe"), "locally patched").unwrap();

        let mut journal = Journal::new();
        let report = remove_owned(&fx.layout, &r1, "t5", &mut journal).unwrap();
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("modified after install"));
        assert!(bin_names(&fx.layout).is_empty());
        assert!(journal.undo().is_empty());
        assert_eq!(
            fs::read_to_string(fx.layout.bin.join("tool.exe")).unwrap(),
            "locally patched"
        );
        assert!(fx.layout.receipt_path("demo").exists());
        assert!(fx.layout.version_dir("demo", "1.0.0").is_dir());
    }

    #[test]
    fn undo_reports_instead_of_clobbering_when_an_original_cannot_return() {
        let fx = fixture();
        let target = fx.layout.bin.join("a");
        let aside = fx.layout.root.join("aside");
        fs::write(&target, "orig").unwrap();
        let mut journal = Journal::new();
        journal.move_aside(&target, &aside).unwrap();
        fs::write(&target, "intruder").unwrap();
        let problems = journal.undo();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("is occupied"));
        assert_eq!(fs::read_to_string(&aside).unwrap(), "orig");
        assert_eq!(fs::read_to_string(&target).unwrap(), "intruder");
    }
}
