//! Safe archive extraction (`.tar.gz`, `.tar.xz`, `.zip`).
//!
//! Every entry name is validated before anything is written: no absolute
//! paths, drive/UNC prefixes, `..`, backslashes or reserved device names; no
//! symlinks or hardlinks; no duplicates; bounded entry count and size. A
//! single top-level directory is stripped. Extraction is two-pass (scan, then
//! write) so the strip decision sees the whole archive.

use crate::install::fsx::{self, CopyError};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

pub const MAX_ENTRIES: usize = 50_000;
pub const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    TarXz,
    Zip,
}

pub fn format_for(asset: &str) -> Result<ArchiveFormat, String> {
    let lower = asset.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Ok(ArchiveFormat::TarGz)
    } else if lower.ends_with(".tar.xz") {
        Ok(ArchiveFormat::TarXz)
    } else if lower.ends_with(".zip") {
        Ok(ArchiveFormat::Zip)
    } else {
        Err(format!(
            "unsupported archive type for {asset}; expected .tar.gz, .tar.xz or .zip"
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    pub entries: usize,
    pub unpacked_bytes: u64,
    /// The single top-level directory that was stripped, if any.
    pub stripped_top: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

#[derive(Debug, Clone)]
struct Entry {
    components: Vec<String>,
    kind: Kind,
}

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate an archive entry name and split it into safe components.
/// Returns an empty vector for the archive root (`./`).
fn sanitize(raw: &str) -> Result<Vec<String>, String> {
    let bad = |why: &str| Err(format!("unsafe archive entry {raw:?}: {why}"));
    if raw.is_empty() {
        return bad("empty name");
    }
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return bad("control character in name");
    }
    if raw.contains('\\') {
        return bad("backslash in name");
    }
    if raw.starts_with('/') {
        return bad("absolute path");
    }
    let mut components = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => continue,
            ".." => return bad("parent directory component"),
            _ => {}
        }
        if part.contains(':') {
            return bad("drive prefix or alternate data stream");
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return bad("component ends with a dot or space");
        }
        let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return bad("reserved device name");
        }
        components.push(part.to_string());
    }
    Ok(components)
}

struct Sink<'a> {
    dest: Option<&'a Path>,
    strip: Option<&'a str>,
    seen: HashSet<String>,
    entries: Vec<Entry>,
    unpacked: u64,
}

impl<'a> Sink<'a> {
    fn scanning() -> Sink<'static> {
        Sink {
            dest: None,
            strip: None,
            seen: HashSet::new(),
            entries: Vec::new(),
            unpacked: 0,
        }
    }

    fn writing(dest: &'a Path, strip: Option<&'a str>) -> Sink<'a> {
        Sink {
            dest: Some(dest),
            strip,
            seen: HashSet::new(),
            entries: Vec::new(),
            unpacked: 0,
        }
    }

    /// Validate one entry and, when writing, materialize it.
    fn accept(
        &mut self,
        name: &str,
        kind: Kind,
        mode: Option<u32>,
        data: Option<&mut dyn Read>,
    ) -> Result<(), String> {
        let components = sanitize(name)?;
        if components.is_empty() {
            return Ok(());
        }
        let key = components
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join("/");
        if !self.seen.insert(key) {
            return Err(format!("duplicate archive entry {name:?}"));
        }
        if self.seen.len() > MAX_ENTRIES {
            return Err(format!("archive has more than {MAX_ENTRIES} entries"));
        }
        let Some(dest) = self.dest else {
            self.entries.push(Entry { components, kind });
            return Ok(());
        };
        let relative: &[String] = match self.strip {
            Some(top) => {
                if components[0] != top {
                    return Err(format!("archive entry {name:?} escapes the stripped root"));
                }
                &components[1..]
            }
            None => &components,
        };
        if relative.is_empty() {
            return Ok(());
        }
        let target = relative
            .iter()
            .fold(dest.to_path_buf(), |path, part| path.join(part));
        match kind {
            Kind::Dir => {
                fs::create_dir_all(&target)
                    .map_err(|e| format!("could not create {}: {e}", target.display()))?;
            }
            Kind::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
                }
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(|e| format!("could not create {}: {e}", target.display()))?;
                if let Some(reader) = data {
                    let remaining = MAX_UNPACKED_BYTES - self.unpacked;
                    let (bytes, _) =
                        fsx::copy_hashing(reader, &mut file, remaining).map_err(|e| match e {
                            CopyError::Io(e) => format!("could not extract {name:?}: {e}"),
                            CopyError::TooLarge(_) => {
                                format!("archive unpacks to more than {MAX_UNPACKED_BYTES} bytes")
                            }
                        })?;
                    self.unpacked += bytes;
                }
                file.flush()
                    .map_err(|e| format!("could not write {}: {e}", target.display()))?;
                drop(file);
                set_mode(&target, mode)?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    // Preserve rwx bits only; never setuid/setgid/sticky.
    let mode = mode.map(|m| m & 0o777).unwrap_or(0o644) | 0o600;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("could not set permissions on {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

fn run_tar<R: Read>(reader: R, sink: &mut Sink<'_>) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| format!("could not read tar archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("could not read tar entry: {e}"))?;
        let kind = entry.header().entry_type();
        let name = entry
            .path()
            .map_err(|e| format!("tar entry has an invalid path: {e}"))?
            .to_string_lossy()
            .into_owned();
        let mode = entry.header().mode().ok();
        match kind {
            tar::EntryType::XGlobalHeader => continue,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                sink.accept(&name, Kind::File, mode, Some(&mut entry))?
            }
            tar::EntryType::Directory => sink.accept(&name, Kind::Dir, mode, None)?,
            tar::EntryType::Symlink | tar::EntryType::Link => {
                return Err(format!(
                    "archive entry {name:?} is a symlink or hardlink, which is not allowed"
                ))
            }
            other => {
                return Err(format!(
                    "archive entry {name:?} has unsupported type {other:?}"
                ))
            }
        }
    }
    Ok(())
}

