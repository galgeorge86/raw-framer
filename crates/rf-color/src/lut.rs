//! `.cube` LUT parsing (Adobe / Resolve flavours) and interpolation.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum LutError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("line {line}: {msg}")]
    Parse { line: usize, msg: String },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct Lut1D {
    pub size: usize,
    pub data: Vec<[f32; 3]>,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct Lut3D {
    pub size: usize,
    /// Red index fastest: data[r + g*n + b*n*n].
    pub data: Vec<[f32; 3]>,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct CubeLut {
    pub title: String,
    pub name: String,
    pub lut1d: Option<Lut1D>,
    pub lut3d: Option<Lut3D>,
}

impl CubeLut {
    pub fn load(path: &Path) -> Result<Self, LutError> {
        let text = std::fs::read_to_string(path)?;
        let mut lut = Self::parse(&text)?;
        lut.name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(lut)
    }

    pub fn parse(text: &str) -> Result<Self, LutError> {
        let mut title = String::new();
        let mut size1 = 0usize;
        let mut size3 = 0usize;
        let mut dmin = [0.0f32; 3];
        let mut dmax = [1.0f32; 3];
        let mut range1: Option<(f32, f32)> = None;
        let mut range3: Option<(f32, f32)> = None;
        let mut values: Vec<[f32; 3]> = Vec::new();

        for (i, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let err = |msg: &str| LutError::Parse { line: i + 1, msg: msg.to_string() };
            let mut it = line.split_whitespace();
            let first = it.next().unwrap();
            let floats = |it: std::str::SplitWhitespace| -> Result<Vec<f32>, LutError> {
                it.map(|t| t.parse::<f32>().map_err(|_| err("bad number"))).collect()
            };
            match first.to_ascii_uppercase().as_str() {
                "TITLE" => title = line[5..].trim().trim_matches('"').to_string(),
                "LUT_1D_SIZE" => size1 = it.next().and_then(|v| v.parse().ok()).ok_or_else(|| err("bad LUT_1D_SIZE"))?,
                "LUT_3D_SIZE" => size3 = it.next().and_then(|v| v.parse().ok()).ok_or_else(|| err("bad LUT_3D_SIZE"))?,
                "DOMAIN_MIN" => {
                    let v = floats(it)?;
                    if v.len() != 3 {
                        return Err(err("DOMAIN_MIN needs 3 values"));
                    }
                    dmin = [v[0], v[1], v[2]];
                }
                "DOMAIN_MAX" => {
                    let v = floats(it)?;
                    if v.len() != 3 {
                        return Err(err("DOMAIN_MAX needs 3 values"));
                    }
                    dmax = [v[0], v[1], v[2]];
                }
                "LUT_1D_INPUT_RANGE" => {
                    let v = floats(it)?;
                    if v.len() == 2 {
                        range1 = Some((v[0], v[1]));
                    }
                }
                "LUT_3D_INPUT_RANGE" => {
                    let v = floats(it)?;
                    if v.len() == 2 {
                        range3 = Some((v[0], v[1]));
                    }
                }
                _ => {
                    if first.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+' || c == '.') {
                        let mut v = vec![first.parse::<f32>().map_err(|_| err("bad number"))?];
                        v.extend(floats(it)?);
                        if v.len() != 3 {
                            return Err(err("data line needs 3 values"));
                        }
                        values.push([v[0], v[1], v[2]]);
                    }
                    // Unknown keywords are ignored for compatibility.
                }
            }
        }

        if size1 == 0 && size3 == 0 {
            return Err(LutError::Invalid("no LUT_1D_SIZE or LUT_3D_SIZE".into()));
        }
        if size3 != 0 && !(2..=256).contains(&size3) {
            return Err(LutError::Invalid(format!("unsupported 3D size {size3}")));
        }
        let need = size1 + size3 * size3 * size3;
        if values.len() != need {
            return Err(LutError::Invalid(format!("expected {need} entries, found {}", values.len())));
        }
        let r3 = |r: Option<(f32, f32)>, dmin, dmax| match r {
            Some((a, b)) => ([a; 3], [b; 3]),
            None => (dmin, dmax),
        };
        let lut1d = (size1 > 0).then(|| {
            let (mn, mx) = r3(range1, dmin, dmax);
            Lut1D { size: size1, data: values[..size1].to_vec(), domain_min: mn, domain_max: mx }
        });
        let lut3d = (size3 > 0).then(|| {
            // With a Resolve shaper, the 3D LUT's input is the 1D output in [0,1] unless specified.
            let (mn, mx) = if size1 > 0 {
                r3(range3, [0.0; 3], [1.0; 3])
            } else {
                r3(range3, dmin, dmax)
            };
            Lut3D { size: size3, data: values[size1..].to_vec(), domain_min: mn, domain_max: mx }
        });
        Ok(CubeLut { title, name: String::new(), lut1d, lut3d })
    }

    pub fn apply(&self, mut rgb: [f32; 3]) -> [f32; 3] {
        if let Some(l) = &self.lut1d {
            rgb = l.apply(rgb);
        }
        if let Some(l) = &self.lut3d {
            rgb = l.apply(rgb);
        }
        rgb
    }

    pub fn description(&self) -> String {
        let mut s = String::new();
        if let Some(l) = &self.lut1d {
            s += &format!("1D {} ", l.size);
        }
        if let Some(l) = &self.lut3d {
            s += &format!("3D {}³", l.size);
        }
        s.trim().to_string()
    }
}

impl Lut1D {
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let mut out = [0.0; 3];
        for c in 0..3 {
            let t = ((rgb[c] - self.domain_min[c]) / (self.domain_max[c] - self.domain_min[c])).clamp(0.0, 1.0)
                * (n - 1) as f32;
            let i0 = (t.floor() as usize).min(n - 1);
            let i1 = (i0 + 1).min(n - 1);
            let f = t - i0 as f32;
            out[c] = self.data[i0][c] * (1.0 - f) + self.data[i1][c] * f;
        }
        out
    }
}

