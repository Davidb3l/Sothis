//! Where release assets come from: GitHub over HTTPS, or a local mirror tree.

use crate::install::fsx::{self, CopyError};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Upper bound for one release archive.
pub const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;
/// Upper bound for the small sidecar files (`.sha256`, `.sigstore.json`).
pub const MAX_SIDECAR_BYTES: u64 = 1024 * 1024;

const GITHUB: &str = "https://github.com";

/// One file of a GitHub release: `https://github.com/<repository>/releases/download/v<version>/<name>`.
#[derive(Debug, Clone, Copy)]
pub struct AssetRef<'a> {
    pub repository: &'a str,
    pub version: &'a str,
    pub name: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub bytes: u64,
    /// sha256 (lowercase hex) of exactly the bytes written to the destination.
    pub sha256: String,
}

pub trait Fetcher {
    /// Fetch `asset` into `dest`, hashing while streaming. On any failure the
    /// partial destination is removed.
    fn fetch(&self, asset: &AssetRef<'_>, dest: &Path, cap: u64) -> Result<Fetched, String>;
    /// Human description of the source, for error messages and the summary.
    fn describe(&self) -> String;
}

fn check_component(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value == "."
        || value.contains("..")
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
    {
        return Err(format!("unsafe release {label}: {value:?}"));
    }
    Ok(())
}

fn check_asset(asset: &AssetRef<'_>) -> Result<(), String> {
    let mut parts = asset.repository.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) => {
            check_component("repository owner", owner)?;
            check_component("repository name", repo)?;
        }
        _ => {
            return Err(format!(
                "release repository must be owner/name, got {:?}",
                asset.repository
            ))
        }
    }
    check_component("version", asset.version)?;
    check_component("asset name", asset.name)
}

/// `<mirror>/<owner>/<repo>/v<version>/<name>`.
pub fn mirror_path(mirror: &Path, asset: &AssetRef<'_>) -> Result<PathBuf, String> {
    check_asset(asset)?;
    Ok(asset
        .repository
        .split('/')
        .fold(mirror.to_path_buf(), |path, part| path.join(part))
        .join(format!("v{}", asset.version))
        .join(asset.name))
}

pub fn release_url(base: &str, asset: &AssetRef<'_>) -> Result<String, String> {
    check_asset(asset)?;
    Ok(format!(
        "{}/{}/releases/download/v{}/{}",
        base.trim_end_matches('/'),
        asset.repository,
        asset.version,
        asset.name
    ))
}

fn write_destination<F>(dest: &Path, produce: F) -> Result<Fetched, String>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<(u64, String), String>,
{
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let file =
        File::create(dest).map_err(|e| format!("could not create {}: {e}", dest.display()))?;
    let mut writer = BufWriter::new(file);
    let result = produce(&mut writer).and_then(|done| {
        writer
            .flush()
            .map_err(|e| format!("could not write {}: {e}", dest.display()))
            .map(|_| done)
    });
    drop(writer);
    match result {
        Ok((bytes, sha256)) => Ok(Fetched { bytes, sha256 }),
        Err(error) => {
            let _ = fsx::remove_any(dest);
            Err(error)
        }
    }
}

fn describe_copy_error(error: CopyError, what: &str) -> String {
    match error {
        CopyError::Io(e) => format!("could not transfer {what}: {e}"),
        CopyError::TooLarge(cap) => format!("{what} exceeds the {cap}-byte size limit"),
    }
}

/// Reads assets from a local directory tree (tests, E2E, air-gapped installs).
#[derive(Debug, Clone)]
pub struct MirrorFetcher {
    pub dir: PathBuf,
}

impl Fetcher for MirrorFetcher {
    fn fetch(&self, asset: &AssetRef<'_>, dest: &Path, cap: u64) -> Result<Fetched, String> {
        let source = mirror_path(&self.dir, asset)?;
        let what = format!("mirror file {}", source.display());
        let metadata = fs::metadata(&source)
            .map_err(|e| format!("release file is missing from the mirror: {what}: {e}"))?;
        if !metadata.is_file() {
            return Err(format!("{what} is not a regular file"));
        }
        let mut input = File::open(&source).map_err(|e| format!("could not open {what}: {e}"))?;
        write_destination(dest, |writer| {
            fsx::copy_hashing(&mut input, writer, cap).map_err(|e| describe_copy_error(e, &what))
        })
    }

