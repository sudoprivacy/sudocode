//! Color distance and blending adapted from OpenAI Codex b741e480.
//! Copyright 2025 OpenAI. Licensed under Apache-2.0; see third_party/codex.
//! Palette/contrast resolution keeps Codex's rules, with cached conversions.

use super::ColorSupport;
use crossterm::style::Color;
use std::cell::RefCell;
use std::collections::HashMap;
type Rgb = (u8, u8, u8);
type ForegroundKey = (Rgb, Option<Rgb>, ColorSupport);

thread_local! {
    static FOREGROUNDS: RefCell<HashMap<ForegroundKey, Color>> = RefCell::default();
}

pub(super) fn xterm(index: u8) -> Rgb {
    if index >= 232 {
        let value = 8 + (index - 232) * 10;
        return (value, value, value);
    }
    let cube = [0, 95, 135, 175, 215, 255];
    let index = usize::from(index.saturating_sub(16));
    (cube[index / 36], cube[index / 6 % 6], cube[index % 6])
}

/// Resolve a foreground against the painted surface, then reduce its palette.
pub(super) fn foreground(preferred: Rgb, background: Option<Rgb>, support: ColorSupport) -> Color {
    if matches!(support, ColorSupport::NoColor | ColorSupport::Ansi16) {
        return Color::Reset;
    }
    if support == ColorSupport::TrueColor && background.is_none() {
        return Color::Rgb {
            r: preferred.0,
            g: preferred.1,
            b: preferred.2,
        };
    }
    FOREGROUNDS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = (preferred, background, support);
        if let Some(color) = cache.get(&key) {
            return *color;
        }
        let resolved = match support {
            ColorSupport::TrueColor => {
                let rgb = background.map_or(preferred, |bg| {
                    if ratio(preferred, bg) >= 4.5 {
                        return preferred;
                    }
                    let endpoint = if ratio((0, 0, 0), bg) >= ratio((255, 255, 255), bg) {
                        (0, 0, 0)
                    } else {
                        (255, 255, 255)
                    };
                    (1_u8..=255)
                        .map(|step| blend(endpoint, preferred, f32::from(step) / 255.0))
                        .find(|candidate| ratio(*candidate, bg) >= 4.5)
                        .unwrap_or(endpoint)
                });
                Color::Rgb {
                    r: rgb.0,
                    g: rgb.1,
                    b: rgb.2,
                }
            }
            ColorSupport::Ansi256 => (16..=255)
                .filter(|index| background.is_none_or(|bg| ratio(xterm(*index), bg) >= 4.5))
                .min_by(|a, b| {
                    perceptual_distance(xterm(*a), preferred)
                        .total_cmp(&perceptual_distance(xterm(*b), preferred))
                })
                .map_or(Color::Reset, Color::AnsiValue),
            ColorSupport::NoColor | ColorSupport::Ansi16 => Color::Reset,
        };
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(key, resolved);
        resolved
    })
}

fn ratio(a: Rgb, b: Rgb) -> f64 {
    let luminance = |(r, g, b): Rgb| {
        let [r, g, b] = [r, g, b].map(|c| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

// Preserve Codex's truncation to 8-bit channels; alpha is in [0, 1].
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let r = (f32::from(fg.0) * alpha + f32::from(bg.0) * (1.0 - alpha)) as u8;
    let g = (f32::from(fg.1) * alpha + f32::from(bg.1) * (1.0 - alpha)) as u8;
    let b = (f32::from(fg.2) * alpha + f32::from(bg.2) * (1.0 - alpha)) as u8;
    (r, g, b)
}

/// Returns the perceptual color distance between two RGB colors.
/// Uses the CIE76 formula (Euclidean distance in Lab space approximation).
// Standard RGB/XYZ/Lab coordinate names make the reference formula comparable.
#[allow(clippy::many_single_char_names)]
fn perceptual_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    // Convert sRGB to linear RGB
    fn srgb_to_linear(c: u8) -> f32 {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    // Convert RGB to XYZ
    fn rgb_to_xyz(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
        let r = srgb_to_linear(r);
        let g = srgb_to_linear(g);
        let b = srgb_to_linear(b);

        let x = r * 0.4124 + g * 0.3576 + b * 0.1805;
        let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
        let z = r * 0.0193 + g * 0.1192 + b * 0.9505;
        (x, y, z)
    }

    // Convert XYZ to Lab
    fn xyz_to_lab(x: f32, y: f32, z: f32) -> (f32, f32, f32) {
        fn f(t: f32) -> f32 {
            if t > 0.008_856 {
                t.powf(1.0 / 3.0)
            } else {
                7.787 * t + 16.0 / 116.0
            }
        }
        // D65 reference white
        let xr = x / 0.95047;
        let yr = y / 1.00000;
        let zr = z / 1.08883;

        let fx = f(xr);
        let fy = f(yr);
        let fz = f(zr);

        let l = 116.0 * fy - 16.0;
        let a = 500.0 * (fx - fy);
        let b = 200.0 * (fy - fz);
        (l, a, b)
    }

    let (x1, y1, z1) = rgb_to_xyz(a.0, a.1, a.2);
    let (x2, y2, z2) = rgb_to_xyz(b.0, b.1, b.2);

    let (l1, a1, b1) = xyz_to_lab(x1, y1, z1);
    let (l2, a2, b2) = xyz_to_lab(x2, y2, z2);

    let dl = l1 - l2;
    let da = a1 - a2;
    let db = b1 - b2;

    (dl * dl + da * da + db * db).sqrt()
}
