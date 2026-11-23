//! A small reader for the Linear DNGs this crate writes: finds the raw IFD (in IFD0 or a
//! SubIFD), decodes its tiles to f32 and exposes the tags. Used to verify exports end to
//! end; it is not a general TIFF reader.

use std::collections::BTreeMap;
use std::io::Read;

pub struct Tag {
    pub typ: u16,
    pub count: u32,
    pub raw: Vec<u8>,
}

impl Tag {
    /// BYTE/SHORT/LONG values.
    pub fn ints(&self) -> Vec<u32> {
        match self.typ {
            1 => self.raw.iter().map(|&b| b as u32).collect(),
            3 => self.raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32).collect(),
            4 => self.raw.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
            _ => Vec::new(),
        }
    }
    /// RATIONAL/SRATIONAL values.
    pub fn reals(&self) -> Vec<f64> {
        match self.typ {
            5 => self.raw.chunks(8).map(|c| u32::from_le_bytes(c[0..4].try_into().unwrap()) as f64 / u32::from_le_bytes(c[4..8].try_into().unwrap()).max(1) as f64).collect(),
            10 => self.raw.chunks(8).map(|c| i32::from_le_bytes(c[0..4].try_into().unwrap()) as f64 / i32::from_le_bytes(c[4..8].try_into().unwrap()).max(1) as f64).collect(),
            _ => self.ints().into_iter().map(|v| v as f64).collect(),
        }
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.raw).trim_end_matches('\0').to_string()
    }
}

pub type Ifd = BTreeMap<u16, Tag>;

pub struct DngRead {
    pub ifd0: Ifd,
    /// The IFD holding the raw (LinearRaw) image.
    pub raw_ifd: Ifd,
    pub exif: Option<Ifd>,
    pub width: u32,
    pub height: u32,
    /// Interleaved RGB samples as stored (0..1 after the writer's normalisation).
    pub pixels: Vec<f32>,
}

impl DngRead {
    /// A tag from the raw IFD, falling back to IFD0 (DNG puts color tags in IFD0).
    pub fn tag(&self, t: u16) -> Option<&Tag> {
        self.raw_ifd.get(&t).or_else(|| self.ifd0.get(&t))
    }
    pub fn reals(&self, t: u16) -> Vec<f64> {
        self.tag(t).map(|t| t.reals()).unwrap_or_default()
    }
    /// Exif ISOSpeedRatings.
    pub fn exif_iso(&self) -> Option<u32> {
        self.exif.as_ref()?.get(&34855).and_then(|t| t.ints().first().copied())
    }
}

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn type_size(t: u16) -> Result<usize, String> {
    Ok(match t {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return Err(format!("bad TIFF type {t}")),
    })
}

fn read_ifd(b: &[u8], off: usize) -> Result<Ifd, String> {
    if off + 2 > b.len() {
        return Err("IFD offset out of range".into());
    }
    let n = rd16(b, off) as usize;
    let mut tags = BTreeMap::new();
    for i in 0..n {
        let e = off + 2 + i * 12;
        if e + 12 > b.len() {
            return Err("IFD truncated".into());
        }
        let tag = rd16(b, e);
        let typ = rd16(b, e + 2);
        let count = rd32(b, e + 4);
        let size = type_size(typ)? * count as usize;
        let raw = if size <= 4 {
            b[e + 8..e + 8 + size].to_vec()
        } else {
            let o = rd32(b, e + 8) as usize;
            b.get(o..o + size).ok_or(format!("tag {tag} out of bounds"))?.to_vec()
        };
        tags.insert(tag, Tag { typ, count, raw });
    }
    Ok(tags)
}

fn first(ifd: &Ifd, t: u16) -> Option<u32> {
    ifd.get(&t).and_then(|t| t.ints().first().copied())
}

