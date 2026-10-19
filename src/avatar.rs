//! Rakazo's public avatar value is a data image or a colored mascot, not a URL.
//! No renderer endpoint exists: this Rust compatibility adapter rasterizes the
//! frontend's canonical geometry instead of requiring a browser on the server.
//! @see github:FuturePresentLabs/fpl-agents@0d1fb12aa3f3bf9ff0885b9a5c6945ebb96a41f8 packages/core/src/avatar-shape.ts, packages/ui-web/src/bot-avatar.tsx (Apache-2.0)
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::f64::consts::TAU;

pub struct Avatar {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

pub fn render(identity: &str, value: &str, style: &str) -> Result<Avatar> {
    ensure!(
        value.len() <= 200_000,
        "Rakazo avatar exceeds its contract limit"
    );
    if value.starts_with("data:") {
        let (prefix, data) = value.split_once(',').context("invalid avatar data image")?;
        let mime = match prefix {
            "data:image/png;base64" => "image/png",
            "data:image/jpeg;base64" => "image/jpeg",
            "data:image/webp;base64" => "image/webp",
            "data:image/gif;base64" => "image/gif",
            _ => bail!("unsupported avatar image type"),
        };
        let bytes = STANDARD.decode(data).context("invalid avatar base64")?;
        let valid = match mime {
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
            "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            _ => false,
        };
        ensure!(valid, "avatar bytes do not match declared image type");
        return Ok(Avatar { mime, bytes });
    }
    let (color, shape) = value
        .split_once("::shape_")
        .map_or((value, None), |(color, shape)| (color, Some(shape)));
    let color = if color.is_empty() { "#8B5CF6" } else { color };
    ensure!(
        matches!(color.len(), 4 | 7)
            && color.starts_with('#')
            && color[1..].bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid mascot color"
    );
    let seed = identity_seed(identity);
    let assets: serde_json::Value =
        serde_json::from_str(include_str!("../assets/avatar-shapes.json"))?;
    let hex = color.trim_start_matches('#');
    let expanded = if hex.len() == 3 {
        hex.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        hex.to_string()
    };
    let rgb = u32::from_str_radix(&expanded, 16)?;
    let red = (rgb >> 16) & 255;
    let green = (rgb >> 8) & 255;
    let blue = rgb & 255;
    let dark_default = format!(
        "#{:02x}{:02x}{:02x}",
        (f64::from(red) * 0.8).floor() as u8,
        (f64::from(green) * 0.8).floor() as u8,
        (f64::from(blue) * 0.8).floor() as u8
    );
    let palette = assets["palette"]
        .as_array()
        .context("avatar palette missing")?
        .iter()
        .find(|entry| {
            entry["hex"]
                .as_str()
                .is_some_and(|hex| hex.eq_ignore_ascii_case(color))
        });
    let light = palette
        .and_then(|entry| entry["light"].as_str())
        .unwrap_or(color);
    let dark = palette
        .and_then(|entry| entry["dark"].as_str())
        .unwrap_or(&dark_default);
    let eye = palette
        .and_then(|entry| entry["eyeColor"].as_str())
        .unwrap_or(if red * 299 + green * 587 + blue * 114 > 180_000 {
            "#141414"
        } else {
            "#FFFFFF"
        });
    let (viewbox, body) = if let Some(shape) = shape {
        let index: usize = shape.parse().context("invalid avatar shape index")?;
        let key = assets["keys"][index % 8]
            .as_str()
            .context("missing avatar shape key")?;
        let path = assets["paths"][key]
            .as_str()
            .context("missing avatar shape")?;
        let center = assets["center"].as_f64().context("avatar center missing")?;
        let left = center - 29.;
        let right = center + 29.;
        let y = center - 8.;
        (
            assets["viewBox"]
                .as_str()
                .context("avatar viewbox missing")?,
            format!(
                "<defs><linearGradient id=\"ink\" x2=\"100%\" y2=\"100%\"><stop stop-color=\"{light}\"/><stop offset=\"1\" stop-color=\"{dark}\"/></linearGradient></defs><path d=\"{path}\" fill=\"url(#ink)\"/><g fill=\"{eye}\"><ellipse cx=\"{left}\" cy=\"{y}\" rx=\"10\" ry=\"7\"/><ellipse cx=\"{right}\" cy=\"{y}\" rx=\"10\" ry=\"7\"/></g>"
            ),
        )
    } else if style == "organic" {
        let path = organic_path(seed);
        let rotation = (seed % 9) as i32 - 4;
        (
            "-60 -60 120 120",
            format!(
                "<path d=\"{path}\" fill=\"{color}\"/><g transform=\"rotate({rotation})\" fill=\"#0B0C0E\"><rect x=\"-14\" y=\"-12\" width=\"7\" height=\"24\" rx=\"3.5\"/><rect x=\"7\" y=\"-12\" width=\"7\" height=\"24\" rx=\"3.5\"/></g>"
            ),
        )
    } else {
        ensure!(style == "robot", "unknown Rakazo avatar style");
        (
            "0 0 100 100",
            format!(
                "<defs><radialGradient id=\"body\" cx=\"35%\" cy=\"26%\" r=\"80%\"><stop stop-color=\"{light}\"/><stop offset=\".55\" stop-color=\"{color}\"/><stop offset=\"1\" stop-color=\"{dark}\"/></radialGradient><linearGradient id=\"visor\" x2=\"0\" y2=\"1\"><stop stop-color=\"#101014\"/><stop offset=\"1\" stop-color=\"#030305\"/></linearGradient></defs><circle cx=\"50\" cy=\"50\" r=\"50\" fill=\"url(#body)\"/><rect x=\"16\" y=\"28\" width=\"68\" height=\"44\" rx=\"23\" fill=\"url(#visor)\"/><g fill=\"white\"><rect x=\"31\" y=\"39\" width=\"14\" height=\"22\" rx=\"7\"/><rect x=\"55\" y=\"39\" width=\"14\" height=\"22\" rx=\"7\"/></g>"
            ),
        )
    };
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"256\" height=\"256\" viewBox=\"{viewbox}\">{body}</svg>"
    );
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())?;
    let mut pixels = resvg::tiny_skia::Pixmap::new(256, 256).context("allocate avatar bitmap")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixels.as_mut(),
    );
    Ok(Avatar {
        mime: "image/png",
        bytes: pixels.encode_png()?,
    })
}