fn run_zip(archive: &Path, sink: &mut Sink<'_>) -> Result<(), String> {
    let file =
        File::open(archive).map_err(|e| format!("could not open {}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|e| format!("could not read zip archive: {e}"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(format!("archive has more than {MAX_ENTRIES} entries"));
    }
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| format!("could not read zip entry {index}: {e}"))?;
        let name = entry
            .name()
            .map_err(|e| format!("could not read the name of zip entry {index}: {e}"))?
            .into_owned();
        let mode = entry.unix_mode();
        if let Some(mode) = mode {
            // S_IFMT: only regular files and directories are allowed.
            let file_type = mode & 0o170000;
            if file_type != 0 && file_type != 0o100000 && file_type != 0o040000 {
                return Err(format!(
                    "archive entry {name:?} is a symlink or special file, which is not allowed"
                ));
            }
        }
        if entry.is_dir() {
            sink.accept(&name, Kind::Dir, mode, None)?;
        } else {
            sink.accept(&name, Kind::File, mode, Some(&mut entry))?;
        }
    }
    Ok(())
}

struct LimitedWriter<W: Write> {
    inner: W,
    remaining: u64,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed data exceeds the size limit",
            ));
        }
        self.remaining -= buf.len() as u64;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Run one pass over the archive into `sink`.
fn pass(
    archive: &Path,
    format: ArchiveFormat,
    xz_tar: Option<&Path>,
    sink: &mut Sink<'_>,
) -> Result<(), String> {
    match format {
        ArchiveFormat::TarGz => {
            let file = File::open(archive)
                .map_err(|e| format!("could not open {}: {e}", archive.display()))?;
            run_tar(flate2::read::GzDecoder::new(BufReader::new(file)), sink)
        }
        ArchiveFormat::TarXz => {
            let tar_path = xz_tar.ok_or("internal error: missing decompressed tar")?;
            let file = File::open(tar_path)
                .map_err(|e| format!("could not open {}: {e}", tar_path.display()))?;
            run_tar(BufReader::new(file), sink)
        }
        ArchiveFormat::Zip => run_zip(archive, sink),
    }
}

