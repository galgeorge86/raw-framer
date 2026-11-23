//! Minimal little-endian TIFF IFD builder used by the DNG writer.

pub const BYTE: u16 = 1;
pub const ASCII: u16 = 2;
pub const SHORT: u16 = 3;
pub const LONG: u16 = 4;
pub const RATIONAL: u16 = 5;
pub const UNDEFINED: u16 = 7;
pub const SRATIONAL: u16 = 10;

#[derive(Debug, Clone)]
pub struct Entry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct Ifd {
    pub entries: Vec<Entry>,
}

impl Ifd {
    pub fn push(&mut self, tag: u16, typ: u16, count: u32, data: Vec<u8>) {
        self.entries.retain(|e| e.tag != tag);
        self.entries.push(Entry { tag, typ, count, data });
    }
    pub fn bytes(&mut self, tag: u16, v: &[u8]) {
        self.push(tag, BYTE, v.len() as u32, v.to_vec());
    }
    pub fn undefined(&mut self, tag: u16, v: &[u8]) {
        self.push(tag, UNDEFINED, v.len() as u32, v.to_vec());
    }
    pub fn ascii(&mut self, tag: u16, s: &str) {
        let mut d: Vec<u8> = s.bytes().filter(|b| *b != 0 && b.is_ascii()).collect();
        d.push(0);
        self.push(tag, ASCII, d.len() as u32, d);
    }
    pub fn shorts(&mut self, tag: u16, v: &[u16]) {
        self.push(tag, SHORT, v.len() as u32, v.iter().flat_map(|x| x.to_le_bytes()).collect());
    }
    pub fn longs(&mut self, tag: u16, v: &[u32]) {
        self.push(tag, LONG, v.len() as u32, v.iter().flat_map(|x| x.to_le_bytes()).collect());
    }
    pub fn rationals(&mut self, tag: u16, v: &[(u32, u32)]) {
        let d = v.iter().flat_map(|(n, d)| n.to_le_bytes().into_iter().chain(d.to_le_bytes())).collect();
        self.push(tag, RATIONAL, v.len() as u32, d);
    }
    pub fn srationals(&mut self, tag: u16, v: &[(i32, i32)]) {
        let d = v.iter().flat_map(|(n, d)| n.to_le_bytes().into_iter().chain(d.to_le_bytes())).collect();
        self.push(tag, SRATIONAL, v.len() as u32, d);
    }

    /// Total bytes this IFD occupies (directory + out-of-line values), word aligned.
    pub fn byte_size(&self) -> u32 {
        let dir = 2 + 12 * self.entries.len() as u32 + 4;
        let extra: u32 = self
            .entries
            .iter()
            .filter(|e| e.data.len() > 4)
            .map(|e| (e.data.len() as u32 + 1) & !1)
            .sum();
        dir + extra
    }

    /// Serialise at absolute file offset `base` (must be even).
    pub fn serialize(&self, base: u32, next_ifd: u32) -> Vec<u8> {
        let mut entries = self.entries.clone();
        entries.sort_by_key(|e| e.tag);
        let n = entries.len() as u32;
        let mut extra_off = base + 2 + 12 * n + 4;
        let mut dir = Vec::with_capacity(self.byte_size() as usize);
        let mut extra = Vec::new();
        dir.extend((n as u16).to_le_bytes());
        for e in &entries {
            dir.extend(e.tag.to_le_bytes());
            dir.extend(e.typ.to_le_bytes());
            dir.extend(e.count.to_le_bytes());
            if e.data.len() <= 4 {
                let mut v = e.data.clone();
                v.resize(4, 0);
                dir.extend(v);
            } else {
                dir.extend(extra_off.to_le_bytes());
                extra.extend(&e.data);
                if e.data.len() % 2 == 1 {
                    extra.push(0);
                }
                extra_off += (e.data.len() as u32 + 1) & !1;
            }
        }
        dir.extend(next_ifd.to_le_bytes());
        dir.extend(extra);
        dir
    }
}

/// f64 -> SRATIONAL with a fixed denominator.
pub fn srat(v: f64, den: i32) -> (i32, i32) {
    ((v * den as f64).round() as i32, den)
}

/// f64 -> RATIONAL, choosing a sensible denominator.
pub fn urat(v: f64) -> (u32, u32) {
    let v = v.max(0.0);
    if v > 0.0 && v < 1.0 && ((1.0 / v) - (1.0 / v).round()).abs() < 1e-3 {
        return (1, (1.0 / v).round() as u32);
    }
    ((v * 10000.0).round() as u32, 10000)
}
