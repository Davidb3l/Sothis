//! Shell-free child process helpers (explicit args, captured output, timeout).

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Captured {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `command` to completion, capturing stdout/stderr, killing it after
/// `timeout`. Stdin is closed. No shell is involved.
pub fn run_capture(mut command: Command, timeout: Duration) -> io::Result<Captured> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(out) = out.as_mut() {
            let _ = out.read_to_end(&mut buffer);
        }
        buffer
    });
    let err_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(err) = err.as_mut() {
            let _ = err.read_to_end(&mut buffer);
        }
        buffer
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("process did not finish within {}s", timeout.as_secs()),
                ));
            }
            None => std::thread::sleep(Duration::from_millis(15)),
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Captured {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Last `max_lines` non-empty lines of `text`, joined with " | ".
pub fn tail(text: &str, max_lines: usize) -> String {
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join(" | ")
}

/// Describe a failure to spawn `path`, translating the Windows Smart App
/// Control / App Control block (os error 4551) into actionable guidance.
pub fn describe_spawn_error(path: &Path, error: &io::Error) -> String {
    describe_spawn_error_for(path, error, cfg!(windows))
}

pub fn describe_spawn_error_for(path: &Path, error: &io::Error, windows: bool) -> String {
    if windows && error.raw_os_error() == Some(4551) {
        format!(
            "could not run {}: blocked by Windows Smart App Control / App Control policy (os error 4551) — see SOT-28 and the Smart App Control section of the Sothis README; build from source or use a signed build",
            path.display()
        )
    } else {
        format!("could not run {}: {error}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_error_4551_maps_to_the_smart_app_control_message_on_windows_only() {
        let error = io::Error::from_raw_os_error(4551);
        let message = describe_spawn_error_for(Path::new("C:/x/amt.exe"), &error, true);
        assert!(message.contains("Smart App Control"), "{message}");
        assert!(message.contains("os error 4551"));
        assert!(message.contains("SOT-28"));
        let other = describe_spawn_error_for(Path::new("/x/amt"), &error, false);
        assert!(!other.contains("Smart App Control"));
        let plain = io::Error::from(io::ErrorKind::NotFound);
        assert!(!describe_spawn_error_for(Path::new("a"), &plain, true).contains("4551"));
    }

    #[test]
    fn tail_keeps_the_last_non_empty_lines() {
        assert_eq!(tail("a\n\nb\nc\nd\n", 2), "c | d");
        assert_eq!(tail("", 3), "");
        assert_eq!(tail("only", 3), "only");
    }

    #[test]
    fn run_capture_collects_output_and_status() {
        let exe = std::env::current_exe().unwrap();
        // The test harness exits 0 and prints a listing for --list.
        let mut command = Command::new(exe);
        command.args(["--list", "--format", "terse"]);
        let captured = run_capture(command, Duration::from_secs(60)).unwrap();
        assert!(captured.success);
        assert!(captured
            .stdout
            .contains("tail_keeps_the_last_non_empty_lines"));
    }

    #[test]
    fn run_capture_reports_spawn_failures() {
        let missing = Path::new("definitely-not-a-real-binary-sothis");
        assert!(run_capture(Command::new(missing), Duration::from_secs(5)).is_err());
    }
}
