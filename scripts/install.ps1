<#
.SYNOPSIS
    Bootstrap the `sothis` binary from a GitHub release, verify it, place it in
    the Sothis bin directory, then hand off to `sothis install`.

.DESCRIPTION
    One-liner (Windows PowerShell 5.1 or PowerShell 7):

        irm https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.ps1 | iex

    With options (a piped script cannot take parameters, so build a script block):

        & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.ps1))) -Profile docs -NoModifyPath

    or download the file and run it: .\install.ps1 -Profile docs

    Verification, in this order and before anything is unpacked:
      1. sha256 of the download against the release's `.sha256` sidecar (one
         line, `<hex> *<asset>`; the filename must match the asset). Mandatory.
      2. The Sigstore bundle with cosign, pinned to this repository's release
         workflow at the release tag:
           https://github.com/Davidb3l/Sothis/.github/workflows/release.yml@refs/tags/v<version>
           issuer https://token.actions.githubusercontent.com
         A missing cosign is fatal unless -AllowMissingCosign (loud warning,
         checksum-only). A cosign that runs and rejects is ALWAYS fatal.

    cosign is looked for in this order: $env:SOTHIS_COSIGN, `cosign` on PATH,
    `cosign-windows-amd64` on PATH, then winget's package directory
    (%LOCALAPPDATA%\Microsoft\WinGet\Packages\Sigstore.Cosign_*).

    sothis.exe is placed in %LOCALAPPDATA%\Sothis\bin (or $env:SOTHIS_HOME\bin
    when SOTHIS_HOME is set) by renaming any running copy aside, never by
    overwriting it. The script never edits PATH itself; `sothis install` does
    (unless -NoModifyPath).

    This script never calls `exit` when it is piped to iex or dot-sourced, so it
    cannot close your terminal. It sets $LASTEXITCODE and throws on failure. It
    only exits when it owns the process (`powershell -File install.ps1`).

.PARAMETER Version
    Release to install, e.g. 0.1.0 or v0.1.0. Default: the latest release.

.PARAMETER Profile
    Profile handed to `sothis install`: core (default), docs, bell, eval, full.

.PARAMETER NoInstall
    Only place sothis; do not run `sothis install`.

.PARAMETER AllowMissingCosign
    Proceed, with a loud warning, when cosign is not installed. Also passed to
    `sothis install` so the tools' own Sigstore checks follow the same choice.

.PARAMETER NoModifyPath
    Passed to `sothis install`: do not edit the user PATH.

.PARAMETER NoPlugins
    Passed to `sothis install`: do not install Claude Code plugins.

.NOTES
    Environment: SOTHIS_HOME, SOTHIS_COSIGN, SOTHIS_BOOTSTRAP_MIRROR
    (`<dir>\Davidb3l\Sothis\v<version>\<asset>` instead of GitHub; tests and
    air-gapped installs; needs -Version).

    Windows PowerShell 5.1 compatible and pure ASCII (5.1 reads a BOM-less
    script in the ANSI code page).
#>

