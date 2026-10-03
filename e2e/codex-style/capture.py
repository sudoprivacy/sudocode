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
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    reference = args.codex.resolve()
    work = args.work_dir.resolve()
    (work / "src").mkdir(parents=True, exist_ok=True)
    tui = reference / "codex-rs/tui/src"
    highlight = (tui / "render/highlight.rs").read_text()
    markdown = (tui / "markdown_render.rs").read_text()
    color = (tui / "color.rs").read_text()
    lock = tomllib.loads((reference / "codex-rs/Cargo.lock").read_text())
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
    prelude = """
#![allow(dead_code)]
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
    for (name, theme_name, link) in [
        ("dark", EmbeddedThemeName::CatppuccinMocha, RtColor::Rgb(99,168,248)),
        ("light", EmbeddedThemeName::CatppuccinLatte, RtColor::Rgb(28,100,200)),
    ] {
        let theme = two_face::theme::extra().get(theme_name).clone();
        let markdown = MarkdownStyles::for_theme(&theme);
        for indexed in [false, true] {
            let mut roles = serde_json::Map::new();
            for (name, style) in [
                ("inline", markdown.code), ("h1", markdown.h1), ("h2", markdown.h2),
                ("h3", markdown.h3), ("h4", markdown.h4), ("h5", markdown.h5),
                ("h6", markdown.h6), ("strong", markdown.strong),
                ("emphasis", markdown.emphasis), ("strike", markdown.strikethrough),
                ("quote", markdown.blockquote), ("ordered", markdown.ordered_list_marker),
                ("link", Style::new().fg(link).underlined()),
                ("prose", Style::new()), ("bold_code", markdown.code.bold()),
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
    (work / "input.json").write_text(json.dumps({"documents": documents}))
    output = subprocess.check_output([
        "cargo", "run", "--quiet", "--manifest-path", str(work / "Cargo.toml"),
        "--", str(work / "input.json"),
    ], text=True)
    variants = json.loads(output)
    for variant in variants.values():
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
            "commit": subprocess.check_output(["git", "-C", str(reference), "rev-parse", "HEAD"], text=True).strip(),
            "versions": versions,
            "helper_source_sha256": hashlib.sha256(extracted.encode()).hexdigest(),
            "method": "Unmodified upstream MarkdownStyles and highlighting helpers; terminal cell styles use the upstream CIE76 palette-distance helper.",
        },
        "variants": variants,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(args.output)


if __name__ == "__main__":
    main()