fn decompress_xz(archive: &Path, out: &Path) -> Result<(), String> {
    let input =
        File::open(archive).map_err(|e| format!("could not open {}: {e}", archive.display()))?;
    let output =
        File::create(out).map_err(|e| format!("could not create {}: {e}", out.display()))?;
    let mut writer = LimitedWriter {
        inner: io::BufWriter::new(output),
        remaining: MAX_UNPACKED_BYTES,
    };
    lzma_rs::xz_decompress(&mut BufReader::new(input), &mut writer)
        .map_err(|e| format!("could not decompress {}: {e}", archive.display()))?;
    writer
        .flush()
        .map_err(|e| format!("could not write {}: {e}", out.display()))
}

fn strip_prefix(entries: &[Entry]) -> Option<String> {
    let first = entries.first()?.components.first()?.clone();
    if !entries.iter().all(|entry| entry.components[0] == first) {
        return None;
    }
    let is_directory = entries.iter().any(|entry| entry.components.len() > 1)
        || entries
            .iter()
            .any(|entry| entry.components.len() == 1 && entry.kind == Kind::Dir);
    is_directory.then_some(first)
}

/// Extract `archive` into `dest` (created if missing, must be empty).
/// `work_dir` holds the temporary decompressed tar for `.tar.xz`.
pub fn extract_archive(
    archive: &Path,
    format: ArchiveFormat,
    dest: &Path,
    work_dir: &Path,
) -> Result<ExtractReport, String> {
    fs::create_dir_all(dest).map_err(|e| format!("could not create {}: {e}", dest.display()))?;
    let occupied = fs::read_dir(dest)
        .map_err(|e| format!("could not read {}: {e}", dest.display()))?
        .next()
        .is_some();
    if occupied {
        return Err(format!("extraction target {} is not empty", dest.display()));
    }
    let xz_tar = work_dir.join("decompressed.tar");
    let xz_path = if format == ArchiveFormat::TarXz {
        fs::create_dir_all(work_dir)
            .map_err(|e| format!("could not create {}: {e}", work_dir.display()))?;
        decompress_xz(archive, &xz_tar)?;
        Some(xz_tar.as_path())
    } else {
        None
    };
    let result = (|| {
        let mut scan = Sink::scanning();
        pass(archive, format, xz_path, &mut scan)?;
        let strip = strip_prefix(&scan.entries);
        let mut write = Sink::writing(dest, strip.as_deref());
        pass(archive, format, xz_path, &mut write)?;
        Ok(ExtractReport {
            entries: scan.entries.len(),
            unpacked_bytes: write.unpacked,
            stripped_top: strip,
        })
    })();
    if xz_path.is_some() {
        let _ = fs::remove_file(&xz_tar);
    }
    if result.is_err() {
        // Never leave a half-extracted tree behind.
        let _ = fsx::remove_any(dest);
    }
    result
}

#[cfg(test)]
pub mod testutil {
    //! Archive builders shared by the extract and engine tests.
    use std::io::Write;

