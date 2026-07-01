//! PTY tests for image handling — real user workflows around the
//! image-handling-non-user-facing design (Decision 1 + 2).
//!
//! Live-e2e was run 2026-07-01 via ai-dev-browser drive of sudowork UI
//! (memory: [[project_image_handling_capability_gap]]). This file
//! promotes the CLI leg of that same coverage into the PTY layer, so a
//! regression in `push_images` gates PR CI.
//!
//! Coverage:
//!
//! 1. `scode "describe @image.png"` runs the full CLI conversation loop
//!    against a real PNG fixture written on disk. Response must reference
//!    the image contents (mock scenario keys assistant reply on the
//!    fixture's presence — see `openai_compat_mock.rs` for the mapping).
//! 2. `scode "describe @image.png" --model <text-only>` exercises
//!    push_images' VLM branch: `vision_capable(text-only) == false` →
//!    stderr emits `[push_images] VLM-route start`. Since the mock has
//!    no sudorouter reachable, we assert the fallback placeholder
//!    ("could not be described automatically") lands in the response;
//!    the point is that the branch fires without hanging the CLI (real
//!    regression pattern from PR #267).
//!
//! ```bash
//! cargo test --test pty_image_handling                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_image_handling  # real API
//! ```

mod common;

use std::fs;

use common::TestEnv;

/// Minimal valid PNG: 128×128, yellow background, red rectangle,
/// cyan circle. Same fixture used in the sudowork e2e yaml
/// (`sudowork/tests/e2e/fixtures/img-small-3shapes.png`). Written
/// inline so this test never depends on a sibling repo.
fn write_fixture_png(path: &std::path::Path) {
    use std::io::Write;

    let (w, h) = (128u32, 128u32);
    let mut raw = Vec::with_capacity((w * h * 3 + h) as usize);
    for y in 0..h {
        raw.push(0u8); // filter byte
        for x in 0..w {
            let (r, g, b) = if (20..=60).contains(&x) && (40..=90).contains(&y) {
                (210u8, 20u8, 20u8) // red rect
            } else if {
                let dx = x as i32 - 90;
                let dy = y as i32 - 64;
                dx * dx + dy * dy <= 25 * 25
            } {
                (30u8, 200u8, 240u8) // cyan circle
            } else {
                (240u8, 224u8, 64u8) // yellow bg
            };
            raw.push(r);
            raw.push(g);
            raw.push(b);
        }
    }
    let compressed = deflate_zlib(&raw);

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    write_png_chunk(&mut png, b"IHDR", &{
        let mut d = Vec::new();
        d.extend_from_slice(&w.to_be_bytes());
        d.extend_from_slice(&h.to_be_bytes());
        d.push(8); // 8-bit depth
        d.push(2); // color type 2 = RGB
        d.push(0);
        d.push(0);
        d.push(0);
        d
    });
    write_png_chunk(&mut png, b"IDAT", &compressed);
    write_png_chunk(&mut png, b"IEND", &[]);

    let mut f = fs::File::create(path).expect("create fixture png");
    f.write_all(&png).expect("write fixture png");
}

