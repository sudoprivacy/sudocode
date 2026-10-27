#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: build-deb.sh --version VERSION --binary PATH --output DIR [--arch amd64|arm64]

Build a Debian package from an existing Linux scode binary (default: amd64).
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
  amd64|x64|x86_64) ARCH=amd64; LIBC_DEPENDENCY="" ;;
  arm64|aarch64) ARCH=arm64; LIBC_DEPENDENCY=", libc6 (>= 2.35), libgcc-s1" ;;
  *) printf 'unsupported architecture: %s\n' "$ARCH" >&2; exit 2 ;;
esac

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

pkg="$TMP/scode_${VERSION}_$ARCH"
mkdir -p \
  "$pkg/DEBIAN" \
  "$pkg/usr/bin" \
  "$pkg/usr/lib/scode" \
  "$pkg/usr/share/doc/scode"

sed -e "s/@VERSION@/$VERSION/g" -e "s/@ARCH@/$ARCH/g" -e "s/@LIBC_DEPENDENCY@/$LIBC_DEPENDENCY/g" \
  "$SCRIPT_DIR/control.template" > "$pkg/DEBIAN/control"
install -m 0755 "$SCRIPT_DIR/postinst" "$pkg/DEBIAN/postinst"
install -m 0755 "$SCRIPT_DIR/prerm" "$pkg/DEBIAN/prerm"
install -m 0755 "$BINARY" "$pkg/usr/bin/scode"
install -m 0755 "$SCRIPT_DIR/scode-setup" "$pkg/usr/lib/scode/scode-setup"
install -m 0644 "$ROOT/THIRD_PARTY_NOTICES.txt" "$pkg/usr/share/doc/scode/THIRD_PARTY_NOTICES.txt"
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
"$DPKG_DEB" --build --root-owner-group "$pkg" "$OUTPUT/scode_${VERSION}_$ARCH.deb" >/dev/null
printf '%s\n' "$OUTPUT/scode_${VERSION}_$ARCH.deb"
