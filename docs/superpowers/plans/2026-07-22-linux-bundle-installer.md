# Linux Bundle Installer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a cross-distribution Linux bundle artifact that lets customers run `sudo ./scode-setup install` to install `scode` globally and then reuse `scode-setup` to update model configuration.

**Architecture:** Reuse the existing static musl `scode` binary and `packaging/linux/deb/scode-setup` script. Add a bundle builder that stages `scode`, `scode-setup`, and Chinese install docs into a tarball; extend `scode-setup` with explicit `install`, `configure`, `doctor`, and `uninstall` commands while preserving the existing configuration behavior.

**Tech Stack:** Bash, tar, GitHub Actions, existing Rust Linux musl release binary.

---

## File Map

- Create `packaging/linux/bundle/build-bundle.sh`: builds `scode-linux-x64-bundle.tar.gz` from an existing binary and setup script.
- Create `packaging/linux/bundle/tests/test_bundle.sh`: verifies bundle layout and `scode-setup install` behavior with a fake install root.
- Modify `packaging/linux/deb/scode-setup`: adds install/configure/doctor/uninstall commands without changing existing config flows.
- Modify `.github/workflows/release.yml`: uploads `scode-linux-x64-bundle.tar.gz` from the Linux x64 job.
- Create `docs/linux-bundle-install.zh-CN.md`: customer-facing Chinese install and update guide.

## Task 1: Bundle Test

**Files:**
- Create: `packaging/linux/bundle/tests/test_bundle.sh`

- [ ] **Step 1: Write failing bundle layout and install test**

The test creates a fake `scode`, runs `build-bundle.sh`, inspects the tarball, extracts it, and runs `scode-setup install` into a temporary install root.

- [ ] **Step 2: Run test to verify it fails**

Run: `bash packaging/linux/bundle/tests/test_bundle.sh`

Expected: FAIL because `packaging/linux/bundle/build-bundle.sh` does not exist.

## Task 2: Bundle Builder

**Files:**
- Create: `packaging/linux/bundle/build-bundle.sh`

- [ ] **Step 1: Implement `build-bundle.sh`**

Support `--version`, `--binary`, and `--output`. Stage `scode`, `scode-setup`, `README-install.zh-CN.md`, and `SHA256SUMS.txt`, then write `scode-linux-x64-bundle.tar.gz`.

- [ ] **Step 2: Run bundle test**

Run: `bash packaging/linux/bundle/tests/test_bundle.sh`

Expected: FAIL at install command until `scode-setup install` exists.

## Task 3: scode-setup Install Commands

**Files:**
- Modify: `packaging/linux/deb/scode-setup`

- [ ] **Step 1: Add command parser**

Support `install`, `configure`, `doctor`, and `uninstall`. Keep no subcommand as `configure`.

- [ ] **Step 2: Add install root testing hooks**

Support `SCODE_INSTALL_ROOT` and `SCODE_SKIP_CONFIG=1` so tests can install under a temp directory without touching `/usr/local/bin` or user configs.

- [ ] **Step 3: Add installer behavior**

Install sibling `scode` and `scode-setup` to `/usr/local/bin` by default, write `/usr/local/lib/scode/install-manifest`, and optionally run config for the real user.

- [ ] **Step 4: Run bundle test**

Run: `bash packaging/linux/bundle/tests/test_bundle.sh`

Expected: PASS.

## Task 4: Workflow Integration

**Files:**
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Build bundle from Linux x64 job**

After staging the static Linux x64 binary, call `../packaging/linux/bundle/build-bundle.sh`.

- [ ] **Step 2: Upload bundle artifact**

Upload `rust/dist/scode-linux-x64-bundle.tar.gz`.

- [ ] **Step 3: Include bundle in release assets and COS mirror**

Existing `dist/scode-*` release upload already matches the bundle name; verify the COS upload loop includes `.tar.gz`.

## Task 5: Documentation

**Files:**
- Create: `docs/linux-bundle-install.zh-CN.md`

- [ ] **Step 1: Write Chinese customer guide**

Include download, upload, `sudo ./scode-setup install`, validation, model updates, non-interactive config, upgrade, uninstall, and troubleshooting.

## Task 6: Verification and Git

**Files:**
- No new files.

- [ ] **Step 1: Run script syntax checks**

Run: `bash -n packaging/linux/bundle/build-bundle.sh packaging/linux/bundle/tests/test_bundle.sh packaging/linux/deb/scode-setup`

Expected: PASS.

- [ ] **Step 2: Run tests**

Run: `bash packaging/linux/deb/tests/test_build_deb.sh && bash packaging/linux/bundle/tests/test_bundle.sh`

Expected: PASS.

- [ ] **Step 3: Validate workflow YAML**

Run: `ruby -e 'require "yaml"; YAML.load_file(".github/workflows/release.yml"); puts "workflow yaml ok"'`

Expected: `workflow yaml ok`.

- [ ] **Step 4: Commit and push**

Stage only the bundle feature files and push `codex/linux-bundle-installer`.
