//! Scheduling classification, shared by every tool executor.
//!
//! Concurrency is independent of permission: an approved writer still needs a
//! serial barrier. Unknown commands and syntax remain executable, but serial.

mod commands;
mod flags;

use serde_json::Value;
use tree_sitter::{Node, Parser};

/// Built-in calls whose executions may overlap. Dynamic tools supply their
/// own capability through `ToolExecutor::is_concurrency_safe`.
#[must_use]
pub fn builtin_is_concurrency_safe(name: &str, input: &str) -> bool {
    let Ok(input) = serde_json::from_str::<Value>(input) else {
        return false;
    };
    match crate::tool_names::canonicalize_tool_name(name).as_str() {
        "bash" => input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(bash_is_read_only),
        "read_file" | "read_tool_output" | "glob_search" | "grep_search" | "ToolSearch"
        | "WebFetch" | "WebSearch" | "pid_status" | "pid_output" | "agent_list" | "agent_spawn"
        | "pid_fork" | "CronList" | "AskUserQuestion" => true,
        _ => false,
    }
}

/// Recognize literal read-only shell commands, including pipes and lists.
/// A real Bash syntax tree prevents quoted operators from being mistaken for
/// shell structure. Expansions, executable substitutions, writes and unknown
/// syntax are serial; classification never evaluates a shell expression.
#[must_use]
pub fn bash_is_read_only(command: &str) -> bool {
    if command.is_empty() || command.len() > 64 * 1024 {
        return false;
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(command, None) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    let mut commands = Vec::new();
    if !collect_commands(root, command, &mut commands) || commands.is_empty() {
        return false;
    }
    // Match CC's conservative handling of cwd changes around Git. Each Bash
    // invocation already owns its process cwd; this rule also avoids treating
    // a different repository's command configuration as known read-only input.
    let has_cd = commands.iter().any(|args| args[0] == "cd");
    let has_git = commands.iter().any(|args| args[0] == "git");
    !(has_cd && has_git) && commands.iter().all(|args| command_is_read_only(args))
}

fn collect_commands(node: Node<'_>, source: &str, commands: &mut Vec<Vec<String>>) -> bool {
    match node.kind() {
        "program" | "list" | "pipeline" | "redirected_statement" => {
            let mut cursor = node.walk();
            let valid = node.children(&mut cursor).all(|child| {
                if child.is_named() {
                    collect_commands(child, source, commands)
                } else {
                    // Background shell jobs outlive this invocation's barrier.
                    matches!(child.kind(), ";" | "\n" | "&&" | "||" | "|" | "|&")
                }
            });
            valid
        }
        "command" => {
            let mut args = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "file_redirect" {
                    if !read_only_redirect(child, source) {
                        return false;
                    }
                } else if let Some(word) = literal_word(child, source) {
                    args.push(word);
                } else {
                    return false;
                }
            }
            if args.is_empty() {
                return false;
            }
            if has_glob(node, source) && !commands::accepts_globs(&args[0]) {
                return false;
            }
            commands.push(args);
            true
        }
        "file_redirect" => read_only_redirect(node, source),
        "comment" => true,
        _ => false,
    }
}

fn literal_word(node: Node<'_>, source: &str) -> Option<String> {
    if !literal_syntax(node, source) {
        return None;
    }
    let words = shell_words::split(&source[node.byte_range()]).ok()?;
    (words.len() == 1).then(|| words[0].clone())
}

fn literal_syntax(node: Node<'_>, source: &str) -> bool {
    // Reject expansion recursively, including within double quotes and joined
    // words. Single-quoted text remains literal, as it is to Bash itself.
    match node.kind() {
        "raw_string" => {}
        "command_name" | "string" | "concatenation" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !literal_syntax(child, source) {
                    return false;
                }
            }
        }
        "word" | "string_content" | "number" => {
            if node.kind() == "word" && source[node.byte_range()].contains(['{', '}', '~']) {
                return false;
            }
        }
        _ => return false,
    }
    true
}

