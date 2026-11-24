//! `scode a2a setup` hands the work to the bridge package and never starts a
//! session, whatever is typed after `a2a`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("scode-a2a-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).expect("temp home");
    home.canonicalize().expect("canonical temp home")
}

fn scode(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_scode"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home);
    command
}

fn node() -> Option<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// A stand-in bridge package: it passes or fails its integrity check on
/// request, and otherwise reports the arguments it was started with.
fn fixture_package(home: &Path, verifies: bool) -> PathBuf {
    let package = home.join("package");
    fs::create_dir_all(package.join("dist")).expect("package folder");
    fs::write(
        package.join("dist").join("server.cjs"),
        format!(
            r"const fs = require('node:fs'), path = require('node:path');
const args = process.argv.slice(2);
if (args[0] === '--verify') {{
  if ({verifies}) {{ console.log(JSON.stringify({{ verified: true }})); process.exit(0); }}
  console.error('Package integrity mismatch: dist/server.cjs'); process.exit(1);
}}
fs.writeFileSync(path.join(__dirname, 'setup-ran'), '');
console.log(JSON.stringify({{ ok: args.includes('--issuer'), argv: args }}));
process.exit(args.includes('--issuer') ? 0 : 1);
"
        ),
    )
    .expect("fixture entry");
    package
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is one JSON object ({error}): {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn anything_after_a2a_that_is_not_a_command_is_refused_without_a_session() {
    let home = temp_home("usage");
    for args in [
        vec!["a2a"],
        vec!["a2a", "setup"],
        vec!["a2a", "setup", "emacs"],
        vec!["a2a", "what", "is", "this"],
    ] {
        let output = scode(&home).args(&args).output().expect("scode runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} must not succeed");
        assert!(
            output.stdout.is_empty(),
            "{args:?} printed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            stderr.contains("Usage") || stderr.contains("usage"),
            "{args:?} shows how to use the command: {stderr}"
        );
    }
    // No session, so no session store was created in the working folder.
    assert!(!home.join(".scode").exists());
    let help = scode(&home)
        .args(["a2a", "setup", "--help"])
        .output()
        .expect("scode runs");
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("--issuer") && text.contains("claude"),
        "{text}"
    );
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn a_node_that_cannot_run_is_reported_with_the_flag_to_fix_it() {
    let home = temp_home("node");
    let output = scode(&home)
        .args(["a2a", "setup", "claude", "--node"])
        .arg(home.join("no-such-node"))
        .output()
        .expect("scode runs");
    assert_eq!(output.status.code(), Some(1));
    let report = json(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(report["state"], "needs_input");
    assert_eq!(report["changedAnything"], false);
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("is not Node.js 20 or newer"),
        "{report}"
    );
    let next = report["next"][0]["argv"]
        .as_array()
        .expect("an exact next command");
    assert_eq!(next[1..4], ["a2a", "setup", "claude"]);
    assert_eq!(next[next.len() - 2], "--node");
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn setup_hands_over_to_the_package_with_what_the_caller_typed() {
    let Some(node) = node() else {
        eprintln!("skipped: no node on PATH");
        return;
    };
    let home = temp_home("handover");
    let package = fixture_package(&home, true);
    let run = |extra: &[&str]| {
        scode(&home)
            // Only node is on PATH, so no other scode can be found.
            .env("PATH", node.parent().expect("node has a folder"))
            .args(["a2a", "setup", "claude", "--sdk"])
            .arg(&package)
            .args(extra)
            .output()
            .expect("scode runs")
    };

    let output = run(&["--issuer", "https://founder.example:2126"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json(&output);
    let argv: Vec<&str> = report["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        argv[..3],
        ["--setup-claude", "--issuer", "https://founder.example:2126"]
    );
    let scode_flag = argv
        .iter()
        .position(|arg| *arg == "--scode")
        .expect("this scode is named when no other is installed");
    assert_eq!(
        Path::new(argv[scode_flag + 1]).file_name(),
        Path::new(env!("CARGO_BIN_EXE_scode")).file_name()
    );
    let caller: Vec<String> = serde_json::from_str(
        argv[argv.iter().position(|arg| *arg == "--caller-argv").unwrap() + 1],
    )
    .unwrap();
    assert_eq!(caller[1..4], ["a2a", "setup", "claude"]);
    assert!(
        caller.contains(&"--issuer".to_string()),
        "the caller's own flags are repeated: {caller:?}"
    );

    // The package's verdict is the command's exit code.
    assert_eq!(run(&[]).status.code(), Some(1));
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn codex_goes_to_its_own_setup_and_misplaced_options_are_refused() {
    let Some(node) = node() else {
        eprintln!("skipped: no node on PATH");
        return;
    };
    let home = temp_home("codex");
    let package = fixture_package(&home, true);

    // Codex goes to the package's Codex setup, with its own options.
    let codex = scode(&home)
        .env("PATH", node.parent().expect("node has a folder"))
        .args([
            "a2a",
            "setup",
            "codex",
            "--issuer",
            "https://founder.example:2126",
        ])
        .args(["--approve-tools", "--port", "24000", "--sdk"])
        .arg(&package)
        .output()
        .expect("scode runs");
    assert_eq!(
        codex.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&codex.stderr)
    );
    let report = json(&codex);
    let argv: Vec<&str> = report["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(argv[0], "--setup-codex");
    assert!(
        argv.windows(2).any(|pair| pair == ["--port", "24000"]),
        "{argv:?}"
    );
    assert!(argv.contains(&"--approve-tools"), "{argv:?}");

    // An option for the other program is refused by name, before anything runs.
    fs::remove_file(package.join("dist").join("setup-ran")).expect("the fixture ran above");
    let misplaced = scode(&home)
        .env("PATH", node.parent().expect("node has a folder"))
        .args(["a2a", "setup", "claude", "--approve-tools", "--sdk"])
        .arg(&package)
        .output()
        .expect("scode runs");
    assert_eq!(misplaced.status.code(), Some(1));
    let report = json(&misplaced);
    assert_eq!(report["state"], "needs_input");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("--approve-tools does not apply to `setup claude`"),
        "{report}"
    );
    assert!(!package.join("dist").join("setup-ran").exists());
    fs::remove_dir_all(home).expect("cleanup");
}

#[test]
fn a_package_that_fails_its_integrity_check_sets_nothing_up() {
    let Some(node) = node() else {
        eprintln!("skipped: no node on PATH");
        return;
    };
    let home = temp_home("integrity");
    let package = fixture_package(&home, false);
    let output = scode(&home)
        .env("PATH", node.parent().expect("node has a folder"))
        .args([
            "a2a",
            "setup",
            "claude",
            "--issuer",
            "https://founder.example:2126",
            "--sdk",
        ])
        .arg(&package)
        .output()
        .expect("scode runs");
    assert_eq!(output.status.code(), Some(1));
    let report = json(&output);
    assert_eq!(report["state"], "failed");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("Package integrity mismatch"),
        "{report}"
    );
    assert!(
        !package.join("dist").join("setup-ran").exists(),
        "setup must not run on a package that fails its check"
    );
    fs::remove_dir_all(home).expect("cleanup");
}
