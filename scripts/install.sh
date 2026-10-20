#!/bin/sh
# install.sh - bootstrap the `sothis` binary from a GitHub release, verify it,
# place it in the Sothis bin directory, then hand off to `sothis install`.
#
#   curl -fsSL https://raw.githubusercontent.com/Davidb3l/Sothis/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --profile docs --no-modify-path
#
# Options:
#   --version V             Release to install (default: the latest release)
#   --profile P             Profile handed to `sothis install` (default: core)
#   --no-install            Only place sothis; do not run `sothis install`
#   --allow-missing-cosign  Proceed, with a loud warning, if cosign is not
#                           installed (a cosign that runs and FAILS is always
#                           fatal). Also passed to `sothis install`.
#   --no-modify-path        Passed to `sothis install`
#   --no-plugins            Passed to `sothis install`
#   --allow-virtualized-root  Passed to `sothis install` (see the note below)
#   -h, --help              This text
#
# What it verifies, in order, before anything is unpacked: the download's
# sha256 against the release's `.sha256` (the filename inside must match), then
# its Sigstore bundle with cosign, pinned to this repository's release workflow
# at the release tag. A missing cosign is fatal unless --allow-missing-cosign.
#
# Where sothis goes (the same directory `sothis env` reports):
#   Windows (Git Bash/MSYS)  %USERPROFILE%\.sothis\bin
#                            (not %LOCALAPPDATA%: processes started by the Claude desktop app
#                            have their writes under AppData redirected into a private
#                            folder, so an install there is invisible to everything else)
#   $SOTHIS_HOME set         $SOTHIS_HOME/bin
#   macOS / Linux            ~/.local/bin
#
# Environment:
#   SOTHIS_HOME              Replace the whole layout (absolute path)
#   SOTHIS_COSIGN            Absolute path to the cosign binary
#   SOTHIS_BOOTSTRAP_MIRROR  Read <dir>/<owner>/<repo>/v<ver>/<asset> instead of
#                            GitHub (tests, air-gapped installs); needs --version
#   SOTHIS_BOOTSTRAP_PLATFORM  Override platform detection (tests only)
#
# POSIX sh; also runs in Git Bash/MSYS. Exit: 0 ok, otherwise the failing
# step's code (sothis's own code after hand-off), 2 for usage errors.

set -eu

REPO="Davidb3l/Sothis"
OIDC_ISSUER="https://token.actions.githubusercontent.com"

die() {
  printf 'install-sothis: error: %s\n' "$*" >&2
  exit 1
}

warn() {
  printf 'install-sothis: warning: %s\n' "$*" >&2
}

note() {
  printf 'install-sothis: %s\n' "$*" >&2
}

usage_error() {
  printf 'install-sothis: %s\n' "$*" >&2
  printf 'run with --help for usage\n' >&2
  exit 2
}

show_help() {
  cat <<'EOF'
install-sothis: bootstrap the sothis binary from a GitHub release, then run `sothis install`.

Usage: install.sh [options]
  --version V             Release to install (default: the latest release)
  --profile P             Profile for `sothis install` (default: core)
  --no-install            Only place sothis; do not run `sothis install`
  --allow-missing-cosign  Proceed with a loud warning if cosign is not installed
                          (a cosign that runs and fails is always fatal)
  --no-modify-path        Passed to `sothis install`
  --no-plugins            Passed to `sothis install`
  --allow-virtualized-root  Passed to `sothis install`
  -h, --help              This text

Verifies the sha256 and the Sigstore signature before unpacking anything.
Environment: SOTHIS_HOME, SOTHIS_COSIGN, SOTHIS_BOOTSTRAP_MIRROR.
EOF
}

VERSION=""
PROFILE="core"
NO_INSTALL=0
ALLOW_MISSING_COSIGN=0
NO_MODIFY_PATH=0
NO_PLUGINS=0
ALLOW_VIRTUALIZED=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      [ $# -ge 2 ] || usage_error "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#--version=}"
      shift
      ;;
    --profile)
      [ $# -ge 2 ] || usage_error "--profile requires a value"
      PROFILE="$2"
      shift 2
      ;;
    --profile=*)
      PROFILE="${1#--profile=}"
      shift
      ;;
    --no-install) NO_INSTALL=1; shift ;;
    --allow-missing-cosign) ALLOW_MISSING_COSIGN=1; shift ;;
    --no-modify-path) NO_MODIFY_PATH=1; shift ;;
    --no-plugins) NO_PLUGINS=1; shift ;;
    --allow-virtualized-root) ALLOW_VIRTUALIZED=1; shift ;;
    -h | --help)
      show_help
      exit 0
      ;;
    *) usage_error "unknown option: $1" ;;
  esac
done

