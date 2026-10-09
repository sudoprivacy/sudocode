//! Resolve a working Python 3 interpreter for tests that drive Python helpers.
//! Probe the interpreter rather than trusting Windows Store execution aliases.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub fn resolve_python() -> String {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED.get_or_init(resolve_python_uncached).clone()
}

fn resolve_python_uncached() -> String {
    #[cfg(windows)]
    let candidates: &[&str] = &["python", "python3"];
    #[cfg(not(windows))]
    let candidates: &[&str] = &["python3", "python"];

    for candidate in candidates {
        if let Some(path) = python3_path(candidate, &[]) {
            return path;
        }
    }

    #[cfg(windows)]
    if let Some(path) = python3_path("py", &["-3"]) {
        return path;
    }

    panic!("no working Python 3 interpreter found (tried {candidates:?} and the `py` launcher)");
}

fn python3_path(command: &str, prefix: &[&str]) -> Option<String> {
    let output = Command::new(command)
        .args(prefix)
        .args([
            "-c",
            "import sys; sys.exit(1) if sys.version_info[0] != 3 else print(sys.executable)",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if !std::path::Path::new(&path).is_absolute() {
        return None;
    }
    Command::new(&path)
        .args([
            "-c",
            "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        .then_some(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolved_interpreter_is_absolute_and_runs_python3() {
        let path = super::resolve_python();
        assert!(std::path::Path::new(&path).is_absolute());
        assert_eq!(super::python3_path(&path, &[]), Some(path));
    }
}
