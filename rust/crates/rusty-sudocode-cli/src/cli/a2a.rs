//! `scode a2a setup <frontend>`: attach Claude Code to the agent network.
//!
//! The setup logic lives in the bridge package (`sdk/typescript`), which is
//! also what the frontend runs afterwards. A release build of scode carries
//! that package, so this command needs nothing else transferred to the machine
//! and the two versions always match. It starts no session and calls no model.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

// `BUNDLED_SDK: &[(&str, &[u8])]`, written by build.rs: the bridge package's
// files when the build was given `SCODE_A2A_SDK_DIR`, otherwise empty.
include!(concat!(env!("OUT_DIR"), "/a2a_sdk.rs"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum A2aFrontend {
    /// Claude Code
    Claude,
}

impl A2aFrontend {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub(crate) struct A2aSetupArgs {
    /// The program to attach
    #[arg(value_enum)]
    pub(crate) frontend: A2aFrontend,
    /// The node that issues agent names, as https://HOST:PORT. Needed once per machine
    #[arg(long, value_name = "URL")]
    pub(crate) issuer: Option<String>,
    /// Data folder of the Nexus node on this machine (default: from ~/.nexus/run/node.json)
    #[arg(long, value_name = "DIR")]
    pub(crate) node_data_dir: Option<PathBuf>,
    /// The nexus-agent-enroll executable (default: PATH, then ~/.local/share/nexus/operator)
    #[arg(long, value_name = "FILE")]
    pub(crate) nexus_enroll: Option<PathBuf>,
    /// The scode each attached agent runs (default: ~/.nexus/bin/scode, then PATH, then this one)
    #[arg(long, value_name = "FILE")]
    pub(crate) scode: Option<PathBuf>,
    /// The claude executable (default: PATH)
    #[arg(long, value_name = "FILE")]
    pub(crate) claude_command: Option<PathBuf>,
    /// Where setup keeps its files (default: ~/.config/sudocode/acp/a2a)
    #[arg(long, value_name = "DIR")]
    pub(crate) config_root: Option<PathBuf>,
    /// The Node.js executable, version 20 or newer (default: PATH)
    #[arg(long, value_name = "FILE")]
    pub(crate) node: Option<PathBuf>,
    /// Use this bridge package folder instead of the one built into scode
    #[arg(long, value_name = "DIR")]
    pub(crate) sdk: Option<PathBuf>,
}

/// Why setup stopped before the bridge package could take over, in the same
/// shape the package itself reports, so a caller reads one format.
struct Stop {
    state: &'static str,
    error: String,
    instructions: Vec<String>,
    next: Vec<(String, Vec<String>)>,
}

impl Stop {
    fn print(&self, frontend: A2aFrontend) {
        let next: Vec<_> = self
            .next
            .iter()
            .map(|(why, argv)| json!({ "why": why, "argv": argv }))
            .collect();
        let report = json!({
            "ok": false,
            "frontend": frontend.name(),
            "state": self.state,
            "changed": [],
            "verified": [],
            "notVerified": [],
            "instructions": self.instructions,
            "next": next,
            "changedAnything": false,
            "error": self.error,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a JSON value serializes")
        );
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn on_path(name: &str) -> Option<PathBuf> {
    let names: &[String] = &if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|directory| directory.is_absolute())
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|candidate| candidate.is_file())
}

/// The major version `node --version` reports, as in `v22.21.0`.
fn node_major(node: &Path) -> Option<u32> {
    let output = Command::new(node).arg("--version").output().ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn find_node(args: &A2aSetupArgs, typed: &[String]) -> Result<PathBuf, Stop> {
    let retry = |extra: &[&str]| {
        typed
            .iter()
            .cloned()
            .chain(extra.iter().map(ToString::to_string))
            .collect::<Vec<_>>()
    };
    let Some(node) = args.node.clone().or_else(|| on_path("node")) else {
        return Err(Stop {
            state: "needs_input",
            error: "Node.js was not found".into(),
            instructions: vec![
                "The bridge that attaches Claude Code is a Node.js program, and `node` is not on PATH.".into(),
                "Install Node.js 20 or newer, or pass the absolute path of an existing one with --node.".into(),
            ],
            next: vec![(
                "Run again naming the Node.js executable".into(),
                retry(&["--node", "/ABSOLUTE/PATH/node"]),
            )],
        });
    };
    match node_major(&node) {
        Some(major) if major >= 20 => Ok(node),
        found => Err(Stop {
            state: "needs_input",
            error: format!(
                "{} is not Node.js 20 or newer{}",
                node.display(),
                found.map_or_else(String::new, |major| format!(" (it is version {major})"))
            ),
            instructions: vec![
                "The bridge needs Node.js 20 or newer.".into(),
                "Install a newer Node.js, or pass the absolute path of one with --node.".into(),
            ],
            next: vec![(
                "Run again naming a newer Node.js".into(),
                retry(&["--node", "/ABSOLUTE/PATH/node"]),
            )],
        }),
    }
}

fn package_commit(files: &[(&str, &[u8])]) -> Option<String> {
    let (_, manifest) = files.iter().find(|(name, _)| *name == "manifest.json")?;
    let manifest: serde_json::Value = serde_json::from_slice(manifest).ok()?;
    let commit = manifest.get("sourceCommit")?.as_str()?;
    (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| commit.to_string())
}

/// Write the package `files` to its versioned folder under `versions`, once.
///
/// The layout is the one `npm install --prefix <folder>` of the same archive
/// produces, so either way of installing a version yields the same paths.
fn unpack(versions: &Path, files: &[(&str, &[u8])]) -> Result<PathBuf, String> {
    let commit = package_commit(files).ok_or("the bridge package names no source commit")?;
    let version = versions.join(&commit);
    let package = version.join("node_modules").join("@sudocode").join("acp");
    if package.join("manifest.json").is_file() {
        return Ok(package);
    }
    if version.exists() {
        return Err(format!(
            "{} exists but holds no complete bridge package; remove that folder and run again",
            version.display()
        ));
    }
    fs::create_dir_all(versions).map_err(|e| format!("create {}: {e}", versions.display()))?;
    let staging = versions.join(format!(".unpack-{commit}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let staged = staging.join("node_modules").join("@sudocode").join("acp");
    let written = files.iter().try_for_each(|(name, bytes)| {
        let file = staged.join(name);
        fs::create_dir_all(file.parent().expect("a package file has a parent"))
            .and_then(|()| fs::write(&file, bytes))
            .map_err(|e| format!("write {}: {e}", file.display()))
    });
    // Rename as the last step: a version folder is either absent or complete.
    let installed = written.and_then(|()| {
        fs::rename(&staging, &version).or_else(|e| {
            // Another run finished the same version first.
            if package.join("manifest.json").is_file() {
                Ok(())
            } else {
                Err(format!("install {}: {e}", version.display()))
            }
        })
    });
    let _ = fs::remove_dir_all(&staging);
    installed.map(|()| package)
}

fn bridge_entry(args: &A2aSetupArgs, typed: &[String]) -> Result<PathBuf, Stop> {
    let package = if let Some(sdk) = &args.sdk {
        sdk.clone()
    } else if BUNDLED_SDK.is_empty() {
        return Err(Stop {
            state: "needs_input",
            error: "this scode build carries no bridge package".into(),
            instructions: vec![
                "Release builds of scode include the bridge package; this build was made without it.".into(),
                "Install the package (`@sudocode/acp`) into its own folder and pass that folder with --sdk, or use a release build of scode.".into(),
            ],
            next: vec![(
                "Run again naming the bridge package folder".into(),
                typed
                    .iter()
                    .cloned()
                    .chain(["--sdk".to_string(), "/ABSOLUTE/PATH/node_modules/@sudocode/acp".to_string()])
                    .collect(),
            )],
        });
    } else {
        let fail = |error: String| Stop {
            state: "failed",
            error,
            instructions: vec![
                "The bridge package built into scode could not be written to this user's folder."
                    .into(),
                "Fix the cause named in \"error\", then run the same command again.".into(),
            ],
            next: Vec::new(),
        };
        let home =
            home().ok_or_else(|| fail("no home folder is set (HOME or USERPROFILE)".into()))?;
        let versions = home
            .join(".local")
            .join("share")
            .join("sudocode")
            .join("acp")
            .join("versions");
        unpack(&versions, BUNDLED_SDK).map_err(fail)?
    };
    let entry = package.join("dist").join("server.cjs");
    if entry.is_file() && entry.is_absolute() {
        Ok(entry)
    } else {
        Err(Stop {
            state: "needs_input",
            error: format!("{} is not a bridge package", package.display()),
            instructions: vec![
                "--sdk must be the absolute folder of an installed `@sudocode/acp` package: the one that contains dist/server.cjs.".into(),
            ],
            next: Vec::new(),
        })
    }
}

/// Run setup for `args`; the returned value is the process exit code.
pub(crate) fn run_setup(args: &A2aSetupArgs, typed: &[String]) -> i32 {
    let (node, entry) =
        match find_node(args, typed).and_then(|node| Ok((node, bridge_entry(args, typed)?))) {
            Ok(found) => found,
            Err(stop) => {
                stop.print(args.frontend);
                return 1;
            }
        };
    // A copied or truncated package is refused before it registers anything.
    let verified = Command::new(&node).arg(&entry).arg("--verify").output();
    if !verified
        .as_ref()
        .is_ok_and(|output| output.status.success())
    {
        Stop {
            state: "failed",
            error: format!(
                "the bridge package at {} failed its integrity check: {}",
                entry.display(),
                verified.map_or_else(|e| e.to_string(), |o| String::from_utf8_lossy(&o.stderr).trim().to_string())
            ),
            instructions: vec![
                "The bridge package's files do not match its manifest, so nothing was set up.".into(),
                "Remove the package's version folder and run the same command again; scode writes a fresh copy.".into(),
            ],
            next: Vec::new(),
        }
        .print(args.frontend);
        return 1;
    }

    let mut command = Command::new(&node);
    command
        .arg(&entry)
        .arg(format!("--setup-{}", args.frontend.name()));
    let mut pass = |flag: &str, value: Option<String>| {
        if let Some(value) = value {
            command.arg(flag).arg(value);
        }
    };
    let text = |path: &Option<PathBuf>| {
        path.as_ref()
            .map(|path| path.to_string_lossy().into_owned())
    };
    pass("--issuer", args.issuer.clone());
    pass("--node-data-dir", text(&args.node_data_dir));
    pass("--nexus-enroll", text(&args.nexus_enroll));
    pass("--claude-command", text(&args.claude_command));
    pass("--config-root", text(&args.config_root));
    // The package looks for scode in the usual places. Only when it would find
    // none is this executable named, so a shared entry that gets repointed
    // stays the one the agents follow.
    let usual = home().is_some_and(|home| {
        home.join(".nexus")
            .join("bin")
            .join(if cfg!(windows) { "scode.exe" } else { "scode" })
            .exists()
    }) || on_path("scode").is_some();
    pass(
        "--scode",
        text(&args.scode).or_else(|| {
            (!usual)
                .then(|| {
                    std::env::current_exe()
                        .ok()
                        .map(|exe| exe.to_string_lossy().into_owned())
                })
                .flatten()
        }),
    );
    // What the user typed, so the package's "next" commands are ones they can run.
    pass(
        "--caller-argv",
        Some(serde_json::to_string(typed).expect("strings serialize")),
    );
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            Stop {
                state: "failed",
                error: format!("could not start {}: {error}", node.display()),
                instructions: vec!["Node.js could not be started; nothing was set up.".into()],
                next: Vec::new(),
            }
            .print(args.frontend);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_without_the_package_says_how_to_proceed() {
        if !BUNDLED_SDK.is_empty() {
            return;
        }
        let args = A2aSetupArgs {
            frontend: A2aFrontend::Claude,
            issuer: None,
            node_data_dir: None,
            nexus_enroll: None,
            scode: None,
            claude_command: None,
            config_root: None,
            node: None,
            sdk: None,
        };
        let typed = ["scode", "a2a", "setup", "claude"].map(String::from);
        let stop = bridge_entry(&args, &typed).expect_err("no package to use");
        assert_eq!(stop.state, "needs_input");
        assert_eq!(
            stop.next[0].1,
            [
                "scode",
                "a2a",
                "setup",
                "claude",
                "--sdk",
                "/ABSOLUTE/PATH/node_modules/@sudocode/acp"
            ]
        );
    }

    #[test]
    fn the_package_is_written_once_under_its_commit() {
        let versions =
            std::env::temp_dir().join(format!("scode-a2a-unpack-{}", std::process::id()));
        let _ = fs::remove_dir_all(&versions);
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let manifest = format!(r#"{{"sourceCommit":"{commit}","files":{{}}}}"#);
        let files: &[(&str, &[u8])] = &[
            ("dist/server.cjs", b"// entry"),
            ("package.json", b"{}"),
            ("manifest.json", manifest.as_bytes()),
        ];

        let package = unpack(&versions, files).expect("unpack");
        assert_eq!(
            package,
            versions
                .join(commit)
                .join("node_modules")
                .join("@sudocode")
                .join("acp"),
            "the layout an npm install of the archive produces"
        );
        assert_eq!(
            fs::read(package.join("dist").join("server.cjs")).unwrap(),
            b"// entry"
        );
        assert_eq!(
            fs::read_dir(&versions).unwrap().count(),
            1,
            "no staging folder is left behind"
        );

        // A later run reuses the folder and writes nothing.
        fs::write(package.join("dist").join("server.cjs"), b"// kept").unwrap();
        assert_eq!(unpack(&versions, files).expect("reuse"), package);
        assert_eq!(
            fs::read(package.join("dist").join("server.cjs")).unwrap(),
            b"// kept"
        );

        // A version folder without its manifest is never completed in place.
        fs::remove_file(package.join("manifest.json")).unwrap();
        let error = unpack(&versions, files).expect_err("incomplete folder");
        assert!(
            error.contains("remove that folder and run again"),
            "{error}"
        );

        fs::remove_dir_all(versions).unwrap();
    }

    #[test]
    fn a_package_without_a_commit_is_not_installed() {
        let versions =
            std::env::temp_dir().join(format!("scode-a2a-nocommit-{}", std::process::id()));
        let files: &[(&str, &[u8])] = &[("manifest.json", br#"{"sourceCommit":"main"}"#)];
        assert!(unpack(&versions, files).is_err());
        assert!(
            !versions.exists(),
            "nothing is created for a package it cannot place"
        );
    }

    #[test]
    fn a_folder_that_is_not_a_package_is_refused() {
        let directory =
            std::env::temp_dir().join(format!("scode-a2a-not-a-package-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let args = A2aSetupArgs {
            frontend: A2aFrontend::Claude,
            issuer: None,
            node_data_dir: None,
            nexus_enroll: None,
            scode: None,
            claude_command: None,
            config_root: None,
            node: None,
            sdk: Some(directory.clone()),
        };
        let stop = bridge_entry(&args, &[]).expect_err("not a package");
        assert_eq!(stop.state, "needs_input");
        assert!(
            stop.error.contains("is not a bridge package"),
            "{}",
            stop.error
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
