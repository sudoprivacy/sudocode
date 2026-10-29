//! Image fixtures shared by the image PTY tests.

use std::fs;

use crate::common::TestEnv;

/// Minimal valid PNG: 128×128, yellow background, red rectangle,
/// cyan circle. Same fixture used in the sudowork e2e yaml
/// (`sudowork/tests/e2e/fixtures/img-small-3shapes.png`). Written
/// inline so this test never depends on a sibling repo.
pub fn write_fixture_png(path: &std::path::Path) {
    use std::io::Write;

    let (width, height) = (128_u32, 128_u32);
    let mut raw = Vec::with_capacity(usize::try_from(width * height * 3 + height).unwrap());
    for y in 0..height {
        raw.push(0_u8); // filter byte
        for x in 0..width {
            let dx = i64::from(x) - 90;
            let dy = i64::from(y) - 64;
            let is_circle = dx * dx + dy * dy <= 25 * 25;
            let (red, green, blue) = if (20..=60).contains(&x) && (40..=90).contains(&y) {
                (210_u8, 20_u8, 20_u8)
            } else if is_circle {
                (30_u8, 200_u8, 240_u8)
            } else {
                (240_u8, 224_u8, 64_u8)
            };
            raw.extend_from_slice(&[red, green, blue]);
        }
    }
    let compressed = deflate_zlib(&raw);

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    write_png_chunk(&mut png, *b"IHDR", &{
        let mut d = Vec::new();
        d.extend_from_slice(&width.to_be_bytes());
        d.extend_from_slice(&height.to_be_bytes());
        d.push(8); // 8-bit depth
        d.push(2); // color type 2 = RGB
        d.push(0);
        d.push(0);
        d.push(0);
        d
    });
    write_png_chunk(&mut png, *b"IDAT", &compressed);
    write_png_chunk(&mut png, *b"IEND", &[]);

    let mut f = fs::File::create(path).expect("create fixture png");
    f.write_all(&png).expect("write fixture png");
}

fn write_png_chunk(png: &mut Vec<u8>, tag: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).expect("fixture PNG chunk fits u32");
    png.extend_from_slice(&len.to_be_bytes());
    let crc_start = png.len();
    png.extend_from_slice(&tag);
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
    // Inline table-driven CRC-32 (PNG polynomial 0xedb8_8320).
    // Avoids adding a crc32 dep to the test-only tree.
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            t[i as usize] = c;
        }
        t
    });
    let mut c = 0xffff_ffff_u32;
    for &b in data {
        c = table[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

/// Mark the mock's model as text-only, so an image must be rejected.
pub fn mark_model_text_only(env: &TestEnv) {
    env.set_model_catalog(serde_json::json!({"data": [{
        "id": "claude-sonnet-4-6", "context_window": 200_000,
        "max_output_tokens": 64000, "vision_supported": false
    }]}));
    env.prime_model_catalog();
}
