//! Slash-command parsing for explicit skill invocation.
//!
//! When the user starts a message with `/<skill-name>`, we treat it as
//! an explicit invocation request and extract `(skill_name, args)`.
//! Skill names follow the kebab-case convention: only `[a-z0-9-]` are
//! valid characters, and the name cannot be empty.

/// Parsed slash-command from user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub skill_name: String,
    pub args: String,
}

/// Parse `input` as a slash-command.
///
/// Returns `Some(SlashCommand)` when the input starts with `/` followed
/// by a non-empty kebab-case identifier. Returns `None` otherwise.
#[must_use]
pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let stripped = input.strip_prefix('/')?;
    if stripped.is_empty() {
        return None;
    }
    // Split into (name, args) on first whitespace.
    let (name, rest) = match stripped.find(char::is_whitespace) {
        Some(idx) => (&stripped[..idx], stripped[idx..].trim_start()),
        None => (stripped, ""),
    };
    if !is_valid_skill_name(name) {
        return None;
    }
    Some(SlashCommand {
        skill_name: name.to_string(),
        args: rest.to_string(),
    })
}

fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // Disallow leading/trailing hyphen for cleanliness; nothing in the
    // task spec demands it, but it avoids quirky names like `-foo` and
    // `foo-`.
    if name.starts_with('-') || name.ends_with('-') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_slash_command_valid() {
        let cmd = parse_slash_command("/browser open google.com").unwrap();
        assert_eq!(cmd.skill_name, "browser");
        assert_eq!(cmd.args, "open google.com");
    }

    #[test]
    fn test_parse_slash_command_no_args() {
        let cmd = parse_slash_command("/cron").unwrap();
        assert_eq!(cmd.skill_name, "cron");
        assert_eq!(cmd.args, "");
    }

    #[test]
    fn test_parse_slash_command_kebab_name() {
        let cmd = parse_slash_command("/git-clean-reset --force").unwrap();
        assert_eq!(cmd.skill_name, "git-clean-reset");
        assert_eq!(cmd.args, "--force");
    }

    #[test]
    fn test_parse_slash_command_with_digits() {
        let cmd = parse_slash_command("/skill-42 do thing").unwrap();
        assert_eq!(cmd.skill_name, "skill-42");
    }

    #[test]
    fn test_parse_slash_command_invalid_chars() {
        assert!(parse_slash_command("/Foo").is_none());
        assert!(parse_slash_command("/foo_bar").is_none());
        assert!(parse_slash_command("/foo.bar").is_none());
        assert!(parse_slash_command("/FOO BAR").is_none());
    }

    #[test]
    fn test_parse_slash_command_no_slash() {
        assert!(parse_slash_command("hello world").is_none());
        assert!(parse_slash_command("").is_none());
        assert!(parse_slash_command(" /foo").is_none());
    }

    #[test]
    fn test_parse_slash_command_just_slash() {
        assert!(parse_slash_command("/").is_none());
    }

    #[test]
    fn test_parse_slash_command_leading_or_trailing_hyphen() {
        assert!(parse_slash_command("/-foo").is_none());
        assert!(parse_slash_command("/foo-").is_none());
    }

    #[test]
    fn test_parse_slash_command_tab_separated_args() {
        let cmd = parse_slash_command("/foo\targ1 arg2").unwrap();
        assert_eq!(cmd.skill_name, "foo");
        assert_eq!(cmd.args, "arg1 arg2");
    }
}
