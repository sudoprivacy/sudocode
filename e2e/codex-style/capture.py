#!/usr/bin/env python3
"""Capture upstream Codex style expectations without adding ratatui to scode."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tomllib


def block(source, marker):
    start = source.index(marker)
    return source[start:source.index("\n}", start) + 2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True, type=Path)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--refresh-code-fixtures", action="store_true",
                        help="Also refresh the adjacent code and Bash reference fixtures")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    reference = args.codex.resolve()
    revision = subprocess.check_output(["git", "-C", str(reference), "rev-parse", "HEAD"], text=True).strip()
    def upstream(path):
        return subprocess.check_output(["git", "-C", str(reference), "show", f"{revision}:codex-rs/{path}"], text=True)
    work = args.work_dir.resolve()
    (work / "src").mkdir(parents=True, exist_ok=True)
    highlight = upstream("tui/src/render/highlight.rs")
    markdown = upstream("tui/src/markdown_render.rs")
    color = upstream("tui/src/color.rs")
    style = upstream("tui/src/style.rs")
    contrast = upstream("tui/src/style/contrast.rs")
    palette = upstream("tui/src/terminal_palette.rs")
    diff = upstream("tui/src/diff_render.rs")
    lock = tomllib.loads(upstream("Cargo.lock"))
    versions = {
        package["name"]: package["version"] for package in lock["package"]
        if package["name"] in {"syntect", "two-face", "ratatui"}
    }
    manifest = '\n'.join([
        '[package]', 'name = "codex-style-reference"', 'version = "0.0.0"',
        'edition = "2024"', '', '[dependencies]',
        f'syntect = "={versions["syntect"]}"',
        f'two-face = "={versions["two-face"]}"',
        f'ratatui = {{ version = "={versions["ratatui"]}", default-features = false }}',
        'serde_json = "1"', '',
    ])
    (work / "Cargo.toml").write_text(manifest)
    names = [
        "fn syntax_set(",
        "fn ansi_palette_color(",
        "pub(crate) fn convert_syntect_color(",
        "fn convert_style(",
        "fn find_syntax(",
        "fn highlight_to_line_spans_with_theme(",
        "fn highlighted_line_spans(",
        "pub(crate) fn foreground_style_for_scopes_with_theme(",
    ]
    extracted = "\n\n".join(block(highlight, name) for name in names)
    extracted += "\n\n" + block(markdown, "struct MarkdownStyles {")
    extracted += "\n\n" + block(markdown, "impl MarkdownStyles {")
    extracted += "\n\n" + block(color, "pub(crate) fn perceptual_distance(")
    for name in ["DARK_TC_ADD_LINE_BG_RGB", "DARK_TC_DEL_LINE_BG_RGB",
                 "LIGHT_TC_ADD_LINE_BG_RGB", "LIGHT_TC_DEL_LINE_BG_RGB"]:
        extracted += "\n" + re.search(r"^const " + name + r":.*$", diff, re.M)[0]
    # Preserve the upstream contrast implementation as well as MarkdownStyles.
    # Only terminal observations are injected: these reference variants model
    # the PTY's missing OSC palette response, then quantize terminal cells below.
    color_module = "\n\n".join(block(color, marker) for marker in [
        "pub(crate) fn is_light(", "pub(crate) fn blend(",
        "pub(crate) fn perceptual_distance(",
    ])
    palette_module = "\n\n".join(block(palette, marker) for marker in [
        "pub enum StdoutColorLevel {", "pub fn rgb_color(",
        "pub fn indexed_color(", "pub fn best_color_for_level(",
        "fn best_color_for_color_level(",
    ])
    start = palette.index("pub const XTERM_COLORS:")
    palette_module += "\n" + palette[start:palette.index("\n];", start) + 3]
    palette_module += """
fn xterm_fixed_colors() -> impl Iterator<Item = (usize, (u8, u8, u8))> {
    XTERM_COLORS.into_iter().enumerate().skip(16)
}
pub fn default_fg() -> Option<(u8,u8,u8)> { None }
pub fn default_bg() -> Option<(u8,u8,u8)> { None }
pub fn effective_stdout_color_level() -> StdoutColorLevel { StdoutColorLevel::TrueColor }
"""
    style_module = "\n\n".join(block(style, marker) for marker in [
        "fn background_rgb(", "pub(crate) fn readable_color_on(",
    ])
    (work / "src/color.rs").write_text(color_module)
    (work / "src/terminal_palette.rs").write_text(
        "use ratatui::style::Color;\nuse crate::color::perceptual_distance;\n"
        "#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]\n" + palette_module
    )
    (work / "src/contrast.rs").write_text(contrast)
    (work / "src/style.rs").write_text(
        "use ratatui::style::Color;\n"
        "use crate::terminal_palette::{default_bg,default_fg,effective_stdout_color_level};\n"
        '#[path="contrast.rs"] mod contrast;\n'
        "const CHATGPT_BLUE_100: (u8,u8,u8) = (164,205,251);\n"
        "const CHATGPT_BLUE_200: (u8,u8,u8) = (99,168,248);\n" + style_module
    )
    prelude = """