fn identity_seed(identity: &str) -> u32 {
    identity
        .encode_utf16()
        .fold(0i32, |hash, c| {
            hash.wrapping_mul(31).wrapping_add(i32::from(c))
        })
        .unsigned_abs()
}

fn organic_path(seed: u32) -> String {
    let family = seed % 10;
    let fixed: &[[f64; 2]] = match family {
        0 => &[
            [-43., 22.],
            [-51., 16.],
            [-53., 6.],
            [-49., -4.],
            [-40., -12.],
            [-30., -13.],
            [-27., -24.],
            [-18., -34.],
            [-6., -36.],
            [4., -32.],
            [12., -42.],
            [25., -45.],
            [37., -38.],
            [43., -27.],
            [42., -17.],
            [51., -11.],
            [56., 0.],
            [54., 12.],
            [46., 20.],
            [30., 24.],
            [0., 25.],
            [-29., 25.],
        ],
        1 => &[
            [0., -52.],
            [12., -35.],
            [28., -17.],
            [39., 2.],
            [42., 21.],
            [34., 38.],
            [19., 49.],
            [0., 52.],
            [-19., 49.],
            [-34., 38.],
            [-42., 21.],
            [-39., 2.],
            [-28., -17.],
            [-12., -35.],
        ],
        2 => &[
            [-42., -23.],
            [-27., -40.],
            [-7., -46.],
            [14., -40.],
            [33., -27.],
            [46., -8.],
            [46., 14.],
            [35., 34.],
            [14., 46.],
            [-8., 46.],
            [-29., 38.],
            [-43., 21.],
            [-48., 0.],
        ],
        6 => &[
            [0., 48.],
            [-14., 36.],
            [-32., 20.],
            [-45., 0.],
            [-43., -20.],
            [-29., -36.],
            [-10., -39.],
            [0., -27.],
            [10., -39.],
            [29., -36.],
            [43., -20.],
            [45., 0.],
            [32., 20.],
            [14., 36.],
        ],
        7 => &[
            [-48., 35.],
            [-38., 22.],
            [-34., -8.],
            [-27., -31.],
            [-12., -44.],
            [0., -48.],
            [12., -44.],
            [27., -31.],
            [34., -8.],
            [38., 22.],
            [48., 35.],
            [26., 42.],
            [0., 44.],
            [-26., 42.],
        ],
        8 => &[
            [-45., -19.],
            [-28., -38.],
            [-4., -46.],
            [22., -41.],
            [42., -25.],
            [49., -2.],
            [42., 23.],
            [22., 42.],
            [-4., 47.],
            [-29., 38.],
            [-46., 18.],
            [-50., 0.],
        ],
        9 => &[
            [-50., 0.],
            [-45., -20.],
            [-30., -38.],
            [0., -48.],
            [30., -38.],
            [45., -20.],
            [50., 0.],
            [35., 10.],
            [18., 12.],
            [16., 38.],
            [0., 46.],
            [-16., 38.],
            [-18., 12.],
            [-35., 10.],
        ],
        _ => &[],
    };
    let source: Vec<[f64; 2]> = if fixed.is_empty() {
        let count = match family {
            3 => 40,
            4 => 36,
            _ => 32,
        };
        (0..count)
            .map(|index| {
                let angle = f64::from(index) / f64::from(count) * TAU;
                if family == 5 {
                    let x = angle.cos();
                    let y = angle.sin();
                    [
                        x.signum() * x.abs().sqrt() * 46.,
                        y.signum() * y.abs().sqrt() * 43.,
                    ]
                } else {
                    let radius = if family == 3 {
                        44. + (angle * 5.).cos() * 5.
                    } else {
                        43. + (angle * 6.).cos() * 7.
                    };
                    [angle.cos() * radius, angle.sin() * radius]
                }
            })
            .collect()
    } else {
        fixed.to_vec()
    };
    let phase = f64::from(seed % 360) * std::f64::consts::PI / 180.;
    let mut points: Vec<[f64; 2]> = source
        .into_iter()
        .map(|[x, y]| {
            [
                x * (1. + phase.sin() * 0.035) + y * (phase * 1.7).sin() * 0.025,
                y * (1. + phase.cos() * 0.025),
            ]
        })
        .collect();
    for _ in 0..if (3..=5).contains(&family) { 1 } else { 2 } {
        points = (0..points.len())
            .flat_map(|index| {
                let [x, y] = points[index];
                let [nx, ny] = points[(index + 1) % points.len()];
                [
                    [x * 0.75 + nx * 0.25, y * 0.75 + ny * 0.25],
                    [x * 0.25 + nx * 0.75, y * 0.25 + ny * 0.75],
                ]
            })
            .collect();
    }
    let number = |value: f64| {
        let value = (value * 100. + 0.5).floor() / 100.;
        if value == 0. {
            "0".into()
        } else {
            value.to_string()
        }
    };
    let mut path = format!("M{} {}", number(points[0][0]), number(points[0][1]));
    let count = points.len();
    for i in 0..count {
        let before = points[(i + count - 1) % count];
        let current = points[i];
        let next = points[(i + 1) % count];
        let after = points[(i + 2) % count];
        path += &format!(
            "C{} {} {} {} {} {}",
            number(current[0] + (next[0] - before[0]) / 6.),
            number(current[1] + (next[1] - before[1]) / 6.),
            number(next[0] - (after[0] - current[0]) / 6.),
            number(next[1] - (after[1] - current[1]) / 6.),
            number(next[0]),
            number(next[1])
        );
    }
    path + "Z"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mascot_geometry_matches_canonical_rakazo_vectors() {
        use sha2::{Digest, Sha256};
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../assets/avatar-vectors.json")).unwrap();
        for entry in vectors["paths"].as_array().unwrap() {
            let seed = entry["seed"].as_u64().unwrap() as u32;
            assert_eq!(
                hex::encode(Sha256::digest(organic_path(seed).as_bytes())),
                entry["sha256"].as_str().unwrap(),
                "seed {seed}"
            );
        }
        for entry in vectors["identities"].as_array().unwrap() {
            assert_eq!(
                u64::from(identity_seed(entry["identity"].as_str().unwrap())),
                entry["seed"].as_u64().unwrap()
            );
        }
    }
    #[test]
    fn generated_avatars_are_pngs_and_identity_specific() {
        let first = render("alfred", "#3EC5A8", "organic").unwrap();
        assert!(first.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(
            first.bytes,
            render("alfred", "#3EC5A8", "organic").unwrap().bytes
        );
        assert_ne!(
            first.bytes,
            render("other", "#3EC5A8", "organic").unwrap().bytes
        );
        for i in 0..10 {
            assert!(organic_path(i).ends_with('Z'));
        }
        assert!(render("robot", "#F2622A", "robot").is_ok());
        for i in 0..8 {
            assert!(render("shape", &format!("#D9508A::shape_{i}"), "organic").is_ok());
        }
    }
    #[test]
    fn uploaded_images_are_preserved_and_external_urls_are_rejected() {
        let image = render("alfred", "#3EC5A8", "organic").unwrap();
        let value = format!("data:image/png;base64,{}", STANDARD.encode(&image.bytes));
        assert_eq!(
            render("unused", &value, "robot").unwrap().bytes,
            image.bytes
        );
        for invalid in [
            "https://example.com/avatar.png",
            "data:image/svg+xml;base64,PHN2Zz4=",
            "data:image/png;base64,aGVsbG8=",
            "#ff0000\"><script>",
        ] {
            assert!(render("agent", invalid, "organic").is_err());
        }
        assert!(render("agent", &"a".repeat(200_001), "organic").is_err());
    }
}