function Install-Sothis {
    [CmdletBinding()]
    param(
        [string]$Version,
        [Alias('Profile')]
        [string]$SuiteProfile = 'core',
        [switch]$NoInstall,
        [switch]$AllowMissingCosign,
        [switch]$NoModifyPath,
        [switch]$NoPlugins
    )

    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    # The progress bar makes Invoke-WebRequest an order of magnitude slower in 5.1.
    $ProgressPreference = 'SilentlyContinue'

    $Repo = 'Davidb3l/Sothis'
    $Issuer = 'https://token.actions.githubusercontent.com'

    function Write-Note([string]$Message) { [Console]::Error.WriteLine('install-sothis: ' + $Message) }
    function Write-Warn([string]$Message) { [Console]::Error.WriteLine('install-sothis: warning: ' + $Message) }

    function Test-SemVer([string]$Text) {
        return ($Text -match '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$')
    }

    # Windows PowerShell 5.1 spawns exes with a single command-line string, so
    # quote each argument by the CommandLineToArgvW rules.
    function ConvertTo-QuotedArgument([string]$Text) {
        if ($Text.Length -gt 0 -and $Text -notmatch '[\s"]') { return $Text }
        $quoted = New-Object System.Text.StringBuilder
        [void]$quoted.Append('"')
        $slashes = 0
        foreach ($c in $Text.ToCharArray()) {
            if ($c -eq '\') {
                $slashes++
            } elseif ($c -eq '"') {
                [void]$quoted.Append([char]92, $slashes * 2 + 1)
                [void]$quoted.Append('"')
                $slashes = 0
            } else {
                if ($slashes -gt 0) { [void]$quoted.Append([char]92, $slashes) }
                [void]$quoted.Append($c)
                $slashes = 0
            }
        }
        if ($slashes -gt 0) { [void]$quoted.Append([char]92, $slashes * 2) }
        [void]$quoted.Append('"')
        return $quoted.ToString()
    }

    # Run a native program without a shell, capturing both streams. Using
    # System.Diagnostics.Process (rather than `& exe 2>&1`) avoids Windows
    # PowerShell 5.1 turning native stderr text into terminating errors.
    function Invoke-Native([string]$Exe, [string[]]$Arguments, [int]$TimeoutSeconds = 300) {
        $info = New-Object System.Diagnostics.ProcessStartInfo
        $info.FileName = $Exe
        $info.Arguments = (($Arguments | ForEach-Object { ConvertTo-QuotedArgument $_ }) -join ' ')
        $info.UseShellExecute = $false
        $info.RedirectStandardOutput = $true
        $info.RedirectStandardError = $true
        $info.CreateNoWindow = $true
        $process = New-Object System.Diagnostics.Process
        $process.StartInfo = $info
        [void]$process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            try { $process.Kill() } catch { }
            throw ("{0} did not finish within {1}s" -f $Exe, $TimeoutSeconds)
        }
        $process.WaitForExit()
        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout   = $stdout.Result
            Stderr   = $stderr.Result
        }
    }

    function Find-Cosign {
        if (-not [string]::IsNullOrEmpty($env:SOTHIS_COSIGN)) {
            # An explicitly configured cosign that is unusable is an error, never
            # a silent fall-through to "not installed".
            if (-not [System.IO.Path]::IsPathRooted($env:SOTHIS_COSIGN)) {
                throw ('SOTHIS_COSIGN must be an absolute path (got {0})' -f $env:SOTHIS_COSIGN)
            }
            if (-not (Test-Path -LiteralPath $env:SOTHIS_COSIGN -PathType Leaf)) {
                throw ('SOTHIS_COSIGN points at {0}, which is not a file' -f $env:SOTHIS_COSIGN)
            }
            return $env:SOTHIS_COSIGN
        }
        foreach ($name in @('cosign', 'cosign-windows-amd64')) {
            $found = @(Get-Command $name -CommandType Application -ErrorAction SilentlyContinue)
            foreach ($candidate in $found) {
                if ($candidate.Path -match '\.(exe|com)$') { return $candidate.Path }
            }
        }
        if (-not [string]::IsNullOrEmpty($env:LOCALAPPDATA)) {
            $packages = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
            if (Test-Path -LiteralPath $packages -PathType Container) {
                $dirs = @(Get-ChildItem -LiteralPath $packages -Directory -Filter 'Sigstore.Cosign_*' -ErrorAction SilentlyContinue | Sort-Object Name)
                foreach ($dir in $dirs) {
                    $exes = @(Get-ChildItem -LiteralPath $dir.FullName -File -Filter 'cosign*.exe' -ErrorAction SilentlyContinue | Sort-Object Name)
                    if ($exes.Count -gt 0) { return $exes[0].FullName }
                }
            }
        }
        return $null
    }

    # ---------------------------------------------------------------- platform

    $isWindowsHost = $true
    if ($PSVersionTable.PSVersion.Major -ge 6) {
        $isWindowsHost = [bool]$IsWindows
    }
    if (-not $isWindowsHost) {
        throw 'this is the Windows installer; on macOS and Linux use: curl -fsSL https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.sh | sh'
    }
    $arch = $env:PROCESSOR_ARCHITEW6432
    if ([string]::IsNullOrEmpty($arch)) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') {
        throw ('Windows on {0} is not supported (Sothis publishes windows-x64 only; there is no native Windows ARM64 release)' -f $arch)
    }
    try {
        # Windows PowerShell 5.1 defaults to TLS 1.0/1.1, which GitHub rejects.
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch { }

    if ($SuiteProfile -notmatch '^[a-z0-9-]+$') { throw ('invalid -Profile: {0}' -f $SuiteProfile) }

    # ----------------------------------------------------------------- version

    $mirror = $env:SOTHIS_BOOTSTRAP_MIRROR
    if ([string]::IsNullOrEmpty($Version)) {
        if (-not [string]::IsNullOrEmpty($mirror)) {
            throw 'SOTHIS_BOOTSTRAP_MIRROR needs an explicit -Version (there is no "latest" on a mirror)'
        }
        Write-Note ('resolving the latest release of {0}' -f $Repo)
        try {
            $release = Invoke-RestMethod -UseBasicParsing -Uri ('https://api.github.com/repos/{0}/releases/latest' -f $Repo) -Headers @{ 'User-Agent' = 'install-sothis'; 'Accept' = 'application/vnd.github+json' }
        } catch {
            throw ('could not query the latest release of {0}: {1} (has a release been published? pass -Version to pick one)' -f $Repo, $_.Exception.Message)
        }
        $Version = [string]$release.tag_name
        if ([string]::IsNullOrEmpty($Version)) { throw ('could not read tag_name from the latest release of {0} (has a release been published? pass -Version to pick one)' -f $Repo) }
    }
    if ($Version.StartsWith('v')) { $Version = $Version.Substring(1) }
    if (-not (Test-SemVer $Version)) { throw ('not a semantic version: {0}' -f $Version) }

    $platform = 'windows-x64'
    $tag = 'v' + $Version
    $topDir = 'sothis-{0}-{1}' -f $Version, $platform
    $asset = $topDir + '.zip'
    $identity = 'https://github.com/{0}/.github/workflows/release.yml@refs/tags/{1}' -f $Repo, $tag

    # ------------------------------------------------------------------ cosign

    $cosign = Find-Cosign
    if ($null -eq $cosign) {
        if ($AllowMissingCosign) {
            Write-Warn ('cosign was not found. The Sigstore signature of {0} will NOT be verified (-AllowMissingCosign); only its sha256 is checked, which cannot detect a tampered release.' -f $asset)
        } else {
            throw 'cosign was not found, so the Sigstore signature cannot be verified. Install it (winget install Sigstore.Cosign, or a release from https://github.com/sigstore/cosign/releases) or set SOTHIS_COSIGN to its absolute path; pass -AllowMissingCosign only to accept checksum-only verification.'
        }
    }

    # ----------------------------------------------------------------- staging

    $work = Join-Path ([System.IO.Path]::GetTempPath()) ('sothis-bootstrap-' + [Guid]::NewGuid().ToString('N'))
    $exitCode = 0
    try {
        $download = Join-Path $work 'download'
        $extract = Join-Path $work 'extract'
        [void](New-Item -ItemType Directory -Path $download -Force)
        [void](New-Item -ItemType Directory -Path $extract -Force)

        function Get-ReleaseFile([string]$Name) {
            $target = Join-Path $download $Name
            if (-not [string]::IsNullOrEmpty($mirror)) {
                $source = Join-Path (Join-Path (Join-Path (Join-Path (Join-Path $mirror 'Davidb3l') 'Sothis') $tag) '') $Name
                if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
                    throw ('{0} is missing from the mirror ({1})' -f $Name, $source)
                }
                Copy-Item -LiteralPath $source -Destination $target
            } else {
                $url = 'https://github.com/{0}/releases/download/{1}/{2}' -f $Repo, $tag, $Name
                try {
                    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $target -Headers @{ 'User-Agent' = 'install-sothis' }
                } catch {
                    throw ('could not download {0} from the {1} release of {2}: {3}' -f $Name, $tag, $Repo, $_.Exception.Message)
                }
            }
            return $target
        }

        Write-Note ('installing sothis {0} ({1})' -f $Version, $platform)
        $assetPath = Get-ReleaseFile $asset
        $sumPath = Get-ReleaseFile ($asset + '.sha256')
        $bundlePath = $null
        if ($null -ne $cosign) { $bundlePath = Get-ReleaseFile ($asset + '.sigstore.json') }

        # 1. sha256 (mandatory): one line, "<64 hex> *<asset>" or two spaces.
        $lines = @(Get-Content -LiteralPath $sumPath | Where-Object { $_.Trim().Length -gt 0 })
        if ($lines.Count -ne 1) { throw ('{0}.sha256 must contain exactly one line (found {1})' -f $asset, $lines.Count) }
        $match = [regex]::Match($lines[0].Trim(), '^([0-9A-Fa-f]{64})\s+\*?(.+)$')
        if (-not $match.Success) { throw ('{0}.sha256 does not start with a 64-digit hex sha256 followed by a filename' -f $asset) }
        $expected = $match.Groups[1].Value.ToLowerInvariant()
        $named = $match.Groups[2].Value.Trim()
        if ($named -cne $asset) { throw ("{0}.sha256 names '{1}', not '{0}'" -f $asset, $named) }
        # .NET directly, not Get-FileHash: that is a script function in a module,
        # and Windows PowerShell 5.1 started from PowerShell 7 inherits a
        # PSModulePath that cannot load it ("Get-FileHash is not recognized").
        $sha = [System.Security.Cryptography.SHA256]::Create()
        $stream = [System.IO.File]::OpenRead($assetPath)
        try {
            $actual = ([System.BitConverter]::ToString($sha.ComputeHash($stream)) -replace '-', '').ToLowerInvariant()
        } finally {
            $stream.Dispose()
            $sha.Dispose()
        }
        if ($actual -ne $expected) {
            throw ('sha256 mismatch for {0}: published {1}, downloaded {2}; the download is corrupt or has been tampered with. Nothing was installed.' -f $asset, $expected, $actual)
        }
        Write-Note ('sha256 ok ({0})' -f $actual)

        # 2. Sigstore. A cosign that runs and fails is always fatal.
        if ($null -ne $cosign) {
            $result = Invoke-Native $cosign @('verify-blob', $assetPath, '--bundle', $bundlePath, '--certificate-identity', $identity, '--certificate-oidc-issuer', $Issuer)
            if ($result.ExitCode -ne 0) {
                if ($result.Stderr) { [Console]::Error.WriteLine($result.Stderr.TrimEnd()) }
                throw ('cosign rejected the signature of {0} (exit {1}). Nothing was installed.' -f $asset, $result.ExitCode)
            }
            Write-Note ('sigstore signature verified ({0})' -f $identity)
        }

        # 3. Extract only after verification, and refuse unsafe entries.
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [System.IO.Compression.ZipFile]::OpenRead($assetPath)
        try {
            foreach ($entry in $zip.Entries) {
                $name = $entry.FullName.Replace('\', '/')
                if ($name.StartsWith('/') -or $name -match '(^|/)\.\.(/|$)' -or $name -match '^[A-Za-z]:') {
                    throw ('{0} contains an unsafe path ({1}); refusing to extract' -f $asset, $entry.FullName)
                }
            }
        } finally {
            $zip.Dispose()
        }
        # Same reason as the hash above: Expand-Archive is a module script function.
        [System.IO.Compression.ZipFile]::ExtractToDirectory($assetPath, $extract)
        $newExe = Join-Path (Join-Path $extract $topDir) 'sothis.exe'
        if (-not (Test-Path -LiteralPath $newExe -PathType Leaf)) { throw ('{0} does not contain {1}/sothis.exe' -f $asset, $topDir) }
        if ((Get-Item -LiteralPath $newExe -Force).Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw ('{0}/sothis.exe is a link; refusing to install it' -f $topDir)
        }

        # ------------------------------------------------------------- placement

        if (-not [string]::IsNullOrEmpty($env:SOTHIS_HOME)) {
            if (-not [System.IO.Path]::IsPathRooted($env:SOTHIS_HOME)) {
                throw ('SOTHIS_HOME must be an absolute path (got {0})' -f $env:SOTHIS_HOME)
            }
            $binDir = Join-Path $env:SOTHIS_HOME 'bin'
        } else {
            if ([string]::IsNullOrEmpty($env:LOCALAPPDATA)) { throw 'LOCALAPPDATA is not set' }
            $binDir = Join-Path (Join-Path $env:LOCALAPPDATA 'Sothis') 'bin'
        }
        [void](New-Item -ItemType Directory -Path $binDir -Force)
        $dest = Join-Path $binDir 'sothis.exe'
        $staged = Join-Path $binDir ('.sothis-new-{0}.exe' -f $PID)
        Copy-Item -LiteralPath $newExe -Destination $staged -Force
        $aside = $null
        try {
            if (Test-Path -LiteralPath $dest) {
                # A running executable can be renamed but not overwritten.
                $aside = Join-Path $binDir ('sothis.exe.old-{0}' -f $PID)
                Move-Item -LiteralPath $dest -Destination $aside -Force
            }
            Move-Item -LiteralPath $staged -Destination $dest -Force
        } catch {
            $failure = $_.Exception.Message
            if (($null -ne $aside) -and (Test-Path -LiteralPath $aside) -and -not (Test-Path -LiteralPath $dest)) {
                Move-Item -LiteralPath $aside -Destination $dest -Force
            }
            if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
            throw ('could not place {0}: {1}' -f $dest, $failure)
        }
        # Earlier swaps that could not be deleted while the old copy was running.
        foreach ($old in @(Get-ChildItem -LiteralPath $binDir -File -Filter 'sothis.exe.old-*' -ErrorAction SilentlyContinue)) {
            Remove-Item -LiteralPath $old.FullName -Force -ErrorAction SilentlyContinue
        }
        Write-Note ('placed {0}' -f $dest)

        # The placed binary must run and report the release version.
        $probe = Invoke-Native $dest @('--version') 30
        $reported = ($probe.Stdout -split "`r?`n")[0].Trim()
        if ($probe.ExitCode -ne 0 -or $reported -ne ('sothis ' + $Version)) {
            Write-Warn ("{0} --version printed '{1}' (exit {2}; expected 'sothis {3}')" -f $dest, $reported, $probe.ExitCode, $Version)
        }

        # --------------------------------------------------------------- hand-off

        if ($NoInstall) {
            Write-Note ('sothis {0} is in {1} (-NoInstall). Next: sothis install --profile {2}' -f $Version, $binDir, $SuiteProfile)
            $onPath = @(($env:PATH -split ';') | Where-Object { $_.TrimEnd('\') -ieq $binDir.TrimEnd('\') }).Count -gt 0
            if (-not $onPath) {
                Write-Note ('{0} is not on PATH; `sothis install` adds it, or run {1} directly' -f $binDir, $dest)
            }
        } else {
            $installArgs = @('install', '--profile', $SuiteProfile)
            if ($AllowMissingCosign) { $installArgs += '--allow-missing-cosign' }
            if ($NoModifyPath) { $installArgs += '--no-modify-path' }
            if ($NoPlugins) { $installArgs += '--no-plugins' }
            Write-Note ('running: sothis ' + ($installArgs -join ' '))
            # Inherit the console: sothis prints its own plan and results.
            # (Out-Host keeps its output out of this function's return value.)
            & $dest @installArgs | Out-Host
            $exitCode = $LASTEXITCODE
        }
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
    return [int]$exitCode
}

# ---- entry point -------------------------------------------------------------
# Never `exit` unless this script owns the process: under `irm | iex` (no script
# file) or when dot-sourced, exiting would close the user's terminal. Report
# through $LASTEXITCODE and a thrown error instead.
$sothisExit = 1
try {
    # The last value is the exit code; anything earlier would be stray output.
    $sothisResult = @(Install-Sothis @args)
    $sothisExit = [int]$sothisResult[-1]
} catch {
    [Console]::Error.WriteLine('install-sothis: error: ' + $_.Exception.Message)
    $sothisExit = 1
}
Remove-Variable sothisResult -ErrorAction SilentlyContinue
$global:LASTEXITCODE = $sothisExit
if ($sothisExit -ne 0) {
    $sothisMessage = 'sothis bootstrap failed (exit code {0})' -f $sothisExit
    if ((-not [string]::IsNullOrEmpty($PSCommandPath)) -and ($MyInvocation.InvocationName -ne '.')) {
        # Run as a script file (powershell -File, or .\install.ps1): the exit
        # ends this script, not the user's shell, and sets the process code.
        exit $sothisExit
    }
    Remove-Variable sothisExit -ErrorAction SilentlyContinue
    throw $sothisMessage
}
Remove-Variable sothisExit -ErrorAction SilentlyContinue
