#!/usr/bin/env bash
# Native live acceptance for the builtin Vault plugin (sudoprivacy/sudocode#1458)
# on hosts run.sh's Linux container cannot stand in for: macOS, whose signed
# Vault plugin has no Linux build. Same test (pty_vault_live.rs) and the same
# pins as run.sh: the nexus-vfs release rust/Cargo.lock resolves, with the
# checksums rust/crates/service-host/runtime-artifacts.json records for the
# target. Linux x86_64 works too, as a check of this script itself.
#
#   run-native.sh bundle TARGET OUT_DIR
#       On a host with the Rust toolchain, a C toolchain (on macOS, the Command
#       Line Tools) and protoc, from a clean checkout of the commit to
#       qualify: build `scode` and the test for TARGET (a Rust target triple,
#       e.g. aarch64-apple-darwin; an Intel Mac cross-builds it), fetch the
#       pinned daemon and Vault plugin archives for TARGET, and pack all four
#       with a MANIFEST into OUT_DIR/vault-live-<commit>-<TARGET>.tar.gz.
#       Prints the bundle's path and sha256.
#
#   run-native.sh run BUNDLE SHA256
#       On the TARGET host, which needs no compiler, git, network or checkout,
#       only this script and lib.sh beside it: refuse BUNDLE unless it hashes
#       to SHA256 (nothing is extracted before that), re-check every file in
#       it against its MANIFEST and the archives against their pins, then run
#       the test. Evidence: versions.env, test.log and one JSON report per
#       case.
#
#   run-native.sh
#       Both, for this host, from a checkout.
#
# Host contract for `run`
#   - Needs only bash (3.2 is enough), tar, ps, awk, and sha256sum, shasum or
#     openssl, as a macOS install has them without the Command Line Tools.
#   - Everything the run creates lives under one work directory
#     ($VAULT_LIVE_WORK, default a fresh `mktemp -d`): the extracted bundle,
#     daemon and plugin, the test's scratch homes, daemon data and logs, and
#     the evidence. It is removed on exit unless VAULT_LIVE_KEEP=1; set
#     VAULT_LIVE_OUT to copy the evidence out first.
#   - The daemons and the canary HTTP target bind 127.0.0.1 on ephemeral
#     ports. Nothing binds a fixed port, installs a service, or reads or writes
#     the caller's home or settings: every scode under test gets a scratch
#     home. On exit, including on INT, TERM or HUP, the run kills its own
#     processes: its descendants, plus any orphan whose command line names the
#     work directory (a daemon whose test was killed). It signals nothing else,
#     nor the shell that started it.
#   - The run opens no network connection beyond loopback, but unlike run.sh's
#     `--network none` container a native host stays online; that is the one
#     isolation run.sh has and this lacks.
#   - VAULT_LIVE_TEST_FILTER selects cases.
#
# The `bundle` step installs the pinned toolchain and TARGET into the caller's
# RUSTUP_HOME and CARGO_HOME (VAULT_LIVE_ISOLATE=1 puts both in its own work
# directory instead) and is the only step that uses the network.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib.sh
. "$HERE/lib.sh"

usage() { die "usage: $0 [bundle TARGET OUT_DIR | run BUNDLE SHA256]"; }

# The release archives and plugin library for a Rust target triple.
target_pins() {
    case "$1" in
        aarch64-apple-darwin) NEXUS_ARCH=macos-aarch64 VAULT_ARCH=macos-arm64 PLUGIN=libnexus_vault.dylib ;;
        x86_64-apple-darwin) NEXUS_ARCH=macos-x86_64 VAULT_ARCH=macos-x86_64 PLUGIN=libnexus_vault.dylib ;;
        x86_64-unknown-linux-gnu) NEXUS_ARCH=linux-x86_64 VAULT_ARCH=linux-x86_64 PLUGIN=libnexus_vault.so ;;
        *) die "no pinned daemon and Vault plugin for $1" ;;
    esac
}

host_target() {
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) echo aarch64-apple-darwin ;;
        Darwin-x86_64) echo x86_64-apple-darwin ;;
        Linux-x86_64) echo x86_64-unknown-linux-gnu ;;
        *) die "no pinned daemon and Vault plugin for $(uname -s) $(uname -m)" ;;
    esac
}

require() {
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
    done
}

mode="${1:-}"
case "$mode" in
    bundle) [ $# = 3 ] || usage; target_pins "$2"; mkdir -p "$3"; DIST="$(cd "$3" && pwd -P)" ;;
    run)
        [ $# = 3 ] || usage
        [[ "$3" =~ ^[0-9a-f]{64}$ ]] || die "SHA256 must be 64 lowercase hex digits"
        [ -f "$2" ] || die "no bundle at $2"
        actual="$(sha256_of "$2")"
        [ "$actual" = "$3" ] || die "$2 hashes to $actual, not $3; nothing was extracted"
        BUNDLE="$(cd "$(dirname "$2")" && pwd -P)/$(basename "$2")"
        ;;
    "") ;;
    *) usage ;;
