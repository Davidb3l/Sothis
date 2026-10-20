# ADR 0002: Staged, verified, receipted install

- Status: accepted
- Date: 2026-10-12

## Context

ADR 0001 made Sothis responsible for installing, updating and removing the
suite's tools. The `0.1.x` foundation only resolved plans (`--plan`); machine
mutation was withheld until it could be done without weakening the supply-chain
checks each tool already publishes (a `.sha256` for every asset, plus a Sigstore
bundle for Hayvenhurst and Sirius Forester) and without being able to leave a
half-installed machine behind. Windows adds two constraints: a running `.exe`
cannot be overwritten (but can be renamed), and a user's PATH lives in the
registry.

## Decision

`sothis install|update|uninstall|rollback` run one pipeline per tool, under a
global OS file lock (`<root>/state/lock`, fail fast with exit 1), and every
mutation is recorded in a journal so a failure restores the exact prior state.

1. **Layout.** State root: `%LOCALAPPDATA%\Sothis` on Windows,
   `$XDG_DATA_HOME/sothis` or `~/.local/share/sothis` elsewhere. Entry points go
   to `<root>\bin` on Windows and `~/.local/bin` on macOS/Linux.
   `SOTHIS_HOME` (absolute) replaces both with `<SOTHIS_HOME>` and
   `<SOTHIS_HOME>/bin` on every OS, which keeps tests and trials hermetic.
   Each installed version is retained whole under `tools/<id>/<version>/`
   (current plus previous; older ones are pruned).
2. **Source.** GitHub release URLs over HTTPS (redirects followed, timeouts and
   a 512 MiB size cap), or a local mirror tree
   (`--mirror` / `SOTHIS_RELEASE_MIRROR`: `<dir>/<owner>/<repo>/v<version>/<asset>`)
   for tests and air-gapped installs. Downloads are hashed while streaming.
3. **Verification, fail closed.** The `.sha256` sidecar must be one line whose
   filename equals the asset; a mismatch is fatal. For `sha256+sigstore` tools
   `cosign verify-blob` is run (no shell) against the asset's `.sigstore.json`
   with the manifest's identity (`{version}` substituted) and the GitHub Actions
   OIDC issuer. A cosign that runs and rejects is always fatal. A missing cosign
   is fatal too, unless the user passes `--allow-missing-cosign`, which proceeds
   with a loud warning and records `signature: skipped-no-cosign` in the
   receipt. Checksum-only tools (Ametrite) record `not-published`. Nothing is
   unpacked before verification passes.
4. **Safe extraction.** `.tar.gz`, `.tar.xz`, `.zip`. Absolute paths, `..`,
   backslashes, drive/UNC prefixes, alternate-data-stream colons, reserved
   Windows device names, symlinks, hardlinks, special files and duplicate
   entries are rejected before anything is written; entry count and unpacked
   size are bounded; a single top-level directory is stripped.
5. **Manifest-declared payload.** Each `tool-installer` tool declares `files`
   (top-level entries, `{exe}` expands to `.exe` on Windows). Only those entries
   are placed; all must exist in the payload before the bin directory is
   touched. The JSON schema and `manifest::validate` enforce the same grammar.
6. **Atomic placement.** Entries are copied to `.sothis-stage-*` beside their
   destination, an existing entry is *renamed* aside (never overwritten), and
   the staged copy is renamed in. After placement the installed
   `<command> --version` must report exactly the installed version; otherwise
   the journal is undone. Unowned prior files are "adopted": backed up under
   `<root>/backups/<id>/` and restored on uninstall.
7. **Receipts.** `<root>/state/receipts/<id>.json` (`schemaVersion` 1, unknown
   fields rejected, written via temp file + rename) records version, asset and
   its sha256, signature status, bin dir, per-file hashes, previous version and
   adopted priors. Only receipt-owned entries are ever removed or replaced.
   Install of an unchanged version is a no-op; `update` never downgrades
   without `--allow-downgrade`; `rollback <tool>` re-places the retained
   previous version after re-checking its recorded hashes. Tool data
   (`.ametrite`, `.hayven`, `.sirius`, `.suite`) is never touched.
8. **PATH.** Windows: prepend the bin directory to `HKCU\Environment\Path`
   (idempotent, case-insensitive, trailing-separator tolerant, `REG_SZ` vs
   `REG_EXPAND_SZ` preserved) and broadcast `WM_SETTINGCHANGE`; remove it on
   uninstall only when no receipts remain. macOS/Linux: shell rc files are never
   edited; the exact line to add is printed. `--no-modify-path` disables edits.
   Output always notes that PATH changes are visible only to new processes, and
   warns when another directory earlier on PATH provides the same command.
