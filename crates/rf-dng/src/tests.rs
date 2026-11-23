//! Read-back tests: an independent mini TIFF reader parses what the writer produced.

use super::*;
use std::collections::BTreeMap;
use std::io::Read;

struct Tag {
    typ: u16,
    count: u32,
    raw: Vec<u8>,
}

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn type_size(t: u16) -> usize {
    match t {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => panic!("bad type {t}"),
    }
}

fn read_ifd(b: &[u8], off: usize) -> (BTreeMap<u16, Tag>, u32) {
    assert_eq!(off % 2, 0, "IFD offset must be word aligned");
    let n = rd16(b, off) as usize;
    let mut tags = BTreeMap::new();
    let mut last = 0u16;
    for i in 0..n {
        let e = off + 2 + i * 12;
        let tag = rd16(b, e);
        assert!(tag > last, "tags must be sorted ascending");
        last = tag;
        let typ = rd16(b, e + 2);
        let count = rd32(b, e + 4);
        let size = type_size(typ) * count as usize;
        let raw = if size <= 4 {
            b[e + 8..e + 8 + size].to_vec()
        } else {
            let o = rd32(b, e + 8) as usize;
            assert!(o + size <= b.len(), "tag {tag} out of bounds");
            b[o..o + size].to_vec()
        };
        tags.insert(tag, Tag { typ, count, raw });
    }
    (tags, rd32(b, off + 2 + n * 12))
}

fn longs(t: &Tag) -> Vec<u32> {
    match t.typ {
        3 => t.raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32).collect(),
        4 => t.raw.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
        _ => panic!("not integer"),
    }
}

fn srational(t: &Tag, i: usize) -> f64 {
    let n = i32::from_le_bytes(t.raw[i * 8..i * 8 + 4].try_into().unwrap());
    let d = i32::from_le_bytes(t.raw[i * 8 + 4..i * 8 + 8].try_into().unwrap());
    n as f64 / d as f64
}

fn test_image(w: u32, h: u32) -> Vec<f32> {
    (0..w * h * 3)
        .map(|i| {
            let p = i / 3;
            let c = i % 3;
            ((p % w) as f32 / w as f32 * 0.7 + (p / w) as f32 / h as f32 * 0.2 + c as f32 * 0.03).powf(2.2)
        })
        .collect()
}

/// Decode the raw IFD back into f32 samples.
fn decode_raw(b: &[u8], tags: &BTreeMap<u16, Tag>) -> (u32, u32, Vec<f32>) {
    let w = longs(&tags[&256])[0];
    let h = longs(&tags[&257])[0];
    let bps = longs(&tags[&258])[0] as usize / 8;
    let comp = longs(&tags[&259])[0];
    let fmt = longs(&tags[&339])[0];
    let pred = tags.get(&317).map(|t| longs(t)[0]).unwrap_or(1);
    assert_eq!(longs(&tags[&262])[0], 34892);
    let ts = longs(&tags[&322])[0];
    let offs = longs(&tags[&324]);
    let counts = longs(&tags[&325]);
    let tx = w.div_ceil(ts);
    assert_eq!(offs.len() as u32, tx * h.div_ceil(ts));
    let mut out = vec![0f32; (w * h * 3) as usize];
    for (i, (&o, &c)) in offs.iter().zip(&counts).enumerate() {
        let data = &b[o as usize..(o + c) as usize];
        let mut t = if comp == 8 {
            let mut d = Vec::new();
            flate2::read::ZlibDecoder::new(data).read_to_end(&mut d).unwrap();
            d
        } else {
            data.to_vec()
        };
        assert_eq!(t.len(), (ts * ts * 3) as usize * bps);
        for row in t.chunks_mut(ts as usize * 3 * bps) {
            match pred {
                3 => fp_unpredict_row(row, bps, 3),
                2 => hundiff_u16_row(row, 3),
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
                    let v = match (fmt, bps) {
                        (3, 2) => half::f16::from_le_bytes([t[s], t[s + 1]]).to_f32(),
                        (3, 4) => f32::from_le_bytes(t[s..s + 4].try_into().unwrap()),
                        (1, 2) => u16::from_le_bytes([t[s], t[s + 1]]) as f32 / 65535.0,
                        _ => panic!(),
                    };
                    out[((iy * w + ix) * 3 + ch) as usize] = v;
                }
            }
        }
    }
    (w, h, out)
}

