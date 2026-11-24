use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
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

/// Embed the A2A bridge package when the build names a built one.
///
/// `SCODE_A2A_SDK_DIR` is the `sdk/typescript` folder after `npm run build`, or
/// an installed `@sudocode/acp` package. Without it nothing is embedded, so an
/// ordinary `cargo build` needs no Node.js; `scode a2a setup` then asks for
/// `--sdk`.
fn bundle_a2a_sdk() {
    println!("cargo:rerun-if-env-changed=SCODE_A2A_SDK_DIR");
    let mut source = String::from("pub(crate) static BUNDLED_SDK: &[(&str, &[u8])] = &[\n");
    if let Some(directory) = env::var_os("SCODE_A2A_SDK_DIR").filter(|value| !value.is_empty()) {
        let directory = PathBuf::from(directory);
        assert!(
            directory.is_absolute(),
            "SCODE_A2A_SDK_DIR must be an absolute path"
        );
        let manifest_path = directory.join("manifest.json");
        println!("cargo:rerun-if-changed={}", manifest_path.display());
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&manifest_path)
                .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display())),
        )
        .unwrap_or_else(|e| panic!("parse {}: {e}", manifest_path.display()));
        // The package installs under its source commit, so one commit must
        // always mean the same bytes.
        assert!(
            manifest.get("dirty").and_then(serde_json::Value::as_bool) != Some(true),
            "SCODE_A2A_SDK_DIR was built from uncommitted sources; build it from a clean commit"
        );
        let files = manifest
            .get("files")
            .and_then(serde_json::Value::as_object)
            .expect("the bridge package manifest lists its files");
        for name in files.keys().map(String::as_str).chain(["manifest.json"]) {
            assert!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
                    && name
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != ".."),
                "unexpected file name in the bridge package manifest: {name}"
            );
            let file = directory.join(name);
            assert!(
                file.is_file(),
                "{} is listed in the manifest but missing",
                file.display()
            );
            println!("cargo:rerun-if-changed={}", file.display());
            // Debug formatting writes both as Rust string literals, which
            // escapes the backslashes of a Windows path.
            let path = file
                .to_str()
                .expect("the bridge package path is valid UTF-8");
            writeln!(source, "    ({name:?}, include_bytes!({path:?})),")
                .expect("writing to a String cannot fail");
        }
    }
    source.push_str("];\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("a2a_sdk.rs");
    std::fs::write(&out, source).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
}

fn main() {
    bundle_a2a_sdk();
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

    // Set by e2e/vault-live/run-native.sh to build the live Vault acceptance off Linux.
    println!("cargo::rustc-check-cfg=cfg(vault_live)");

    // Cargo executes here in the nested package, not the repository root.
    // Let Git resolve worktree/common-dir metadata and watch existing inputs.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=BUILD_DATE");
    for path in git_inputs() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