9. **Plugins.** Through the `claude plugin` CLI only: add the `sothis`
   marketplace if unknown, then `install <name>@sothis` for each tool plugin and
   for `sothis@sothis`. A legacy `<name>@sirius-forester` install is reported
   with the migration commands and left alone. `--no-plugins` skips the step;
   uninstall removes plugins only with `--plugins`. The Bun runtime is checked,
   never installed.
10. **Failure scope.** A failing tool is rolled back; tools that already
    succeeded are kept and the command exits 1. Usage errors exit 2.

## Consequences

- A machine is either in the old state or the new one per tool; the worst
  residue of a hard crash is a `rollback/` directory that the next run reports
  (it is never auto-deleted because it may hold a user's only original file).
- Windows installs work while the old binary is running, and `rollback` is a
  file move away.
- Security rests on the checksum for Ametrite and on checksum + Sigstore for
  the others; the checksum is fetched from the same origin as the asset, so it
  protects integrity, and the Sigstore bundle provides authenticity.
- cosign is an external prerequisite for signed tools; its absence is an
  explicit, recorded user decision rather than a silent downgrade.
- Windows Smart App Control can block newly placed unsigned executables
  (os error 4551); the failure is reported with that explanation (SOT-28)
  instead of a bare spawn error.
- The crate gained dependencies (ureq/rustls, sha2, flate2, tar, lzma-rs, zip,
  and `winreg`/`windows-sys` on Windows) and its minimum Rust version is now
  1.89 (std file locking).

## Amendment (v0.1.1): the Windows root moves out of AppData

Date: 2026-10-20. Item 1 (Layout) above is superseded for Windows.

The Claude desktop app on Windows is an MSIX-packaged app. Processes it
launches, including Claude Code's shells and therefore `sothis` and
`install.ps1`, run inside the package container, where file-system
virtualization redirects new writes under `%LOCALAPPDATA%` and `%APPDATA%` to
`%LOCALAPPDATA%\Packages\<PackageFamilyName>\LocalCache\Local\...`. Registry
writes are not virtualized. A v0.1.0 install run from Claude Code therefore
landed in the package's private folder, visible only inside the container, while
the user `PATH` (`HKCU\Environment`) received a real entry for
`%LOCALAPPDATA%\Sothis\bin`, a directory that does not exist for any normal
process, which kept using its old tool copies. Writes outside AppData
(`%USERPROFILE%\.local\bin`, `%USERPROFILE%\.cargo\bin`) are not virtualized,
which is why the tools' own installers worked.

Decision, all on Windows (macOS and Linux are unchanged, and `SOTHIS_HOME` still
replaces everything):

1. **Default root `%USERPROFILE%\.sothis`**, bin `%USERPROFILE%\.sothis\bin`,
   mirroring `.cargo` and `.bun`. `USERPROFILE` (else the OS profile directory)
   must be absolute. The bootstrap scripts place `sothis` in the same directory.
2. **Legacy migration.** `install`/`update` remove exactly the Sothis-owned
   `%LOCALAPPDATA%\Sothis\bin` entry (expanded or unexpanded spelling, case and
   trailing-slash insensitive) from the user `PATH` while prepending the new bin,
   and report a migration line; nothing else on `PATH` is touched, and the value
   type is preserved. A non-empty `%LOCALAPPDATA%\Sothis\state\receipts` is never
   adopted or deleted: it is reported (a warning from `install`/`update`, and the
   `legacy_root` warning in `doctor`) with the commands that remove it.
3. **Packaged-context detection** via `GetCurrentPackageFullName`
   (`ERROR_SUCCESS`/`ERROR_INSUFFICIENT_BUFFER` when packaged,
   `APPMODEL_ERROR_NO_PACKAGE` otherwise), behind a `PackageContext` trait so
   tests can fake it. `doctor` reports the package and fails
   `root_not_virtualized` for a root under `%LOCALAPPDATA%` or `%APPDATA%` while
   packaged; `install`/`update` refuse, writing nothing, unless
   `--allow-virtualized-root` is given (`uninstall` and `rollback` stay possible
   so an old install can still be removed). `SOTHIS_ASSUME_PACKAGE` makes the
   process report a package for tests; it can only make the checks stricter.
