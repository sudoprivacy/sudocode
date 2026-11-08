use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn git_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(git_output(&["rev-parse", "--git-path", name])?);
    Some(if path.is_absolute() {
        path
    } else {
        env::current_dir().ok()?.join(path)
    })
}

fn git_inputs() -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    for name in ["HEAD", "packed-refs"] {
        if let Some(path) = git_path(name).filter(|path| path.exists()) {
            paths.insert(path);
        }
    }
    if let Some(reference) = git_output(&["symbolic-ref", "--quiet", "HEAD"]) {
        if let Some(mut path) = git_path(&reference) {
            // A packed/unborn branch has no loose ref yet. Watch its nearest
            // existing parent for creation, never a permanently missing path.
            while !path.exists() && path.pop() {}
            if path.exists() {
                paths.insert(path);
            }
        }
    }
    if let Some(root) = git_output(&["rev-parse", "--show-toplevel"]) {
        let pointer = Path::new(&root).join(".git");
        if pointer.is_file() {
            paths.insert(pointer);
        }
    }
    paths
}

fn main() {
    // Get git SHA (short hash)
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());

    println!("cargo:rustc-env=GIT_SHA={git_sha}");

    // TARGET is always set by Cargo during build
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET={target}");
    if target.ends_with("-apple-darwin") {
        // Intel's linker otherwise leaves the executable unsigned. Generate the
        // signature at link time so cold activation cannot stall before main.
        println!("cargo:rustc-link-arg-bin=scode=-Wl,-adhoc_codesign");
    }

    // Build date from SOURCE_DATE_EPOCH (reproducible builds) or current UTC date.
    // Intentionally ignoring time component to keep output deterministic within a day.
    let build_date = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .map(|_ts| {
            // Use SOURCE_DATE_EPOCH to derive date via chrono if available;
            // for simplicity we just use the env var as a signal and fall back
            // to build-time env. In practice CI sets this via workflow.
            std::env::var("BUILD_DATE").unwrap_or_else(|_| "unknown".to_string())
        })
        .or_else(|| std::env::var("BUILD_DATE").ok())
        .unwrap_or_else(|| {
            // Fall back to current date via `date` command
            Command::new("date")
                .args(["+%Y-%m-%d"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout).ok()
                    } else {
                        None
                    }
                })
                .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string())
        });
    println!("cargo:rustc-env=BUILD_DATE={build_date}");

    // Cargo executes here in the nested package, not the repository root.
    // Let Git resolve worktree/common-dir metadata and watch existing inputs.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=BUILD_DATE");
    for path in git_inputs() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
