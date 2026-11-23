//! Minimal ICC v2 display profiles (matrix + TRC) for graded exports, so Photoshop,
//! Lightroom and browsers know a file is Rec.709 Gamma 2.4 or sRGB.

use crate::matrix::{bradford, Primaries, D50};
use crate::view::OutputMode;

/// The profile matching a viewer output, or `None` for "Log (as decoded)" (no standard space).
pub fn display_profile(output: OutputMode) -> Option<Vec<u8>> {
    match output {
        OutputMode::Rec709G24 => Some(build("Rec.709 Gamma 2.4 (Raw Framer)", Trc::Gamma(2.4))),
        OutputMode::Srgb => Some(build("sRGB (Raw Framer)", Trc::Srgb)),
        OutputMode::AsDecoded => None,
    }
}

enum Trc {
    Gamma(f64),
    Srgb,
}

fn s15(v: f64) -> [u8; 4] {
    ((v * 65536.0).round() as i32).to_be_bytes()
}

fn xyz_tag(x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut d = b"XYZ \0\0\0\0".to_vec();
    d.extend(s15(x));
    d.extend(s15(y));
    d.extend(s15(z));
    d
}

fn curve_tag(t: &Trc) -> Vec<u8> {
    let mut d = b"curv\0\0\0\0".to_vec();
    match t {
        Trc::Gamma(g) => {
            d.extend(1u32.to_be_bytes());
            d.extend(((g * 256.0).round() as u16).to_be_bytes());
        }
        Trc::Srgb => {
            const N: usize = 1024;
            d.extend((N as u32).to_be_bytes());
            for i in 0..N {
                let v = i as f64 / (N - 1) as f64;
                let l = if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
                d.extend(((l * 65535.0).round() as u16).to_be_bytes());
            }
        }
    }
    d
}

fn desc_tag(s: &str) -> Vec<u8> {
    let mut d = b"desc\0\0\0\0".to_vec();
    d.extend((s.len() as u32 + 1).to_be_bytes());
    d.extend(s.as_bytes());
    d.push(0);
    d.extend([0u8; 4 + 4]); // Unicode language code + count
    d.extend([0u8; 2 + 1 + 67]); // ScriptCode code + count + 67 bytes
    d
}

fn text_tag(s: &str) -> Vec<u8> {
    let mut d = b"text\0\0\0\0".to_vec();
    d.extend(s.as_bytes());
    d.push(0);
    d
}

fn build(name: &str, trc: Trc) -> Vec<u8> {
    // Colorants: Rec.709 RGB -> XYZ, Bradford-adapted to the D50 PCS.
    let m = bradford(Primaries::REC709.white, D50).mul(&Primaries::REC709.rgb_to_xyz()).0;
    let d50 = [0.9642, 1.0, 0.8249];
    let curve = curve_tag(&trc);
    // (signature, data); the three TRCs share one data block.
    let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"desc", desc_tag(name)),
        (b"cprt", text_tag("No copyright, use freely")),
        (b"wtpt", xyz_tag(d50[0], d50[1], d50[2])),
        (b"rXYZ", xyz_tag(m[0][0], m[1][0], m[2][0])),
        (b"gXYZ", xyz_tag(m[0][1], m[1][1], m[2][1])),
        (b"bXYZ", xyz_tag(m[0][2], m[1][2], m[2][2])),
        (b"rTRC", curve),
    ];
    let n_entries = tags.len() as u32 + 2; // gTRC and bTRC point at rTRC's data
    let mut table = Vec::new();
    let mut data = Vec::new();
    let mut offset = 128 + 4 + 12 * n_entries;
    let mut trc_at = (0u32, 0u32);
    for (sig, d) in &tags {
        table.extend(*sig);
        table.extend(offset.to_be_bytes());
        table.extend((d.len() as u32).to_be_bytes());
        if *sig == b"rTRC" {
            trc_at = (offset, d.len() as u32);
        }
        data.extend(d);
        while data.len() % 4 != 0 {
            data.push(0);
        }
        offset = 128 + 4 + 12 * n_entries + data.len() as u32;
    }
    for sig in [b"gTRC", b"bTRC"] {
        table.extend(sig);
        table.extend(trc_at.0.to_be_bytes());
        table.extend(trc_at.1.to_be_bytes());
    }
    let size = 128 + 4 + table.len() as u32 + data.len() as u32;
    let mut h = Vec::with_capacity(size as usize);
    h.extend(size.to_be_bytes());
    h.extend([0u8; 4]); // preferred CMM
    h.extend([0x02, 0x10, 0x00, 0x00]); // version 2.1
    h.extend(b"mntr");
    h.extend(b"RGB ");
    h.extend(b"XYZ ");
    h.extend([0u8; 12]); // creation date (unset)
    h.extend(b"acsp");
    h.extend([0u8; 4 + 4 + 4 + 4 + 8 + 4]); // platform, flags, manufacturer, model, attributes, intent
    h.extend(s15(d50[0]));
    h.extend(s15(d50[1]));
    h.extend(s15(d50[2]));
    h.extend([0u8; 4]); // creator
    h.extend([0u8; 44]); // profile ID (v4) + reserved
    debug_assert_eq!(h.len(), 128);
    h.extend((tags.len() as u32 + 2).to_be_bytes());
    h.extend(table);
    h.extend(data);
    debug_assert_eq!(h.len() as u32, size);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_well_formed() {
        for o in [OutputMode::Rec709G24, OutputMode::Srgb] {
            let p = display_profile(o).unwrap();
            assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()) as usize, p.len());
            assert_eq!(&p[36..40], b"acsp");
            let n = u32::from_be_bytes(p[128..132].try_into().unwrap()) as usize;
            assert_eq!(n, 9);
            for i in 0..n {
                let e = &p[132 + i * 12..144 + i * 12];
                let off = u32::from_be_bytes(e[4..8].try_into().unwrap()) as usize;
                let len = u32::from_be_bytes(e[8..12].try_into().unwrap()) as usize;
                assert!(off % 4 == 0 && off + len <= p.len(), "tag {i} out of range");
            }
            // Colorants sum to the D50 white (within s15Fixed16 rounding).
            let find = |sig: &[u8]| {
                let i = (0..n).find(|i| &p[132 + i * 12..136 + i * 12] == sig).unwrap();
                let off = u32::from_be_bytes(p[136 + i * 12..140 + i * 12].try_into().unwrap()) as usize;
                (0..3).map(|k| i32::from_be_bytes(p[off + 8 + k * 4..off + 12 + k * 4].try_into().unwrap()) as f64 / 65536.0).collect::<Vec<_>>()
            };
            let (r, g, b) = (find(b"rXYZ"), find(b"gXYZ"), find(b"bXYZ"));
            for (k, w) in [0.9642, 1.0, 0.8249].iter().enumerate() {
                assert!((r[k] + g[k] + b[k] - w).abs() < 2e-3, "white {k}");
            }
        }
        assert!(display_profile(OutputMode::AsDecoded).is_none());
    }
}