esac
require tar ps awk

WORK="${VAULT_LIVE_WORK:-$(mktemp -d "${TMPDIR:-/tmp}/vault-live-native.XXXXXX")}"
mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd -P)"
CACHE="$WORK/cache"

# This run's live processes: its descendants, and orphans whose command line
# names $WORK. Never this shell or an ancestor, which a caller's command line
# may name $WORK in.
run_pids() {
    ps -axww -o pid=,ppid=,args= | SELF=$$ WORK="$WORK" awk '
        { parent[$1] = $2; line[$1] = $0; order[NR] = $1 }
        END {
            self = ENVIRON["SELF"] ""
            for (p = self; p != "" && p != "0" && p != "1"; p = parent[p]) ancestor[p] = 1
            for (i = 1; i <= NR; i++) {
                p = order[i]
                if ((p in ancestor) || line[p] ~ /^ *[0-9]+ +[0-9]+ +ps -axww/) continue
                mine = index(line[p], ENVIRON["WORK"]) > 0
                for (q = parent[p]; !mine && q != "" && q != "0" && q != "1"; q = parent[q])
                    if (q == self) mine = 1
                if (mine) print p
            }
        }'
}

stop_run() {
    local round pids
    for round in 1 2; do
        pids="$(run_pids)"
        # shellcheck disable=SC2086
        [ -z "$pids" ] || kill -KILL $pids 2>/dev/null || true
        [ "$round" = 2 ] || sleep 1
    done
}

cleanup() {
    local status=$?
    trap - EXIT HUP INT TERM
    stop_run
    if [ -n "${VAULT_LIVE_OUT:-}" ] && [ -d "$WORK/out" ]; then
        mkdir -p "$VAULT_LIVE_OUT"
        cp -R "$WORK/out/." "$VAULT_LIVE_OUT/"
        log "evidence copied to $VAULT_LIVE_OUT"
    fi
    if [ "${VAULT_LIVE_KEEP:-0}" = 1 ]; then
        log "kept $WORK"
    else
        rm -rf "$WORK"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$WORK/tmp"
export TMPDIR="$WORK/tmp"

# Bash runs a trap only once a foreground command returns; waiting on a
# background one lets a signal stop the run at once.
interruptible() {
    "$@" &
    wait "$!"
}

# The executable cargo built for the pty_vault_live test, from its JSON messages.
test_executable() {
    sed -n 's/.*"name":"pty_vault_live".*"executable":"\([^"]*\)".*/\1/p' "$1" | tail -n 1
}

build_binaries() {
    local target="$1" messages="$2"
    cd rust
    cargo build --locked --release --target "$target" -p rusty-sudocode-cli --bin scode
    cargo test --locked --release --target "$target" -p rusty-sudocode-cli --test pty_vault_live \
        --no-run --message-format=json >"$messages"
}

# Build a bundle for `target` into `dist`; sets BUNDLE and BUNDLE_SHA256.
make_bundle() {
    local target="$1" dist="$2" head channel stage exe name
    require curl git rustup
    [ -n "${PROTOC:-}" ] || command -v protoc >/dev/null 2>&1 || die "protoc is required (PATH or PROTOC)"
    target_pins "$target"
    cd "$HERE/../.."
    head="$(git rev-parse HEAD)"
    [ -z "$(git status --porcelain)" ] || head="$head+dirty"
    if [ "${VAULT_LIVE_ISOLATE:-0}" = 1 ]; then
        export RUSTUP_HOME="$WORK/rustup" CARGO_HOME="$WORK/cargo"
        export PATH="$CARGO_HOME/bin:$PATH"
    fi
    export CARGO_TARGET_DIR="$WORK/target"
    # The live test and its certgen dev-dependency compile off Linux through
    # this cfg; on Linux the target cfg already selects them.
    case "$target" in
        *-linux-*) ;;
        *) export RUSTFLAGS="${RUSTFLAGS:-} --cfg vault_live" ;;
    esac

    log "bundle $target from $head"
    channel="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml | tr -d '\r')"
    # Only the bundle line may reach stdout.
    interruptible rustup toolchain install "$channel" --profile minimal >&2
    interruptible rustup target add "$target" --toolchain "$channel" >&2
    fetch_archives "$NEXUS_ARCH" "$VAULT_ARCH"
    interruptible build_binaries "$target" "$WORK/build.json"

    stage="$WORK/stage/vault-live-bundle"
    rm -rf "$WORK/stage"
    mkdir -p "$stage/bin" "$stage/archives" "$dist"
    exe="$(test_executable "$WORK/build.json")"
    [ -n "$exe" ] || die "cargo reported no pty_vault_live executable"
    cp "$CARGO_TARGET_DIR/$target/release/scode" "$stage/bin/scode"
    cp "$exe" "$stage/bin/pty_vault_live"
    cp "$CACHE/archives/archives.env" \
        "$CACHE/archives/$(env_value NEXUS_FILE "$CACHE/archives/archives.env")" \
        "$CACHE/archives/$(env_value VAULT_FILE "$CACHE/archives/archives.env")" "$stage/archives/"
    cat >"$stage/MANIFEST" <<EOF
