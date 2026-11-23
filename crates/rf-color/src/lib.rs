//! Color science for Raw Framer. All constants are documented in .claude/COLOR_PIPELINE.md.

pub mod icc;
pub mod lut;
pub mod matrix;
pub mod transfer;
pub mod view;

pub use lut::{CubeLut, Lut1D, Lut3D, LutError};
pub use matrix::{Mat3, Primaries};
pub use view::{LutPlacement, OutputMode, ToneMap, ViewSettings, ViewPipeline};

use rf_core::{Gamut, Transfer};

/// Map a backend gamma name (e.g. from the BRAW SDK) to a [`Transfer`] we know how to handle.
pub fn classify_gamma(name: &str, color_science_gen: Option<u16>) -> Transfer {
    let n = name.to_ascii_lowercase();
    if n.contains("linear") {
        Transfer::Linear
    } else if n.contains("film") && !n.contains("video") {
        // Only Gen 5 "Blackmagic Design Film" has a published formula (BMD Film Gen 5).
        // Gen 4 / Gen 6 film curves are treated as unknown; the app then previews via the
        // SDK's "Linear" gamma instead.
        match color_science_gen {
            Some(5) | None => Transfer::BmdFilmGen5,
            _ => Transfer::Unknown(name.to_string()),
        }
    } else if n.contains("video")
        || n.contains("709")
        || n.contains("2100")
        || n.contains("hybrid")
        || n.contains("st2084")
        || n.contains("custom")
    {
        Transfer::Display(name.to_string())
    } else {
        Transfer::Unknown(name.to_string())
    }
}

/// Map a backend gamut name to a [`Gamut`].
pub fn classify_gamut(name: &str) -> Gamut {
    let n = name.to_ascii_lowercase();
    if n.contains("blackmagic") {
        Gamut::BmdWideGamut
    } else if n.contains("2020") {
        Gamut::Rec2020
    } else if n.contains("709") {
        Gamut::Rec709
    } else if n.contains("p3") && n.contains("d65") {
        Gamut::P3D65
    } else if n.contains("ap0") {
        Gamut::AcesAp0
    } else {
        Gamut::Unknown(name.to_string())
    }
}

pub fn primaries_of(g: &Gamut) -> Option<Primaries> {
    Some(match g {
        Gamut::BmdWideGamut => Primaries::BMD_WIDE_GAMUT,
        Gamut::Rec709 => Primaries::REC709,
        Gamut::Rec2020 => Primaries::REC2020,
        Gamut::P3D65 => Primaries::P3_D65,
        Gamut::AcesAp0 => Primaries::ACES_AP0,
        Gamut::Unknown(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classify() {
        assert_eq!(classify_gamma("Blackmagic Design Film", Some(5)), Transfer::BmdFilmGen5);
        assert!(matches!(classify_gamma("Blackmagic Design Film", Some(4)), Transfer::Unknown(_)));
        assert!(matches!(classify_gamma("Blackmagic Design Extended Video", Some(5)), Transfer::Display(_)));
        assert_eq!(classify_gamma("Linear", Some(5)), Transfer::Linear);
        assert_eq!(classify_gamut("Blackmagic Design"), Gamut::BmdWideGamut);
        assert_eq!(classify_gamut("Rec.709"), Gamut::Rec709);
        assert_eq!(classify_gamut("DCI-P3 D65"), Gamut::P3D65);
        assert!(matches!(classify_gamut("DCI-P3 Theater"), Gamut::Unknown(_)));
        assert!(matches!(classify_gamut("ACES AP1"), Gamut::Unknown(_)));
        assert_eq!(classify_gamut("ACES AP0"), Gamut::AcesAp0);
        assert!(matches!(classify_gamma("Blackmagic Design Film", Some(6)), Transfer::Unknown(_)));
    }
}