    fn describe(&self) -> String {
        format!("mirror {}", self.dir.display())
    }
}

/// How long a download may go without receiving a single byte before it is
/// abandoned. ureq 3 only has whole-phase timeouts (no per-read idle timeout
/// like ureq 2), so [`StallGuardReader`] enforces this.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Downloads from GitHub release URLs.
pub struct HttpFetcher {
    base: String,
    agent: ureq::Agent,
    idle_timeout: Duration,
}

impl HttpFetcher {
    pub fn github() -> Self {
        HttpFetcher::with_base(GITHUB)
    }

    /// `base` must be `https://...` in production; plain `http://` is accepted
    /// only so tests can talk to a loopback server.
    pub fn with_base(base: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(base.starts_with("https://"))
            .max_redirects(8)
            // ureq 3 reads HTTP(S)_PROXY from the environment by default;
            // ureq 2 (and therefore every earlier Sothis) did not. Keep the
            // direct, verified connection.
            .proxy(None)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_send_request(Some(Duration::from_secs(60)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_global(Some(Duration::from_secs(30 * 60)))
            .user_agent(concat!("sothis/", env!("CARGO_PKG_VERSION")))
            .build();
        HttpFetcher {
            base: base.trim_end_matches('/').to_string(),
            agent: ureq::Agent::new_with_config(config),
            idle_timeout: IDLE_TIMEOUT,
        }
    }
}

/// Reads a body on a helper thread so that a connection that goes silent for
/// longer than `idle` fails instead of hanging until the global timeout. The
/// helper exits when the body ends, an error occurs, the reader is dropped, or
/// (for a stalled socket) when the agent's global timeout fires.
struct StallGuardReader {
    chunks: mpsc::Receiver<io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    offset: usize,
    idle: Duration,
}

impl StallGuardReader {
    fn spawn<R: Read + Send + 'static>(mut source: R, idle: Duration) -> io::Result<Self> {
        let (sender, chunks) = mpsc::sync_channel(4);
        thread::Builder::new()
            .name("sothis-download".to_string())
            .spawn(move || loop {
                let mut buffer = vec![0u8; 64 * 1024];
                match source.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        buffer.truncate(read);
                        if sender.send(Ok(buffer)).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            })?;
        Ok(StallGuardReader {
            chunks,
            pending: Vec::new(),
            offset: 0,
            idle,
        })
    }
}

impl Read for StallGuardReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.offset >= self.pending.len() {
            match self.chunks.recv_timeout(self.idle) {
                Ok(Ok(chunk)) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Ok(Err(error)) => return Err(error),
                // The helper dropped its sender: the body ended.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "no data received for {} seconds; the connection stalled",
                            self.idle.as_secs()
                        ),
                    ))
                }
            }
        }
        let count = out.len().min(self.pending.len() - self.offset);
        out[..count].copy_from_slice(&self.pending[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(&self, asset: &AssetRef<'_>, dest: &Path, cap: u64) -> Result<Fetched, String> {
        let url = release_url(&self.base, asset)?;
        let response = match self.agent.get(&url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(code)) => {
                return Err(format!("download of {url} failed: HTTP {code}"))
            }
            Err(error) => return Err(format!("download of {url} failed: {error}")),
        };
        if let Some(length) = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            if length > cap {
                return Err(format!(
                    "{url} is {length} bytes, above the {cap}-byte size limit"
                ));
            }
        }
        let mut reader =
            StallGuardReader::spawn(response.into_body().into_reader(), self.idle_timeout)
                .map_err(|e| format!("could not start the download of {url}: {e}"))?;
        let what = url.clone();
        write_destination(dest, |writer| {
            fsx::copy_hashing(&mut reader, writer, cap).map_err(|e| describe_copy_error(e, &what))
        })
    }

    fn describe(&self) -> String {
        format!("{} (HTTPS)", self.base)
    }
}