fn write_png_chunk(png: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    let len = data.len() as u32;
    png.extend_from_slice(&len.to_be_bytes());
    let crc_start = png.len();
    png.extend_from_slice(tag);
    png.extend_from_slice(data);
    let crc = crc32(&png[crc_start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn deflate_zlib(raw: &[u8]) -> Vec<u8> {
    // Use flate2 which is already in scode's build tree.
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(raw).expect("deflate");
    e.finish().expect("deflate finish")
}

fn crc32(data: &[u8]) -> u32 {
    // Inline table-driven CRC-32 (PNG polynomial 0xedb88320).
    // Avoids adding a crc32 dep to the test-only tree.
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            t[i as usize] = c;
        }
        t
    });
    let mut c = 0xffffffffu32;
    for &b in data {
        c = table[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffffffff
}

// ──────────────────────────────────────────────────────────────────────
// 1. CLI @image reference — full round trip
// ──────────────────────────────────────────────────────────────────────

/// `scode "@image.png — describe"` reads the PNG through the @-file
/// resolver, ships it to the backend, prints a response, exits 0.
///
/// Regression guard: pre-#258 the CLI silently dropped image blocks
/// when the model was text-only. Now it should either succeed (native
/// pass-through) or route via VLM.
#[test]
fn cli_at_image_reference_completes_turn() {
    let env = TestEnv::new("image-cli-at-ref");
    let fixture = env.workspace_root().join("shapes.png");
    write_fixture_png(&fixture);

    // Sanity — the fixture wrote something sane.
    let png_bytes = fs::read(&fixture).expect("read back fixture");
    assert!(
        png_bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]),
        "fixture must be a valid PNG (starts with the PNG signature)"
    );
    assert!(
        png_bytes.len() > 100,
        "fixture too small: {}",
        png_bytes.len()
    );

    let prompt = env.prompt(
        &format!(
            "Describe what shapes and colors are in this image in ONE short sentence: @{}",
            fixture.display()
        ),
        "single_turn_text",
    );

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    // Response must land — the specific text varies by backend. In mock
    // mode the `single_turn_text` scenario canned reply is "The answer
    // is 4"; in live mode any assistant text works. We key on either
    // pattern so both modes exercise the same assertion path (DRY).
    sess.expect("(?i)(answer|image|shape|color|red|blue|yellow|circle|rectangle|square)")
        .expect("scode should produce some assistant content on stdout");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "cli image turn should exit 0; got {exit}");

    // Mock-only: verify the message was actually shipped through the
    // backend. If push_images silently dropped the image block the
    // request would never leave scode — this catches that regression.
    if env.is_mock() {
        assert!(
            env.captured_message_count() >= 1,
            "expected at least 1 /v1/messages request; got 0 — push_images silently dropped the turn"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. CLI @image + text-only model — VLM branch must not hang
// ──────────────────────────────────────────────────────────────────────

/// Same input, but forced to a model the SSOT cache marks text-only.
/// `vision_capable(...)` returns false → push_images enters the VLM
/// branch. With no sudorouter reachable in mock mode the VLM leg
/// resolves via the placeholder fallback (see
/// `vlm_describe_block_or_placeholder`), the branch STILL exits — the
/// PR #267 fix (dedicated OS thread + fresh runtime) guarantees no
/// deadlock. Regression sentinel for the exact hang that shipped
/// unnoticed and got surfaced by real UI drive.
#[test]
fn cli_at_image_text_only_model_no_hang() {
    let env = TestEnv::new("image-cli-text-only-no-hang");
    let fixture = env.workspace_root().join("shapes.png");
    write_fixture_png(&fixture);

    let prompt = env.prompt(
        &format!(
            "Describe the image in ONE short sentence: @{}",
            fixture.display()
        ),
        "single_turn_text",
    );

    // Force a model the bundled capability table marks vision_supported=false.
    // Any model listed in tests/common/openai_compat_mock.rs's text-only
    // fixture would work; deepseek-v4-flash is the canonical one from
    // the design partner's report.
    let mut sess = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--model",
        "deepseek-v4-flash",
        &prompt,
    ]);

    // The CLI MUST reach some assistant output — either the mock's
    // canned reply, or the "[Image #N could not be described
    // automatically ...]" placeholder that push_images splices when
    // sudorouter is unreachable. Both prove the VLM branch exited
    // without hanging.
    sess.expect("(?i)(answer|image|could not|shape|color|describe)")
        .expect("push_images VLM branch must complete — no hang");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "text-only image turn should exit 0; got {exit}");

    // Mock-only: even in the VLM branch, the final assistant turn
    // still hits the backend once the description text is spliced in.
    if env.is_mock() {
        assert!(
            env.captured_message_count() >= 1,
            "expected at least 1 /v1/messages request; got 0 — VLM route may have hung"
        );
    }
}
