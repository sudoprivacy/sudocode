#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: build-bundle.sh --version VERSION --binary PATH --output DIR [--arch amd64|arm64]

Build a Linux bundle from an existing scode binary (default architecture: amd64).
EOF
}

VERSION=""
BINARY=""
OUTPUT=""
ARCH=amd64

while [ $# -gt 0 ]; do
  case "$1" in
    --arch)
      [ $# -ge 2 ] || { usage >&2; exit 2; }
      ARCH="$2"
      shift 2
      ;;
    --arch=*)
      ARCH="${1#--arch=}"
      shift
      ;;
    --version)
      [ $# -ge 2 ] || { usage >&2; exit 2; }
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#--version=}"
      shift
      ;;
    --binary)
      [ $# -ge 2 ] || { usage >&2; exit 2; }
      BINARY="$2"
      shift 2
      ;;
    --binary=*)
      BINARY="${1#--binary=}"
      shift
      ;;
    --output)
      [ $# -ge 2 ] || { usage >&2; exit 2; }
      OUTPUT="$2"
      shift 2
      ;;
    --output=*)
      OUTPUT="${1#--output=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

case "$ARCH" in
  amd64|x64|x86_64) BUNDLE_ARCH=x64 ;;
  arm64|aarch64) BUNDLE_ARCH=arm64 ;;
  *) printf 'unsupported architecture: %s\n' "$ARCH" >&2; exit 2 ;;
esac

[ -n "$VERSION" ] || { printf 'missing --version\n' >&2; exit 2; }
[ -n "$BINARY" ] || { printf 'missing --binary\n' >&2; exit 2; }
[ -n "$OUTPUT" ] || { printf 'missing --output\n' >&2; exit 2; }
[ -f "$BINARY" ] || { printf 'binary not found: %s\n' "$BINARY" >&2; exit 2; }
[ -x "$BINARY" ] || { printf 'binary is not executable: %s\n' "$BINARY" >&2; exit 2; }
command -v tar >/dev/null 2>&1 || { printf 'tar is required\n' >&2; exit 2; }

case "$VERSION" in
  v*) VERSION="${VERSION#v}" ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM HUP

bundle_name="scode-linux-$BUNDLE_ARCH-bundle"
bundle_dir="$TMP/$bundle_name"
mkdir -p "$bundle_dir" "$OUTPUT"

install -m 0755 "$BINARY" "$bundle_dir/scode"
install -m 0755 "$ROOT/packaging/linux/deb/scode-setup" "$bundle_dir/scode-setup"
install -m 0644 "$ROOT/THIRD_PARTY_NOTICES.txt" "$bundle_dir/THIRD_PARTY_NOTICES.txt"

if [ -f "$ROOT/docs/linux-bundle-install.zh-CN.md" ]; then
  install -m 0644 "$ROOT/docs/linux-bundle-install.zh-CN.md" "$bundle_dir/README-install.zh-CN.md"
else
  cat > "$bundle_dir/README-install.zh-CN.md" <<EOF
# scode Linux bundle

解压后运行：

\`\`\`bash
sudo ./scode-setup install
\`\`\`
EOF
fi

cat > "$bundle_dir/VERSION" <<EOF
$VERSION
EOF

(
  cd "$bundle_dir"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum scode scode-setup README-install.zh-CN.md VERSION THIRD_PARTY_NOTICES.txt > SHA256SUMS.txt
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 scode scode-setup README-install.zh-CN.md VERSION THIRD_PARTY_NOTICES.txt > SHA256SUMS.txt
  else
    printf 'sha256sum or shasum is required\n' >&2
    exit 2
  fi
)

tar -C "$TMP" -czf "$OUTPUT/$bundle_name.tar.gz" "$bundle_name"
printf '%s\n' "$OUTPUT/$bundle_name.tar.gz"