/// IFD0 of any little-endian TIFF (used to check graded TIFF exports).
pub fn read_ifd0(b: &[u8]) -> Result<Ifd, String> {
    if b.len() < 8 || &b[0..4] != b"II*\0" {
        return Err("not a little-endian TIFF".into());
    }
    read_ifd(b, rd32(b, 4) as usize)
}

pub fn read(b: &[u8]) -> Result<DngRead, String> {
    if b.len() < 8 || &b[0..4] != b"II*\0" {
        return Err("not a little-endian TIFF".into());
    }
    let ifd0 = read_ifd(b, rd32(b, 4) as usize)?;
    let is_raw = |ifd: &Ifd| first(ifd, 254).unwrap_or(0) == 0 && first(ifd, 262) == Some(34892);
    let mut raw_ifd = None;
    if is_raw(&ifd0) {
        raw_ifd = Some(read_ifd(b, rd32(b, 4) as usize)?);
    } else if let Some(subs) = ifd0.get(&330) {
        for o in subs.ints() {
            let s = read_ifd(b, o as usize)?;
            if is_raw(&s) {
                raw_ifd = Some(s);
                break;
            }
        }
    }
    let raw_ifd = raw_ifd.ok_or("no raw (LinearRaw) IFD")?;
    let exif = match first(&ifd0, 34665) {
        Some(o) if o != 0 => Some(read_ifd(b, o as usize)?),
        _ => None,
    };
    let (width, height, pixels) = decode_raw(b, &raw_ifd)?;
    Ok(DngRead { ifd0, raw_ifd, exif, width, height, pixels })
}

fn decode_raw(b: &[u8], tags: &Ifd) -> Result<(u32, u32, Vec<f32>), String> {
    let need = |t: u16| first(tags, t).ok_or(format!("raw IFD lacks tag {t}"));
    let (w, h) = (need(256)?, need(257)?);
    let bps = need(258)? as usize / 8;
    let comp = need(259)?;
    let fmt = first(tags, 339).unwrap_or(1);
    let pred = first(tags, 317).unwrap_or(1);
    let ts = need(322)?;
    let offs = tags.get(&324).map(|t| t.ints()).ok_or("no tile offsets")?;
    let counts = tags.get(&325).map(|t| t.ints()).ok_or("no tile byte counts")?;
    let tx = w.div_ceil(ts);
    let mut out = vec![0f32; (w * h * 3) as usize];
    for (i, (&o, &c)) in offs.iter().zip(&counts).enumerate() {
        let data = b.get(o as usize..(o + c) as usize).ok_or("tile out of bounds")?;
        let mut t = if comp == 8 {
            let mut d = Vec::new();
            flate2::read::ZlibDecoder::new(data).read_to_end(&mut d).map_err(|e| e.to_string())?;
            d
        } else {
            data.to_vec()
        };
        if t.len() != (ts * ts * 3) as usize * bps {
            return Err("tile has the wrong size".into());
        }
        for row in t.chunks_mut(ts as usize * 3 * bps) {
            match pred {
                3 => crate::fp_unpredict_row(row, bps, 3),
                2 => crate::hundiff_u16_row(row, 3),
                _ => {}
            }
        }
        let (gx, gy) = (i as u32 % tx, i as u32 / tx);
        for y in 0..ts {
            for x in 0..ts {
                let (ix, iy) = (gx * ts + x, gy * ts + y);
                if ix >= w || iy >= h {
                    continue;
                }
                for ch in 0..3 {
                    let s = ((y * ts + x) * 3 + ch) as usize * bps;
                    out[((iy * w + ix) * 3 + ch) as usize] = match (fmt, bps) {
                        (3, 2) => half::f16::from_le_bytes([t[s], t[s + 1]]).to_f32(),
                        (3, 4) => f32::from_le_bytes(t[s..s + 4].try_into().unwrap()),
                        (1, 2) => u16::from_le_bytes([t[s], t[s + 1]]) as f32 / 65535.0,
                        _ => return Err(format!("unsupported sample format {fmt}/{bps}")),
                    };
                }
            }
        }
    }
    Ok((w, h, out))
}