# Both values end up in URLs, file names and the sothis command line.
case "$PROFILE" in
  '' | *[!a-z0-9-]*) usage_error "invalid --profile: $PROFILE" ;;
esac

semver_ok() {
  printf '%s' "$1" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$'
}

# ------------------------------------------------------------------ platform

detect_platform() {
  if [ -n "${SOTHIS_BOOTSTRAP_PLATFORM:-}" ]; then
    printf '%s' "$SOTHIS_BOOTSTRAP_PLATFORM"
    return
  fi
  os="$(uname -s 2>/dev/null || echo unknown)"
  arch="$(uname -m 2>/dev/null || echo unknown)"
  case "$arch" in
    x86_64 | amd64) arch="x64" ;;
    aarch64 | arm64) arch="arm64" ;;
    *) die "unsupported architecture: $arch" ;;
  esac
  case "$os" in
    Linux)
      if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        die "this is a musl libc system; Sothis publishes glibc Linux builds only. Build from source: cargo install --git https://github.com/$REPO"
      fi
      if [ "$arch" = "x64" ]; then printf 'linux-x64-glibc'; else printf 'linux-arm64'; fi
      ;;
    Darwin) printf 'macos-%s' "$arch" ;;
    MINGW* | MSYS* | CYGWIN*)
      [ "$arch" = "x64" ] || die "Windows on $arch is not supported (no native Windows ARM64 release)"
      printf 'windows-x64'
      ;;
    *) die "unsupported operating system: $os" ;;
  esac
}

PLATFORM="$(detect_platform)"
case "$PLATFORM" in
  windows-x64)
    ARCHIVE_EXT="zip"
    EXE="sothis.exe"
    ;;
  linux-x64-glibc | linux-arm64 | macos-x64 | macos-arm64)
    ARCHIVE_EXT="tar.gz"
    EXE="sothis"
    ;;
  *) die "unsupported platform: $PLATFORM" ;;
esac

# ------------------------------------------------------------------- helpers

have() { command -v "$1" >/dev/null 2>&1; }

http_get() { # url, output file
  if have curl; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 2 -o "$2" "$1"
  elif have wget; then
    wget -q --https-only -O "$2" "$1"
  else
    die "neither curl nor wget is installed"
  fi
}

http_text() { # url -> stdout
  if have curl; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 2 -H 'Accept: application/vnd.github+json' "$1"
  elif have wget; then
    wget -q --https-only -O - "$1"
  else
    die "neither curl nor wget is installed"
  fi
}

sha256_of() { # file -> lowercase hex
  if have sha256sum; then
    sha256sum "$1" | sed 's/[[:space:]].*//'
  elif have shasum; then
    shasum -a 256 "$1" | sed 's/[[:space:]].*//'
  elif have openssl; then
    openssl dgst -sha256 "$1" | sed 's/^.*= *//'
  else
    die "no sha256 tool found (need sha256sum, shasum or openssl)"
  fi | tr 'A-F' 'a-f'
}

