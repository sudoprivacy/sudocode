//! Release rendering measurements through the existing real-terminal host.
mod common;

use common::TestEnv;
use serde_json::{json, Value};
use std::{fs, path::Path};

#[test]
#[ignore = "release A/A and A/B gate; invoked by e2e/terminal-resize/benchmark.py"]
fn render_performance_workload() {
    let policy_path = std::env::var_os("SCODE_RENDER_POLICY").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../e2e/terminal-resize/performance-policy.json")
        },
        std::path::PathBuf::from,
    );
    let policy: Value = serde_json::from_slice(&fs::read(policy_path).unwrap()).unwrap();
    let case = std::env::var("SCODE_RENDER_CASE").expect("benchmark selects a case");
    let spec = &policy["cases"][&case];
    assert!(spec.is_object(), "unknown render case {case}");
    let env = TestEnv::new_mock("render-performance");
    let root = env.workspace_root();
    let prompt = if spec["kind"] == "background" || spec["kind"] == "foreground" {
        env.prompt(
            &format!(
                "TOOL_BATCH:{}",
                json!([{
                    "id": "background", "name": "Bash", "input": {
                    "command": common::render_measurement::background_output_command(Some(spec["batches"].as_u64().unwrap())),
                    "description": "Tool output workload", "run_in_background": spec["kind"] == "background",
                    },
                }])
            ),
            "tool_concurrency",
        )
    } else {
        let size = usize::try_from(spec["bytes"].as_u64().unwrap()).unwrap();
        let mut document = String::from("RenderBodySTART ");
        if spec["kind"] == "markdown" {
            document.push_str("\n\n## RenderHeading\n\n```rust\nfn render_code() { println!(\"CodeSentinel\"); }\n```\n\n| Name | Value |\n| --- | --- |\n| TableSentinel | 42 |\n\n中文界 e\u{301} 👩🏽‍💻\n\n");
        }
        let suffix = " RenderBodyEND.";
        let words = "bounded ordinary prose with clear words ";
        let padding = words.repeat(size.div_ceil(words.len()));
        document.push_str(&padding[..size - document.len() - suffix.len()]);
        document.push_str(suffix);
        assert_eq!(
            document.len(),
            size,
            "fixture must straddle the exact byte limit"
        );
        common::terminal_host::prose_prompt(
            &env,
            &document,
            json!({
                "start_ready": root.join("stream-ready"), "start_release": root.join("stream-release"),
                "chunk_chars": document.chars().count().div_ceil(45), "delay_ms": 40,
            }),
        )
    };
    let flag = std::env::var("SUDOCODE_EXPERIMENT_PROSE_PREVIEW").unwrap_or_else(|_| "0".into());
    let output = common::terminal_host::run(
        &env,
        &[
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
        ],
        "performance",
        json!({
            "prompt": prompt, "case": case, "spec": spec, "sampling": policy["sampling"], "cols": 120, "rows": 50,
            "report": std::env::var("SCODE_RENDER_REPORT").expect("benchmark report path"),
            "env": {"SUDOCODE_EXPERIMENT_PROSE_PREVIEW": flag},
        }),
    );
    // Release the real shell even if a terminal assertion aborted.
    fs::write(root.join("release"), "release").unwrap();
    common::terminal_host::assert_success(&output);
}
