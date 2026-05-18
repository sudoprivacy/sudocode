#!/bin/sh
# install.sh — one-line installer for the `scode` CLI.
#
#   curl -fsSL https://raw.githubusercontent.com/sudoprivacy/sudocode/main/install.sh | sh
#
# Environment overrides:
#   SCODE_VERSION       Release tag to install (e.g. v0.1.5). Default: latest.
#   SCODE_INSTALL_DIR   Directory to install into. Default: $HOME/.local/bin.
#   NO_COLOR            Disable ANSI color output when set.
#
# Flags:
#   --version vX.Y.Z    Pin a specific release.
#   --prefix DIR        Install dir. If DIR/bin exists, install to DIR/bin.
#   --help              Show help.

set -eu

REPO="sudoprivacy/sudocode"
BIN_NAME="scode"
# The release workflow publishes the checksum file as SHA256SUMS.txt.
CHECKSUM_FILE="SHA256SUMS.txt"
API_BASE="https://api.github.com/repos/${REPO}/releases"
DOWNLOAD_BASE="https://github.com/${REPO}/releases/download"
RELEASES_PAGE="https://github.com/${REPO}/releases"

# ----- output helpers --------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RESET=$(printf '\033[0m')
    C_BOLD=$(printf '\033[1m')
    C_DIM=$(printf '\033[2m')
    C_RED=$(printf '\033[31m')
    C_GREEN=$(printf '\033[32m')
    C_YELLOW=$(printf '\033[33m')
    C_BLUE=$(printf '\033[34m')
else
    C_RESET=""; C_BOLD=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_BLUE=""
fi

info() { printf '%s==>%s %s\n' "$C_BLUE" "$C_RESET" "$*"; }
ok()   { printf '%sok:%s  %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%swarn:%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
err()  { printf '%serror:%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; }
dim()  { printf '%s%s%s\n' "$C_DIM" "$*" "$C_RESET"; }
die()  { err "$*"; exit 1; }

usage() {
    cat <<EOF
${C_BOLD}scode installer${C_RESET}

Install the scode CLI (sudoprivacy/sudocode) from GitHub Releases.

${C_BOLD}USAGE${C_RESET}
    install.sh [--version vX.Y.Z] [--prefix DIR] [--help]

${C_BOLD}ENVIRONMENT${C_RESET}
    SCODE_VERSION       Pin a release tag (default: latest).
    SCODE_INSTALL_DIR   Install directory (default: \$HOME/.local/bin).
    NO_COLOR            Disable colored output.

${C_BOLD}EXAMPLES${C_RESET}
    curl -fsSL https://raw.githubusercontent.com/sudoprivacy/sudocode/main/install.sh | sh
    SCODE_VERSION=v0.1.5 sh install.sh
    sh install.sh --prefix /usr/local
EOF
}

# ----- argument parsing ------------------------------------------------------
VERSION="${SCODE_VERSION:-}"
PREFIX=""
INSTALL_DIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || die "--version requires an argument (e.g. v0.1.6)"
            VERSION="$2"; shift 2 ;;
        --version=*)
            VERSION="${1#--version=}"; shift ;;
        --prefix)
            [ $# -ge 2 ] || die "--prefix requires a directory argument"
            PREFIX="$2"; shift 2 ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"; shift ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            die "unknown argument: $1 (try --help)" ;;
    esac
done

# Resolve install dir. --prefix wins, then SCODE_INSTALL_DIR, then default.
if [ -n "$PREFIX" ]; then
    # If PREFIX/bin exists as a dir, treat PREFIX as a Unix-style prefix
    # (so --prefix=/usr/local installs to /usr/local/bin).
    prefix_trim="${PREFIX%/}"
    if [ -d "${prefix_trim}/bin" ]; then
        INSTALL_DIR="${prefix_trim}/bin"
    else
        INSTALL_DIR="$prefix_trim"
    fi
else
    INSTALL_DIR="${SCODE_INSTALL_DIR:-$HOME/.local/bin}"
fi

# ----- tool detection --------------------------------------------------------
have() { command -v "$1" >/dev/null 2>&1; }

HAS_CURL=""
if have curl; then
    HAS_CURL=1
elif have wget; then
    HAS_CURL=""
else
    die "need curl or wget on PATH"
fi

SHA_CMD=""
if have shasum; then
    SHA_CMD="shasum -a 256"
elif have sha256sum; then
    SHA_CMD="sha256sum"
else
    die "need shasum or sha256sum for checksum verification"
fi

have tar || die "need tar to extract the release archive"
have mktemp || die "need mktemp to stage downloads"

# ----- platform detection ----------------------------------------------------
detect_target() {
    uname_s=$(uname -s)
    uname_m=$(uname -m)
    case "$uname_s" in
        Darwin)
            case "$uname_m" in
                arm64|aarch64) printf '%s' "macos-arm64" ;;
                x86_64)        printf '%s' "macos-x64" ;;
                *) die "unsupported macOS architecture: $uname_m" ;;
            esac
            ;;
        Linux)
            case "$uname_m" in
                x86_64|amd64)  printf '%s' "linux-x64" ;;
                aarch64|arm64) printf '%s' "linux-arm64" ;;
                *) die "unsupported Linux architecture: $uname_m" ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*|Windows_NT)
            err "Windows is not supported by this installer."
            err "Download the prebuilt zip manually from:"
            err "  ${RELEASES_PAGE}"
            err "and extract scode.exe somewhere on your PATH."
            exit 1
            ;;
        *)
            die "unsupported OS: $uname_s"
            ;;
    esac
}

