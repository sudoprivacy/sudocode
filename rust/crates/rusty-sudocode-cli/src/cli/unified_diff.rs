//! Recognize unified patches in visible tool output without changing its text.

use std::borrow::Cow;

use crate::render::{diff_colors, theme, ColorSupport, RESET};

/// Only complete file-header pairs followed by a valid hunk activate diff
/// styles. Count hunk rows so unrelated +/- logs after a patch stay untouched.
/// Accept a truncated final hunk (e.g. `git diff | head`); work is bounded by
/// the existing output preview limit, and native ANSI output keeps its styles.
pub(crate) fn highlight<'a>(lines: &[&'a str]) -> Vec<Cow<'a, str>> {
    let mut output: Vec<_> = lines.iter().map(|line| Cow::Borrowed(*line)).collect();
    if ColorSupport::detect() == ColorSupport::NoColor
        || lines.iter().any(|line| line.contains('\x1b'))
    {
        return output;
    }
    let mut i = 0;
    while i + 2 < lines.len() {
        let Some(old_path) = lines[i].strip_prefix("--- ") else {
            i += 1;
            continue;
        };
        let Some(new_path) = lines[i + 1].strip_prefix("+++ ") else {
            i += 1;
            continue;
        };
        if hunk_counts(lines[i + 2]).is_none() {
            i += 1;
            continue;
        }
        let path = if new_path == "/dev/null" {
            old_path
        } else {
            new_path
        };
        let path = path.split('\t').next().unwrap_or(path).trim_matches('"');
        let language = super::format::language_token_from_path(path);
        for index in i..i + 2 {
            output[index] = metadata(lines[index], true);
        }
        // Recognize the optional Git preamble without treating arbitrary
        // preceding output as metadata. Plain `diff -u` needs no Git header.
        let mut preamble = i;
        if preamble > 0 && lines[preamble - 1].starts_with("index ") {
            preamble -= 1;
            output[preamble] = metadata(lines[preamble], false);
        }
        if preamble > 0 && lines[preamble - 1].starts_with("diff --git ") {
            output[preamble - 1] = metadata(lines[preamble - 1], true);
        }
        i += 2;
        while i < lines.len() {
            let Some((mut old, mut new)) = hunk_counts(lines[i]) else {
                break;
            };
            output[i] = metadata(lines[i], false);
            i += 1;
            let mut rows = Vec::new();
            let mut indexes = Vec::new();
            while i < lines.len() {
                if lines[i] == "\\ No newline at end of file" {
                    output[i] = metadata(lines[i], false);
                    i += 1;
                    continue;
                }
                let Some(sign) = lines[i].chars().next() else {
                    break;
                };
                match sign {
                    ' ' if old > 0 && new > 0 => {
                        old -= 1;
                        new -= 1;
                    }
                    '-' if old > 0 => old -= 1,
                    '+' if new > 0 => new -= 1,
                    _ => break,
                }
                rows.push((sign, &lines[i][1..]));
                indexes.push(i);
                i += 1;
            }
            for (index, rendered) in indexes
                .into_iter()
                .zip(diff_colors::render_rows(&rows, language, ""))
            {
                output[index] = Cow::Owned(rendered);
            }
            if old != 0 || new != 0 {
                break;
            }
        }
    }
    output
}

fn metadata(line: &str, is_file: bool) -> Cow<'_, str> {
    Cow::Owned(format!("{}{line}{RESET}", theme().diff_header_fg(is_file)))
}

fn hunk_counts(line: &str) -> Option<(usize, usize)> {
    let (ranges, _) = line.strip_prefix("@@ ")?.split_once(" @@")?;
    let mut ranges = ranges.split_ascii_whitespace();
    let old = range_count(ranges.next()?.strip_prefix('-')?)?;
    let new = range_count(ranges.next()?.strip_prefix('+')?)?;
    ranges.next().is_none().then_some((old, new))
}

fn range_count(range: &str) -> Option<usize> {
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    start.parse::<usize>().ok()?;
    count.parse().ok()
}
