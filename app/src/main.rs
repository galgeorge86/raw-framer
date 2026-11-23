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
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("Raw Framer", options, Box::new(move |cc| Ok(Box::new(app::RawFramerApp::new(cc, startup)))))
}
