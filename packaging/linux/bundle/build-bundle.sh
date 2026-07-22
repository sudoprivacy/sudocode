#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: build-bundle.sh --version VERSION --binary PATH --output DIR

Build scode-linux-x64-bundle.tar.gz from an existing Linux x64 scode binary.
EOF
}

VERSION=""
BINARY=""
OUTPUT=""

while [ $# -gt 0 ]; do
  case "$1" in
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

bundle_dir="$TMP/scode-linux-x64-bundle"
mkdir -p "$bundle_dir" "$OUTPUT"

install -m 0755 "$BINARY" "$bundle_dir/scode"
install -m 0755 "$ROOT/packaging/linux/deb/scode-setup" "$bundle_dir/scode-setup"

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
    sha256sum scode scode-setup README-install.zh-CN.md VERSION > SHA256SUMS.txt
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 scode scode-setup README-install.zh-CN.md VERSION > SHA256SUMS.txt
  else
    printf 'sha256sum or shasum is required\n' >&2
    exit 2
  fi
)

tar -C "$TMP" -czf "$OUTPUT/scode-linux-x64-bundle.tar.gz" "scode-linux-x64-bundle"
printf '%s\n' "$OUTPUT/scode-linux-x64-bundle.tar.gz"
