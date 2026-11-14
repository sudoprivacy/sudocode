//! Tab completion for slash-command lines, names and arguments.
//!
//! Argument choices come from each command's `argument_hint`, the same text
//! `/help` prints, so a command's completion cannot drift from its usage.
//! A hint is a sequence of slots: a bare word is a literal (`--confirm`),
//! `<name>` or `[name]` is a placeholder, and `[a|b <x>|c]` is a group of
//! alternatives, each its own sequence. A lone bracketed word is a placeholder
//! unless it is a flag (`[--confirm]`) or `[all]`, the one literal choice
//! hints spell that way. Placeholders that name runtime values
//! (`model`, `session-id`, `path`, …) draw from a [`CompletionContext`] the
//! REPL refreshes between turns, so a key press never waits on the network.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{find_slash_command_spec, suggest_slash_commands};

/// The most candidates one Tab offers.
const LIMIT: usize = 50;

/// Lists values on demand, when a completion actually needs them.
pub type ValueList = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Runtime values a REPL can offer for slash-command arguments.
#[derive(Clone, Default)]
pub struct CompletionContext {
    /// Models `/model` can switch to, current first.
    pub models: Vec<String>,
    /// Session ids `/resume` and `/session` accept, most recent first. Listed
    /// only when a session argument is completed: reading the session store
    /// parses transcripts, which no other key press should pay for.
    pub sessions: Option<ValueList>,
    /// Directory relative paths resolve against.
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    Literal(String),
    Placeholder(String),
    Group(Vec<Vec<Slot>>),
}

/// One completion: the replacement for the word under the cursor, and whether
/// the hint has more slots after it (so a space should follow).
struct Choice {
    word: String,
    more: bool,
}

/// Complete a REPL input line that starts with `/`.
///
/// Returns whole replacement lines, best first. With no space yet the command
/// name is completed (fuzzy, as typo suggestions are); after it, the argument
/// under the cursor is completed from the command's hint and `context`.
#[must_use]
pub fn complete_slash_input(line: &str, context: &CompletionContext) -> Vec<String> {
    if !line.starts_with('/') {
        return Vec::new();
    }
    let Some((command, rest)) = line.split_once(char::is_whitespace) else {
        return suggest_slash_commands(line, 10);
    };
    let Some(spec) = find_slash_command_spec(command.trim_start_matches('/')) else {
        return Vec::new();
    };
    let Some(hint) = spec.argument_hint else {
        return Vec::new();
    };
    let completed: Vec<&str> = rest.split_whitespace().collect();
    let (completed, partial) = if rest.ends_with(char::is_whitespace) || completed.is_empty() {
        (completed.as_slice(), "")
    } else {
        let Some((last, before)) = completed.split_last() else {
            return Vec::new();
        };
        (before, *last)
    };
    let stem = &line[..line.len() - partial.len()];

    let mut lines = Vec::new();
    for choice in next_choices(&parse_sequence(hint), completed) {
        let words = match choice {
            Next::Literal(choice) => {
                if choice
                    .word
                    .to_ascii_lowercase()
                    .starts_with(&partial.to_ascii_lowercase())
                {
                    vec![choice]
                } else {
                    Vec::new()
                }
            }
            Next::Placeholder(name, more) => placeholder_values(&name, partial, context)
                .into_iter()
                .map(|word| Choice {
                    more: more && !word.ends_with('/'),
                    word,
                })
                .collect(),
        };
        for choice in words {
            let candidate = format!(
                "{stem}{}{}",
                choice.word,
                if choice.more { " " } else { "" }
            );
            if candidate != line && !lines.contains(&candidate) {
                lines.push(candidate);
            }
        }
    }
    lines.truncate(LIMIT);
    lines
}

enum Next {
    Literal(Choice),
    Placeholder(String, bool),
}

/// What may come after `args` in `sequence`.
fn next_choices(sequence: &[Slot], args: &[&str]) -> Vec<Next> {
    let Some((slot, tail)) = sequence.split_first() else {
        return Vec::new();
    };
    match slot {
        Slot::Group(alternatives) => alternatives
            .iter()
            .flat_map(|alternative| {
                let joined: Vec<Slot> = alternative.iter().chain(tail).cloned().collect();
                next_choices(&joined, args)
            })
            .collect(),
        Slot::Literal(word) => match args.split_first() {
            None => vec![Next::Literal(Choice {
                word: word.clone(),
                more: !tail.is_empty(),
            })],
            Some((arg, rest)) if arg.eq_ignore_ascii_case(word) => next_choices(tail, rest),
            Some(_) => Vec::new(),
        },
        Slot::Placeholder(name) => match args.split_first() {
            None => vec![Next::Placeholder(name.clone(), !tail.is_empty())],
            // A variadic placeholder (`path...`) keeps accepting values.
            Some((_, rest)) if name.ends_with("...") => next_choices(sequence, rest),
            Some((_, rest)) => next_choices(tail, rest),
        },
    }
}

fn placeholder_values(name: &str, partial: &str, context: &CompletionContext) -> Vec<String> {
    let starts = |value: &&String| value.starts_with(partial);
    match name.trim_end_matches("...") {
        "model" => context.models.iter().filter(starts).cloned().collect(),
        "session-id" | "session-path" => context
            .sessions
            .as_ref()
            .map_or_default(|list| list())
            .into_iter()
            .filter(|value| value.starts_with(partial))
            .collect(),
        "path" | "file" => path_values(partial, &context.cwd),
        _ => Vec::new(),
    }
}

/// Entries of the directory `partial` names, relative to `cwd`; directories end
/// in `/`. Hidden entries appear only once the typed name starts with `.`.
fn path_values(partial: &str, cwd: &Path) -> Vec<String> {
    let (directory, prefix) = match partial.rfind('/') {
        Some(index) => (&partial[..=index], &partial[index + 1..]),
        None => ("", partial),
    };
    let expanded = if let Some(home) = directory.strip_prefix("~/") {
        std::env::var_os("HOME").map(|value| PathBuf::from(value).join(home))
    } else {
        Some(cwd.join(directory))
    };
    let Some(Ok(entries)) = expanded.map(std::fs::read_dir) else {
        return Vec::new();
    };
    let mut values: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let slash = if entry.file_type().ok()?.is_dir() {
                "/"
            } else {
                ""
            };
            Some(format!("{directory}{name}{slash}"))
        })
        .collect();
    values.sort();
    values
}

/// Parse a hint into its top-level slots.
fn parse_sequence(hint: &str) -> Vec<Slot> {
    split_top_level(hint, char::is_whitespace)
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(parse_slot)
        .collect()
}

fn parse_slot(part: &str) -> Slot {
    if let Some(inner) = part.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        return Slot::Placeholder(inner.to_string());
    }
    if let Some(inner) = part.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let alternatives: Vec<Vec<Slot>> = split_top_level(inner, |ch| ch == '|')
            .into_iter()
            .map(parse_sequence)
            .collect();
        // `[name]` is an optional placeholder, not the literal word `name`.
        if let [single] = alternatives.as_slice() {
            if let [Slot::Literal(word)] = single.as_slice() {
                if !word.starts_with('-') && word != "all" {
                    return Slot::Placeholder(word.clone());
                }
            }
        }
        return Slot::Group(alternatives);
    }
    Slot::Literal(part.to_string())
}

/// Split at `separator` characters outside brackets.
fn split_top_level(text: &str, separator: impl Fn(char) -> bool) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    for (index, ch) in text.char_indices() {
        match ch {
            '[' | '<' => depth += 1,
            ']' | '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 && separator(ch) => {
                parts.push(&text[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}