native_path() { # POSIX path -> a path native tools accept (Windows form under MSYS)
  if [ "$PLATFORM" = "windows-x64" ] && have cygpath; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

# ------------------------------------------------------------------- version

MIRROR="${SOTHIS_BOOTSTRAP_MIRROR:-}"

if [ -z "$VERSION" ]; then
  [ -z "$MIRROR" ] || usage_error "SOTHIS_BOOTSTRAP_MIRROR needs an explicit --version (there is no 'latest' on a mirror)"
  note "resolving the latest release of $REPO"
  tag="$(http_text "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$tag" ] || die "could not read tag_name from the latest release of $REPO (has a release been published? pass --version to pick one)"
  VERSION="${tag#v}"
else
  VERSION="${VERSION#v}"
fi
semver_ok "$VERSION" || usage_error "not a semantic version: $VERSION"

ASSET="sothis-$VERSION-$PLATFORM.$ARCHIVE_EXT"
TOP_DIR="sothis-$VERSION-$PLATFORM"
TAG="v$VERSION"
IDENTITY="https://github.com/$REPO/.github/workflows/release.yml@refs/tags/$TAG"

# -------------------------------------------------------------------- cosign

find_cosign() {
  if [ -n "${SOTHIS_COSIGN:-}" ]; then
    printf '%s' "$SOTHIS_COSIGN"
    return
  fi
  if have cosign; then
    command -v cosign
    return
  fi
  if [ "$PLATFORM" = "windows-x64" ]; then
    if have cosign-windows-amd64; then
      command -v cosign-windows-amd64
      return
    fi
    if [ -n "${LOCALAPPDATA:-}" ] && have cygpath; then
      packages="$(cygpath -u "$LOCALAPPDATA")/Microsoft/WinGet/Packages"
      for dir in "$packages"/Sigstore.Cosign_*; do
        [ -d "$dir" ] || continue
        for exe in "$dir"/cosign*.exe; do
          [ -f "$exe" ] || continue
          printf '%s' "$exe"
          return
        done
      done
    fi
  fi
  return 1
}

# An explicitly configured cosign that is unusable is an error, never a silent
# fall-through to "not installed".
if [ -n "${SOTHIS_COSIGN:-}" ] && [ ! -f "$SOTHIS_COSIGN" ]; then
  die "SOTHIS_COSIGN points at $SOTHIS_COSIGN, which is not a file"
fi
COSIGN=""
if COSIGN="$(find_cosign)"; then
  :
else
  COSIGN=""
fi
if [ -z "$COSIGN" ]; then
  if [ "$ALLOW_MISSING_COSIGN" -eq 1 ]; then
    warn "cosign was not found. The Sigstore signature of $ASSET will NOT be verified (--allow-missing-cosign); only its sha256 is checked, which cannot detect a tampered release."
  else
    die "cosign was not found, so the Sigstore signature cannot be verified. Install it (winget install Sigstore.Cosign, brew install cosign, or a release from https://github.com/sigstore/cosign/releases) or set SOTHIS_COSIGN to its absolute path; pass --allow-missing-cosign only to accept checksum-only verification."
  fi
fi

# ------------------------------------------------------------------- staging

WORK="$(mktemp -d "${TMPDIR:-/tmp}/sothis-bootstrap.XXXXXX")" || die "could not create a temporary directory"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

fetch() { # asset name -> $WORK/download/<name>
  name="$1"
  out="$WORK/download/$name"
  if [ -n "$MIRROR" ]; then
    src="$MIRROR/Davidb3l/Sothis/$TAG/$name"
    [ -f "$src" ] || die "$name is missing from the mirror ($src)"
    cp "$src" "$out"
  else
    http_get "https://github.com/$REPO/releases/download/$TAG/$name" "$out" \
      || die "could not download $name from the $TAG release of $REPO"
  fi
}

mkdir -p "$WORK/download" "$WORK/extract"
note "installing sothis $VERSION ($PLATFORM)"
fetch "$ASSET"
fetch "$ASSET.sha256"
if [ -n "$COSIGN" ]; then
  fetch "$ASSET.sigstore.json"
fi

# 1. sha256: one line, "<64 hex> *<asset>" (or two spaces); the name must match.
SUMS="$WORK/download/$ASSET.sha256"
LINES="$(grep -c . "$SUMS" || true)"
[ "$LINES" = "1" ] || die "$ASSET.sha256 must contain exactly one line (found $LINES)"
EXPECTED="$(sed -n 's/^\([0-9A-Fa-f]\{64\}\)[[:space:]].*$/\1/p' "$SUMS" | head -n 1 | tr 'A-F' 'a-f')"
[ -n "$EXPECTED" ] || die "$ASSET.sha256 does not start with a 64-digit hex sha256"
NAMED="$(sed -n 's/^[0-9A-Fa-f]\{64\}[[:space:]]\{1,\}\*\{0,1\}//p' "$SUMS" | head -n 1 | tr -d '\r')"
[ "$NAMED" = "$ASSET" ] || die "$ASSET.sha256 names '$NAMED', not '$ASSET'"
ACTUAL="$(sha256_of "$WORK/download/$ASSET")"
if [ "$ACTUAL" != "$EXPECTED" ]; then
  die "sha256 mismatch for $ASSET: published $EXPECTED, downloaded $ACTUAL; the download is corrupt or has been tampered with. Nothing was installed."
fi
note "sha256 ok ($ACTUAL)"

# 2. Sigstore. A cosign that runs and fails is always fatal.
if [ -n "$COSIGN" ]; then
  if "$COSIGN" verify-blob "$(native_path "$WORK/download/$ASSET")" \
    --bundle "$(native_path "$WORK/download/$ASSET.sigstore.json")" \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$OIDC_ISSUER" >/dev/null 2>"$WORK/cosign.err"; then
    note "sigstore signature verified ($IDENTITY)"
  else
    cat "$WORK/cosign.err" >&2
    die "cosign rejected the signature of $ASSET. Nothing was installed."
  fi
fi

# 3. Extract (only after verification) and sanity-check the layout.
case "$ARCHIVE_EXT" in
  tar.gz)
    if tar -tzf "$WORK/download/$ASSET" | grep -Eq '(^/|(^|/)\.\.(/|$)|\\)'; then
      die "$ASSET contains an unsafe path; refusing to extract"
    fi
    tar -xzf "$WORK/download/$ASSET" -C "$WORK/extract"
    ;;
  zip)
    if have unzip; then
      # unzip exits 1 for warnings (for example archives written with backslash
      # separators by Windows PowerShell 5.1); anything higher is a real error,
      # and the layout check below proves the expected file came out.
      if unzip -q "$WORK/download/$ASSET" -d "$WORK/extract"; then
        :
      else
        status=$?
        [ "$status" -eq 1 ] || die "could not extract $ASSET (unzip exit $status)"
      fi
    elif have powershell.exe; then
      powershell.exe -NoProfile -NonInteractive -Command \
        "Expand-Archive -LiteralPath '$(native_path "$WORK/download/$ASSET")' -DestinationPath '$(native_path "$WORK/extract")' -Force" \
        || die "could not extract $ASSET"
    else
      die "cannot extract $ASSET: neither unzip nor powershell.exe is available"
    fi
    ;;
