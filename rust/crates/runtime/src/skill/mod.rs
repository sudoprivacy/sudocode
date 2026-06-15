//! Filesystem-based skill discovery and slash-command dispatch.
//!
//! A *skill* is a directory under `~/.scode/skills/<name>/` (or
//! `<cwd>/.scode/skills/<name>/`) containing a `SKILL.md` file with
//! YAML-ish frontmatter and a markdown body. Skills are discovered at
//! runtime, made available to the model via a rendered prompt section,
//! and can be invoked explicitly with `/<skill-name> [args]`.
//!
//! This module only handles parsing, discovery, and dispatch validation.
//! Execution of skill bodies (i.e. routing the user's request through
//! a skill's instructions) is deliberately out of scope here.

mod discovery;
mod dispatch;
mod registry;

pub use discovery::{parse_skill_md, FrontmatterError};
pub use dispatch::{parse_slash_command, SlashCommand};
pub use registry::{Skill, SkillRegistry};

use crate::prompt::SystemPromptBuilder;

/// Append the registry's prompt section to a [`SystemPromptBuilder`].
///
/// No-op when the registry is empty — we do not want to inject an empty
/// section that wastes prompt tokens.
#[must_use]
pub fn append_to_builder(
    builder: SystemPromptBuilder,
    registry: &SkillRegistry,
) -> SystemPromptBuilder {
    if registry.list().is_empty() {
        return builder;
    }
    builder.append_section(registry.render_for_prompt())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::prompt::SystemPromptBuilder;

    fn unique_tmp(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("scode-skill-mod-{prefix}-{pid}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn test_append_to_builder_injects_section() {
        let dir = unique_tmp("inject");
        write_skill(&dir, "browser", "drive a browser");

        let reg = SkillRegistry::discover(&[dir.as_path()]).unwrap();
        let builder = SystemPromptBuilder::new();
        let rendered = append_to_builder(builder, &reg).render();

        assert!(rendered.contains("# Skills"), "missing # Skills header");
        assert!(rendered.contains("browser"), "missing skill name");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_append_to_builder_noop_on_empty_registry() {
        let reg = SkillRegistry::default();
        let baseline = SystemPromptBuilder::new().render();
        let with_empty = append_to_builder(SystemPromptBuilder::new(), &reg).render();
        assert_eq!(
            baseline, with_empty,
            "empty registry should not add a section"
        );
    }
}