SOURCE=$head
TARGET=$target
PLUGIN=$PLUGIN
PROFILE=release
RUSTC=$(cd rust && rustc -V)
CARGO=$(cd rust && cargo -V)
BUILD_HOST=$(uname -srm)
SCODE_SHA256=$(sha256_of "$stage/bin/scode")
TEST_SHA256=$(sha256_of "$stage/bin/pty_vault_live")
EOF
    name="vault-live-$(printf '%s' "$head" | cut -c1-12)$(case "$head" in *+dirty) echo -dirty ;; esac)-$target.tar.gz"
    COPYFILE_DISABLE=1 tar -czf "$dist/$name" -C "$WORK/stage" vault-live-bundle
    BUNDLE="$dist/$name"
    BUNDLE_SHA256="$(sha256_of "$BUNDLE")"
    log "bundle $BUNDLE sha256 $BUNDLE_SHA256"
    printf '%s %s\n' "$BUNDLE" "$BUNDLE_SHA256"
}

run_bundle_test() {
    local bin="$1" live="$2" out="$3"
    local args=(--test-threads=1 --nocapture)
    if [ -n "${VAULT_LIVE_TEST_FILTER:-}" ]; then
        args=("$VAULT_LIVE_TEST_FILTER" --test-threads=1 --nocapture)
    fi
    (
        SUDOCODE_VAULT_LIVE="$live" SUDOCODE_VAULT_LIVE_EVIDENCE="$out" SUDOCODE_VAULT_SCODE="$bin/scode" \
            "$bin/pty_vault_live" "${args[@]}" 2>&1 | tee "$out/test.log"
        exit "${PIPESTATUS[0]}"
    )
}

# Run the bundle at BUNDLE, whose sha256 the caller already checked.
run_bundle() {
    local sha="$1" host bundle manifest kind tag out
    host="$(host_target)"
    target_pins "$host"
    mkdir -p "$WORK/run"
    tar -xzf "$BUNDLE" -C "$WORK/run"
    bundle="$WORK/run/vault-live-bundle"
    manifest="$bundle/MANIFEST"
    [ "$(env_value TARGET "$manifest")" = "$host" ] \
        || die "the bundle targets $(env_value TARGET "$manifest"), this host is $host"
    [ "$(env_value PLUGIN "$manifest")" = "$PLUGIN" ] || die "the bundle names another plugin"
    for kind in SCODE:scode TEST:pty_vault_live; do
        [ "$(sha256_of "$bundle/bin/${kind#*:}")" = "$(env_value "${kind%%:*}_SHA256" "$manifest")" ] \
            || die "bin/${kind#*:} does not match the MANIFEST"
    done
    lay_out "$bundle/archives" "$PLUGIN" "$WORK/live"
    tag="$(env_value NEXUS_TAG "$WORK/live/artifacts.env")"
    out="$WORK/out/$tag"
    mkdir -p "$out"
    {
        echo "SOURCE=$(env_value SOURCE "$manifest")"
        echo "BUNDLE_SHA256=$sha"
        grep -E '^(TARGET|PROFILE|RUSTC|CARGO|BUILD_HOST|SCODE_SHA256|TEST_SHA256)=' "$manifest"
        echo "HOST=$(uname -srm)"
        if command -v sw_vers >/dev/null 2>&1; then
            echo "MACOS=$(sw_vers -productVersion) ($(sw_vers -buildVersion))"
        fi
        cat "$WORK/live/artifacts.env"
        echo "NEXUSD_VERSION=$("$WORK/live/nexusd-cluster" --version 2>&1 | head -1)"
    } | tee "$out/versions.env"
    log "test against $tag on $host"
    interruptible run_bundle_test "$bundle/bin" "$WORK/live" "$out"
    # The cases skip, and so pass, without SUDOCODE_VAULT_LIVE; never count that.
    ! grep -q 'skipping the live Vault acceptance' "$out/test.log" || die "the live cases were skipped"
    log "passed against $tag on $host"
}

case "$mode" in
    bundle) make_bundle "$2" "$DIST" ;;
    run) run_bundle "$3" ;;
    "")
        make_bundle "$(host_target)" "$WORK/dist" >/dev/null
        run_bundle "$BUNDLE_SHA256"
        ;;
esac