esac
NEW="$WORK/extract/$TOP_DIR/$EXE"
[ -f "$NEW" ] && [ ! -L "$NEW" ] || die "$ASSET does not contain $TOP_DIR/$EXE"

# ----------------------------------------------------------------- placement

if [ -n "${SOTHIS_HOME:-}" ]; then
  case "$SOTHIS_HOME" in
    /* | [A-Za-z]:[\\/]*) ;;
    *) die "SOTHIS_HOME must be an absolute path (got $SOTHIS_HOME)" ;;
  esac
  BIN_DIR="${SOTHIS_HOME%/}/bin"
elif [ "$PLATFORM" = "windows-x64" ]; then
  # %USERPROFILE%\.sothis, mirroring .cargo and .bun; never under AppData.
  PROFILE_DIR="${USERPROFILE:-${HOME:-}}"
  [ -n "$PROFILE_DIR" ] || die "USERPROFILE is not set"
  if have cygpath; then
    BIN_DIR="$(cygpath -u "$PROFILE_DIR")/.sothis/bin"
  else
    BIN_DIR="${PROFILE_DIR%/}/.sothis/bin"
  fi
else
  [ -n "${HOME:-}" ] || die "HOME is not set"
  BIN_DIR="$HOME/.local/bin"
fi

mkdir -p "$BIN_DIR" || die "could not create $BIN_DIR"
DEST="$BIN_DIR/$EXE"
STAGED="$BIN_DIR/.sothis-new-$$"
cp "$NEW" "$STAGED" || die "could not copy sothis into $BIN_DIR"
chmod 755 "$STAGED" 2>/dev/null || true

ASIDE=""
if [ -e "$DEST" ] || [ -L "$DEST" ]; then
  # A running Windows executable can be renamed but not overwritten, so move
  # the old one aside first (also fine on Unix, and it keeps the swap atomic).
  ASIDE="$BIN_DIR/$EXE.old-$$"
  if ! mv -f "$DEST" "$ASIDE"; then
    rm -f "$STAGED"
    die "could not move the existing $DEST aside"
  fi
fi
if ! mv -f "$STAGED" "$DEST"; then
  [ -z "$ASIDE" ] || mv -f "$ASIDE" "$DEST" || true
  rm -f "$STAGED"
  die "could not place $DEST"
fi
[ -z "$ASIDE" ] || rm -f "$ASIDE" 2>/dev/null || true
# Leftovers of earlier swaps that could not be deleted while running.
for stale in "$BIN_DIR/$EXE".old-*; do
  [ -e "$stale" ] || continue
  rm -f "$stale" 2>/dev/null || true
done
note "placed $DEST"

# The placed binary must run and report the release version.
REPORTED="$("$DEST" --version 2>/dev/null | head -n 1 || true)"
if [ "$REPORTED" != "sothis $VERSION" ]; then
  warn "$DEST --version printed '$REPORTED' (expected 'sothis $VERSION')"
fi

# ------------------------------------------------------------------- hand-off

if [ "$NO_INSTALL" -eq 1 ]; then
  note "sothis $VERSION is in $BIN_DIR (--no-install). Next: sothis install --profile $PROFILE"
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) note "$BIN_DIR is not on PATH; \`sothis install\` adds it, or run $DEST directly" ;;
  esac
  exit 0
fi

set -- install --profile "$PROFILE"
[ "$ALLOW_MISSING_COSIGN" -eq 0 ] || set -- "$@" --allow-missing-cosign
[ "$NO_MODIFY_PATH" -eq 0 ] || set -- "$@" --no-modify-path
[ "$NO_PLUGINS" -eq 0 ] || set -- "$@" --no-plugins
[ "$ALLOW_VIRTUALIZED" -eq 0 ] || set -- "$@" --allow-virtualized-root
note "running: sothis $*"
# Not exec: the EXIT trap must still remove the temporary directory.
"$DEST" "$@"
