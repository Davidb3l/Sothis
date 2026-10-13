---
name: sothis
description: >-
  Install, initialize, validate, update, roll back, uninstall, or diagnose the
  complete Sothis suite. Use for "let's Sothis this up", suite installation,
  suite compatibility, cross-tool health, and platform support. Sothis owns
  distribution and compatibility; Sirius Forester owns the claim-to-receipt
  runtime loop.
---

# Sothis suite distribution

Use the native `sothis` CLI as the source of truth. Do not reproduce platform
or version tables from memory; they are embedded from `suite.manifest.json`.

If `sothis --version` is not found, give the user the bootstrap one-liner and
stop (do not run it unprompted; never fetch other scripts):

- Windows: `irm https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.ps1 | iex`
- macOS / Linux / Git Bash: `curl -fsSL https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.sh | sh`

## Install

1. Run `sothis manifest validate`.
2. Run `sothis install --profile <profile> --plan` and show the resolved plan.
3. Never round `native-preview`, `wsl2`, `source-only`, or `unsupported` up to
   native support.
4. **Get the user's explicit confirmation**, then run
   `sothis install --profile <profile>`. It mutates the machine (downloads,
   places binaries, edits the Windows user `PATH`, installs Claude plugins);
   `--no-modify-path` and `--no-plugins` opt out. Add `--allow-missing-cosign`
   only if the user knowingly accepts checksum-only verification.
5. Run `sothis doctor` after any machine mutation and report every `[WARN]` and
   `[FAIL]`. `PATH` changes only reach new processes: tell the user to restart
   terminals, editor and Claude Code.
6. Initialize project-level suite configuration with `sothis init`; it is
   idempotent and never overwrites an existing `.suite/config.json`.

## Maintain

Each of these changes the machine: show what will happen and get confirmation
first (`sothis update --plan` previews an update).

- `sothis update [--profile <p>]` moves Sothis-managed tools to the manifest's
  recommended versions. It never downgrades without `--allow-downgrade` and
  reports tools it did not install as `unmanaged`.
- `sothis rollback <tool>` returns one tool to its previous retained version;
  run it again to go forward.
- `sothis uninstall [--profile <p>]` removes only what Sothis installed, in
  reverse order, and restores files it backed up. `--plugins` also removes the
  Claude plugins. Tool data (`.ametrite`, `.hayven`, `.sirius`, `.suite`) is
  never touched.
- `sothis env` prints the Sothis root and bin directory (`SOTHIS_HOME`
  overrides both, which makes a throwaway trial safe).

A failed tool is rolled back and the command exits non-zero; report exactly
which tools succeeded. A sha256 mismatch or a rejected signature is never
bypassed.

## Diagnose

`sothis doctor [--profile <p>]` checks versions against the manifest minimums,
copies of a command shadowing each other on `PATH`, the Claude plugin version
(Catryna needs 1.3.1), the Bun runtime, whether placed files still match their
receipts, and Sirius's integration contracts (run from the enclosing `.sirius/`
workspace, skipped outside one). Quote its lines; `SKIP` means "not applicable
here", `WARN` means "works, but fix it". The cross-tool rules are in
`docs/CONTRACTS.md`.

Profiles:

- `core`: Ametrite, Hayvenhurst, and Sirius Forester.
- `docs`: core plus Catryna Wikinelli (Bun is checked, never installed).
- `bell`: PingMyBell, built from source where supported (not installed by Sothis).
- `eval`: Guignet, including its explicit WSL2 bridge on Windows.
- `full`: every tool, with unsupported entries skipped rather than hidden.

Canonical Claude plugin identities use `@sothis`. Treat `@sirius-forester` as
a migration source only, according to the two-release window in D-2.
