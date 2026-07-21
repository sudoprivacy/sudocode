# Linux deb Installer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and publish a Ubuntu/Debian `.deb` package for `scode` with a terminal-only setup command that can configure and later update model settings.

**Architecture:** Add a focused `packaging/linux/deb` package builder that assembles Debian filesystem layout from an existing release binary. Ship `scode-setup` as a reusable shell wizard, run it from `postinst` only when an interactive terminal exists, and integrate the package artifact into the existing GitHub Release workflow.

**Tech Stack:** POSIX shell/bash, `dpkg-deb`, GitHub Actions, existing Rust release binary.

---

## File Map

- Create `packaging/linux/deb/build-deb.sh`: validates inputs, builds package root, renders `control`, installs files, runs `dpkg-deb --build`.
- Create `packaging/linux/deb/scode-setup`: terminal setup and update wizard for `~/.nexus/sudocode`.
- Create `packaging/linux/deb/postinst`: safe install hook that runs setup only for interactive installs.
- Create `packaging/linux/deb/prerm`: minimal maintainer script.
- Create `packaging/linux/deb/control.template`: Debian package metadata template.
- Create `packaging/linux/deb/tests/test_build_deb.sh`: shell tests for package layout and noninteractive postinst behavior.
- Modify `.github/workflows/release.yml`: produce `scode_0.1.12_amd64.deb` from the linux-x64 matrix build and upload it as an artifact.
- Keep `docs/superpowers/specs/2026-07-21-linux-deb-installer-design.md` as the design record.

## Task 1: Package Builder Test

**Files:**
- Create: `packaging/linux/deb/tests/test_build_deb.sh`

- [ ] **Step 1: Write the failing package layout test**

```bash
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

"$ROOT/packaging/linux/deb/build-deb.sh" \
  --version 0.1.12 \
  --binary "$fake_bin" \
  --output "$TMP/dist"

deb="$TMP/dist/scode_0.1.12_amd64.deb"
test -f "$deb"
dpkg-deb --contents "$deb" | grep -q './usr/bin/scode$'
dpkg-deb --contents "$deb" | grep -q './usr/bin/scode-setup$'
dpkg-deb --contents "$deb" | grep -q './usr/lib/scode/scode-setup$'
dpkg-deb --info "$deb" | grep -q 'Package: scode'
dpkg-deb --info "$deb" | grep -q 'Version: 0.1.12'
```

- [ ] **Step 2: Run test to verify it fails**

Run: `bash packaging/linux/deb/tests/test_build_deb.sh`

Expected: FAIL because `packaging/linux/deb/build-deb.sh` does not exist.

## Task 2: Minimal deb Builder

**Files:**
- Create: `packaging/linux/deb/build-deb.sh`
- Create: `packaging/linux/deb/control.template`
- Create: `packaging/linux/deb/postinst`
- Create: `packaging/linux/deb/prerm`
- Create: `packaging/linux/deb/scode-setup`

- [ ] **Step 1: Implement builder and package scripts**

Create executable shell files that install `scode`, `scode-setup`, README, control metadata, and maintainer scripts into a temporary package root, then run `dpkg-deb --build`.

- [ ] **Step 2: Run package layout test**

Run: `bash packaging/linux/deb/tests/test_build_deb.sh`

Expected: PASS.

## Task 3: Setup Script Behavior Test

**Files:**
- Modify: `packaging/linux/deb/tests/test_build_deb.sh`

- [ ] **Step 1: Extend test for noninteractive setup**

Add assertions:

```bash
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
! grep -q '"web_search"' "$setup_home/.nexus/sudocode/sudocode.json"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `bash packaging/linux/deb/tests/test_build_deb.sh`

Expected: FAIL until `scode-setup --non-interactive` supports environment-driven config.

## Task 4: Setup Script Implementation

**Files:**
- Modify: `packaging/linux/deb/scode-setup`

- [ ] **Step 1: Implement noninteractive config**

Support `SCODE_BASE_URL`, `SCODE_API_KEY`, `SCODE_MODEL`, `SCODE_MODELS`, `SCODE_ENABLE_SEARCH`, and `--non-interactive`.

- [ ] **Step 2: Implement interactive menus**

Support first-run full config and existing-config menu for default model, model refresh, credential rebuild, and web_search toggling.

- [ ] **Step 3: Run setup behavior test**

Run: `bash packaging/linux/deb/tests/test_build_deb.sh`

Expected: PASS.

## Task 5: Release Workflow Integration

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Add deb build step for linux-x64**

After tar.gz creation, run `../packaging/linux/deb/build-deb.sh --version "${GITHUB_REF_NAME#v}" --binary target/release/scode --output dist` only when `matrix.name == 'linux-x64'`.

- [ ] **Step 2: Upload deb artifact**

Add a conditional upload-artifact step for `rust/dist/scode_*_amd64.deb`.

- [ ] **Step 3: Include deb in COS mirror upload**

Change the COS loop to include `dist/scode_*.deb` along with tar.gz and zip.

## Task 6: Verification

**Files:**
- No new files.

- [ ] **Step 1: Run package test**

Run: `bash packaging/linux/deb/tests/test_build_deb.sh`

Expected: PASS.

- [ ] **Step 2: Build local package from existing root `scode` binary**

Run: `packaging/linux/deb/build-deb.sh --version 0.1.12 --binary ./scode --output /tmp/scode-deb-test`

Expected: creates `/tmp/scode-deb-test/scode_0.1.12_amd64.deb`.

- [ ] **Step 3: Inspect package metadata**

Run: `dpkg-deb --info /tmp/scode-deb-test/scode_0.1.12_amd64.deb && dpkg-deb --contents /tmp/scode-deb-test/scode_0.1.12_amd64.deb`

Expected: package metadata and required files are present.

- [ ] **Step 4: Commit and push**

Stage only:

```bash
git add .github/workflows/release.yml docs/superpowers/specs/2026-07-21-linux-deb-installer-design.md docs/superpowers/plans/2026-07-21-linux-deb-installer.md packaging/linux/deb
git commit -m "feat: add linux deb installer packaging"
git push -u origin codex/linux-deb-installer
```
