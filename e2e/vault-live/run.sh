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

ARCH=linux-x86_64
CACHE=/target/vault-live

log() { echo "[vault-live] $*" >&2; }
die() { echo "[vault-live] $*" >&2; exit 1; }

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

nexus_tag() {
    if [ -n "${VAULT_LIVE_NEXUS_TAG:-}" ]; then
        echo "$VAULT_LIVE_NEXUS_TAG"
        return
    fi
    local tags
    tags="$(sed -n 's/.*nexus-vfs?tag=\([^#"]*\)#[0-9a-f]*.*/\1/p' rust/Cargo.lock | sort -u)"
    [ "$(printf '%s\n' "$tags" | grep -c .)" = 1 ] \
        || die "rust/Cargo.lock must pin exactly one nexus-vfs tag, found: ${tags:-none}"
    echo "$tags"
}

# The value runtime-artifacts.json records for `key`, from a `"key": "value"` line.
artifact_field() {
    local key="${1//./\\.}"
    sed -n "s|^ *\"$key\": *\"\([^\"]*\)\".*|\1|p" rust/crates/service-host/runtime-artifacts.json
}

# Download `name` from the first URL that answers into the archive cache and
# require `sha`. A cached archive is re-hashed rather than trusted.
fetch_verified() {
    local name="$1" sha="$2" dest="$CACHE/archives/$1"
    shift 2
    [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "no pinned sha256 for $name"
    mkdir -p "$CACHE/archives"
    if [ -f "$dest" ] && [ "$(sha256sum "$dest" | cut -d' ' -f1)" = "$sha" ]; then
        log "cached $name ($sha)"
        return
    fi
    local url
    for url in "$@"; do
        log "downloading $url"
        if curl -fsSL --retry 3 --retry-delay 2 --max-time 300 -o "$dest.part" "$url"; then
            local actual
            actual="$(sha256sum "$dest.part" | cut -d' ' -f1)"
            [ "$actual" = "$sha" ] || die "sha256 mismatch for $name: expected $sha, got $actual"
            mv "$dest.part" "$dest"
            log "verified $name ($sha)"
            return
        fi
    done
    die "could not download $name"
}

# Extract exactly one file called `name` from `archive` into `dir`.
extract_one() {
    local archive="$1" name="$2" dir="$3" scratch
    scratch="$(mktemp -d)"
    tar -xzf "$archive" -C "$scratch"
    local found
    found="$(find "$scratch" -type f -name "$name")"
    [ "$(printf '%s\n' "$found" | grep -c .)" = 1 ] || die "$archive must hold one $name"
    install -m 0755 "$found" "$dir/$name"
    rm -rf "$scratch"
}

fetch_pass() {
    prepare_tree
    local channel
    channel="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml | tr -d '\r')"
    rustup toolchain install "$channel" --profile minimal --component clippy,rustfmt

    local tag vault_version nexus_sha vault_sha
    tag="$(nexus_tag)"
    nexus_sha="$(artifact_field "$tag/nexusd-cluster-$ARCH.tar.gz")"
    if [ -n "${VAULT_LIVE_NEXUS_SHA256:-}" ]; then
        [ -z "$nexus_sha" ] || [ "$nexus_sha" = "$VAULT_LIVE_NEXUS_SHA256" ] \
            || die "VAULT_LIVE_NEXUS_SHA256 disagrees with runtime-artifacts.json for $tag"
        nexus_sha="$VAULT_LIVE_NEXUS_SHA256"
    fi
    vault_version="$(artifact_field nexus-vault)"
    vault_sha="$(artifact_field "vault-v$vault_version/nexus-vault-$ARCH.tar.gz")"
    local nexus_archive="nexusd-cluster-$ARCH-$tag.tar.gz"
    local vault_archive="nexus-vault-$ARCH-$vault_version.tar.gz"
    local cos=https://sudowork-runtime-1309794936.cos.accelerate.myqcloud.com
    fetch_verified "$nexus_archive" "$nexus_sha" \
        "$cos/nexus-vfs/release/$tag/nexusd-cluster-$ARCH.tar.gz" \
        "https://github.com/nexi-lab/nexus-vfs/releases/download/$tag/nexusd-cluster-$ARCH.tar.gz"
    fetch_verified "$vault_archive" "$vault_sha" \
        "$cos/nexus-vault/release/v$vault_version/nexus-vault-$ARCH.tar.gz" \
        "https://github.com/nexi-lab/nexus/releases/download/vault-v$vault_version/nexus-vault-$ARCH.tar.gz"

    local live="$CACHE/$tag"
    rm -rf "$live"
    mkdir -p "$live/plugins"
    extract_one "$CACHE/archives/$nexus_archive" nexusd-cluster "$live"
    extract_one "$CACHE/archives/$vault_archive" libnexus_vault.so "$live/plugins"
    extract_one "$CACHE/archives/$vault_archive" libnexus_vault.so.sig "$live/plugins"
    cat >"$live/artifacts.env" <<EOF
NEXUS_TAG=$tag
NEXUS_ARCHIVE_SHA256=$nexus_sha
NEXUS_BINARY_SHA256=$(sha256sum "$live/nexusd-cluster" | cut -d' ' -f1)
VAULT_VERSION=$vault_version
VAULT_ARCHIVE_SHA256=$vault_sha
VAULT_LIBRARY_SHA256=$(sha256sum "$live/plugins/libnexus_vault.so" | cut -d' ' -f1)
EOF
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
        cat "$live/artifacts.env"
        echo "NEXUSD_VERSION=$("$live/nexusd-cluster" --version 2>&1 | head -1)"
        echo "RUSTC=$(cd rust && rustc -V)"
        echo "CARGO=$(cd rust && cargo -V)"
        echo "KERNEL=$(uname -sr)"
    } | tee "$out/versions.env"

    local run=(-- --test-threads=1 --nocapture)
    [ "${VAULT_LIVE_BUILD_ONLY:-}" = 1 ] && run=(--no-run)
    [ -n "${VAULT_LIVE_TEST_FILTER:-}" ] && [ "${run[0]}" = -- ] \
        && run=(-- "$VAULT_LIVE_TEST_FILTER" --test-threads=1 --nocapture)
    cd rust
    SUDOCODE_VAULT_LIVE="$live" SUDOCODE_VAULT_LIVE_EVIDENCE="$out" \
        cargo test --offline --locked -p rusty-sudocode-cli --test pty_vault_live "${run[@]}" \
        2>&1 | tee "$out/cargo-test.log"
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
    local entry=(bash -c 'tr -d "\r" </src/e2e/vault-live/run.sh >/tmp/run.sh && exec bash /tmp/run.sh "$1"' run)
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
