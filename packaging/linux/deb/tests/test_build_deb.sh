#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fake_bin="$TMP/scode"
cat >"$fake_bin" <<'BIN'
#!/usr/bin/env sh
printf 'scode test\n'
BIN
chmod +x "$fake_bin"

fake_dpkg="$TMP/fake-dpkg-deb"
cat >"$fake_dpkg" <<'BIN'
#!/usr/bin/env bash
set -euo pipefail
pkg_root="$3"
out="$4"
mkdir -p "$(dirname "$out")" "$SCODE_TEST_CAPTURE_ROOT"
cp -a "$pkg_root"/. "$SCODE_TEST_CAPTURE_ROOT"/
printf 'fake deb\n' > "$out"
BIN
chmod +x "$fake_dpkg"

SCODE_DPKG_DEB="$fake_dpkg" \
SCODE_TEST_CAPTURE_ROOT="$TMP/package-root" \
"$ROOT/packaging/linux/deb/build-deb.sh" \
  --version 0.1.12 \
  --binary "$fake_bin" \
  --output "$TMP/dist"

deb="$TMP/dist/scode_0.1.12_amd64.deb"
test -f "$deb"
test -x "$TMP/package-root/usr/bin/scode"
test -x "$TMP/package-root/usr/bin/scode-setup"
test -x "$TMP/package-root/usr/lib/scode/scode-setup"
grep -q 'Package: scode' "$TMP/package-root/DEBIAN/control"
grep -q 'Version: 0.1.12' "$TMP/package-root/DEBIAN/control"

if command -v dpkg-deb >/dev/null 2>&1; then
  "$ROOT/packaging/linux/deb/build-deb.sh" \
    --version 0.1.12 \
    --binary "$fake_bin" \
    --output "$TMP/real-dist"
  real_deb="$TMP/real-dist/scode_0.1.12_amd64.deb"
  dpkg-deb --contents "$real_deb" | grep -q './usr/bin/scode$'
  dpkg-deb --contents "$real_deb" | grep -q './usr/bin/scode-setup$'
  dpkg-deb --contents "$real_deb" | grep -q './usr/lib/scode/scode-setup$'
  dpkg-deb --info "$real_deb" | grep -q 'Package: scode'
  dpkg-deb --info "$real_deb" | grep -q 'Version: 0.1.12'
fi

setup_home="$TMP/home"
mkdir -p "$setup_home"
HOME="$setup_home" \
SCODE_BASE_URL="https://example.invalid/v1" \
SCODE_API_KEY="test-key" \
SCODE_MODEL="test-model" \
SCODE_MODELS="test-model,vision-model" \
SCODE_ENABLE_SEARCH=0 \
"$ROOT/packaging/linux/deb/scode-setup" --non-interactive

test -f "$setup_home/.nexus/sudocode/sudocode.json"
test -f "$setup_home/.nexus/sudocode/settings.json"
grep -q '"test-model"' "$setup_home/.nexus/sudocode/settings.json"
grep -q '"baseUrl": "https://example.invalid/v1"' "$setup_home/.nexus/sudocode/sudocode.json"
grep -q '"apiKey": "test-key"' "$setup_home/.nexus/sudocode/sudocode.json"
grep -q '"vision-model"' "$setup_home/.nexus/sudocode/sudocode.json"
! grep -q '"web_search"' "$setup_home/.nexus/sudocode/sudocode.json"

postinst_output="$(
  DEBIAN_FRONTEND=noninteractive \
  "$ROOT/packaging/linux/deb/postinst" configure
)"
printf '%s\n' "$postinst_output" | grep -q 'No interactive terminal detected, skipped setup.'