/// Choose the asset source: the `--mirror` flag, then `SOTHIS_RELEASE_MIRROR`,
/// then GitHub. A relative `--mirror` is resolved against `cwd`; the
/// environment variable must already be absolute.
pub fn select_fetcher(
    flag: Option<&Path>,
    env: Option<&Path>,
    cwd: &Path,
) -> Result<Box<dyn Fetcher>, String> {
    let dir = match (flag, env) {
        (Some(flag), _) => Some(if flag.is_absolute() {
            flag.to_path_buf()
        } else {
            cwd.join(flag)
        }),
        (None, Some(env)) => {
            if !env.is_absolute() {
                return Err(format!(
                    "SOTHIS_RELEASE_MIRROR must be an absolute path (got {})",
                    env.display()
                ));
            }
            Some(env.to_path_buf())
        }
        (None, None) => None,
    };
    match dir {
        Some(dir) => {
            if !dir.is_dir() {
                return Err(format!(
                    "release mirror {} is not a directory",
                    dir.display()
                ));
            }
            Ok(Box::new(MirrorFetcher { dir }))
        }
        None => Ok(Box::new(HttpFetcher::github())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::fsx::testutil::TempDir;
    use std::io::Read;
    use std::net::TcpListener;

    fn asset<'a>(name: &'a str) -> AssetRef<'a> {
        AssetRef {
            repository: "Davidb3l/Hayvenhurst-dev",
            version: "0.0.8",
            name,
        }
    }

    #[test]
    fn urls_and_mirror_paths_follow_the_release_layout() {
        let a = asset("hayvenhurst-0.0.8-windows-x64.tar.gz");
        assert_eq!(
            release_url("https://github.com/", &a).unwrap(),
            "https://github.com/Davidb3l/Hayvenhurst-dev/releases/download/v0.0.8/hayvenhurst-0.0.8-windows-x64.tar.gz"
        );
        let mirror = PathBuf::from("m");
        assert_eq!(
            mirror_path(&mirror, &a).unwrap(),
            mirror
                .join("Davidb3l")
                .join("Hayvenhurst-dev")
                .join("v0.0.8")
                .join("hayvenhurst-0.0.8-windows-x64.tar.gz")
        );
    }

    #[test]
    fn unsafe_release_components_are_rejected() {
        for name in ["../x", "a/b", r"a\b", "", "c:evil", ".."] {
            assert!(release_url("https://x", &asset(name)).is_err(), "{name}");
            assert!(mirror_path(Path::new("m"), &asset(name)).is_err(), "{name}");
        }
        let bad_repo = AssetRef {
            repository: "only-one-part",
            version: "1.0.0",
            name: "a",
        };
        assert!(release_url("https://x", &bad_repo).is_err());
        let traversal = AssetRef {
            repository: "../evil",
            version: "1.0.0",
            name: "a",
        };
        assert!(mirror_path(Path::new("m"), &traversal).is_err());
    }

    fn write_mirror_file(mirror: &Path, name: &str, body: &[u8]) {
        let dir = mirror
            .join("Davidb3l")
            .join("Hayvenhurst-dev")
            .join("v0.0.8");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn mirror_fetch_copies_and_hashes() {
        let mirror = TempDir::new("mirror");
        let work = TempDir::new("mirror-work");
        write_mirror_file(mirror.path(), "a.tar.gz", b"abc");
        let fetcher = MirrorFetcher {
            dir: mirror.path().to_path_buf(),
        };
        let dest = work.path().join("dl").join("a.tar.gz");
        let fetched = fetcher.fetch(&asset("a.tar.gz"), &dest, 100).unwrap();
        assert_eq!(fetched.bytes, 3);
        assert_eq!(
            fetched.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
    }

    #[test]
    fn mirror_fetch_fails_for_missing_and_oversized_files() {
        let mirror = TempDir::new("mirror-neg");
        let work = TempDir::new("mirror-neg-work");
        write_mirror_file(mirror.path(), "big", b"abcdef");
        let fetcher = MirrorFetcher {
            dir: mirror.path().to_path_buf(),
        };
        let missing = fetcher
            .fetch(&asset("nope"), &work.path().join("nope"), 100)
            .unwrap_err();
        assert!(missing.contains("missing from the mirror"), "{missing}");
        let dest = work.path().join("big");
        let big = fetcher.fetch(&asset("big"), &dest, 3).unwrap_err();
        assert!(big.contains("size limit"), "{big}");
        assert!(!dest.exists(), "partial files must be removed");
    }

    #[test]
    fn fetcher_selection_prefers_flag_then_env_then_network() {
        let dir = TempDir::new("select");
        let cwd = dir.path();
        fs::create_dir_all(cwd.join("rel")).unwrap();
        let flag = select_fetcher(Some(Path::new("rel")), None, cwd).unwrap();
        assert_eq!(
            flag.describe(),
            format!("mirror {}", cwd.join("rel").display())
        );
        let env = select_fetcher(None, Some(cwd), Path::new("x")).unwrap();
        assert_eq!(env.describe(), format!("mirror {}", cwd.display()));
        assert!(select_fetcher(None, Some(Path::new("relative")), cwd).is_err());
        assert!(select_fetcher(Some(&cwd.join("missing")), None, cwd).is_err());
        let net = select_fetcher(None, None, cwd).unwrap();
        assert!(net.describe().starts_with("https://github.com"));
    }

    /// Minimal loopback HTTP server: answers each connection from `routes`
    /// (path -> (status line, extra headers, body)).
    fn serve(routes: Vec<(&'static str, &'static str, &'static str, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let route = routes.iter().find(|(p, ..)| *p == path);
                let (status, headers, body): (&str, &str, &[u8]) = match route {
                    Some((_, status, headers, body)) => (status, headers, body),
                    None => ("404 Not Found", "", b"nope"),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn http_fetch_follows_redirects_and_hashes() {
        let target = "/Davidb3l/Hayvenhurst-dev/releases/download/v0.0.8/real.bin";
        let base = serve(vec![
            (
                "/Davidb3l/Hayvenhurst-dev/releases/download/v0.0.8/a.bin",
                "302 Found",
                "Location: /Davidb3l/Hayvenhurst-dev/releases/download/v0.0.8/real.bin\r\n",
                Vec::new(),
            ),
            (target, "200 OK", "", b"abc".to_vec()),
        ]);
        let work = TempDir::new("http");
        let fetcher = HttpFetcher::with_base(&base);
        let dest = work.path().join("a.bin");
        let fetched = fetcher.fetch(&asset("a.bin"), &dest, 100).unwrap();
        assert_eq!(fetched.bytes, 3);
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
        assert_eq!(
            fetched.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn http_fetch_reports_status_errors_and_enforces_the_size_cap() {
        let base = serve(vec![(
            "/Davidb3l/Hayvenhurst-dev/releases/download/v0.0.8/big.bin",
            "200 OK",
            "",
            vec![7u8; 64],
        )]);
        let work = TempDir::new("http-neg");
        let fetcher = HttpFetcher::with_base(&base);
        let missing = fetcher
            .fetch(&asset("absent.bin"), &work.path().join("absent"), 100)
            .unwrap_err();
        assert!(missing.contains("HTTP 404"), "{missing}");
        let dest = work.path().join("big");
        let big = fetcher.fetch(&asset("big.bin"), &dest, 10).unwrap_err();
        assert!(big.contains("size limit"), "{big}");
        assert!(!dest.exists());
    }

    #[test]
    fn http_fetch_fails_when_the_connection_stalls_mid_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                // Promise 100 bytes, deliver 3, then go silent.
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc",
                );
                let _ = stream.flush();
                std::thread::sleep(Duration::from_secs(5));
            }
        });
        let work = TempDir::new("http-stall");
        let mut fetcher = HttpFetcher::with_base(&format!("http://127.0.0.1:{port}"));
        fetcher.idle_timeout = Duration::from_millis(300);
        let dest = work.path().join("stalled");
        let started = std::time::Instant::now();
        let error = fetcher
            .fetch(&asset("stalled.bin"), &dest, 1000)
            .unwrap_err();
        assert!(error.contains("stalled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(4), "{error}");
        assert!(!dest.exists(), "partial files must be removed");
    }
}
