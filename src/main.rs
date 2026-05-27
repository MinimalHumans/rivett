// On Windows this is a GUI application — suppress the console window entirely in release builds.
// For release-local/debug, we might want the console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use rivett::app::RivettApp;
use rivett::settings::AppSettings;
use std::path::PathBuf;
use std::io::Write;

fn init_logging() {
    let mut builder = env_logger::Builder::from_default_env();

    // Also log to a file on Windows to help debugging GUI apps without a console
    if cfg!(target_os = "windows") {
        if let Some(mut log_path) = AppSettings::config_dir() {
            // Ensure the directory exists
            let _ = std::fs::create_dir_all(&log_path);

            log_path.push("rivett.log");

            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let file = std::sync::Arc::new(std::sync::Mutex::new(file));
                builder.format(move |buf, record| {
                    let msg = format!("[{}] {} - {}\n", record.level(), record.target(), record.args());
                    let _ = buf.write_all(msg.as_bytes());
                    if let Ok(mut f) = file.lock() {
                        let _ = f.write_all(msg.as_bytes());
                    }
                    Ok(())
                });
            }
        }
    }

    builder.init();
}
fn parse_args() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    args.get(1).map(PathBuf::from)
}

fn main() -> eframe::Result<()> {
    init_logging();
    log::info!("Rivett starting up...");

    let initial_image = parse_args();
    let settings = AppSettings::load();

    // Load icon
    let icon_data = include_bytes!("../resources/icon.png");
    let icon = image::load_from_memory(icon_data)
        .expect("Failed to load embedded icon")
        .to_rgba8();
    let (width, height) = icon.dimensions();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([400.0, 300.0])
            .with_drag_and_drop(true)
            .with_title("Rivett")
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            }),
        ..Default::default()
    };

    eframe::run_native(
        "Rivett",
        native_options,
        Box::new(move |cc| Ok(Box::new(RivettApp::new(cc, settings, initial_image)))),
    )
}