#![allow(dead_code)]
mod color;
mod terminal_palette;
mod style;
use std::sync::OnceLock;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SyntectColor, FontStyle, Highlighter, Style as SyntectStyle, Theme};
use syntect::parsing::{Scope, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;
use serde_json::{json, Value};
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;
const OPAQUE_ALPHA: u8 = 0xFF;
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;
const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;
fn accent_color() -> RtColor { RtColor::Rgb(99,168,248) }
"""
    driver = """
fn fixed_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let v = 8 + (index - 232) * 10;
        return (v,v,v);
    }
    let cube = [0,95,135,175,215,255];
    let n = usize::from(index - 16);
    (cube[n / 36], cube[n / 6 % 6], cube[n % 6])
}
fn cell_color(color: Option<RtColor>, indexed: bool) -> String {
    match color {
        None | Some(RtColor::Reset) => "Default".into(),
        Some(RtColor::Rgb(r,g,b)) if indexed => {
            let i = (16..=255).min_by(|left,right| {
                perceptual_distance(fixed_rgb(*left),(r,g,b))
                    .partial_cmp(&perceptual_distance(fixed_rgb(*right),(r,g,b))).unwrap()
            }).unwrap();
            format!("Idx({i})")
        },
        Some(RtColor::Rgb(r,g,b)) => format!("Rgb({r}, {g}, {b})"),
        Some(RtColor::Green) => "Idx(2)".into(),
        Some(RtColor::LightBlue) => "Idx(12)".into(),
        Some(RtColor::Indexed(i)) => format!("Idx({i})"),
        other => panic!("unhandled reference color {other:?}"),
    }
}
fn cell_style(style: Style, indexed: bool) -> Value {
    json!({
        "fg": cell_color(style.fg, indexed),
        "bg": "Default",
        "bold": style.add_modifier.contains(Modifier::BOLD),
        "italic": style.add_modifier.contains(Modifier::ITALIC),
        "underline": style.add_modifier.contains(Modifier::UNDERLINED),
        "strike": style.add_modifier.contains(Modifier::CROSSED_OUT),
    })
}
fn main() {
    let input: Value = serde_json::from_str(&std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap()).unwrap();
    let mut variants = serde_json::Map::new();
    for (name, theme_name) in [
        ("dark", EmbeddedThemeName::CatppuccinMocha),
        ("light", EmbeddedThemeName::CatppuccinLatte),
    ] {
        let theme = two_face::theme::extra().get(theme_name).clone();
        let markdown = MarkdownStyles::for_theme(&theme);
        let string = foreground_style_for_scopes_with_theme(&theme, &["string"]).unwrap_or_default();
        let (add_bg, del_bg) = if name == "light" {
            (LIGHT_TC_ADD_LINE_BG_RGB, LIGHT_TC_DEL_LINE_BG_RGB)
        } else {
            (DARK_TC_ADD_LINE_BG_RGB, DARK_TC_DEL_LINE_BG_RGB)
        };
        let on_diff = |bg: (u8,u8,u8)| string.fg.map_or(string, |fg| string.fg(crate::style::readable_color_on(fg, Some(RtColor::Rgb(bg.0,bg.1,bg.2)))));
        for indexed in [false, true] {
            let mut roles = serde_json::Map::new();
            for (name, style) in [
                ("inline", markdown.code), ("h1", markdown.h1), ("h2", markdown.h2),
                ("h3", markdown.h3), ("h4", markdown.h4), ("h5", markdown.h5),
                ("h6", markdown.h6), ("strong", markdown.strong),
                ("emphasis", markdown.emphasis), ("strike", markdown.strikethrough),
                ("quote", markdown.blockquote), ("ordered", markdown.ordered_list_marker),
                ("unordered", markdown.unordered_list_marker),
                ("link", markdown.link),
                ("prose", Style::new()), ("bold_code", markdown.code.bold()),
                ("strong_emphasis", markdown.strong.patch(markdown.emphasis)),
                ("emphasis_strong", markdown.emphasis.patch(markdown.strong)),
                ("quote_strong", markdown.blockquote.patch(markdown.strong)),
                ("quote_emphasis", markdown.blockquote.patch(markdown.emphasis)),
                ("quote_code", markdown.blockquote.patch(markdown.code)),
                ("strong_link", markdown.strong.patch(markdown.link)),
                ("heading_strong", markdown.h2.patch(markdown.strong)),
                ("diagram_node", foreground_style_for_scopes_with_theme(&theme, &["entity.name.type", "support.type", "variable"]).unwrap_or_else(|| Style::new().cyan())),
                ("diagram_edge", foreground_style_for_scopes_with_theme(&theme, &["comment"]).unwrap_or_else(|| Style::new().dim())),
                ("diff_added_string", on_diff(add_bg)),
                ("diff_removed_string", on_diff(del_bg)),
            ] {
                roles.insert(name.into(), cell_style(style, indexed));
            }
            let documents: Vec<Value> = input["documents"].as_array().unwrap().iter().map(|doc| {
                let code = doc["code"].as_str().unwrap();
                let lang = doc["language"].as_str().unwrap().split([',',' ',char::from(9)]).next().unwrap();
                let lines = highlight_to_line_spans_with_theme(code, lang, &theme)
                    .unwrap_or_else(|| code.lines().map(|line| vec![Span::raw(line.to_owned())]).collect());
                let lines: Vec<Value> = lines.iter().map(|spans| {
                    let cells: Vec<Value> = spans.iter().map(|span| {
                        json!({"text":span.content, "style":cell_style(span.style,indexed)})
                    }).collect();
                    json!({"text":spans.iter().map(|span|span.content.as_ref()).collect::<String>(),"spans":cells})
                }).collect();
                json!({"language":doc["language"],"lines":lines})
            }).collect();
            variants.insert(format!("{name}-{}",if indexed {"indexed"} else {"truecolor"}), json!({"roles":roles,"documents":documents}));
        }
    }
    println!("{}",serde_json::to_string_pretty(&variants).unwrap());
}
"""
    (work / "src/main.rs").write_text(prelude + extracted + driver)
    mock = (repo / "rust/crates/mock-anthropic-service/src/lib.rs").read_text()
    fixture = re.search(r'pub const SYNTAX_SHOWCASE_DOC: &str = r#"(.*?)"#;', mock, re.S)[1]
    fence = chr(96) * 3
    documents = [
        {"language": language, "code": code}
        for language, code in re.findall(fence + r"([^\n]*)\n(.*?)" + fence, fixture, re.S)
    ]
    showcase_count = len(documents)
    legacy = {}
    if args.refresh_code_fixtures:
        fixture_dir = repo / "rust/crates/rusty-sudocode-cli/tests/fixtures"
        for name in ["codex_default_colors.json", "codex_bash_colors.json"]:
            fixture = json.loads((fixture_dir / name).read_text())
            samples = fixture["themes"][0]["samples"]
            legacy[name] = (fixture, len(documents), len(samples))
            documents.extend({"language": sample["language"], "code": sample["code"]}
                             for sample in samples)
    (work / "input.json").write_text(json.dumps({"documents": documents}))
    output = subprocess.check_output([
        "cargo", "run", "--quiet", "--manifest-path", str(work / "Cargo.toml"),
        "--", str(work / "input.json"),
    ], text=True)
    variants = json.loads(output)
    digest = hashlib.sha256(
        (extracted + color_module + palette_module + style_module + contrast).encode()
    ).hexdigest()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    for name, (fixture, start, count) in legacy.items():
        for theme in fixture["themes"]:
            variant = variants["light-truecolor" if theme["theme"].endswith("latte") else "dark-truecolor"]
            for sample, document in zip(theme["samples"], variant["documents"][start:start + count], strict=True):
                sample["rows"] = [[{
                    "text": span["text"], "foreground": span["style"]["fg"],
                    "bold": span["style"]["bold"],
                } for span in line["spans"]] for line in document["lines"]]
            for key in ["inline_foreground", "diff_added_string", "diff_removed_string"]:
                if key in theme:
                    theme[key] = variant["roles"]["inline" if key == "inline_foreground" else key]["fg"]
        fixture.update(revision=revision, helper_source_sha256=digest, two_face=versions["two-face"])
        (args.output.parent / name).write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
    for variant in variants.values():
        variant["documents"] = variant["documents"][:showcase_count]
        styles = []
        def intern(style):
            if style not in styles:
                styles.append(style)
            return styles.index(style)
        variant["roles"] = {name: intern(style) for name, style in variant["roles"].items()}
        for document in variant["documents"]:
            for line in document["lines"]:
                for span in line["spans"]:
                    span["style"] = intern(span["style"])
        variant["styles"] = styles
    result = {
        "reference": {
            "repository": "https://github.com/openai/codex",
            "commit": revision,
            "versions": versions,
            "helper_source_sha256": digest,
            "method": "Unmodified upstream MarkdownStyles, highlighting and contrast helpers; absent OSC foreground/background, TrueColor style resolution, and CIE76 indexed terminal-cell conversion. Nested roles use upstream Style::patch order.",
        },
        "variants": variants,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(args.output)


if __name__ == "__main__":
    main()