fn roundtrip(sample: SampleType, compression: Compression, layout: Layout, tol: f32) {
    let (w, h) = (300, 170); // not tile multiples on purpose
    let data = test_image(w, h);
    let img = LinearImage { width: w, height: h, data: &data };
    let thumb = Thumbnail { width: 30, height: 17, rgb: vec![128; 30 * 17 * 3] };
    let meta = DngMeta {
        make: "Blackmagic Design".into(),
        model: "Test Cam".into(),
        baseline_exposure: 2.5,
        color_matrix1: [1.866382, -0.518397, -0.234610, -0.600342, 1.378149, 0.176732, 0.002452, 0.086400, 0.836943],
        exif: ExifInfo { iso: Some(800), exposure_time_s: Some(1.0 / 48.0), lens_model: Some("Lens".into()), ..Default::default() },
        cinema: Some(CinemaInfo {
            frame_rate: (24000, 1001),
            timecode: Some(Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4, drop_frame: false }),
            reel_name: Some("A001".into()),
            camera_label: None,
        }),
        ..Default::default()
    };
    let opts = DngOptions { sample, compression, layout, tile_size: 64, deflate_level: 5 };
    // A real baseline JPEG preview (mid-grey 120x68).
    let (pw, ph) = (120u16, 68u16);
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 90).encode(&vec![100u8; pw as usize * ph as usize * 3], pw, ph, jpeg_encoder::ColorType::Rgb).unwrap();
    let previews = Previews {
        thumbnail: Some(thumb),
        jpeg: Some(JpegPreview { width: pw as u32, height: ph as u32, data: jpeg, subsampling: [2, 2] }),
        date_time: Some("2026-05-30T21:06:31".into()),
    };
    let mut buf = Vec::new();
    write_dng(&mut buf, &img, Some(&previews), &meta, &opts).unwrap();

    assert_eq!(&buf[0..4], b"II*\0");
    let (ifd0, next) = read_ifd(&buf, rd32(&buf, 4) as usize);
    assert_eq!(next, 0);
    assert_eq!(ifd0[&50706].raw, vec![1, 4, 0, 0]);
    assert!((srational(&ifd0[&50730], 0) - 2.5).abs() < 1e-4);
    assert!((srational(&ifd0[&50721], 0) - 1.866382).abs() < 1e-4);
    assert_eq!(ifd0[&51043].raw, vec![0x04, 0x03, 0x02, 0x01, 0, 0, 0, 0]);
    assert_eq!(ifd0[&50708].count as usize, "Raw Framer Linear".len() + 1);

    let (exif, _) = read_ifd(&buf, longs(&ifd0[&34665])[0] as usize);
    assert_eq!(longs(&exif[&34855])[0], 800);
    assert_eq!(&exif[&36864].raw, b"0230");

    let raw_tags = if layout == Layout::PreviewInIfd0 {
        assert_eq!(longs(&ifd0[&254])[0], 1);
        let so = longs(&ifd0[&273])[0] as usize;
        assert_eq!(&buf[so..so + 3], &[128, 128, 128]);
        let subs = longs(&ifd0[&330]);
        assert_eq!(subs.len(), 2, "raw + JPEG preview SubIFDs");
        // Preview IFD: baseline JPEG, YCbCr, sRGB, decodable at the declared size.
        let (pv, _) = read_ifd(&buf, subs[1] as usize);
        assert_eq!(longs(&pv[&254])[0], 1);
        assert_eq!(longs(&pv[&259])[0], 7);
        assert_eq!(longs(&pv[&262])[0], 6);
        assert_eq!(longs(&pv[&50970])[0], 2);
        let (o, n) = (longs(&pv[&273])[0] as usize, longs(&pv[&279])[0] as usize);
        assert_eq!(&buf[o..o + 2], &[0xFF, 0xD8], "JPEG SOI marker");
        let mut dec = jpeg_decoder::Decoder::new(&buf[o..o + n]);
        dec.decode().unwrap();
        let info = dec.info().unwrap();
        assert_eq!((info.width as u32, info.height as u32), (longs(&pv[&256])[0], longs(&pv[&257])[0]));
        read_ifd(&buf, subs[0] as usize).0
    } else {
        ifd0
    };
    assert_eq!(longs(&raw_tags[&254])[0], 0);
    let (rw, rh, back) = decode_raw(&buf, &raw_tags);
    assert_eq!((rw, rh), (w, h));
    let max_err = data.iter().zip(&back).map(|(a, b)| (a - b).abs() / a.abs().max(1e-3)).fold(0f32, f32::max);
    assert!(max_err <= tol, "{sample:?} {compression:?}: max rel err {max_err}");
}