impl Lut3D {
    #[inline]
    fn at(&self, r: usize, g: usize, b: usize) -> [f32; 3] {
        self.data[r + g * self.size + b * self.size * self.size]
    }

    /// Tetrahedral interpolation (matches Resolve's "Tetrahedral" 3D LUT interpolation).
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let m = (n - 1) as f32;
        let mut idx = [0usize; 3];
        let mut f = [0f32; 3];
        for c in 0..3 {
            let t = ((rgb[c] - self.domain_min[c]) / (self.domain_max[c] - self.domain_min[c])).clamp(0.0, 1.0) * m;
            let i = (t.floor() as usize).min(n - 2);
            idx[c] = i;
            f[c] = t - i as f32;
        }
        let (r, g, b) = (idx[0], idx[1], idx[2]);
        let (fx, fy, fz) = (f[0], f[1], f[2]);
        let c000 = self.at(r, g, b);
        let c111 = self.at(r + 1, g + 1, b + 1);
        let lerp = |a: [f32; 3], b2: [f32; 3], c: [f32; 3], d: [f32; 3], w1: f32, w2: f32, w3: f32| {
            let mut o = [0.0; 3];
            for k in 0..3 {
                o[k] = a[k] + w1 * (b2[k] - a[k]) + w2 * (c[k] - b2[k]) + w3 * (d[k] - c[k]);
            }
            o
        };
        if fx > fy {
            if fy > fz {
                lerp(c000, self.at(r + 1, g, b), self.at(r + 1, g + 1, b), c111, fx, fy, fz)
            } else if fx > fz {
                lerp(c000, self.at(r + 1, g, b), self.at(r + 1, g, b + 1), c111, fx, fz, fy)
            } else {
                lerp(c000, self.at(r, g, b + 1), self.at(r + 1, g, b + 1), c111, fz, fx, fy)
            }
        } else if fz > fy {
            lerp(c000, self.at(r, g, b + 1), self.at(r, g + 1, b + 1), c111, fz, fy, fx)
        } else if fz > fx {
            lerp(c000, self.at(r, g + 1, b), self.at(r, g + 1, b + 1), c111, fy, fz, fx)
        } else {
            lerp(c000, self.at(r, g + 1, b), self.at(r + 1, g + 1, b), c111, fy, fx, fz)
        }
    }

    pub fn identity(n: usize) -> Lut3D {
        let mut data = Vec::with_capacity(n * n * n);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let s = (n - 1) as f32;
                    data.push([r as f32 / s, g as f32 / s, b as f32 / s]);
                }
            }
        }
        Lut3D { size: n, data, domain_min: [0.0; 3], domain_max: [1.0; 3] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube_text(n: usize, f: impl Fn([f32; 3]) -> [f32; 3]) -> String {
        let mut s = format!("TITLE \"test\"\n# comment\nLUT_3D_SIZE {n}\n");
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let d = (n - 1) as f32;
                    let o = f([r as f32 / d, g as f32 / d, b as f32 / d]);
                    s += &format!("{} {} {}\n", o[0], o[1], o[2]);
                }
            }
        }
        s
    }

    #[test]
    fn identity_is_identity() {
        let lut = CubeLut::parse(&cube_text(33, |c| c)).unwrap();
        assert_eq!(lut.title, "test");
        for v in [[0.1, 0.5, 0.9], [0.33, 0.33, 0.33], [1.0, 0.0, 0.25], [0.77, 0.12, 0.5]] {
            let o = lut.apply(v);
            for c in 0..3 {
                assert!((o[c] - v[c]).abs() < 1e-5, "{v:?} -> {o:?}");
            }
        }
    }

    #[test]
    fn tetrahedral_exact_on_linear_function() {
        // Tetrahedral interpolation reproduces any affine function exactly.
        let f = |c: [f32; 3]| [0.5 * c[0] + 0.2 * c[1], 0.1 + 0.3 * c[2], c[1] * 0.9];
        let lut = CubeLut::parse(&cube_text(17, f)).unwrap();
        for v in [[0.13, 0.57, 0.91], [0.9, 0.1, 0.4], [0.4, 0.4, 0.8]] {
            let o = lut.apply(v);
            let e = f(v);
            for c in 0..3 {
                assert!((o[c] - e[c]).abs() < 1e-5, "{v:?}: {o:?} vs {e:?}");
            }
        }
    }

    #[test]
    fn swaps_channels_red_fastest() {
        let lut = CubeLut::parse(&cube_text(2, |c| [c[2], c[1], c[0]])).unwrap();
        let o = lut.apply([1.0, 0.0, 0.0]);
        assert!((o[2] - 1.0).abs() < 1e-6 && o[0].abs() < 1e-6);
    }

    #[test]
    fn rejects_wrong_count() {
        assert!(CubeLut::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err());
    }
}
