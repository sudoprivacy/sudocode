# Shared by run.sh (container) and run-native.sh (host): resolve the pinned
# nexus-vfs release and Vault plugin, fetch them against their pinned sha256,
# lay them out for the test, and run it. Callers set CACHE (where archives and
# the extracted artifacts live); the steps that read the checkout's pins run
# from the checkout root.

log() { echo "[vault-live] $*" >&2; }
die() { echo "[vault-live] $*" >&2; exit 1; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        openssl dgst -sha256 -r "$1" | cut -d' ' -f1
    fi
}

# The value of `key` in a KEY=value file, read rather than executed.
env_value() {
    sed -n "s/^$1=//p" "$2" | head -n 1
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
    if [ -f "$dest" ] && [ "$(sha256_of "$dest")" = "$sha" ]; then
        log "cached $name ($sha)"
        return
    fi
    local url
    for url in "$@"; do
        log "downloading $url"
        if curl -fsSL --retry 3 --retry-delay 2 --max-time 300 -o "$dest.part" "$url"; then
            local actual
            actual="$(sha256_of "$dest.part")"
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

# Fetch the pinned daemon archive for `nexus_arch` and Vault plugin archive
# for `vault_arch` into $CACHE/archives, each refused unless it matches the
# sha256 this checkout pins, and describe the pair in
# $CACHE/archives/archives.env.
fetch_archives() {
    local nexus_arch="$1" vault_arch="$2"
    local tag vault_version nexus_sha vault_sha
    tag="$(nexus_tag)"
    nexus_sha="$(artifact_field "$tag/nexusd-cluster-$nexus_arch.tar.gz")"
    if [ -n "${VAULT_LIVE_NEXUS_SHA256:-}" ]; then
        [ -z "$nexus_sha" ] || [ "$nexus_sha" = "$VAULT_LIVE_NEXUS_SHA256" ] \
            || die "VAULT_LIVE_NEXUS_SHA256 disagrees with runtime-artifacts.json for $tag"
        nexus_sha="$VAULT_LIVE_NEXUS_SHA256"
    fi
    vault_version="$(artifact_field nexus-vault)"
    vault_sha="$(artifact_field "vault-v$vault_version/nexus-vault-$vault_arch.tar.gz")"
    local nexus_file="nexusd-cluster-$nexus_arch-$tag.tar.gz"
    local vault_file="nexus-vault-$vault_arch-$vault_version.tar.gz"
    local cos=https://sudowork-runtime-1309794936.cos.accelerate.myqcloud.com
    fetch_verified "$nexus_file" "$nexus_sha" \
        "$cos/nexus-vfs/release/$tag/nexusd-cluster-$nexus_arch.tar.gz" \
        "https://github.com/nexi-lab/nexus-vfs/releases/download/$tag/nexusd-cluster-$nexus_arch.tar.gz"
    fetch_verified "$vault_file" "$vault_sha" \
        "$cos/nexus-vault/release/v$vault_version/nexus-vault-$vault_arch.tar.gz" \
        "https://github.com/nexi-lab/nexus/releases/download/vault-v$vault_version/nexus-vault-$vault_arch.tar.gz"
    cat >"$CACHE/archives/archives.env" <<EOF
NEXUS_TAG=$tag
NEXUS_ARCHIVE=nexusd-cluster-$nexus_arch.tar.gz
NEXUS_FILE=$nexus_file
NEXUS_ARCHIVE_SHA256=$nexus_sha
VAULT_VERSION=$vault_version
VAULT_ARCHIVE=nexus-vault-$vault_arch.tar.gz
VAULT_FILE=$vault_file
VAULT_ARCHIVE_SHA256=$vault_sha
EOF
}

# Lay out the two archives `dir`/archives.env describes in `live` as the test
# expects (`nexusd-cluster`, `plugins/<plugin>` and its `.sig`), re-checking
# each archive against its sha256 first, and record what was used in
# `live`/artifacts.env.
lay_out() {
    local dir="$1" plugin="$2" live="$3" kind file sha
    for kind in NEXUS VAULT; do
        file="$dir/$(env_value "${kind}_FILE" "$dir/archives.env")"
        sha="$(env_value "${kind}_ARCHIVE_SHA256" "$dir/archives.env")"
        [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "no pinned sha256 for the $kind archive"
        [ -f "$file" ] && [ "$(sha256_of "$file")" = "$sha" ] || die "$file does not match its pinned sha256"
    done
    rm -rf "$live"
    mkdir -p "$live/plugins"
    extract_one "$dir/$(env_value NEXUS_FILE "$dir/archives.env")" nexusd-cluster "$live"
    file="$dir/$(env_value VAULT_FILE "$dir/archives.env")"
    extract_one "$file" "$plugin" "$live/plugins"
    extract_one "$file" "$plugin.sig" "$live/plugins"
    {
        grep -v '_FILE=' "$dir/archives.env"
        echo "NEXUS_BINARY_SHA256=$(sha256_of "$live/nexusd-cluster")"
        echo "VAULT_LIBRARY_SHA256=$(sha256_of "$live/plugins/$plugin")"
    } >"$live/artifacts.env"
}

# fetch_archives, then lay_out into $CACHE/<tag>. Prints the tag.
fetch_artifacts() {
    local nexus_arch="$1" vault_arch="$2" plugin="$3" tag
    fetch_archives "$nexus_arch" "$vault_arch"
    tag="$(env_value NEXUS_TAG "$CACHE/archives/archives.env")"
    lay_out "$CACHE/archives" "$plugin" "$CACHE/$tag"
    echo "$tag"
}

# The artifact, daemon and toolchain lines of a run's versions.env.
artifact_versions() {
    local live="$1"
    cat "$live/artifacts.env"
    echo "NEXUSD_VERSION=$("$live/nexusd-cluster" --version 2>&1 | head -1)"
    echo "RUSTC=$(cd rust && rustc -V)"
    echo "CARGO=$(cd rust && cargo -V)"
}

# Run the live test against the artifacts in `live`, writing evidence to `out`.
# VAULT_LIVE_BUILD_ONLY=1 compiles without running; VAULT_LIVE_TEST_FILTER
# selects cases.
run_live_test() {
    local live="$1" out="$2"
    local run=(-- --test-threads=1 --nocapture)
    if [ "${VAULT_LIVE_BUILD_ONLY:-}" = 1 ]; then
        run=(--no-run)
    elif [ -n "${VAULT_LIVE_TEST_FILTER:-}" ]; then
        run=(-- "$VAULT_LIVE_TEST_FILTER" --test-threads=1 --nocapture)
    fi
    (
        cd rust
        SUDOCODE_VAULT_LIVE="$live" SUDOCODE_VAULT_LIVE_EVIDENCE="$out" \
            cargo test --offline --locked -p rusty-sudocode-cli --test pty_vault_live "${run[@]}" \
            2>&1 | tee "$out/cargo-test.log"
        exit "${PIPESTATUS[0]}"
    )
}
