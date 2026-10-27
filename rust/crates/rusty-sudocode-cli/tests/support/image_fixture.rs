//! Image fixtures shared by the image PTY tests.

use std::fs;

use crate::common::TestEnv;

/// Minimal valid PNG: 128×128, yellow background, red rectangle,
/// cyan circle. Same fixture used in the sudowork e2e yaml
/// (`sudowork/tests/e2e/fixtures/img-small-3shapes.png`). Written
/// inline so this test never depends on a sibling repo.
pub fn write_fixture_png(path: &std::path::Path) {
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

/// Mark the mock's model as text-only, so an image must be rejected.
pub fn mark_model_text_only(env: &TestEnv) {
    env.set_model_catalog(serde_json::json!({"data": [{
        "id": "claude-sonnet-4-6", "context_window": 200000,
        "max_output_tokens": 64000, "vision_supported": false
    }]}));
    env.prime_model_catalog();
}
