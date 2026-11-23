//! Plain display-referred RGB TIFF (8 or 16 bit) for graded exports, with an optional
//! embedded ICC profile. Strips, Deflate + horizontal predictor.

use crate::tiff::*;
use crate::{hdiff_u16_row, DngError};
use rayon::prelude::*;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct RgbTiffMeta {
    pub description: Option<String>,
    pub software: Option<String>,
    /// "YYYY:MM:DD HH:MM:SS"
    pub date_time: Option<String>,
    pub icc_profile: Option<Vec<u8>>,
}

const ROWS_PER_STRIP: u32 = 32;

/// Encode a 16-bit interleaved RGB image as a little-endian TIFF.
pub fn encode_rgb16_tiff(width: u32, height: u32, rgb: &[u16], meta: &RgbTiffMeta, compress: bool) -> Result<Vec<u8>, DngError> {
    if rgb.len() != (width * height * 3) as usize {
        return Err(DngError::Invalid(format!("RGB buffer is {} samples, expected {}", rgb.len(), width * height * 3)));
    }
    let row_len = width as usize * 3;
    let strips: Vec<Vec<u8>> = rgb
        .par_chunks(row_len * ROWS_PER_STRIP as usize)
        .map(|chunk| {
            let mut b: Vec<u8> = chunk.iter().flat_map(|v| v.to_le_bytes()).collect();
            if !compress {
                return b;
            }
            for row in b.chunks_mut(row_len * 2) {
                hdiff_u16_row(row, 3);
            }
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
            z.write_all(&b).expect("in-memory write");
            z.finish().expect("in-memory write")
        })
        .collect();

    let mut ifd = Ifd::default();
    ifd.longs(256, &[width]);
    ifd.longs(257, &[height]);
    ifd.shorts(258, &[16, 16, 16]);
    ifd.shorts(259, &[if compress { 8 } else { 1 }]);
    ifd.shorts(262, &[2]); // RGB
    if let Some(d) = &meta.description {
        ifd.ascii(270, d);
    }
    ifd.shorts(274, &[1]);
    ifd.shorts(277, &[3]);
    ifd.longs(278, &[ROWS_PER_STRIP]);
    ifd.shorts(284, &[1]);
    ifd.rationals(282, &[(72, 1)]);
    ifd.rationals(283, &[(72, 1)]);
    ifd.shorts(296, &[2]);
    if let Some(s) = &meta.software {
        ifd.ascii(305, s);
    }
    if let Some(d) = &meta.date_time {
        ifd.ascii(306, d);
    }
    if compress {
        ifd.shorts(317, &[2]);
    }
    if let Some(icc) = &meta.icc_profile {
        ifd.undefined(34675, icc);
    }
    // Placeholders sized correctly; real offsets are filled in once the IFD size is known.
    let n = strips.len();
    ifd.longs(273, &vec![0; n]);
    ifd.longs(279, &strips.iter().map(|s| s.len() as u32).collect::<Vec<_>>());
    let ifd_at = 8u32;
    let mut data_at = ifd_at + ifd.byte_size();
    let mut offsets = Vec::with_capacity(n);
    for s in &strips {
        offsets.push(data_at);
        data_at += (s.len() as u32 + 1) & !1;
    }
    ifd.longs(273, &offsets);

    let total: usize = strips.iter().map(|s| s.len() + 1).sum::<usize>() + ifd.byte_size() as usize + 8;
    let mut out = Vec::with_capacity(total);
    out.extend(b"II");
    out.extend(42u16.to_le_bytes());
    out.extend(ifd_at.to_le_bytes());
    out.extend(ifd.serialize(ifd_at, 0));
    for s in &strips {
        out.extend(s);
        if s.len() % 2 == 1 {
            out.push(0);
        }
    }
    Ok(out)
}

/// Write bytes to `path` atomically (via `<path>.part` + rename). Returns the file size.
pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<u64, DngError> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let res = (|| {
        let mut f = std::fs::File::create(&part)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&part, path)?;
        Ok(bytes.len() as u64)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read;

    #[test]
    fn rgb16_round_trip() {
        let (w, h) = (37u32, 70u32);
        let px: Vec<u16> = (0..w * h * 3).map(|i| (i * 977 % 65536) as u16).collect();
        for compress in [false, true] {
            let meta = RgbTiffMeta { description: Some("test".into()), icc_profile: Some(vec![1, 2, 3, 4, 5]), ..Default::default() };
            let b = encode_rgb16_tiff(w, h, &px, &meta, compress).unwrap();
            let ifd = read::read_ifd0(&b).unwrap();
            assert_eq!(ifd.get(&256).unwrap().ints()[0], w);
            assert_eq!(ifd.get(&34675).unwrap().raw, vec![1, 2, 3, 4, 5]);
            let offs = ifd.get(&273).unwrap().ints();
            let lens = ifd.get(&279).unwrap().ints();
            let mut got = Vec::new();
            for (o, l) in offs.iter().zip(lens) {
                let s = &b[*o as usize..(*o + l) as usize];
                let mut raw = if compress {
                    let mut d = Vec::new();
                    std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(s), &mut d).unwrap();
                    d
                } else {
                    s.to_vec()
                };
                if compress {
                    for row in raw.chunks_mut(w as usize * 6) {
                        crate::hundiff_u16_row(row, 3);
                    }
                }
                got.extend(raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
            }
            assert_eq!(got, px);
        }
    }
}
