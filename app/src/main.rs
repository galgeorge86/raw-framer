//! Raw Framer: preview Blackmagic RAW clips with the correct color transform and export
//! frames as Linear DNG / CinemaDNG stills. See .claude/PLAN.md and .claude/ARCHITECTURE.md.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assists;
mod audio;
mod dock;
mod export;
mod player;
mod safety;
mod saved_frames;
mod session;
mod theme;
mod thumbs;
mod viewer;
mod widgets;

fn main() -> eframe::Result {
    // Before anything else: log file, crash reports and unclean-exit detection.
    let startup = safety::begin();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).format_timestamp_millis().init();
    let (total, avail) = safety::memory().unwrap_or((0, 0));
    log::info!("Raw Framer {} starting (memory: {} MB free of {} MB)", env!("CARGO_PKG_VERSION"), avail >> 20, total >> 20);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Raw Framer")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1120.0, 640.0])
            .with_drag_and_drop(true)
            .with_icon(app_icon()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("Raw Framer", options, Box::new(move |cc| Ok(Box::new(app::RawFramerApp::new(cc, startup)))))
}

/// Window / taskbar icon: the largest 32-bit bitmap image in the .ico, converted to RGBA.
/// Falls back to egui's default if the file is unusual (e.g. PNG-compressed entries).
fn app_icon() -> egui::IconData {
    ico_rgba(include_bytes!("../assets/icon/raw-framer.ico")).unwrap_or_default()
}

fn ico_rgba(b: &[u8]) -> Option<egui::IconData> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?));
    let count = u16_at(4)? as usize;
    // Pick the biggest 32-bit entry (a width byte of 0 means 256).
    let (size, off) = (0..count)
        .filter_map(|i| {
            let e = 6 + 16 * i;
            let w = match *b.get(e)? { 0 => 256, w => w as u32 };
            (u16_at(e + 6)? == 32).then_some((w, u32_at(e + 12)? as usize))
        })
        .max_by_key(|(w, _)| *w)?;
    // BITMAPINFOHEADER, then bottom-up BGRA rows (height is doubled for the AND mask).
    if b.get(off..off + 4)? == b"\x89PNG" || u32_at(off + 14)? as u16 != 32 {
        return None;
    }
    let header = u32_at(off)? as usize;
    let pixels = b.get(off + header..off + header + (size * size * 4) as usize)?;
    let mut rgba = vec![0u8; pixels.len()];
    let row = (size * 4) as usize;
    for (y, src) in pixels.chunks_exact(row).enumerate() {
        let dst = &mut rgba[(size as usize - 1 - y) * row..][..row];
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
    }
    Some(egui::IconData { rgba, width: size, height: size })
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn embedded_icon_decodes() {
        let icon = super::ico_rgba(include_bytes!("../assets/icon/raw-framer.ico")).expect("icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        // Rounded tile: transparent corner, opaque centre.
        let at = |x: usize, y: usize| icon.rgba[(y * 256 + x) * 4 + 3];
        assert_eq!(at(0, 0), 0, "corner should be transparent");
        assert_eq!(at(128, 128), 255, "centre should be opaque");
    }
}
