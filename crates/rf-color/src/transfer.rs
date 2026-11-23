//! Transfer functions (OETFs) and their inverses.

/// Blackmagic Film Generation 5 constants (Blackmagic Gen 5 Color Science Technical Reference, 2021).
pub mod film_gen5 {
    pub const A: f64 = 0.08692876065491224;
    pub const B: f64 = 0.005494072432257808;
    pub const C: f64 = 0.5300133392291939;
    pub const D: f64 = 8.283605932402494;
    pub const E: f64 = 0.09246575342465753;
    pub const LIN_CUT: f64 = 0.005;
    pub const LOG_CUT: f64 = D * LIN_CUT + E;
    /// Scene-linear value encoded as code value 1.0.
    pub const LINEAR_MAX: f64 = 222.86;
}

#[inline]
pub fn film_gen5_encode(x: f32) -> f32 {
    use film_gen5::*;
    if (x as f64) < LIN_CUT {
        (D as f32) * x + E as f32
    } else {
        (A as f32) * (x + B as f32).ln() + C as f32
    }
}

#[inline]
pub fn film_gen5_decode(y: f32) -> f32 {
    use film_gen5::*;
    if (y as f64) < LOG_CUT {
        (y - E as f32) / D as f32
    } else {
        ((y - C as f32) / A as f32).exp() - B as f32
    }
}

/// f64 versions for exact export math.
pub fn film_gen5_decode_f64(y: f64) -> f64 {
    use film_gen5::*;
    if y < LOG_CUT {
        (y - E) / D
    } else {
        ((y - C) / A).exp() - B
    }
}

#[inline]
pub fn srgb_encode(l: f32) -> f32 {
    let l = l.max(0.0);
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

#[inline]
pub fn gamma_encode(l: f32, gamma: f32) -> f32 {
    l.max(0.0).powf(1.0 / gamma)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gen5_reference_values() {
        // Values from the Blackmagic mapping table.
        assert!((film_gen5_encode(0.0) - 0.092_465_75).abs() < 1e-6);
        assert!((film_gen5_encode(0.18) - 0.383_561_64).abs() < 1e-5);
        assert!((film_gen5_encode(1.0) - 0.530_489_6).abs() < 1e-5);
        assert!((film_gen5_encode(10.0) - 0.730_222).abs() < 1e-5);
        assert!((film_gen5_encode(100.0) - 0.930_339_9).abs() < 1e-5);
        assert!((film_gen5_decode_f64(1.0) - 222.86).abs() < 0.05);
    }
    #[test]
    fn gen5_roundtrip() {
        for i in 0..2000 {
            let x = (i as f32 / 2000.0).powi(3) * 200.0;
            let y = film_gen5_encode(x);
            let back = film_gen5_decode(y);
            assert!((back - x).abs() <= 1e-4 * x.max(1.0), "{x} -> {y} -> {back}");
        }
    }
}