#[test]
fn f16_deflate_preview() {
    roundtrip(SampleType::F16, Compression::Deflate, Layout::PreviewInIfd0, 1e-3);
}
#[test]
fn f32_deflate_preview() {
    roundtrip(SampleType::F32, Compression::Deflate, Layout::PreviewInIfd0, 0.0);
}
#[test]
fn f32_uncompressed_raw_ifd0() {
    roundtrip(SampleType::F32, Compression::None, Layout::RawInIfd0, 0.0);
}
#[test]
fn f16_uncompressed_raw_ifd0() {
    roundtrip(SampleType::F16, Compression::None, Layout::RawInIfd0, 1e-3);
}
#[test]
fn u16_deflate_preview() {
    // u16 error is absolute 1/65535; relative tolerance applies to values >= 1e-3.
    roundtrip(SampleType::U16, Compression::Deflate, Layout::PreviewInIfd0, 2e-2);
}

#[test]
fn predictor_roundtrip() {
    let src: Vec<u8> = (0..96u32).map(|i| (i * 37 % 251) as u8).collect();
    for bps in [2usize, 4] {
        let mut r = src.clone();
        fp_predict_row(&mut r, bps, 3);
        fp_unpredict_row(&mut r, bps, 3);
        assert_eq!(r, src);
    }
    let mut r = src.clone();
    hdiff_u16_row(&mut r, 3);
    hundiff_u16_row(&mut r, 3);
    assert_eq!(r, src);
}

#[test]
fn normalization() {
    let mut d = vec![0.18f32; 1_000_000];
    d[0] = 1e9; // single hot pixel must not set the peak
    d[1] = -1.0;
    d[2] = f32::NAN;
    for v in d.iter_mut().skip(10).take(100) {
        *v = 50.0;
    }
    let be = normalize(&mut d, Normalization::Auto);
    let peak = 2f64.powf(be);
    assert!(peak >= 50.0 && peak < 52.0, "peak {peak}");
    assert_eq!(d[1], 0.0);
    assert_eq!(d[2], 0.0);
    assert_eq!(d[0], 1.0);
    assert!(d.iter().all(|v| (0.0..=1.0).contains(v)));

    let mut d = vec![0.5f32; 100];
    let be = normalize(&mut d, Normalization::Fixed(222.86));
    assert!((be - 222.86f64.log2()).abs() < 1e-4);
}

#[test]
fn file_is_written_atomically() {
    let dir = std::env::temp_dir().join(format!("rfdng_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("t.dng");
    let data = test_image(64, 64);
    let img = LinearImage { width: 64, height: 64, data: &data };
    let n = write_dng_file(&p, &img, None, &DngMeta::default(), &DngOptions::default()).unwrap();
    assert!(n > 0 && p.exists());
    assert!(!dir.join("t.dng.part").exists());
    std::fs::remove_dir_all(&dir).ok();
}
