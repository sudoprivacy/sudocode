#!/usr/bin/env bash
# Live acceptance for the builtin Vault plugin (sudoprivacy/sudocode#1458).
#
# Runs rust/crates/rusty-sudocode-cli/tests/pty_vault_live.rs against a real
# `nexusd-cluster serve-user` that loads the real, signed Vault plugin, with the
# real `scode` binary in a PTY. The only stand-in is the model
# (mock-anthropic-service).
#
# Two container passes, because only the first may touch the network:
#
#   fetch   toolchain, crates and the two release archives; each archive must
#           match the sha256 this checkout pins, or nothing runs
#   test    `--network none`: build and run the test offline. The daemons, the
#           mock model and the canary HTTP target all live on the container's
#           own loopback, so no host service is reachable from the run.
#
# The daemon is the nexus-vfs release rust/Cargo.lock pins (the resolution
# e2e/nexus-a2a/fetch-daemon.sh uses), downloaded here rather than through that
# script because it neither checks a sha256 nor keeps the archive to check.
# Checksums come from rust/crates/service-host/runtime-artifacts.json. To run
# against another published release, set VAULT_LIVE_NEXUS_TAG together with
# VAULT_LIVE_NEXUS_SHA256 (that release's linux-x86_64 archive checksum).
# run-native.sh runs the same test on a host without a container: macOS arm64,
# whose Vault plugin has no Linux build.
#
# Usage (Linux, macOS, or Git Bash on Windows; needs docker):
#
#   e2e/vault-live/run.sh                       # fetch, then test
#   VAULT_LIVE_TEST_FILTER=h_ e2e/vault-live/run.sh
#
# Evidence (cargo log, one report per case, versions) lands in
# $VAULT_LIVE_OUT/<tag> (default ${TMPDIR:-/tmp}/vault-live).
#
# Knobs: VAULT_LIVE_IMAGE (rust:1-bookworm), VAULT_LIVE_CPUS (4),
# VAULT_LIVE_JOBS (2), VAULT_LIVE_REGISTRY_VOLUME, VAULT_LIVE_GIT_VOLUME,
# VAULT_LIVE_TARGET_VOLUME (docker volumes for the cargo registry, cargo git
# checkouts and the target/toolchain cache), VAULT_LIVE_BUILD_ONLY=1 (compile
# the test without running it).
set -euo pipefail

CACHE=/target/vault-live
# shellcheck source=lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# ── inside the container ──────────────────────────────────────────────────

# A writable copy of the read-only checkout. `cp -a` keeps mtimes, so cargo
# reuses every unchanged crate from the target volume; CR is stripped from the
# shell scripts a Windows checkout carries with CRLF.
prepare_tree() {
    rm -rf /work
    mkdir -p /work
    cp -a /src/. /work/
    rm -rf /work/.git
    find /work/e2e -name '*.sh' -exec sed -i 's/\r$//' {} +
    cd /work
}

fetch_pass() {
    prepare_tree
    local channel tag
    channel="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml | tr -d '\r')"
    rustup toolchain install "$channel" --profile minimal --component clippy,rustfmt
    tag="$(fetch_artifacts linux-x86_64 linux-x86_64 libnexus_vault.so)"
    echo "$tag" >"$CACHE/selected-tag"
    (cd rust && cargo fetch --locked)
    log "fetch pass complete for $tag"
}

test_pass() {
    prepare_tree
    local tag
    tag="$(cat "$CACHE/selected-tag")"
    [ "$tag" = "$(nexus_tag)" ] || die "fetched $tag, but this run selects $(nexus_tag)"
    local live="$CACHE/$tag" out="/out/$tag"
    mkdir -p "$out"
    {
        echo "SOURCE=${VAULT_LIVE_SOURCE:-unknown}"
        echo "IMAGE=${VAULT_LIVE_IMAGE:-unknown}"
        artifact_versions "$live"
        echo "KERNEL=$(uname -sr)"
    } | tee "$out/versions.env"
    run_live_test "$live" "$out"
}

# ── on the host ───────────────────────────────────────────────────────────

host_path() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s\n' "$1"; fi
}

host_run() {
    command -v docker >/dev/null || die "docker is required"
    local repo out image image_id source
    repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    out="${VAULT_LIVE_OUT:-${TMPDIR:-/tmp}/vault-live}"
    mkdir -p "$out"
    image="${VAULT_LIVE_IMAGE:-rust:1-bookworm}"
    docker image inspect "$image" >/dev/null 2>&1 || docker pull "$image"
    image_id="$(docker image inspect --format '{{.Id}}' "$image")"
    source="$(git -C "$repo" rev-parse HEAD 2>/dev/null || echo unknown)"
    [ -z "$(git -C "$repo" status --porcelain 2>/dev/null)" ] || source="$source+dirty"
    # Git Bash would otherwise rewrite the container paths below.
    export MSYS_NO_PATHCONV=1
    local args=(
        --rm --cpus "${VAULT_LIVE_CPUS:-4}"
        -v "$(host_path "$repo"):/src:ro"
        -v "$(host_path "$out"):/out"
        -v "${VAULT_LIVE_REGISTRY_VOLUME:-vault-live-cargo-registry}:/usr/local/cargo/registry"
        -v "${VAULT_LIVE_GIT_VOLUME:-vault-live-cargo-git}:/usr/local/cargo/git"
        -v "${VAULT_LIVE_TARGET_VOLUME:-vault-live-target}:/target"
        -e LANG=C.UTF-8 -e LC_ALL=C.UTF-8 -e TERM=xterm-256color
        -e CARGO_BUILD_JOBS="${VAULT_LIVE_JOBS:-2}"
        -e CARGO_TARGET_DIR=/target/cargo -e RUSTUP_HOME=/target/rustup
        -e CARGO_NET_GIT_FETCH_WITH_CLI=true
        -e VAULT_LIVE_NEXUS_TAG -e VAULT_LIVE_NEXUS_SHA256
        -e VAULT_LIVE_TEST_FILTER -e VAULT_LIVE_BUILD_ONLY
        -e VAULT_LIVE_SOURCE="$source" -e VAULT_LIVE_IMAGE="$image@$image_id"
    )
    local entry=(bash -c 'for f in run.sh lib.sh; do tr -d "\r" </src/e2e/vault-live/$f >/tmp/$f; done && exec bash /tmp/run.sh "$1"' run)
    log "fetch pass (network) with $image"
    docker run "${args[@]}" "$image" "${entry[@]}" fetch
    log "test pass (--network none)"
    docker run --network none "${args[@]}" "$image" "${entry[@]}" test
    log "evidence in $out"
}

case "${1:-host}" in
    host) host_run ;;
    fetch) fetch_pass ;;
    test) test_pass ;;
    *) die "usage: $0 [host|fetch|test]" ;;
esac
