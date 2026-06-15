//! Filesystem walking + SKILL.md parsing.
//!
//! Each skill lives in its own directory containing a `SKILL.md` file
//! with YAML-ish frontmatter. We hand-roll the frontmatter parser
//! because the field set is tiny (string scalars + inline arrays) and
//! pulling in a full YAML crate just for this would be overkill.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::registry::Skill;

/// Parsed SKILL.md frontmatter values, separate from the markdown body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub fields: HashMap<String, FrontmatterValue>,
}

/// A single frontmatter value: scalar string or inline string array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
}

impl FrontmatterValue {
    #[must_use]
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            Self::Scalar(s) => Some(s.as_str()),
            Self::List(_) => None,
        }
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Self::List(items) => Some(items.as_slice()),
            Self::Scalar(_) => None,
        }
    }
}

/// Errors from the frontmatter parser.
#[derive(Debug)]
pub enum FrontmatterError {
    /// The document has no `---` fence pair at the top.
    Missing,
    /// A line in the frontmatter wasn't `key: value`.
    Malformed(String),
}

impl std::fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "missing frontmatter"),
            Self::Malformed(line) => write!(f, "malformed frontmatter line: {line}"),
        }
    }
}

impl std::error::Error for FrontmatterError {}

/// Parse a SKILL.md document into (frontmatter, body).
///
/// Returns `Ok(None)` if the document has no frontmatter block.
/// Returns `Err` if the frontmatter is present but malformed.
pub fn parse_frontmatter(content: &str) -> Result<Option<(Frontmatter, String)>, FrontmatterError> {
    let mut lines = content.lines();

    let Some(first) = lines.next() else {
        return Ok(None);
    };
    if first.trim() != "---" {
        return Ok(None);
    }

    let mut fields: HashMap<String, FrontmatterValue> = HashMap::new();
    let mut closed = false;
    let mut body_start_offset = first.len() + 1; // include the trailing newline

    for line in lines {
        body_start_offset += line.len() + 1;
        if line.trim() == "---" {
            closed = true;
            break;
        }
        // Blank lines and `#` comments are tolerated inside frontmatter.
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(FrontmatterError::Malformed(line.to_string()));
        };
        let key = key.trim().to_string();
        if key.is_empty() {
            return Err(FrontmatterError::Malformed(line.to_string()));
        }
        let value = value.trim();
        let parsed = parse_value(value);
        fields.insert(key, parsed);
    }

    if !closed {
        return Err(FrontmatterError::Malformed(
            "unterminated frontmatter block".to_string(),
        ));
    }

    // The body is everything after the closing fence. Trim a single leading
    // newline so the body doesn't start with a blank line artefact.
    let body = if body_start_offset >= content.len() {
        String::new()
    } else {
        let rest = &content[body_start_offset..];
        rest.strip_prefix('\n').unwrap_or(rest).to_string()
    };

    Ok(Some((Frontmatter { fields }, body)))
}

fn parse_value(raw: &str) -> FrontmatterValue {
    // Inline array: `[a, b, c]` (possibly quoted entries).
    if raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        let items: Vec<String> = inner
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(strip_quotes)
            .collect();
        return FrontmatterValue::List(items);
    }
    FrontmatterValue::Scalar(strip_quotes(raw))
}

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse a full SKILL.md file (frontmatter + body) into a [`Skill`].
///
/// `name` is taken from the frontmatter; the caller is responsible for
/// supplying the on-disk directory + file paths.
pub fn parse_skill_md(
    content: &str,
    root: PathBuf,
    skill_md_path: PathBuf,
) -> Result<Skill, FrontmatterError> {
    let (frontmatter, body) = parse_frontmatter(content)?.ok_or(FrontmatterError::Missing)?;

    let name = frontmatter
        .fields
        .get("name")
        .and_then(FrontmatterValue::as_scalar)
        .map(str::to_string)
        .ok_or_else(|| FrontmatterError::Malformed("missing `name` field".to_string()))?;

    let description = frontmatter
        .fields
        .get("description")
        .and_then(FrontmatterValue::as_scalar)
        .unwrap_or("")
        .to_string();

    let keywords = frontmatter
        .fields
        .get("keywords")
        .and_then(FrontmatterValue::as_list)
        .map(|items| items.iter().map(String::from).collect())
        .unwrap_or_default();

    let allowed_tools = frontmatter
        .fields
        .get("allowed_tools")
        .and_then(FrontmatterValue::as_list)
        .map(|items| items.iter().map(String::from).collect());

    Ok(Skill {
        name,
        description,
        keywords,
        allowed_tools,
        body,
        root,
        skill_md_path,
    })
}

