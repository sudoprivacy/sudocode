#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: build-deb.sh --version VERSION --binary PATH --output DIR

Build scode_VERSION_amd64.deb from an existing Linux x64 scode binary.
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
DPKG_DEB="${SCODE_DPKG_DEB:-dpkg-deb}"
command -v "$DPKG_DEB" >/dev/null 2>&1 || { printf 'dpkg-deb is required\n' >&2; exit 2; }

case "$VERSION" in
  v*) VERSION="${VERSION#v}" ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM HUP

pkg="$TMP/scode_${VERSION}_amd64"
mkdir -p \
  "$pkg/DEBIAN" \
  "$pkg/usr/bin" \
  "$pkg/usr/lib/scode" \
  "$pkg/usr/share/doc/scode"

sed "s/@VERSION@/$VERSION/g" "$SCRIPT_DIR/control.template" > "$pkg/DEBIAN/control"
install -m 0755 "$SCRIPT_DIR/postinst" "$pkg/DEBIAN/postinst"
install -m 0755 "$SCRIPT_DIR/prerm" "$pkg/DEBIAN/prerm"
install -m 0755 "$BINARY" "$pkg/usr/bin/scode"
install -m 0755 "$SCRIPT_DIR/scode-setup" "$pkg/usr/lib/scode/scode-setup"
cat > "$pkg/usr/bin/scode-setup" <<'EOF'
#!/usr/bin/env sh
exec /usr/lib/scode/scode-setup "$@"
EOF
chmod 0755 "$pkg/usr/bin/scode-setup"

if [ -f "$ROOT/README.md" ]; then
  install -m 0644 "$ROOT/README.md" "$pkg/usr/share/doc/scode/README.md"
else
  printf 'scode\n' > "$pkg/usr/share/doc/scode/README.md"
  chmod 0644 "$pkg/usr/share/doc/scode/README.md"
fi

mkdir -p "$OUTPUT"
"$DPKG_DEB" --build --root-owner-group "$pkg" "$OUTPUT/scode_${VERSION}_amd64.deb" >/dev/null
printf '%s\n' "$OUTPUT/scode_${VERSION}_amd64.deb"