fn has_glob(node: Node<'_>, source: &str) -> bool {
    if matches!(node.kind(), "raw_string" | "string") {
        return false;
    }
    if node.kind() == "word" && source[node.byte_range()].contains(['*', '?', '[', ']']) {
        return true;
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .any(|child| has_glob(child, source));
    found
}

fn read_only_redirect(node: Node<'_>, source: &str) -> bool {
    let raw = source[node.byte_range()].trim();
    if let Some(path) = raw
        .strip_prefix('<')
        .filter(|p| !p.starts_with(['<', '>', '&']))
    {
        let Ok(words) = shell_words::split(path.trim()) else {
            return false;
        };
        if words.len() != 1
            || words[0].starts_with("/dev/tcp/")
            || words[0].starts_with("/dev/udp/")
        {
            return false;
        }
        let mut cursor = node.walk();
        return node
            .named_children(&mut cursor)
            .all(|child| literal_syntax(child, source));
    }
    let text: String = source[node.byte_range()]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    // Descriptor routing and the null device do not mutate a shared file.
    matches!(
        text.as_str(),
        "2>&1" | "1>&2" | ">&2" | ">/dev/null" | "1>/dev/null" | "2>/dev/null"
    )
}

fn command_is_read_only(args: &[String]) -> bool {
    let command = args[0].as_str();
    let args = &args[1..];
    match command {
        "cat" | "head" | "tail" | "ls" | "pwd" | "wc" | "cut" | "tr" | "comm" | "basename"
        | "dirname" | "readlink" | "realpath" | "stat" | "du" | "df" | "uname" | "whoami"
        | "id" | "true" | "false" | "echo" | "sleep" | "cd" | "grep" => true,
        "printf" => !args.iter().any(|arg| arg.starts_with("-v")),
        "sort" => !args.iter().any(|arg| {
            let flag = arg.split('=').next().unwrap_or(arg);
            (flag.starts_with("--")
                && ("--output".starts_with(flag) || "--compress-program".starts_with(flag)))
                || (flag.starts_with('-') && !flag.starts_with("--") && flag.contains('o'))
        }),
        "rg" => !args
            .iter()
            .any(|arg| arg.starts_with("--pre") || arg.starts_with("--hostname-bin")),
        "find" => !args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        // sed has executable and file-writing expressions. The common line
        // preview is recognized explicitly; arbitrary scripts remain serial.
        "sed" => {
            args.first().is_some_and(|arg| arg == "-n")
                && args.get(1).is_some_and(|script| {
                    script.strip_suffix('p').is_some_and(|range| {
                        !range.is_empty()
                            && range
                                .chars()
                                .all(|c| c.is_ascii_digit() || matches!(c, ',' | '$'))
                    })
                })
                && args.iter().skip(2).all(|arg| !arg.starts_with('-'))
        }
        "git" => git_is_read_only(args),
        "fd" | "fdfind" => commands::fd(args),
        "diff" => commands::diff(args),
        "gh" => commands::gh(args),
        _ => false,
    }
}

fn git_is_read_only(args: &[String]) -> bool {
    let Some((subcommand, args)) = args.split_first() else {
        return false;
    };
    if subcommand == "branch" {
        return commands::branch(args);
    }
    // No -c/-C, external helpers, arbitrary aliases, output files or mutation
    // subcommands. A permission grant does not change this scheduling rule.
    matches!(
        subcommand.as_str(),
        "status"
            | "diff"
            | "log"
            | "show"
            | "blame"
            | "ls-files"
            | "ls-tree"
            | "rev-parse"
            | "describe"
            | "shortlog"
    ) && !args.iter().any(|arg| {
        let flag = arg.split('=').next().unwrap_or(arg);
        flag.starts_with("--")
            && flag != "--"
            && [
                "--output",
                "--ext-diff",
                "--textconv",
                "--open-files-in-pager",
                "--exec",
            ]
            .iter()
            .any(|unsafe_flag| unsafe_flag.starts_with(flag))
            || (flag.starts_with('-') && !flag.starts_with("--") && flag.contains('o'))
    })
}
