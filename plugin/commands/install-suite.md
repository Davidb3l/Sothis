---
description: Validate the Sothis manifest, show the platform-resolved plan, install the suite after you confirm, then verify it with sothis doctor.
argument-hint: "[--profile core|docs|bell|eval|full] [--no-modify-path] [--no-plugins]"
allowed-tools: Bash(sothis --version:*), Bash(sothis manifest:*), Bash(sothis install --plan:*), Bash(sothis doctor:*), Bash(sothis env:*)
---

# Install the Sothis suite

The Sothis plugin owns suite distribution. Sirius owns only the supervised
agent loop. Installation is done by the native `sothis` CLI, which downloads
each tool from its own release, verifies the sha256 and (where published) the
Sigstore signature, places the files atomically, and records receipts so every
step can be rolled back.

## 1. Preflight

Run `sothis --version`. If `sothis` is not found, **stop** and give the user the
bootstrap one-liner for their platform; do not run it for them unprompted, and
never fetch or pipe any other script. It downloads, verifies and places the
`sothis` binary, then runs `sothis install --profile core`:

```powershell
irm https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.ps1 | iex
```

```sh
curl -fsSL https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.sh | sh
```

(From a Sothis checkout, `cargo install --path .` installs just the binary.)
Do not silently fall back to a legacy suite script.

## 2. Validate and plan

With no `--profile` argument the CLI uses the project's `.suite/config.json`
profile, else `core`. Forward a caller-supplied argument list exactly once:

```sh
sothis manifest validate
sothis install --plan $ARGUMENTS
```

Show the plan exactly as emitted, including every `native-preview`, `wsl2`,
`source-only` and `unsupported` entry. Never describe those as installed or
supported natively.

## 3. Confirm, then install

`sothis install` **changes the machine**: it downloads and places binaries in the
Sothis bin directory (`sothis env` shows where), on Windows prepends that
directory to the user `PATH`, and installs Claude Code plugins. Unless the
arguments already include `--no-modify-path` / `--no-plugins`, say so.

**Show the plan and get the user's explicit confirmation before running it.**
Then run:

```sh
sothis install $ARGUMENTS
```

- Do not add `--allow-missing-cosign` on your own. If it fails with
  `cosign was not found`, tell the user how to install cosign
  (`winget install Sigstore.Cosign`, `brew install cosign`) and let them decide
  whether checksum-only verification is acceptable.
- A `sha256 mismatch` or `cosign rejected the signature` is never bypassed.
- A failed tool is rolled back; report which ones succeeded and which failed.
- Relay warnings verbatim (a stale copy of a command earlier on `PATH`, the bin
  directory not yet on `PATH`). `PATH` changes only reach new processes: tell the
  user to restart their terminals, editor and Claude Code.

## 4. Verify

Completion is defined by:

```sh
sothis doctor
```

Report every `[WARN]` and `[FAIL]` line. Do not call the install done while a
check fails.

The canonical plugin identities are `sothis@sothis`, `sirius@sothis`,
`hayvenhurst@sothis`, and `catryna@sothis`. The old `@sirius-forester`
identities remain a compatibility path for two Sothis releases under D-2.