# ----- http helpers ----------------------------------------------------------
# $1=url. Streams body to stdout. Fails non-zero on HTTP error.
http_get() {
    if [ -n "$HAS_CURL" ]; then
        curl -fsSL --proto '=https' --tlsv1.2 "$1"
    else
        wget -q -O - "$1"
    fi
}

# $1=url $2=destination file
http_download() {
    _url="$1"; _out="$2"
    if [ -n "$HAS_CURL" ]; then
        dim "  curl -fL -o \"$_out\" \"$_url\""
        curl -fL --proto '=https' --tlsv1.2 --progress-bar -o "$_out" "$_url" || {
            err "download failed: $_url"
            err "see all releases: ${RELEASES_PAGE}"
            exit 1
        }
    else
        dim "  wget -O \"$_out\" \"$_url\""
        wget -O "$_out" "$_url" || {
            err "download failed: $_url"
            err "see all releases: ${RELEASES_PAGE}"
            exit 1
        }
    fi
}

# ----- resolve version -------------------------------------------------------
resolve_latest_version() {
    _json=$(http_get "${API_BASE}/latest") || die "failed to query ${API_BASE}/latest"
    # Pull the first "tag_name": "vX.Y.Z" — robust to JSON whitespace.
    _v=$(printf '%s\n' "$_json" \
        | grep -E '"tag_name"[[:space:]]*:' \
        | head -n 1 \
        | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
    [ -n "$_v" ] || die "could not parse tag_name from GitHub API response"
    printf '%s' "$_v"
}

if [ -z "$VERSION" ]; then
    info "resolving latest release from ${API_BASE}/latest"
    VERSION=$(resolve_latest_version)
fi
ok "version: ${C_BOLD}${VERSION}${C_RESET}"

# Sanity check the tag exists (gives a friendly error before we start downloading).
if ! http_get "${API_BASE}/tags/${VERSION}" >/dev/null 2>&1; then
    err "release ${VERSION} not found"
    err "see all releases: ${RELEASES_PAGE}"
    exit 1
fi

# ----- artifact selection ----------------------------------------------------
TARGET=$(detect_target)
ARCHIVE="scode-${TARGET}.tar.gz"
ARCHIVE_URL="${DOWNLOAD_BASE}/${VERSION}/${ARCHIVE}"
CHECKSUM_URL="${DOWNLOAD_BASE}/${VERSION}/${CHECKSUM_FILE}"

info "target: ${C_BOLD}${TARGET}${C_RESET}"
info "install dir: ${INSTALL_DIR}"

# ----- temp dir with cleanup -------------------------------------------------
TMP_BASE="${TMPDIR:-/tmp}"
TMP=$(mktemp -d "${TMP_BASE%/}/scode-install.XXXXXX")
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM HUP

# ----- download --------------------------------------------------------------
info "downloading archive"
http_download "$ARCHIVE_URL" "$TMP/$ARCHIVE"
info "downloading checksums"
http_download "$CHECKSUM_URL" "$TMP/$CHECKSUM_FILE"

# ----- checksum verify -------------------------------------------------------
info "verifying SHA-256 checksum"
expected=$(grep -E "[[:space:]]${ARCHIVE}\$" "$TMP/$CHECKSUM_FILE" | awk '{print $1}' | head -n 1)
[ -n "$expected" ] || die "no checksum entry for $ARCHIVE in $CHECKSUM_FILE"

# $SHA_CMD intentionally unquoted so word-splitting picks up "shasum -a 256".
actual=$($SHA_CMD "$TMP/$ARCHIVE" | awk '{print $1}')
if [ "$expected" != "$actual" ]; then
    err "checksum mismatch for $ARCHIVE"
    err "  expected: $expected"
    err "  actual:   $actual"
    exit 1
fi
ok "checksum verified (${expected})"

# ----- extract ---------------------------------------------------------------
info "extracting"
tar -xzf "$TMP/$ARCHIVE" -C "$TMP"

EXTRACTED_BIN="$TMP/scode-${TARGET}/${BIN_NAME}"
[ -f "$EXTRACTED_BIN" ] || die "expected binary not found in archive: scode-${TARGET}/${BIN_NAME}"

# ----- install ---------------------------------------------------------------
DEST="${INSTALL_DIR%/}/${BIN_NAME}"

if ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
    die "could not create install dir: $INSTALL_DIR"
fi
if [ ! -w "$INSTALL_DIR" ]; then
    err "install dir is not writable: $INSTALL_DIR"
    err "pick a writable location (e.g. --prefix \$HOME/.local) or chown the directory."
    err "this installer will not run sudo for you."
    exit 1
fi

if [ -e "$DEST" ]; then
    backup="${DEST}.bak"
    info "backing up existing binary: $DEST -> $backup"
    mv -f "$DEST" "$backup"
fi

mv -f "$EXTRACTED_BIN" "$DEST"
chmod +x "$DEST"

# ----- macOS quarantine ------------------------------------------------------
if [ "$(uname -s)" = "Darwin" ] && have xattr; then
    xattr -d com.apple.quarantine "$DEST" >/dev/null 2>&1 || true
fi

ok "installed: ${C_BOLD}${DEST}${C_RESET}"

# ----- PATH check ------------------------------------------------------------
in_path=0
old_ifs=$IFS
IFS=:
for p in $PATH; do
    if [ "$p" = "$INSTALL_DIR" ]; then
        in_path=1
        break
    fi
done
IFS=$old_ifs

if [ "$in_path" -ne 1 ]; then
    case "${SHELL:-}" in
        *zsh)  rc="$HOME/.zshrc" ;;
        *bash) rc="$HOME/.bashrc" ;;
        *fish) rc="$HOME/.config/fish/config.fish" ;;
        *)     rc="$HOME/.profile" ;;
    esac
    printf '\n'
    warn "${INSTALL_DIR} is not in your PATH."
    case "${SHELL:-}" in
        *fish)
            printf '  Add this to %s:\n\n    set -gx PATH "%s" $PATH\n\n' "$rc" "$INSTALL_DIR" ;;
        *)
            printf '  Add this to %s:\n\n    export PATH="%s:$PATH"\n\n' "$rc" "$INSTALL_DIR" ;;
    esac
    printf '  Then reload your shell (or run: %s. "%s"%s).\n\n' "$C_DIM" "$rc" "$C_RESET"
fi

# ----- final version check ---------------------------------------------------
info "verifying install"
if v_out=$("$DEST" --version 2>&1); then
    ok "scode is working: ${C_BOLD}${v_out}${C_RESET}"
else
    warn "ran '$DEST --version' but it exited non-zero:"
    printf '%s\n' "$v_out" >&2
    exit 1
fi
