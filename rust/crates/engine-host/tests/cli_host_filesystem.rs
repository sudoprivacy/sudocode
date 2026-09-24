//! What a CLI session's filesystem is, now that it is a kernel.
//!
//! The co-host's side of this is `cohost_live_llm`: an agent whose file tools
//! reach a kernel. These prove the CLI host reaches one too, and that doing so
//! left the developer-visible contract intact — the files are still the files
//! on disk, and a session still sees its own workspace.
//!
//! What a session may reach is new, and is the part with no prior behaviour to
//! compare against, so it is asserted from both sides: refused outside the
//! mounts, allowed once `additionalDirectories` names the directory.

use std::path::Path;

use engine_host::HostContext;

/// One temp root per test, under a config home that is not the developer's.
///
/// The host reads real configuration at boot, so `SUDO_CODE_CONFIG_HOME` is
/// redirected — otherwise these tests would pass or fail depending on whose
/// machine ran them. It is set ONCE for the whole binary rather than per test:
/// the variable is process-wide, and tests run in parallel, so a per-test
/// assignment would be a race in which one test's config home silently becomes
/// another's.
fn sandbox(name: &str) -> tempfile::TempDir {
    static CONFIG_HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let home = CONFIG_HOME.get_or_init(|| {
        let home = tempfile::Builder::new()
            .prefix("scode-host-config-")
            .tempdir()
            .expect("config home");
        std::env::set_var("SUDO_CODE_CONFIG_HOME", home.path());
        home
    });
    debug_assert!(home.path().exists());
    tempfile::Builder::new()
        .prefix(&format!("scode-host-{name}-"))
        .tempdir()
        .expect("temp dir")
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(path, contents).expect("write fixture");
}

/// The workspace reaches the kernel, and the kernel reaches the real files.
///
/// Both directions matter. Reading a file the test wrote with `std::fs` proves
/// the mount serves what is on disk rather than a copy of it; writing through
/// the backend and finding the bytes on disk proves the same in reverse. A
/// connector that copied instead would pass one and fail the other, and a
/// developer's editor open beside `scode` would be editing a different file.
#[test]
fn a_session_reads_and_writes_the_real_files_in_its_workspace() {
    let dir = sandbox("workspace");
    let workspace = dir.path().join("project");
    write(&workspace.join("notes.txt"), "on disk\n");

    let host = HostContext::for_cli_session(&workspace).expect("boot the session host");
    let fs = &host.fs;

    // The path a tool reports is the one the HOST spells. Not cosmetic: the
    // model passes these paths to `bash`, which runs on the host and cannot
    // open a VFS path.
    let reported = fs.normalize("notes.txt").expect("normalize");
    assert_eq!(
        reported,
        workspace.join("notes.txt").to_string_lossy(),
        "a session should report host paths, not the VFS spelling underneath"
    );

    let read = fs
        .read_to_string(&reported)
        .expect("the workspace file is readable through the kernel");
    assert_eq!(read, "on disk\n");

    fs.write(
        &fs.normalize_allow_missing("written.txt").expect("normalize"),
        b"through the kernel\n",
    )
    .expect("write through the kernel");
    let on_disk = std::fs::read_to_string(workspace.join("written.txt"))
        .expect("the write landed in the real file");
    assert_eq!(on_disk, "through the kernel\n");
}

/// A path outside the session's mounts is refused.
///
/// This is the containment a co-hosted agent has always had, arriving at the
/// CLI: the session's world is its workspace, and a sibling checkout — or an
/// ssh key — is not in it. Asserted against a directory that exists and is
/// readable by the process, so a pass means the mount table refused it, not
/// that the file was missing.
#[test]
fn a_session_cannot_reach_outside_its_mounts() {
    let dir = sandbox("outside");
    let workspace = dir.path().join("project");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let outside = dir.path().join("elsewhere").join("secret.txt");
    write(&outside, "not yours\n");
    assert!(
        std::fs::read_to_string(&outside).is_ok(),
        "the file must be readable on disk, or this proves nothing"
    );

    let host = HostContext::for_cli_session(&workspace).expect("boot the session host");
    let path = host
        .fs
        .normalize(&outside.to_string_lossy())
        .expect("an absolute host path normalizes");
    assert!(
        host.fs.read_to_string(&path).is_err(),
        "a path outside the session's mounts must be refused"
    );
}

/// `additionalDirectories` is how a session reaches a second checkout.
///
/// The same read the previous test refuses, allowed by the one setting that
/// widens the mount table — so the refusal is a policy with a remedy, not a
/// wall.
#[test]
fn additional_directories_widen_what_a_session_reaches() {
    let dir = sandbox("additional");
    let workspace = dir.path().join("project");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sibling = dir.path().join("sibling");
    write(&sibling.join("shared.txt"), "shared\n");
    write(
        &workspace.join(".scode.json"),
        &serde_json::json!({ "additionalDirectories": [sibling.to_string_lossy()] }).to_string(),
    );

    let host = HostContext::for_cli_session(&workspace).expect("boot the session host");
    let path = host
        .fs
        .normalize(&sibling.join("shared.txt").to_string_lossy())
        .expect("an absolute host path normalizes");
    assert_eq!(
        host.fs
            .read_to_string(&path)
            .expect("a declared directory is readable"),
        "shared\n"
    );
}

/// A malformed `additionalDirectories` refuses the boot.
///
/// The setting exists to widen what a session can reach, so ignoring a
/// malformed one would produce the containment refusal with the setting that
/// was meant to prevent it sitting in the config file, apparently applied.
#[test]
fn a_malformed_additional_directories_fails_the_boot() {
    let dir = sandbox("malformed");
    let workspace = dir.path().join("project");
    std::fs::create_dir_all(&workspace).expect("workspace");
    write(
        &workspace.join(".scode.json"),
        r#"{ "additionalDirectories": "/one/directory" }"#,
    );

    let error = HostContext::for_cli_session(&workspace)
        .err()
        .expect("a malformed setting must not boot");
    assert!(
        error.to_string().contains("additionalDirectories"),
        "the error should name the setting; got: {error}"
    );
}