    pub enum Item<'a> {
        File(&'a str, &'a [u8], u32),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
        Hardlink(&'a str, &'a str),
    }

    /// Build a tar by hand so entry names like `../x` can be written verbatim.
    pub fn tar_bytes(items: &[Item<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for item in items {
            let mut header = [0u8; 512];
            let (name, size, mode, kind, link): (&str, usize, u32, u8, &str) = match item {
                Item::File(name, data, mode) => (name, data.len(), *mode, b'0', ""),
                Item::Dir(name) => (name, 0, 0o755, b'5', ""),
                Item::Symlink(name, target) => (name, 0, 0o777, b'2', target),
                Item::Hardlink(name, target) => (name, 0, 0o644, b'1', target),
            };
            assert!(name.len() < 100 && link.len() < 100);
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..107].copy_from_slice(format!("{mode:07o}").as_bytes());
            header[108..115].copy_from_slice(b"0000000");
            header[116..123].copy_from_slice(b"0000000");
            header[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
            header[136..147].copy_from_slice(b"00000000000");
            header[156] = kind;
            header[157..157 + link.len()].copy_from_slice(link.as_bytes());
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let sum: u32 = header.iter().map(|b| *b as u32).sum();
            header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
            header[155] = b' ';
            out.extend_from_slice(&header);
            if let Item::File(_, data, _) = item {
                out.extend_from_slice(data);
                out.extend(std::iter::repeat_n(0u8, (512 - data.len() % 512) % 512));
            }
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    pub fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    pub fn tar_gz(items: &[Item<'_>]) -> Vec<u8> {
        gzip(&tar_bytes(items))
    }

    /// Build a zip with raw entry names (so `../x` and `/abs` are possible).
    pub fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, data) in entries {
                if name.ends_with('/') {
                    writer.add_directory(*name, options).unwrap();
                } else {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(data).unwrap();
                }
            }
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{tar_bytes, tar_gz, zip_bytes, Item};
    use super::*;
    use crate::install::fsx::testutil::TempDir;

    fn run(bytes: &[u8], format: ArchiveFormat) -> (TempDir, Result<ExtractReport, String>) {
        let dir = TempDir::new("extract");
        let archive = dir.path().join("archive.bin");
        fs::write(&archive, bytes).unwrap();
        let result = extract_archive(
            &archive,
            format,
            &dir.path().join("payload"),
            &dir.path().join("work"),
        );
        (dir, result)
    }

    #[test]
    fn formats_are_chosen_by_suffix() {
        assert_eq!(format_for("a.tar.gz").unwrap(), ArchiveFormat::TarGz);
        assert_eq!(format_for("a.TGZ").unwrap(), ArchiveFormat::TarGz);
        assert_eq!(format_for("a.tar.xz").unwrap(), ArchiveFormat::TarXz);
        assert_eq!(format_for("a.zip").unwrap(), ArchiveFormat::Zip);
        assert!(format_for("a.7z").is_err());
        assert!(format_for("a.tar").is_err());
    }

    #[test]
    fn a_single_top_level_directory_is_stripped() {
        let bytes = tar_gz(&[
            Item::Dir("tool-1.0/"),
            Item::File("tool-1.0/tool.exe", b"EXE", 0o755),
            Item::Dir("tool-1.0/skill/"),
            Item::File("tool-1.0/skill/s.md", b"S", 0o644),
        ]);
        let (dir, result) = run(&bytes, ArchiveFormat::TarGz);
        let report = result.unwrap();
        assert_eq!(report.stripped_top.as_deref(), Some("tool-1.0"));
        let payload = dir.path().join("payload");
        assert_eq!(fs::read(payload.join("tool.exe")).unwrap(), b"EXE");
        assert_eq!(fs::read(payload.join("skill").join("s.md")).unwrap(), b"S");
        assert!(!payload.join("tool-1.0").exists());
    }

    #[test]
    fn an_archive_without_a_top_directory_is_extracted_flat() {
        let bytes = zip_bytes(&[("amt.exe", b"EXE"), ("LICENSE", b"L"), ("README.md", b"R")]);
        let (dir, result) = run(&bytes, ArchiveFormat::Zip);
        let report = result.unwrap();
        assert_eq!(report.stripped_top, None);
        assert_eq!(report.entries, 3);
        assert_eq!(
            fs::read(dir.path().join("payload").join("amt.exe")).unwrap(),
            b"EXE"
        );
    }

    #[test]
    fn a_lone_top_level_file_is_not_mistaken_for_a_directory() {
        let bytes = tar_gz(&[Item::File("amt", b"X", 0o755)]);
        let (dir, result) = run(&bytes, ArchiveFormat::TarGz);
        assert_eq!(result.unwrap().stripped_top, None);
        assert!(dir.path().join("payload").join("amt").is_file());
    }

    #[test]
    fn zip_with_a_top_directory_is_stripped_too() {
        let bytes = zip_bytes(&[("top/", b""), ("top/a.txt", b"A")]);
        let (dir, result) = run(&bytes, ArchiveFormat::Zip);
        assert_eq!(result.unwrap().stripped_top.as_deref(), Some("top"));
        assert!(dir.path().join("payload").join("a.txt").is_file());
    }

    #[test]
    fn parent_directory_traversal_is_rejected_and_nothing_is_left_behind() {
        let bytes = tar_gz(&[
            Item::File("ok/a.txt", b"A", 0o644),
            Item::File("ok/../../evil.txt", b"E", 0o644),
        ]);
        let (dir, result) = run(&bytes, ArchiveFormat::TarGz);
        assert!(result.unwrap_err().contains("parent directory"));
        assert!(!dir.path().join("payload").exists());
        assert!(!dir.path().join("evil.txt").exists());
    }

    #[test]
    fn absolute_and_prefixed_paths_are_rejected() {
        for name in ["/etc/passwd", "C:/x.txt", "ok/C:evil", "ok/a:b"] {
            let bytes = tar_gz(&[Item::File(name, b"E", 0o644)]);
            let (_dir, result) = run(&bytes, ArchiveFormat::TarGz);
            assert!(result.is_err(), "{name} should be rejected");
        }
        let (_dir, result) = run(&zip_bytes(&[("/abs.txt", b"E")]), ArchiveFormat::Zip);
        assert!(result.unwrap_err().contains("absolute"));
        let (_dir, result) = run(&zip_bytes(&[(r"..\evil.txt", b"E")]), ArchiveFormat::Zip);
        assert!(result.unwrap_err().contains("backslash"));
        let (_dir, result) = run(&zip_bytes(&[("a/../../evil", b"E")]), ArchiveFormat::Zip);
        assert!(result.unwrap_err().contains("parent directory"));
    }

    #[test]
    fn zip_slip_style_names_never_write_outside_the_destination() {
        let bytes = zip_bytes(&[("safe/a.txt", b"A"), ("safe/../../escape.txt", b"E")]);
        let (dir, result) = run(&bytes, ArchiveFormat::Zip);
        assert!(result.is_err());
        assert!(!dir.path().join("escape.txt").exists());
        assert!(!dir.path().join("payload").exists());
    }

    #[test]
    fn symlinks_and_hardlinks_are_rejected() {
        let sym = tar_gz(&[Item::Dir("top/"), Item::Symlink("top/link", "/etc/passwd")]);
        let (_dir, result) = run(&sym, ArchiveFormat::TarGz);
        assert!(result.unwrap_err().contains("symlink or hardlink"));
        let hard = tar_gz(&[
            Item::File("top/a", b"A", 0o644),
            Item::Hardlink("top/b", "top/a"),
        ]);
        let (_dir, result) = run(&hard, ArchiveFormat::TarGz);
        assert!(result.unwrap_err().contains("symlink or hardlink"));
    }

    #[test]
    fn duplicate_entries_are_rejected_case_insensitively() {
        let bytes = tar_gz(&[
            Item::File("top/a.txt", b"1", 0o644),
            Item::File("top/A.TXT", b"2", 0o644),
        ]);
        let (_dir, result) = run(&bytes, ArchiveFormat::TarGz);
        assert!(result.unwrap_err().contains("duplicate"));
    }

    #[test]
    fn reserved_device_names_and_trailing_dots_are_rejected() {
        for name in ["top/NUL", "top/con.txt", "top/trail.", "top/trail "] {
            let bytes = tar_gz(&[Item::File(name, b"E", 0o644)]);
            let (_dir, result) = run(&bytes, ArchiveFormat::TarGz);
            assert!(result.is_err(), "{name:?} should be rejected");
        }
    }

    #[test]
    fn unknown_tar_entry_types_are_rejected() {
        // A FIFO entry (type '6').
        let mut bytes = tar_bytes(&[Item::File("top/f", b"", 0o644)]);
        bytes[156] = b'6';
        let sum: u32 = bytes[..512]
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    *b as u32
                }
            })
            .sum();
        bytes[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let (_dir, result) = run(
            &crate::install::extract::testutil::gzip(&bytes),
            ArchiveFormat::TarGz,
        );
        assert!(result.unwrap_err().contains("unsupported type"));
    }

    #[test]
    fn a_non_empty_destination_is_refused() {
        let dir = TempDir::new("extract-dirty");
        let archive = dir.path().join("a.zip");
        fs::write(&archive, zip_bytes(&[("a", b"A")])).unwrap();
        let dest = dir.path().join("payload");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("stale"), b"x").unwrap();
        let error = extract_archive(&archive, ArchiveFormat::Zip, &dest, &dir.path().join("w"))
            .unwrap_err();
        assert!(error.contains("not empty"));
        assert!(
            dest.join("stale").exists(),
            "a refused target is left alone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_exec_bits_are_preserved_and_special_bits_dropped() {
        use std::os::unix::fs::PermissionsExt;
        let bytes = tar_gz(&[
            Item::File("top/run", b"#!/bin/sh", 0o4755),
            Item::File("top/data", b"d", 0o644),
        ]);
        let (dir, result) = run(&bytes, ArchiveFormat::TarGz);
        result.unwrap();
        let mode = |name: &str| {
            fs::metadata(dir.path().join("payload").join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("run"), 0o755);
        assert_eq!(mode("data"), 0o644);
    }

    #[test]
    fn xz_archives_extract_through_the_pure_rust_decoder() {
        // xz stream of the tar below, produced once with `xz -9e` and embedded
        // so the test needs no external tool.
        let tar = tar_bytes(&[
            Item::Dir("amt-x86_64-unknown-linux-gnu/"),
            Item::File("amt-x86_64-unknown-linux-gnu/amt", b"BIN", 0o755),
        ]);
        let mut compressed = Vec::new();
        lzma_rs::xz_compress(&mut &tar[..], &mut compressed).unwrap();
        let (dir, result) = run(&compressed, ArchiveFormat::TarXz);
        let report = result.unwrap();
        assert_eq!(
            report.stripped_top.as_deref(),
            Some("amt-x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            fs::read(dir.path().join("payload").join("amt")).unwrap(),
            b"BIN"
        );
        assert!(!dir.path().join("work").join("decompressed.tar").exists());
    }

    #[test]
    fn corrupt_archives_fail_cleanly() {
        let (dir, result) = run(b"this is not an archive", ArchiveFormat::TarGz);
        assert!(result.is_err());
        assert!(!dir.path().join("payload").exists());
        let (_dir, result) = run(b"nope", ArchiveFormat::Zip);
        assert!(result.is_err());
        let (_dir, result) = run(b"nope", ArchiveFormat::TarXz);
        assert!(result.is_err());
    }

    #[test]
    fn strip_logic_requires_a_common_directory() {
        let e = |parts: &[&str], kind| Entry {
            components: parts.iter().map(|s| s.to_string()).collect(),
            kind,
        };
        assert_eq!(
            strip_prefix(&[e(&["a", "x"], Kind::File), e(&["a", "y"], Kind::File)]),
            Some("a".to_string())
        );
        assert_eq!(
            strip_prefix(&[e(&["a", "x"], Kind::File), e(&["b", "y"], Kind::File)]),
            None
        );
        assert_eq!(strip_prefix(&[e(&["a"], Kind::File)]), None);
        assert_eq!(strip_prefix(&[e(&["a"], Kind::Dir)]), Some("a".to_string()));
        assert_eq!(strip_prefix(&[]), None);
    }

    /// Extract every real release archive found in `$SOTHIS_E2E_ASSETS` and
    /// check the files the manifest says it will place. Run manually.
    #[test]
    #[ignore = "needs SOTHIS_E2E_ASSETS (real release assets)"]
    fn real_release_archives_extract_and_contain_the_declared_files() {
        let assets = std::path::PathBuf::from(
            std::env::var_os("SOTHIS_E2E_ASSETS").expect("SOTHIS_E2E_ASSETS is not set"),
        );
        let manifest = crate::manifest::embedded().unwrap();
        let mut checked = 0;
        for tool in manifest
            .tools
            .iter()
            .filter(|t| t.distribution.kind == "tool-installer")
        {
            let version = tool.recommended_version.as_deref().unwrap();
            for (platform, template) in &tool.distribution.assets {
                let asset = template.replace("{version}", version);
                let archive = assets.join(&asset);
                if !archive.is_file() {
                    continue;
                }
                let dir = TempDir::new("real-extract");
                let report = extract_archive(
                    &archive,
                    format_for(&asset).unwrap(),
                    &dir.path().join("payload"),
                    &dir.path().join("work"),
                )
                .unwrap_or_else(|e| panic!("{asset}: {e}"));
                let suffix = if platform.starts_with("windows") {
                    ".exe"
                } else {
                    ""
                };
                for entry in tool.distribution.files.as_ref().unwrap() {
                    let name = crate::manifest::expand_file_entry(entry, suffix);
                    assert!(
                        dir.path().join("payload").join(&name).exists(),
                        "{asset} ({report:?}) lacks {name}"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked >= 3, "only {checked} archives were found");
        println!("extracted and checked {checked} real archives");
    }
}