/// Walk `dir` looking for immediate subdirectories that contain a
/// `SKILL.md`. Returns the parsed [`Skill`] for each one.
///
/// Non-existent directories are silently ignored — discovery is a
/// best-effort scan, not a hard requirement.
pub fn discover_in_dir(dir: &Path) -> std::io::Result<Vec<Skill>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(err) => return Err(err),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let content = fs::read_to_string(&skill_md)?;
        match parse_skill_md(&content, path.clone(), skill_md.clone()) {
            Ok(skill) => found.push(skill),
            Err(err) => {
                // A malformed SKILL.md should not poison the whole scan;
                // skip it and let the user notice via missing entry.
                eprintln!("warning: skipping {} — {}", skill_md.display(), err);
            }
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frontmatter_parser_basic() {
        let doc = "---\nname: foo\ndescription: A test skill\n---\n# Body\nhello\n";
        let (fm, body) = parse_frontmatter(doc)
            .unwrap()
            .expect("frontmatter present");
        assert_eq!(fm.fields.get("name").unwrap().as_scalar(), Some("foo"));
        assert_eq!(
            fm.fields.get("description").unwrap().as_scalar(),
            Some("A test skill")
        );
        assert!(body.starts_with("# Body"));
        assert!(body.contains("hello"));
    }

    #[test]
    fn test_frontmatter_parser_with_list() {
        let doc =
            "---\nname: tools\nkeywords: [alpha, beta, gamma]\nallowed_tools: [Read, Write]\n---\n";
        let (fm, _body) = parse_frontmatter(doc).unwrap().unwrap();
        let kws = fm.fields.get("keywords").unwrap().as_list().unwrap();
        assert_eq!(kws, &["alpha", "beta", "gamma"]);
        let tools = fm.fields.get("allowed_tools").unwrap().as_list().unwrap();
        assert_eq!(tools, &["Read", "Write"]);
    }

    #[test]
    fn test_frontmatter_parser_no_frontmatter() {
        let doc = "# Just markdown\n\nNo frontmatter here.\n";
        assert!(parse_frontmatter(doc).unwrap().is_none());
    }

    #[test]
    fn test_frontmatter_parser_quoted_values() {
        let doc = "---\nname: \"foo-bar\"\ndescription: 'quoted desc'\n---\n";
        let (fm, _) = parse_frontmatter(doc).unwrap().unwrap();
        assert_eq!(fm.fields.get("name").unwrap().as_scalar(), Some("foo-bar"));
        assert_eq!(
            fm.fields.get("description").unwrap().as_scalar(),
            Some("quoted desc")
        );
    }

    #[test]
    fn test_frontmatter_parser_unterminated() {
        let doc = "---\nname: oops\nstill going\n";
        assert!(parse_frontmatter(doc).is_err());
    }

    #[test]
    fn test_parse_skill_md_missing_name() {
        let doc = "---\ndescription: no name\n---\nbody";
        let err =
            parse_skill_md(doc, PathBuf::from("/a"), PathBuf::from("/a/SKILL.md")).unwrap_err();
        match err {
            FrontmatterError::Malformed(msg) => assert!(msg.contains("name")),
            FrontmatterError::Missing => panic!("expected Malformed, got Missing"),
        }
    }
}
