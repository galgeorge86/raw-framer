//! 3x3 matrices, gamut primaries and chromatic adaptation (f64 for precision).

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3(pub [[f64; 3]; 3]);

impl Mat3 {
    pub const IDENTITY: Mat3 = Mat3([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub fn mul(&self, o: &Mat3) -> Mat3 {
        let mut r = [[0.0; 3]; 3];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| self.0[i][k] * o.0[k][j]).sum();
            }
        }
        Mat3(r)
    }

    pub fn apply(&self, v: [f64; 3]) -> [f64; 3] {
        let m = &self.0;
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    }

    pub fn apply_f32(&self, v: [f32; 3]) -> [f32; 3] {
        let r = self.apply([v[0] as f64, v[1] as f64, v[2] as f64]);
        [r[0] as f32, r[1] as f32, r[2] as f32]
    }

    pub fn det(&self) -> f64 {
        let m = &self.0;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    pub fn inverse(&self) -> Mat3 {
        let m = &self.0;
        let d = self.det();
        assert!(d.abs() > 1e-12, "singular matrix");
        let inv = 1.0 / d;
        Mat3([
            [
                (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv,
                (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv,
                (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv,
            ],
            [
                (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv,
                (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv,
                (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv,
            ],
            [
                (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv,
                (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv,
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv,
            ],
        ])
    }

    /// Row-major flattened.
    pub fn flat(&self) -> [f64; 9] {
        let m = &self.0;
        [m[0][0], m[0][1], m[0][2], m[1][0], m[1][1], m[1][2], m[2][0], m[2][1], m[2][2]]
    }

    /// Column-major, padded to vec4 columns (WGSL `mat3x3<f32>` uniform layout).
    pub fn to_wgsl_cols(&self) -> [[f32; 4]; 3] {
        let m = &self.0;
        [
            [m[0][0] as f32, m[1][0] as f32, m[2][0] as f32, 0.0],
            [m[0][1] as f32, m[1][1] as f32, m[2][1] as f32, 0.0],
            [m[0][2] as f32, m[1][2] as f32, m[2][2] as f32, 0.0],
        ]
    }
}

/// CIE 1931 xy chromaticities of a gamut.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Primaries {
    pub r: (f64, f64),
    pub g: (f64, f64),
    pub b: (f64, f64),
    pub white: (f64, f64),
}

pub const D65: (f64, f64) = (0.3127, 0.3290);
pub const D50: (f64, f64) = (0.3457, 0.3585);

impl Primaries {
    /// Blackmagic Wide Gamut (Gen 4/5), from the Gen 5 Color Science Technical Reference.
    pub const BMD_WIDE_GAMUT: Primaries = Primaries {
        r: (0.7177215, 0.3171181),
        g: (0.2280410, 0.8615690),
        b: (0.1005841, -0.0820452),
        white: (0.3127170, 0.3290312),
    };
    pub const REC709: Primaries = Primaries { r: (0.64, 0.33), g: (0.30, 0.60), b: (0.15, 0.06), white: D65 };
    pub const REC2020: Primaries = Primaries { r: (0.708, 0.292), g: (0.170, 0.797), b: (0.131, 0.046), white: D65 };
    pub const P3_D65: Primaries = Primaries { r: (0.680, 0.320), g: (0.265, 0.690), b: (0.150, 0.060), white: D65 };
    pub const ACES_AP0: Primaries = Primaries {
        r: (0.7347, 0.2653),
        g: (0.0, 1.0),
        b: (0.0001, -0.0770),
        white: (0.32168, 0.33767),
    };

    /// Normalised primary matrix: linear RGB -> XYZ (Y of white = 1).
    pub fn rgb_to_xyz(&self) -> Mat3 {
        let xyz = |(x, y): (f64, f64)| [x / y, 1.0, (1.0 - x - y) / y];
        let (r, g, b) = (xyz(self.r), xyz(self.g), xyz(self.b));
        let p = Mat3([[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]]);
        let w = xyz(self.white);
        let s = p.inverse().apply(w);
        Mat3([
            [p.0[0][0] * s[0], p.0[0][1] * s[1], p.0[0][2] * s[2]],
            [p.0[1][0] * s[0], p.0[1][1] * s[1], p.0[1][2] * s[2]],
            [p.0[2][0] * s[0], p.0[2][1] * s[1], p.0[2][2] * s[2]],
        ])
    }
}

/// Bradford chromatic adaptation from one white point to another (XYZ -> XYZ).
pub fn bradford(src: (f64, f64), dst: (f64, f64)) -> Mat3 {
    let b = Mat3([
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ]);
    let xyz = |(x, y): (f64, f64)| [x / y, 1.0, (1.0 - x - y) / y];
    let s = b.apply(xyz(src));
    let d = b.apply(xyz(dst));
    let scale = Mat3([[d[0] / s[0], 0.0, 0.0], [0.0, d[1] / s[1], 0.0], [0.0, 0.0, d[2] / s[2]]]);
    b.inverse().mul(&scale).mul(&b)
}

fn same_white(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
}

/// Linear RGB (src) -> linear RGB (dst), with Bradford adaptation if white points differ.
pub fn rgb_to_rgb(src: &Primaries, dst: &Primaries) -> Mat3 {
    let to_xyz = src.rgb_to_xyz();
    let from_xyz = dst.rgb_to_xyz().inverse();
    if same_white(src.white, dst.white) {
        from_xyz.mul(&to_xyz)
    } else {
        from_xyz.mul(&bradford(src.white, dst.white)).mul(&to_xyz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmd_wg_matches_published_matrix() {
        // Published BMD WG RGB->XYZ (Gen 5 reference, 6 decimals).
        let published = [
            [0.606530, 0.220408, 0.123479],
            [0.267989, 0.832731, -0.100720],
            [-0.029442, -0.086611, 1.204861],
        ];
        let m = Primaries::BMD_WIDE_GAMUT.rgb_to_xyz();
        for i in 0..3 {
            for j in 0..3 {
                assert!((m.0[i][j] - published[i][j]).abs() < 2e-6, "{i}{j}: {} vs {}", m.0[i][j], published[i][j]);
            }
        }
    }

    #[test]
    fn rec709_known_matrix() {
        let m = Primaries::REC709.rgb_to_xyz();
        assert!((m.0[0][0] - 0.4124).abs() < 1e-3);
        assert!((m.0[1][1] - 0.7152).abs() < 1e-3);
        assert!((m.0[2][2] - 0.9505).abs() < 1e-3);
    }

    #[test]
    fn white_maps_to_white() {
        let m = rgb_to_rgb(&Primaries::BMD_WIDE_GAMUT, &Primaries::REC709);
        let w = m.apply([1.0, 1.0, 1.0]);
        for c in w {
            assert!((c - 1.0).abs() < 1e-3, "{w:?}");
        }
        let m = rgb_to_rgb(&Primaries::ACES_AP0, &Primaries::REC709);
        let w = m.apply([1.0, 1.0, 1.0]);
        for c in w {
            assert!((c - 1.0).abs() < 1e-3, "{w:?}");
        }
    }

    #[test]
    fn inverse_roundtrip() {
        let m = Primaries::BMD_WIDE_GAMUT.rgb_to_xyz();
        let i = m.mul(&m.inverse());
        for r in 0..3 {
            for c in 0..3 {
                assert!((i.0[r][c] - if r == c { 1.0 } else { 0.0 }).abs() < 1e-12);
            }
        }
    }
}
