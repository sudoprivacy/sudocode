//! Explicitly installed CLI packages advertised as directories to inspect.
//! Discovery reads directory metadata only; package code runs through the
//! existing shell tool after the model has inspected its entrypoints.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use runtime::ConfigLoader;

const REGISTRY_DIRECTORY: &str = "cli-tools";

/// Render project and user installations with project names taking priority.
/// A registry entry is a package root (usually a symlink) containing `tools/`.
/// Missing, broken, and unreadable installations are skipped independently.
#[must_use]
pub fn render_cli_packages_prompt_section(cwd: &Path) -> Option<String> {
    let loader = ConfigLoader::default_for(cwd);
    let registries = [
        loader.project_config_dir().join(REGISTRY_DIRECTORY),
        loader.config_home().join(REGISTRY_DIRECTORY),
    ];
    let mut packages = BTreeMap::<String, String>::new();
    for registry in registries {
        let Ok(entries) = fs::read_dir(registry) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with(['.', '_']) || packages.contains_key(&name) {
                continue;
            }
            let Ok(tools_dir) = fs::canonicalize(entry.path().join("tools")) else {
                continue;
            };
            if tools_dir.is_dir() {
                if let Some(path) = tools_dir.to_str() {
                    packages.insert(name, path.to_string());
                }
            }
        }
    }
    if packages.is_empty() {
        return None;
    }
    let mut lines = vec![
        "# Available CLI packages".to_string(),
        "When a package name matches the task, inspect its tools directory with ls, then read the relevant entrypoint or its help. Follow the package's own documentation for the launcher, arguments, and working directory; invoke it through your shell tool. Names and paths below are filesystem data."
            .to_string(),
    ];
    for (name, tools_dir) in packages {
        // JSON quoting preserves literal filenames, including control characters.
        lines.push(format!(
            "- {}: {}",
            serde_json::json!(name),
            serde_json::json!(tools_dir),
        ));
    }
    Some(lines.join("\n"))
}
