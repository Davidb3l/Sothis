# Sothis suite contracts

Status: normative for suite `0.1.0`. The words MUST, SHOULD and MAY are used
as in RFC 2119. `sothis doctor` is the executable form of §1, §2, §4 and §6;
the install engine ([ADR 0002](adr/0002-staged-verified-install.md)) relies on
§1.

Each member tool stays independently installable and owns its own runtime
state ([ADR 0001](adr/0001-sothis-owns-suite-distribution.md)). These contracts
are the only things Sothis asks of them, and the only things the members may
assume about each other.

## 1. Version handshake

`<cmd> --version` MUST print a semantic version (`MAJOR.MINOR.PATCH`, optional
pre-release and build parts, optional leading `v`) somewhere in its **first
line**, and exit `0`. Anything else on that line (the tool name, a commit hash
in parentheses) is ignored. Output goes to stdout; stderr is accepted only when
stdout is empty.

The version is compared with the manifest's `minimumVersion` (doctor) or must
equal the installed version (the install engine's post-placement check).
A pre-release sorts below its release (`1.2.0-rc.1 < 1.2.0`). A version that
cannot be parsed is a failure, never a pass.

Tools without a CLI on `PATH` (PingMyBell, a desktop app built from source)
declare no `command` in the manifest and are exempt.

## 2. Doctor envelope

`<cmd> doctor --json` MUST print exactly one JSON object on stdout:

```json
{
  "tool": "amt",
  "version": "0.4.2",
  "schemaVersion": 1,
  "ok": true,
  "capabilities": ["board"],
  "checks": [
    { "name": "db_integrity", "ok": true, "detail": "...", "gating": true },
    { "name": "update_check", "ok": false, "detail": "...", "gating": false }
  ],
  "report": {}
}
```

- `ok` is true **if and only if** every check with `gating: true` passes.
  Non-gating checks are advisory and never change `ok`.
- The process MUST exit `0` whenever the tool ran at all. A non-zero exit means
  "absent or crashed", not "unhealthy": health lives in `ok`, never in the exit
  code. (`sothis doctor` treats a non-zero exit as a failure either way.)
- `schemaVersion` is the version of **this envelope** (currently `1`). It is not
  a database schema version; see §5.
- `report` is optional, tool-specific, and not interpreted by Sothis.
- A check MAY use `pass` instead of `ok` for its own verdict while the envelope
  is being aligned; Sothis reads either.

How Sothis reads it: the top-level `ok` is authoritative. A failing check
explicitly marked `gating: true` fails the doctor even if the tool said
`ok: true`; failing checks marked `gating: false` are listed as advisory and
never blamed. A check with **no** `gating` field is treated as gating.

Conformance at `0.1.0`:

| Tool | `doctor --json` envelope | `gating` on checks |
| --- | --- | --- |
| Ametrite (`amt`) | yes | added in its next release |
| Hayvenhurst (`hayven`) | yes | restored in its next release |
| Sirius Forester (`sirius`) | yes | yes |
| Catryna Wikinelli | yes | not yet: Sothis treats every check as gating |
| Guignet (`guignet`) | in its next release | yes, in its next release |

`sirius doctor --json` is **workspace-bound**: it checks the repo it runs in
(`.ametrite/`, `.hayven/`, the gate configuration). `sothis doctor` therefore
runs it from the nearest ancestor of the current directory that contains a
`.sirius/` directory (the search never looks at or above the user's home
directory), and reports `suite_contracts` as *skipped* when there is none. It
never reports a workspace's health for a directory that is not one.

## 3. Windows spawn contract

Derived from SOT-27. Any suite component that starts a child process on Windows
MUST:

1. start long-lived children **hidden** (`CREATE_NO_WINDOW`, or `windowsHide`
   in Node/Bun), so a background daemon or MCP server never flashes a console
   window and closing a terminal does not kill it;
2. not leak handles: inherit nothing beyond the three standard streams;
3. set the child's working directory to a directory **inside the workspace**,
   never a system directory and never a path that disappears with a temp dir;
4. supervise child trees with a **Job Object** created with
   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so a crashed or killed parent does not
   orphan its descendants;
5. never invoke a bare `bash`. On Windows `bash` resolves to the **WSL
   launcher** (`C:\Windows\System32\bash.exe`), which runs in a different
   filesystem and fails (for example MCP `CONNECTION_CLOSED`). Run the program
   directly, or use an explicitly resolved shell (an absolute path) and name it in
   any error message;
6. strip `\\?\` **verbatim prefixes** from any path handed to another program
   or placed in an environment variable. Many tools (cmd.exe, MSBuild, older
   Node) cannot read them.

Spawning is always shell-free with an explicit argument vector. On Windows,
`.cmd`/`.bat` entry points are launched through the standard library's
escaping-safe path, never by string concatenation.

## 4. Peer resolution

A tool that runs one of its peers (Sirius running `amt` and `hayven`) MUST
resolve the peer **`PATH`-first** to an **absolute path** and spawn that path.

Windows `CreateProcess` searches the *calling executable's own directory*
before `PATH` when given a bare name. A stale peer left next to the caller
(for example `sirius.exe` and an old `amt.exe` both in `~/.local/bin`) is then
used no matter what `PATH` says. Resolving explicitly first removes the
surprise. Until the next Sirius release does this, `sothis doctor` warns when a
stale peer sits beside `sirius.exe`.

`sothis doctor` also lists **every** copy of a command on `PATH`
(`<path> -> <version>`). It warns when there is more than one distinct file, and
fails when the first one is below the manifest minimum while a later one meets
it (`earlier PATH entry <p> (<v>) shadows <q> (<w>); remove the stale copy`).
`sothis install` prints the equivalent warning at install time.

## 5. Ametrite schema contract

`contracts.minimumSchemaVersion` (Ametrite) and `contracts.minimumAmetriteSchemaVersion`
(Sirius Forester) in `suite.manifest.json` refer to the SQLite
`meta.schema_version` of `.ametrite/ametrite.db` (**currently `5`**; the suite
requires at least `3`).

They are **not** the `schemaVersion` of `amt doctor --json`: that is the
envelope version from §2 and is `1`. The two numbers are unrelated and MUST NOT
be compared with each other.

Sothis does not read `.ametrite/ametrite.db`: these two fields are a
compatibility declaration for the tools that open the board, not something
`sothis doctor` verifies.

## 6. Plugin identities

The canonical Claude Code plugin identity is `<name>@sothis`
(`sothis@sothis`, `sirius@sothis`, `hayvenhurst@sothis`, `catryna@sothis`).

The legacy `<name>@sirius-forester` identities are accepted as a migration
source until two Sothis releases after `0.1.0` (ADR 0001, Ametrite decision
D-2); `suite.manifest.json` records the window as `removeAfterSuiteReleases: 2`.
`sothis doctor` passes a legacy install with a "migrate to" note, and
`sothis install` reports the migration commands without touching it.

A tool's manifest `plugin.minimumVersion` is the oldest plugin version the
suite supports. `sothis doctor` reads the highest installed version of
`<name>@*` from `installed_plugins.json` (user and project installs alike),
fails below the minimum with `update the plugin: claude plugin update <id>`, and
warns when the version cannot be read. Catryna requires `1.3.1`: earlier
copies launch their MCP server with a bare `bash` (§3.5), and `1.3.1` launches
with Bun and runs shell-free hooks.
