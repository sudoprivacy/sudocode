//! The [`SkillRegistry`] holds skills discovered from the filesystem
//! and renders the prompt section that informs the model about them.

use std::path::{Path, PathBuf};

use super::discovery::discover_in_dir;

const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_RENDERED_CHARS: usize = 10_000;

/// A skill discovered on disk.
///
/// `body` holds the parsed markdown after the frontmatter; we keep it so
/// callers can decide whether to inline it into the prompt directly or
/// instruct the model to read `skill_md_path` when invoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub body: String,
    pub root: PathBuf,
    pub skill_md_path: PathBuf,
}

/// In-memory collection of discovered [`Skill`]s, keyed by name.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    /// Build a registry from a list of search paths.
    ///
    /// Later paths override earlier ones if names collide — this lets
    /// project-local skills shadow user-global ones.
    pub fn discover(search_paths: &[&Path]) -> std::io::Result<Self> {
        let mut by_name: std::collections::BTreeMap<String, Skill> =
            std::collections::BTreeMap::new();
        for path in search_paths {
            for skill in discover_in_dir(path)? {
                by_name.insert(skill.name.clone(), skill);
            }
        }
        let skills: Vec<Skill> = by_name.into_values().collect();
        Ok(Self { skills })
    }

    /// Discover skills in the default search paths:
    /// 1. `~/.scode/skills/`
    /// 2. `<cwd>/.scode/skills/` (overridable via
    ///    `SUDOCODE_WORKSPACE_SKILLS_DIR`)
    pub fn discover_default() -> std::io::Result<Self> {
        let mut paths: Vec<PathBuf> = Vec::new();
        if let Some(home) = home_dir() {
            paths.push(home.join(".scode").join("skills"));
        }
        let workspace = match std::env::var_os("SUDOCODE_WORKSPACE_SKILLS_DIR") {
            Some(value) => PathBuf::from(value),
            None => std::env::current_dir()?.join(".scode").join("skills"),
        };
        paths.push(workspace);
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        Self::discover(&refs)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    #[must_use]
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    /// Render the prompt section advertising available skills.
    ///
    /// Each line is capped at [`MAX_DESCRIPTION_CHARS`]; the total
    /// output is capped at [`MAX_RENDERED_CHARS`]. If the cap is hit,
    /// the LATER skills are dropped and a footer notes how many.
    #[must_use]
    pub fn render_for_prompt(&self) -> String {
        use std::fmt::Write as _;

        let header = "# Skills\n\nThe following skills are available. When the user's request matches a skill's description, read the SKILL.md and follow its instructions instead of using native tools for the same capability.\n\nAvailable skills:";
        let footer_template = "\n\nWhen the user types `/<skill-name>`, that is an explicit invocation. Load the skill's SKILL.md and execute according to its instructions.";

        let mut out = String::new();
        out.push_str(header);

        let mut dropped = 0usize;
        for (idx, skill) in self.skills.iter().enumerate() {
            let line = format!(
                "\n- {} ({}): {}",
                skill.name,
                truncate(&skill.description, MAX_DESCRIPTION_CHARS),
                skill.skill_md_path.display()
            );
            // Reserve room for the footer plus a possible dropped-note.
            let dropped_note_budget = 64;
            if out.len() + line.len() + footer_template.len() + dropped_note_budget
                > MAX_RENDERED_CHARS
            {
                dropped = self.skills.len() - idx;
                break;
            }
            out.push_str(&line);
        }

        if dropped > 0 {
            let plural = if dropped == 1 { "" } else { "s" };
            let _ = write!(
                &mut out,
                "\n- ... ({dropped} more skill{plural} omitted to fit prompt budget)"
            );
        }

        out.push_str(footer_template);
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = format!(
            "---\nname: {name}\ndescription: {description}\nkeywords: [{name}, test]\n---\n# {name}\nbody for {name}\n"
        );
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    fn unique_tmp(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("scode-skill-test-{prefix}-{pid}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_discover_finds_skills_in_user_dir() {
        let user = unique_tmp("user");
        write_skill(&user, "alpha", "first skill");
        write_skill(&user, "beta", "second skill");

        let reg = SkillRegistry::discover(&[user.as_path()]).unwrap();
        let names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "expected alpha in {names:?}");
        assert!(names.contains(&"beta"), "expected beta in {names:?}");
        assert_eq!(reg.list().len(), 2);

        let alpha = reg.get("alpha").expect("alpha missing");
        assert_eq!(alpha.description, "first skill");
        assert!(alpha.keywords.contains(&"alpha".to_string()));
        assert!(alpha.body.contains("body for alpha"));

        fs::remove_dir_all(&user).ok();
    }

    #[test]
    fn test_discover_project_overrides_user() {
        let user = unique_tmp("user-base");
        let project = unique_tmp("project-base");
        write_skill(&user, "shared", "from user");
        write_skill(&project, "shared", "from project");
        write_skill(&user, "user-only", "user owns this");

        let reg = SkillRegistry::discover(&[user.as_path(), project.as_path()]).unwrap();
        let shared = reg.get("shared").expect("shared missing");
        assert_eq!(
            shared.description, "from project",
            "project skill should override user skill"
        );
        // Sanity: the user-only skill survives.
        assert!(reg.get("user-only").is_some());

        fs::remove_dir_all(&user).ok();
        fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn test_discover_ignores_missing_paths() {
        let nonexistent = std::env::temp_dir().join("scode-definitely-not-a-dir-xyz-123-abc");
        let reg = SkillRegistry::discover(&[nonexistent.as_path()]).unwrap();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn test_discover_skips_dirs_without_skill_md() {
        let dir = unique_tmp("no-skill-md");
        fs::create_dir_all(dir.join("not-a-skill")).unwrap();
        fs::write(dir.join("not-a-skill").join("README.md"), "hi").unwrap();
        write_skill(&dir, "real-skill", "I am real");

        let reg = SkillRegistry::discover(&[dir.as_path()]).unwrap();
        let names: Vec<&str> = reg.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["real-skill"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_render_for_prompt_contains_skill_names() {
        let dir = unique_tmp("render");
        write_skill(&dir, "browser", "drive a browser");
        write_skill(&dir, "cron", "schedule things");

        let reg = SkillRegistry::discover(&[dir.as_path()]).unwrap();
        let rendered = reg.render_for_prompt();
        assert!(rendered.contains("# Skills"));
        assert!(rendered.contains("browser"));
        assert!(rendered.contains("cron"));
        assert!(rendered.contains("SKILL.md"));
        assert!(rendered.contains("/<skill-name>"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_render_for_prompt_truncates_long_description() {
        let skill = Skill {
            name: "verbose".into(),
            description: "x".repeat(500),
            keywords: vec![],
            allowed_tools: None,
            body: String::new(),
            root: PathBuf::from("/tmp"),
            skill_md_path: PathBuf::from("/tmp/SKILL.md"),
        };
        let reg = SkillRegistry {
            skills: vec![skill],
        };
        let out = reg.render_for_prompt();
        // The line with the long description should be capped to ≈200 chars
        // plus the surrounding format text, so the rendered string is well
        // under the 10K cap.
        assert!(out.len() < 1_000);
        assert!(out.contains('…'));
    }

    #[test]
    fn test_render_for_prompt_caps_total_output() {
        let mut skills = Vec::new();
        for i in 0..1000 {
            skills.push(Skill {
                name: format!("skill-{i:04}"),
                description: "padding ".repeat(20),
                keywords: vec![],
                allowed_tools: None,
                body: String::new(),
                root: PathBuf::from("/tmp"),
                skill_md_path: PathBuf::from(format!("/tmp/skill-{i:04}/SKILL.md")),
            });
        }
        let reg = SkillRegistry { skills };
        let out = reg.render_for_prompt();
        assert!(
            out.len() <= 10_000,
            "rendered length {} exceeds cap",
            out.len()
        );
        assert!(out.contains("omitted"));
    }

    #[test]
    fn test_get_returns_none_for_unknown() {
        let reg = SkillRegistry::default();
        assert!(reg.get("nope").is_none());
        assert!(reg.list().is_empty());
    }
}
