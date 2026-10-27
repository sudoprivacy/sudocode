#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fake_bin="$TMP/scode"
cat > "$fake_bin" <<'BIN'
#!/usr/bin/env sh
if [ "${1:-}" = "--version" ]; then
  printf 'scode test version\n'
else
  printf 'scode test\n'
fi
BIN
chmod +x "$fake_bin"

for arch in amd64 arm64; do
  case "$arch" in amd64) bundle_arch=x64 ;; arm64) bundle_arch=arm64 ;; esac
  "$ROOT/packaging/linux/bundle/build-bundle.sh" \
    --version 0.1.12 --arch "$arch" \
    --binary "$fake_bin" \
    --output "$TMP/dist"

  bundle="$TMP/dist/scode-linux-${bundle_arch}-bundle.tar.gz"
  test -f "$bundle"
  # Read the complete archive before matching: grep -q can close a producer's
  # pipe early under pipefail, hiding a valid artifact behind a SIGPIPE result.
  tar -tzf "$bundle" > "$TMP/archive-members.txt"
  grep -qx "scode-linux-${bundle_arch}-bundle/scode" "$TMP/archive-members.txt"
  grep -qx "scode-linux-${bundle_arch}-bundle/scode-setup" "$TMP/archive-members.txt"
  grep -qx "scode-linux-${bundle_arch}-bundle/README-install.zh-CN.md" "$TMP/archive-members.txt"
  grep -qx "scode-linux-${bundle_arch}-bundle/SHA256SUMS.txt" "$TMP/archive-members.txt"
  grep -qx "scode-linux-${bundle_arch}-bundle/THIRD_PARTY_NOTICES.txt" "$TMP/archive-members.txt"

  tar -xzf "$bundle" -C "$TMP"
  bundle_dir="$TMP/scode-linux-${bundle_arch}-bundle"
  test -x "$bundle_dir/scode"
  test -x "$bundle_dir/scode-setup"

  SCODE_INSTALL_ROOT="$TMP/install-root" \
  SCODE_SKIP_CONFIG=1 \
  "$bundle_dir/scode-setup" install --bin-dir /usr/local/bin

  test -x "$TMP/install-root/usr/local/bin/scode"
  test -x "$TMP/install-root/usr/local/bin/scode-setup"
  test -f "$TMP/install-root/usr/local/lib/scode/install-manifest"
  grep -q 'bin_dir=/usr/local/bin' "$TMP/install-root/usr/local/lib/scode/install-manifest"
  grep -q '/usr/local/bin/scode' "$TMP/install-root/usr/local/lib/scode/install-manifest"
  grep -q '/usr/local/bin/scode-setup' "$TMP/install-root/usr/local/lib/scode/install-manifest"

  SCODE_INSTALL_ROOT="$TMP/install-root" \
  "$TMP/install-root/usr/local/bin/scode-setup" doctor

  SCODE_INSTALL_ROOT="$TMP/install-root" \
  "$TMP/install-root/usr/local/bin/scode-setup" uninstall --yes

  test ! -e "$TMP/install-root/usr/local/bin/scode"
  test ! -e "$TMP/install-root/usr/local/bin/scode-setup"

done
